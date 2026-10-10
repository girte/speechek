//! The storage half's own tests: the private root, the length ledger, the
//! quota, retention, recovery and the worker's passes.
//!
//! Every test drives the real writer thread with a `FakeClock` and a root
//! under the process's temporary directory. The filesystem is the real one and
//! the DACLs are the real ones, so a test proves what a run of the application
//! would do; a scripted [`ArchiveIo`] is used only where a failure has to be
//! deterministic (a short write, a failed delete, a child that looks like a
//! link).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use super::super::storage::{ArchiveIo, ListedEntry, Manifest, RealIo, SESSION_RESERVE, SNAPSHOT};
use super::super::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A directory of this test's own, removed when the test ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "speechek-storage-{}-{tag}-{}",
            std::process::id(),
            unique()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a scratch root");
        Self { path }
    }

    /// The archive root the worker will be given.
    fn logs(&self) -> PathBuf {
        self.path.join("logs")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique() -> u64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::process::id() as u64 * 1_000_000 + COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

fn epoch() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn epoch_ms(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis() as u64
}

/// A session directory name, built the way the storage half builds it and with
/// an explicit start: the age tests need a name older than the run.
fn named_dir(logs: &Path, started_ms: u64, generation: u64, counter: u64) -> PathBuf {
    logs.join(format!(
        "{started_ms}-{}-1-dictation-{generation}-{counter}",
        std::process::id()
    ))
}

/// A private directory and a private file, through the same helpers the sink
/// uses: a hand-built bundle has to pass the same access-list check.
fn private_dir(path: &Path) -> PathBuf {
    crate::settings::create_private_directory(path).expect("a private directory");
    path.to_path_buf()
}

fn private_file(path: &Path, bytes: &[u8]) {
    crate::settings::atomic_replace(path, bytes, true).expect("a private file");
}

fn key(generation: u64) -> SessionKey {
    SessionKey {
        owner: Owner::Dictation,
        generation,
    }
}

fn lab(generation: u64) -> SessionKey {
    SessionKey {
        owner: Owner::Lab,
        generation,
    }
}

fn policy(max_age: Duration, max_bytes: u64) -> Policy {
    Policy { max_age, max_bytes }
}

/// Every byte the sink actually owns under a directory.
fn disk_total(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            total += disk_total(&path);
        } else {
            total += metadata.len();
        }
    }
    total
}

fn session_dirs(logs: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(logs) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// The six dash-separated fields of a session directory name, as the archive
/// itself reads them.
fn session_name_parts(dir: &Path) -> Vec<String> {
    dir.file_name()
        .expect("a directory name")
        .to_str()
        .expect("unicode")
        .split('-')
        .map(str::to_owned)
        .collect()
}

fn manifest_of(dir: &Path) -> Option<Manifest> {
    let bytes = std::fs::read(dir.join("session.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn journal_records(dir: &Path) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut segments: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
            name.starts_with("events-") && name.ends_with(".jsonl")
        })
        .collect();
    segments.sort();
    let mut records = Vec::new();
    for segment in segments {
        let text = std::fs::read_to_string(segment).unwrap_or_default();
        for line in text.lines() {
            if let Ok(value) = serde_json::from_str(line) {
                records.push(value);
            }
        }
    }
    records
}

fn has_journal_event(dir: &Path, event: &str) -> bool {
    journal_records(dir)
        .iter()
        .any(|record| record.get("event").and_then(|value| value.as_str()) == Some(event))
}

/// Finishes a take the way the shell does and lets the worker close it: the
/// terminal rises, the grace passes and the passes seal, drain and settle it.
fn finish_take(sink: &Arc<Diagnostics>, handle: &SessionHandle, outcome: Outcome) {
    handle.end_capture(outcome);
    sink.end(handle.key(), outcome);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_passes(6);
}

// ---------------------------------------------------------------------------
// A scripted filesystem
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Faults {
    /// Report every write as this many bytes written and no more.
    short_write: Option<usize>,
    /// Every replacement fails.
    replace_error: bool,
    /// The next replacement fails and leaves this many bytes of its own
    /// staging copy behind, exactly as an interrupted `atomic_replace` can.
    leak_staging_then_fail: Option<u32>,
    /// Every file delete fails without deleting anything.
    delete_file_error: bool,
    /// Every tree delete fails without deleting anything.
    delete_error: bool,
    /// Every tree delete takes the first child in name order and then fails,
    /// exactly as an interrupted recursive delete can.
    partial_delete: bool,
    /// The length of this file name cannot be read.
    unreadable_length: Option<String>,
    /// Creating any file fails.
    create_file_error: bool,
    /// Every write position move fails: the header patch of a FLAC archive is
    /// the only one there is.
    seek_error: bool,
    /// Every sync fails.
    sync_error: bool,
    /// Every publish (the move that gives the archive its verified name)
    /// fails.
    publish_error: bool,
    /// Opening a file for reading fails.
    open_read_error: bool,
    /// The bytes of this file are changed at this offset every time it is
    /// opened for reading, exactly as a disk that hands back different bytes
    /// than were written does.
    corrupt_open_read: Option<(String, usize)>,
    /// A child with this name looks like a reparse point.
    mark_reparse: Option<String>,
}

/// The real filesystem with a scripted failure on top of it. Everything the
/// test does not fault goes through [`RealIo`], so the same private helpers,
/// the same access lists and the same staging names are exercised.
struct ScriptedIo {
    inner: RealIo,
    faults: Mutex<Faults>,
}

impl ScriptedIo {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RealIo,
            faults: Mutex::new(Faults::default()),
        })
    }

    fn fault(&self, change: impl FnOnce(&mut Faults)) {
        change(&mut self.faults.lock());
    }

    fn named(path: &Path) -> String {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned()
    }
}

impl ArchiveIo for ScriptedIo {
    fn create_dir(&self, path: &Path) -> Result<(), DiagnosticCode> {
        self.inner.create_dir(path)
    }

    fn create_file(&self, path: &Path) -> Result<File, DiagnosticCode> {
        if self.faults.lock().create_file_error {
            return Err(DiagnosticCode::Io);
        }
        self.inner.create_file(path)
    }

    fn open_read(&self, path: &Path) -> Result<File, DiagnosticCode> {
        if self.faults.lock().open_read_error {
            return Err(DiagnosticCode::Io);
        }
        let corrupt = {
            let faults = self.faults.lock();
            match &faults.corrupt_open_read {
                Some((name, at)) if *name == Self::named(path) => Some(*at),
                _ => None,
            }
        };
        if let Some(at) = corrupt {
            // The file itself is changed, so what the reader gets back really
            // is not what was written.
            let mut bytes = std::fs::read(path).map_err(|_| DiagnosticCode::Io)?;
            if let Some(byte) = bytes.get_mut(at) {
                *byte ^= 0xff;
            }
            std::fs::write(path, &bytes).map_err(|_| DiagnosticCode::Io)?;
        }
        self.inner.open_read(path)
    }

    fn open_append(&self, path: &Path) -> Result<File, DiagnosticCode> {
        self.inner.open_append(path)
    }

    fn write(&self, file: &mut File, bytes: &[u8]) -> Result<usize, DiagnosticCode> {
        use std::io::Write;
        let limit = self.faults.lock().short_write;
        if let Some(limit) = limit {
            let prefix = bytes.len().min(limit);
            file.write_all(&bytes[..prefix])
                .map_err(|_| DiagnosticCode::Io)?;
            return Ok(prefix);
        }
        self.inner.write(file, bytes)
    }

    fn seek(&self, file: &mut File, position: u64) -> Result<(), DiagnosticCode> {
        if self.faults.lock().seek_error {
            return Err(DiagnosticCode::Io);
        }
        self.inner.seek(file, position)
    }

    fn sync(&self, file: &mut File) -> Result<(), DiagnosticCode> {
        if self.faults.lock().sync_error {
            return Err(DiagnosticCode::Io);
        }
        self.inner.sync(file)
    }

    fn replace(&self, path: &Path, bytes: &[u8]) -> Result<(), DiagnosticCode> {
        let leak = self.faults.lock().leak_staging_then_fail.take();
        if let Some(leak) = leak {
            // The staging copy is written where `atomic_replace` writes it,
            // and then the move over the destination fails.
            let dir = path.parent().unwrap_or_else(|| Path::new("."));
            let staging = dir.join(format!("session.json.{}.1.tmp", std::process::id()));
            let mut file =
                crate::settings::create_private_file(&staging).map_err(|_| DiagnosticCode::Io)?;
            self.inner
                .write(&mut file, &vec![b'x'; leak as usize])
                .map_err(|_| DiagnosticCode::Io)?;
            return Err(DiagnosticCode::Io);
        }
        if self.faults.lock().replace_error {
            return Err(DiagnosticCode::Io);
        }
        self.inner.replace(path, bytes)
    }

    fn publish(&self, source: &Path, destination: &Path) -> Result<(), DiagnosticCode> {
        if self.faults.lock().publish_error {
            return Err(DiagnosticCode::Io);
        }
        self.inner.publish(source, destination)
    }

    fn remove_file(&self, path: &Path) -> Result<(), DiagnosticCode> {
        if self.faults.lock().delete_file_error {
            return Err(DiagnosticCode::Io);
        }
        self.inner.remove_file(path)
    }

    fn remove_tree(&self, dir: &Path) -> Result<(), DiagnosticCode> {
        if self.faults.lock().delete_error {
            return Err(DiagnosticCode::Io);
        }
        if self.faults.lock().partial_delete {
            // One child really goes and then the removal fails, exactly as an
            // interrupted recursive delete can leave a bundle.
            let taken = self
                .inner
                .list(dir)
                .unwrap_or_default()
                .into_iter()
                .find(|child| !child.dir && !child.reparse)
                .map(|child| dir.join(child.name));
            if let Some(path) = taken {
                self.inner.remove_file(&path)?;
            }
            return Err(DiagnosticCode::Io);
        }
        self.inner.remove_tree(dir)
    }

    fn list(&self, dir: &Path) -> Result<Vec<ListedEntry>, DiagnosticCode> {
        let mark = self.faults.lock().mark_reparse.clone();
        let mut listed = self.inner.list(dir)?;
        if let Some(mark) = mark {
            for entry in &mut listed {
                if entry.name == std::ffi::OsString::from(mark.as_str()) {
                    entry.reparse = true;
                }
            }
        }
        Ok(listed)
    }

    fn length(&self, path: &Path) -> Result<Option<u64>, DiagnosticCode> {
        if self.faults.lock().unreadable_length.as_deref() == Some(Self::named(path).as_str()) {
            return Err(DiagnosticCode::Io);
        }
        self.inner.length(path)
    }

    fn verify(&self, path: &Path) -> Result<(), DiagnosticCode> {
        self.inner.verify(path)
    }
}

// ---------------------------------------------------------------------------
// The root, the manifest and the ledger
// ---------------------------------------------------------------------------

#[test]
fn a_first_take_opens_a_private_bundle_with_a_compact_manifest() {
    let scratch = Scratch::new("first");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    let handle = sink.begin(key(7), Mode::Smart).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);

    let dirs = session_dirs(&logs);
    assert_eq!(dirs.len(), 1, "one take, one bundle");
    let parts = session_name_parts(&dirs[0]);
    assert_eq!(parts.len(), 6, "start-pid-run-owner-generation-counter");
    assert_eq!(parts[0], "1700000000000", "the wall clock starts the name");
    assert!(parts[1].parse::<u32>().is_ok());
    assert!(parts[2].parse::<u64>().is_ok());
    assert_eq!(parts[3], "dictation");
    assert_eq!(parts[4], "7");
    assert!(parts[5].parse::<u64>().is_ok());

    let manifest = manifest_of(&dirs[0]).expect("a manifest");
    assert_eq!(manifest.schema_version, 2);
    assert_eq!(manifest.owner, Owner::Dictation);
    assert_eq!(manifest.generation, 7);
    assert_eq!(manifest.mode, Mode::Smart);
    assert_eq!(manifest.started_utc_ms, 1_700_000_000_000);
    assert_eq!(manifest.retention_utc_ms, manifest.started_utc_ms);
    assert_eq!(manifest.outcome, None, "a live take has no outcome yet");
    assert_eq!(manifest.ended_utc_ms, None);
    assert_eq!(manifest.audio_file, None);
    assert!(!manifest.verified);
    assert!(dirs[0].join("session.json").exists());
    assert!(has_journal_event(&dirs[0], "session_begin"));
    assert!(has_journal_event(&dirs[0], "phase"));

    let record = journal_records(&dirs[0])
        .into_iter()
        .find(|record| record["event"] == "session_begin")
        .expect("a session_begin record");
    assert_eq!(record["schemaVersion"], 2);
    assert_eq!(record["owner"], "dictation");
    assert_eq!(record["generation"], 7);
    assert_eq!(record["requestId"], serde_json::Value::Null);
    assert_eq!(record["source"], "shell");
    assert!(record["sequence"].as_u64().is_some_and(|seq| seq >= 1));
    assert!(record["utcMs"].as_u64().is_some());
    assert!(record["elapsedUs"].as_u64().is_some());
    assert!(record["data"]["pid"].as_u64().is_some());

    sink.shutdown(Duration::from_secs(2));
    assert!(sink.is_enabled());
    assert_eq!(sink.error(), None);
}

#[test]
fn no_write_ever_passes_the_cap_and_the_reserve_is_counted_before_it_is_used() {
    let scratch = Scratch::new("cap");
    let logs = scratch.logs();
    let cap = 120_000u64;
    let sink =
        Diagnostics::for_test_with_storage(&logs, policy(Duration::from_secs(86_400), cap), epoch());
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    let mut archive = handle.capture();
    let trace = handle.request(Route::Smart).unwrap();
    for round in 1..=40u64 {
        for frame in 0..64u64 {
            archive.push(round * 64 + frame, &[0u8; 1_024]);
        }
        for _ in 0..3 {
            trace.stage(Stage::UploadStart, StagePhase::Begin, None, false);
        }
        handle.event(Event::Phase(Phase {
            phase: CapturePhase::Recording,
        }));
        sink.worker_passes(1);
        let total = disk_total(&logs);
        assert!(
            total <= cap,
            "round {round}: {total} bytes on disk is over the {cap}-byte cap"
        );
    }
    drop(archive);
    drop(trace);
    sink.shutdown(Duration::from_secs(2));
    assert!(disk_total(&logs) <= cap);
}

#[test]
fn a_take_the_quota_cannot_afford_never_creates_a_directory() {
    let scratch = Scratch::new("tight");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), SESSION_RESERVE - 1),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    let mut archive = handle.capture();
    archive.push(1, &[0u8; 1_024]);
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);

    assert!(session_dirs(&logs).is_empty(), "no directory was admitted");
    let entry = sink.entry_for(key(1)).unwrap();
    assert!(
        !entry.archive_enabled(),
        "the producer stops copying PCM into a queue nobody owns"
    );
    assert_eq!(entry.audio_status(), AudioStatus::Recording);
    assert_eq!(sink.error(), Some(DiagnosticCode::Quota));
    // The frame the capture queued is drained and its buffer recycled once the
    // archive is closed: nothing is invented on disk, and no second copy of it
    // is made anywhere.
    assert_eq!(entry.capture.snapshot().produced_frames, 1);
    assert_eq!(entry.capture.snapshot().dropped_frames, 0);
    archive.push(2, &[0u8; 1_024]);
    let counters = entry.capture.snapshot();
    assert_eq!(counters.produced_frames, 2);
    assert_eq!(counters.dropped_frames, 1);
    assert_eq!(counters.last_sequence, Some(2));
    assert_eq!(disk_total(&logs), 0);

    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_take_with_no_terminal_is_recorded_abandoned() {
    let scratch = Scratch::new("abandoned");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    let handle = sink.begin(key(3), Mode::Live).unwrap();
    handle.end_capture(Outcome::Success);
    // The idle deadline runs out with no terminal ever reported, and then the
    // seal grace passes.
    sink.advance_clock(IDLE_DEADLINE + Duration::from_millis(1));
    sink.advance_all();
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_passes(6);

    let dirs = session_dirs(&logs);
    assert_eq!(dirs.len(), 1);
    let manifest = manifest_of(&dirs[0]).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Abandoned));
    assert_eq!(
        manifest.diagnostic_error,
        Some(DiagnosticCode::MissingTerminal)
    );
    assert!(manifest.ended_utc_ms.is_some());
    assert!(!manifest.verified);
    assert!(has_journal_event(&dirs[0], "session_end"));
    assert!(sink.entry_for(key(3)).is_none(), "the take was let go");

    sink.shutdown(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// Retention and recovery
// ---------------------------------------------------------------------------

#[test]
fn an_aged_bundle_is_pruned_while_a_younger_one_survives() {
    let scratch = Scratch::new("age");
    let logs = scratch.logs();
    let start = epoch();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), 1_000_000),
        start,
    );

    let old = sink.begin(key(1), Mode::Live).unwrap();
    finish_take(&sink, &old, Outcome::Success);
    let old_dir = session_dirs(&logs).remove(0);

    sink.set_wall_clock(start + Duration::from_secs(86_400 * 3));
    let young = sink.begin(key(2), Mode::Live).unwrap();
    finish_take(&sink, &young, Outcome::Success);
    let young_dir = session_dirs(&logs)
        .into_iter()
        .find(|dir| dir != &old_dir)
        .expect("a second bundle");

    // Twelve days after the first and nine after the second: the first is past
    // the ten-day age, the second is not.
    sink.set_wall_clock(start + Duration::from_secs(86_400 * 12));
    sink.advance_clock(Duration::from_secs(61));
    sink.worker_passes(3);

    let remaining = session_dirs(&logs);
    assert_eq!(remaining, vec![young_dir], "the old bundle is gone");
    assert!(!old_dir.exists());
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_manifest_left_open_by_an_earlier_run_is_rewritten_as_abandoned() {
    let scratch = Scratch::new("recover");
    let logs = scratch.logs();
    let start = epoch();
    // A bundle exactly as a crash leaves it: a directory, a manifest whose
    // outcome is still null and an unverified partial beside it.
    private_dir(&logs);
    let dir = named_dir(&logs, epoch_ms(start), 4, 1);
    private_dir(&dir);
    let mut manifest = Manifest {
        schema_version: 1,
        run_id: "earlier-run".to_owned(),
        session_id: dir.file_name().unwrap().to_str().unwrap().to_owned(),
        owner: Owner::Dictation,
        generation: 4,
        mode: Mode::Verbatim,
        package_version: "0.0.1".to_owned(),
        started_utc_ms: epoch_ms(start),
        retention_utc_ms: epoch_ms(start),
        ended_utc_ms: None,
        outcome: None,
        audio_status: AudioStatus::Verifying,
        audio_file: None,
        captured_frames: 512,
        captured_samples: 262_144,
        archived_samples: 0,
        dropped_frames: 0,
        dropped_samples: 0,
        dropped_events: 0,
        last_sequence: Some(512),
        verified: false,
        diagnostic_error: None,
        audio_gap: None,
    };
    private_file(
        &dir.join("session.json"),
        &serde_json::to_vec(&manifest).unwrap(),
    );
    private_file(&dir.join("audio.flac.part"), &[0u8; 4_096]);

    let reopened = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), 1_000_000),
        start,
    );
    reopened.worker_passes(2);
    manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Abandoned));
    assert_eq!(manifest.diagnostic_error, Some(DiagnosticCode::MissingTerminal));
    assert_eq!(
        manifest.schema_version, 2,
        "the run that rewrote the manifest owns the schema it now carries"
    );
    assert!(manifest.ended_utc_ms.is_some());
    assert!(!manifest.verified, "a partial is never verified by recovery");
    assert_eq!(manifest.audio_status, AudioStatus::Incomplete);
    assert_eq!(manifest.audio_file, None);
    assert_eq!(manifest.captured_frames, 512, "the counters were kept");
    assert_eq!(manifest.started_utc_ms, epoch_ms(start));
    assert!(dir.join("audio.flac.part").exists(), "the partial stays");
    assert!(
        !dir.join("audio.flac").exists(),
        "a partial is never renamed into a verified file"
    );
    reopened.shutdown(Duration::from_secs(2));
}

#[test]
fn a_retention_time_from_a_future_clock_is_clamped_once() {
    let scratch = Scratch::new("clock");
    let logs = scratch.logs();
    let start = epoch();
    private_dir(&logs);
    let dir = named_dir(&logs, epoch_ms(start), 9, 1);
    private_dir(&dir);
    let before = Manifest {
        schema_version: 1,
        run_id: "earlier-run".to_owned(),
        session_id: dir.file_name().unwrap().to_str().unwrap().to_owned(),
        owner: Owner::Dictation,
        generation: 9,
        mode: Mode::Live,
        package_version: "0.0.1".to_owned(),
        started_utc_ms: epoch_ms(start),
        retention_utc_ms: u64::MAX,
        ended_utc_ms: Some(1),
        outcome: Some(Outcome::Success),
        audio_status: AudioStatus::None,
        audio_file: None,
        captured_frames: 0,
        captured_samples: 0,
        archived_samples: 0,
        dropped_frames: 0,
        dropped_samples: 0,
        dropped_events: 0,
        last_sequence: None,
        verified: false,
        diagnostic_error: None,
        audio_gap: None,
    };
    private_file(
        &dir.join("session.json"),
        &serde_json::to_vec(&before).unwrap(),
    );

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), 1_000_000),
        start,
    );
    sink.worker_passes(2);
    let after = manifest_of(&dir).expect("a manifest");
    assert_eq!(
        after.retention_utc_ms,
        epoch_ms(start),
        "clamped back to the wall clock"
    );
    assert_eq!(
        after.started_utc_ms, before.started_utc_ms,
        "the start stays historical"
    );
    assert_eq!(after.outcome, Some(Outcome::Success));
    assert!(has_journal_event(&dir, "clock_adjusted"));

    // The clamp happens once: a second pass leaves the manifest alone.
    let bytes = std::fs::read(dir.join("session.json")).unwrap();
    sink.worker_passes(2);
    assert_eq!(
        std::fs::read(dir.join("session.json")).unwrap(),
        bytes,
        "the clamp is not repeated"
    );

    // The clamp is history, not a rolling one: once the wall clock has caught
    // up, the bundle ages normally and the ten-day policy removes it. A clamp
    // applied on every pass would pin the age at zero for good.
    sink.set_wall_clock(start + Duration::from_secs(86_400 * 10 + 1));
    sink.advance_clock(Duration::from_secs(61));
    sink.worker_passes(3);
    assert!(!dir.exists(), "the bundle aged out once the clamp was history");
    sink.shutdown(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// Strangers, failures and the closure of the archive
// ---------------------------------------------------------------------------

#[test]
fn a_foreign_child_disables_the_sink_and_is_left_alone() {
    let scratch = Scratch::new("foreign");
    let logs = scratch.logs();
    {
        let sink = Diagnostics::for_test_with_storage(
            &logs,
            policy(Duration::from_secs(86_400), 1_000_000),
            epoch(),
        );
        sink.worker_passes(1);
        sink.shutdown(Duration::from_secs(2));
    }
    let stranger = logs.join("notes.txt");
    private_file(&stranger, b"somebody else's bytes");

    let reopened = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    reopened.worker_passes(2);
    assert!(reopened.is_sink_disabled());
    assert_eq!(reopened.error(), Some(DiagnosticCode::ForeignArchive));
    assert_eq!(
        std::fs::read(&stranger).unwrap(),
        b"somebody else's bytes".to_vec(),
        "a stranger is never deleted"
    );
    assert!(reopened.begin(key(1), Mode::Live).is_none());
    reopened.shutdown(Duration::from_secs(2));
}

#[test]
fn a_linked_child_disables_the_sink_and_is_left_alone() {
    let scratch = Scratch::new("link");
    let logs = scratch.logs();
    private_dir(&logs);
    let linked = named_dir(&logs, epoch_ms(epoch()), 1, 1);
    private_dir(&linked);

    let io = ScriptedIo::new();
    io.fault(|faults| {
        faults.mark_reparse = Some(
            linked
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        )
    });
    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
        io,
    );
    sink.worker_passes(2);
    assert!(sink.is_sink_disabled());
    assert_eq!(sink.error(), Some(DiagnosticCode::ForeignArchive));
    assert!(linked.exists(), "a link is never followed or removed");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_failed_delete_leaves_the_bytes_charged() {
    let scratch = Scratch::new("delete");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400 * 10), 1_000_000),
        epoch(),
        io.clone(),
    );
    let handle = sink.begin(key(1), Mode::Live).unwrap();
    finish_take(&sink, &handle, Outcome::Success);
    let dir = session_dirs(&logs).remove(0);

    io.fault(|faults| faults.delete_error = true);
    sink.set_wall_clock(epoch() + Duration::from_secs(86_400 * 11));
    sink.advance_clock(Duration::from_secs(61));
    sink.worker_passes(2);
    assert!(dir.exists(), "the bundle stays until the delete succeeds");
    assert_eq!(sink.error(), None, "a refused delete is not a sink failure");

    io.fault(|faults| faults.delete_error = false);
    sink.advance_clock(Duration::from_secs(61));
    sink.worker_passes(2);
    assert!(!dir.exists(), "and it goes once the delete works");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_short_write_is_reconciled_and_the_take_falls_back_to_its_reserve() {
    let scratch = Scratch::new("short");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
        io.clone(),
    );
    io.fault(|faults| faults.short_write = Some(5));
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);

    assert_eq!(sink.error(), Some(DiagnosticCode::Io));
    let entry = sink.entry_for(key(1)).unwrap();
    assert!(!entry.archive_enabled(), "the archive closed");
    assert_eq!(entry.error_code(), Some(DiagnosticCode::Io));
    let dirs = session_dirs(&logs);
    assert_eq!(dirs.len(), 1);
    let dir = &dirs[0];
    // The reserve is the take's own last record and manifest. A closed archive
    // does not spend it on an unknown outcome, so the manifest still says the
    // take is open; the error lives in memory until the terminal is known.
    let open = manifest_of(dir).expect("the take's own manifest");
    assert_eq!(open.outcome, None, "an unsealed take keeps its outcome open");
    assert!(
        disk_total(&logs) > 0,
        "the part that was written stays charged"
    );

    // The archive is closed for good: later records are counted, not written.
    io.fault(|faults| faults.short_write = None);
    let before = disk_total(&logs);
    for _ in 0..4 {
        handle.event(Event::Phase(Phase {
            phase: CapturePhase::Recording,
        }));
    }
    sink.worker_passes(2);
    assert_eq!(disk_total(&logs), before);
    assert!(entry.dropped_events() >= 4);
    // The shell's terminal closes the archive, and only then does the bounded
    // finalization spend the reserve: the final record and the manifest carry
    // the real outcome *and* the failure that closed the ordinary writes.
    handle.end_capture(Outcome::Success);
    sink.end(key(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_passes(4);
    let closed = manifest_of(dir).expect("the final manifest");
    assert_eq!(closed.outcome, Some(Outcome::Success));
    assert_eq!(closed.diagnostic_error, Some(DiagnosticCode::Io));
    assert!(closed.ended_utc_ms.is_some());
    // The short write left a torn line in the journal, so the file's tail is
    // not a record any more; the manifest is what carries the terminal.
    assert!(dir.join("events-000001.jsonl").exists());
    assert!(sink.entry_for(key(1)).is_none(), "sealed and let go");

    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_length_that_cannot_be_read_is_not_a_zero() {
    let scratch = Scratch::new("length");
    let logs = scratch.logs();
    private_dir(&logs);
    let dir = named_dir(&logs, epoch_ms(epoch()), 1, 1);
    private_dir(&dir);
    private_file(&dir.join("session.json"), b"{}");
    let io = ScriptedIo::new();
    io.fault(|faults| faults.unreadable_length = Some("session.json".to_owned()));

    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
        io,
    );
    sink.worker_passes(2);
    assert!(sink.is_sink_disabled());
    assert_eq!(sink.error(), Some(DiagnosticCode::Io));
    assert!(dir.exists(), "nothing is deleted on a failed scan");
    sink.shutdown(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// Sealing, the session limit and the queued PCM
// ---------------------------------------------------------------------------

#[test]
fn a_sealed_take_is_not_finished_until_its_queue_is_drained() {
    let scratch = Scratch::new("drain");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    sink.worker_pause();
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    let mut archive = handle.capture();
    archive.push(1, &[0u8; 1_024]);
    for _ in 0..24 {
        handle.event(Event::Phase(Phase {
            phase: CapturePhase::Recording,
        }));
    }
    archive.finish(1, Outcome::Success);
    sink.end(key(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_resume();

    // The first pass writes what fits and leaves the rest; the take is sealed
    // but the writer is not done with it.
    sink.worker_passes(1);
    let dir = session_dirs(&logs).remove(0);
    assert!(manifest_of(&dir).expect("a manifest").outcome.is_none());
    assert!(sink.entry_for(key(1)).is_some(), "still registered");
    assert!(sink.entry_for(key(1)).unwrap().is_sealed());

    sink.worker_passes(5);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Success));
    assert!(manifest.ended_utc_ms.is_some());
    assert!(sink.entry_for(key(1)).is_none(), "the worker let it go");
    assert!(has_journal_event(&dir, "session_end"));
    assert!(!journal_records(&dir).is_empty());
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn sealed_takes_that_are_not_drained_still_fill_the_session_limit() {
    let scratch = Scratch::new("limit");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    sink.worker_pause();
    for generation in 1..=MAX_OPEN_SESSIONS as u64 {
        let handle = sink
            .begin(key(generation), Mode::Live)
            .unwrap_or_else(|| panic!("take {generation}"));
        handle.end_capture(Outcome::Success);
        sink.end(key(generation), Outcome::Success);
    }
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.advance_all();
    for generation in 1..=MAX_OPEN_SESSIONS as u64 {
        assert!(sink.entry_for(key(generation)).unwrap().is_sealed());
    }
    assert!(
        sink.begin(key(MAX_OPEN_SESSIONS as u64 + 1), Mode::Live)
            .is_none(),
        "a sealed take still holds its slot until its queue is drained"
    );
    assert_eq!(sink.error(), Some(DiagnosticCode::SessionLimit));

    sink.worker_resume();
    sink.worker_passes(12);
    assert!(sink.entry_for(key(1)).is_none(), "drained and let go");
    assert!(
        sink.begin(key(MAX_OPEN_SESSIONS as u64 + 1), Mode::Live)
            .is_some(),
        "the slots came back"
    );
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_frozen_writer_publishes_the_surviving_subset_and_counts_every_gap() {
    let scratch = Scratch::new("freeze");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    // The writer is held, so the pool and the queue are the whole cap.
    sink.worker_pause();
    let handle = sink.begin(key(1), Mode::Live).unwrap();
    let entry = sink.entry_for(key(1)).unwrap();
    let waveform = frame_pcm(300);
    let mut archive = handle.capture();
    for sequence in 1..=300u64 {
        let at = (sequence as usize - 1) * FRAME_BYTES;
        archive.push(sequence, &waveform[at..at + FRAME_BYTES]);
    }
    // Nothing waited for the disk: the frames past the pool are the holes they
    // are, counted exactly, and the capture kept its own watermark.
    let counters = entry.capture.snapshot();
    assert_eq!(counters.produced_frames, 300);
    assert_eq!(
        counters.dropped_frames, 44,
        "the pool and the queue are the cap"
    );
    assert_eq!(counters.dropped_samples, 44 * 512);
    assert_eq!(counters.last_sequence, Some(300));

    archive.finish(300, Outcome::Success);
    drop(archive);
    sink.end(key(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_resume();
    sink.worker_passes(16);

    let dir = bundle(&logs, 1);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Success));
    assert_eq!(
        manifest.audio_status,
        AudioStatus::Incomplete,
        "a surviving subset is never Complete"
    );
    assert_eq!(
        manifest.audio_file.as_deref(),
        Some("audio.incomplete.flac")
    );
    assert!(manifest.verified, "the surviving subset was checked");
    assert_eq!(manifest.archived_samples, 256 * 512);
    assert_eq!(manifest.captured_samples, 300 * 512);
    assert_eq!(manifest.dropped_frames, 44);
    assert_eq!(manifest.dropped_samples, 44 * 512);
    let gap = manifest.audio_gap.expect("the hole is recorded");
    assert_eq!(gap.first_sequence, 257);
    assert_eq!(gap.last_sequence, 300);
    assert_eq!(gap.archived_offset_samples, 256 * 512);
    assert_eq!(gap.lost_samples, 44 * 512);

    assert!(
        !dir.join("audio.flac").exists(),
        "a subset is never given the Complete name"
    );
    assert!(!dir.join("audio.flac.part").exists(), "the partial was published");
    // The independent verdict on what really is in the file: the 256 frames
    // that reached it, sample for sample, in order.
    let published = std::fs::File::open(dir.join("audio.incomplete.flac"))
        .expect("the published subset");
    flac::verify_pcm(published, 256 * 512, &waveform[..256 * FRAME_BYTES])
        .expect("the surviving subset, sample for sample");
    // And the hole the journal records carries the same numbers.
    let record = journal_records(&dir)
        .into_iter()
        .find(|record| record["event"] == "audio_gap")
        .expect("an audio_gap record");
    assert_eq!(record["data"]["first_sequence"], 257);
    assert_eq!(record["data"]["last_sequence"], 300);
    assert_eq!(record["data"]["archived_offset_samples"], 256 * 512);
    assert_eq!(record["data"]["lost_samples"], 44 * 512);
    assert!(has_journal_event(&dir, "audio_verified"));
    assert!(sink.entry_for(key(1)).is_none(), "the take was let go");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_lab_take_and_a_dictation_take_are_separate_bundles() {
    let scratch = Scratch::new("owner");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    let first = sink.begin(lab(1), Mode::Comparison).unwrap();
    let second = sink.begin(key(1), Mode::Smart).unwrap();
    first.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    second.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);

    let dirs = session_dirs(&logs);
    assert_eq!(dirs.len(), 2, "one bundle per take");
    let owners: Vec<String> = dirs
        .iter()
        .map(|dir| session_name_parts(dir)[3].clone())
        .collect();
    assert!(owners.contains(&"lab".to_owned()));
    assert!(owners.contains(&"dictation".to_owned()));
    for dir in &dirs {
        assert!(manifest_of(dir).is_some());
    }
    sink.shutdown(Duration::from_secs(2));
}
#[test]
fn an_overflowing_metadata_queue_still_records_the_terminal() {
    let scratch = Scratch::new("overflow");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    // The writer is held, so the shared queue backs up and overflows: the
    // terminal then lives only in the take's own state until the writer runs.
    sink.worker_pause();
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    let mut archive = handle.capture();
    archive.push(1, &[0u8; 1_024]);
    for _ in 0..META_QUEUE + 20 {
        handle.event(Event::Phase(Phase {
            phase: CapturePhase::Recording,
        }));
    }
    archive.finish(1, Outcome::Success);
    sink.end(key(1), Outcome::Success);
    assert_eq!(
        sink.entry_for(key(1)).unwrap().terminal(),
        Some(Outcome::Success),
        "the terminal never depended on the queue"
    );
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_resume();
    sink.worker_passes(20);

    let dir = session_dirs(&logs).remove(0);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Success));
    assert!(
        manifest.dropped_events > 0,
        "the overflow reached the manifest"
    );
    assert!(has_journal_event(&dir, "session_end"));
    assert!(sink.entry_for(key(1)).is_none());
    sink.shutdown(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// The exact age boundary, the eviction order and the quota's own admission
// ---------------------------------------------------------------------------

/// A finished bundle exactly as a completed run leaves it: a manifest that
/// claims one verified audio file beside the partial it came from, one journal
/// segment and the counters of a real take. The bundle needs no rewriting, so
/// a test that only cares about retention is not disturbed by a recovery.
fn finished_bundle(
    logs: &Path,
    started_ms: u64,
    retention_ms: u64,
    generation: u64,
    counter: u64,
) -> PathBuf {
    private_dir(logs);
    let dir = named_dir(logs, started_ms, generation, counter);
    private_dir(&dir);
    let session_id = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let manifest = Manifest {
        schema_version: 1,
        run_id: "an-earlier-run".to_owned(),
        session_id,
        owner: Owner::Dictation,
        generation,
        mode: Mode::Smart,
        package_version: env!("CARGO_PKG_VERSION").to_owned(),
        started_utc_ms: started_ms,
        retention_utc_ms: retention_ms,
        ended_utc_ms: Some(started_ms + 1),
        outcome: Some(Outcome::Success),
        audio_status: AudioStatus::Complete,
        audio_file: Some("audio.flac".to_owned()),
        captured_frames: 512,
        captured_samples: 262_144,
        archived_samples: 262_144,
        dropped_frames: 0,
        dropped_samples: 0,
        dropped_events: 0,
        last_sequence: Some(512),
        verified: true,
        diagnostic_error: None,
        audio_gap: None,
    };
    private_file(
        &dir.join("session.json"),
        &serde_json::to_vec(&manifest).unwrap(),
    );
    private_file(&dir.join("audio.flac"), &[0u8; 4_096]);
    private_file(&dir.join("audio.flac.part"), &[0u8; 128]);
    private_file(&dir.join("events-000001.jsonl"), &[b'{'; 64]);
    dir
}

/// A staging copy of the manifest as `atomic_replace` leaves one.
fn staging_copy(dir: &Path, bytes: usize) -> PathBuf {
    let path = dir.join(format!("session.json.{}.7.tmp", std::process::id()));
    private_file(&path, &vec![b'x'; bytes]);
    path
}

#[test]
fn the_ten_day_boundary_removes_exactly_the_bundles_that_reached_it() {
    let scratch = Scratch::new("boundary");
    let logs = scratch.logs();
    let day = 86_400_000u64;
    let now = epoch_ms(epoch());
    let kept = finished_bundle(&logs, now - 9 * day, now - 9 * day, 1, 1);
    let exact = finished_bundle(&logs, now - 10 * day, now - 10 * day, 2, 1);
    let past = finished_bundle(&logs, now - 11 * day, now - 11 * day, 3, 1);

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), 10_000_000),
        epoch(),
    );
    sink.worker_passes(3);

    assert_eq!(
        session_dirs(&logs),
        vec![kept.clone()],
        "nine days is inside the ten-day age"
    );
    // The whole segment goes with the bundle: the manifest, every journal
    // segment, the verified file and the partial beside it.
    for dir in [&exact, &past] {
        assert!(!dir.exists(), "the bundle and all of its children are gone");
    }
    assert!(kept.join("session.json").exists());
    assert!(kept.join("audio.flac").exists());
    assert!(kept.join("audio.flac.part").exists());
    assert!(kept.join("events-000001.jsonl").exists());
    assert_eq!(
        manifest_of(&kept).unwrap().outcome,
        Some(Outcome::Success),
        "a surviving bundle is left alone"
    );
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn the_oldest_retention_gives_way_first_and_whole_bundles_only() {
    let scratch = Scratch::new("eviction");
    let logs = scratch.logs();
    let day = 86_400_000u64;
    let now = epoch_ms(epoch());
    // The name order is deliberately the opposite of the retention order: the
    // bundle whose name sorts first is the one whose manifest is the newest,
    // and the retention time is what decides which one goes.
    let newest_name = finished_bundle(&logs, now, now - 6 * day, 1, 1);
    let oldest_name = finished_bundle(&logs, now - 6 * day, now, 2, 1);
    // A staging copy inside the oldest bundle: its bytes belong to the bundle
    // the quota has to count.
    let staged = staging_copy(&newest_name, 4_096);
    let survivor = oldest_name.clone();
    let cap = disk_total(&survivor) + 16;
    assert!(disk_total(&logs) > cap, "pruning has to happen");

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), cap),
        epoch(),
    );
    sink.worker_passes(3);

    assert_eq!(
        session_dirs(&logs),
        vec![survivor],
        "the older retention went, not the older name"
    );
    assert!(!newest_name.exists(), "the oldest bundle went whole");
    assert!(
        !staged.exists(),
        "its staging copy went with the segment it belonged to"
    );
    assert!(disk_total(&logs) <= cap, "nothing is kept over the quota");
    assert!(sink.is_enabled());
    assert_eq!(sink.error(), None);
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn an_active_take_is_never_evicted_and_the_next_one_is_pruned_room_for() {
    let scratch = Scratch::new("protect");
    let logs = scratch.logs();
    let cap = SESSION_RESERVE + 4_000;
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), cap),
        epoch(),
    );
    let live = sink.begin(key(1), Mode::Live).unwrap();
    live.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);
    let live_dir = session_dirs(&logs).remove(0);
    assert!(manifest_of(&live_dir).unwrap().outcome.is_none());

    // While the live take holds its reserve, the next one cannot be afforded;
    // the memory-only answer never evicts the bundle that is still being
    // written, and no write of the live take passes the cap.
    let blocked = sink.begin(key(2), Mode::Live).unwrap();
    blocked.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);
    assert!(live_dir.exists(), "an active bundle is never a candidate");
    assert_eq!(session_dirs(&logs).len(), 1, "no second bundle was admitted");
    assert!(!sink.entry_for(key(2)).unwrap().archive_enabled());
    assert_eq!(sink.error(), Some(DiagnosticCode::Quota));
    assert!(disk_total(&logs) <= cap);

    // The terminal releases the reserve; the room that frees up is pruned for
    // and the next take takes it, still inside the same cap.
    finish_take(&sink, &live, Outcome::Success);
    sink.worker_passes(3);
    assert!(disk_total(&logs) <= cap);
    assert_eq!(manifest_of(&live_dir).unwrap().outcome, Some(Outcome::Success));
    let next = sink.begin(key(3), Mode::Live).unwrap();
    next.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);
    assert_eq!(session_dirs(&logs).len(), 2, "the freed room admitted it");
    assert!(sink.entry_for(key(3)).unwrap().archive_enabled());
    assert!(disk_total(&logs) <= cap, "every write stayed inside the cap");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn an_orphan_bundle_is_recovered_when_the_cap_can_afford_it() {
    let scratch = Scratch::new("orphan");
    let logs = scratch.logs();
    let start = epoch();
    private_dir(&logs);
    let dir = named_dir(&logs, epoch_ms(start), 5, 1);
    private_dir(&dir);
    // A crash leaves the artifacts but no manifest at all.
    private_file(&dir.join("events-000001.jsonl"), &[b'{'; 64]);
    private_file(&dir.join("audio.flac.part"), &[0u8; 4_096]);
    let staged = staging_copy(&dir, 1_024);

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), 1_000_000),
        start,
    );
    sink.worker_passes(3);

    let manifest = manifest_of(&dir).expect("a recovered manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Abandoned));
    assert_eq!(
        manifest.diagnostic_error,
        Some(DiagnosticCode::MissingTerminal)
    );
    assert_eq!(manifest.audio_status, AudioStatus::Incomplete);
    assert_eq!(manifest.audio_file, None);
    assert!(!manifest.verified);
    assert_eq!(manifest.started_utc_ms, epoch_ms(start), "the name dates it");
    assert!(manifest.ended_utc_ms.is_some());
    assert!(dir.join("audio.flac.part").exists(), "nothing was invented");
    assert!(!dir.join("audio.flac").exists());
    assert!(!staged.exists(), "the staging copy is never the manifest");
    assert!(disk_total(&logs) <= 1_000_000);
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn an_orphan_bundle_the_cap_cannot_afford_is_left_exactly_as_it_is() {
    let scratch = Scratch::new("orphan-tight");
    let logs = scratch.logs();
    let start = epoch();
    private_dir(&logs);
    let dir = named_dir(&logs, epoch_ms(start), 6, 1);
    private_dir(&dir);
    private_file(&dir.join("events-000001.jsonl"), &[b'{'; 64]);
    private_file(&dir.join("audio.flac.part"), &[0u8; 4_096]);
    // Exactly the bytes that are already there: the recovery's own manifest
    // does not fit, so the bundle keeps its bytes and gains nothing.
    let cap = disk_total(&logs);

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), cap),
        start,
    );
    sink.worker_passes(3);

    assert!(
        !dir.join("session.json").exists(),
        "no manifest was admitted"
    );
    assert_eq!(disk_total(&logs), cap, "the bundle is untouched");
    assert!(dir.join("audio.flac.part").exists());
    assert!(dir.join("events-000001.jsonl").exists());
    assert!(!sink.is_sink_disabled(), "a refused recovery is not a failure");
    assert_eq!(sink.error(), None);
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_leaked_staging_copy_stays_charged_and_is_never_promoted() {
    let scratch = Scratch::new("staging");
    let logs = scratch.logs();
    let leak = 4_096u32;
    let io = ScriptedIo::new();
    io.fault(|faults| faults.leak_staging_then_fail = Some(leak));
    // Room for two takes' reserves, and not one byte for the leaked copy
    // beside them: the leak is what decides the second admission.
    let cap = 2 * SESSION_RESERVE;
    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400), cap),
        epoch(),
        io.clone(),
    );
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);

    assert_eq!(sink.error(), Some(DiagnosticCode::Io));
    let dirs = session_dirs(&logs);
    assert_eq!(dirs.len(), 1);
    let dir = dirs.into_iter().next().unwrap();
    let staged = dir.join(format!("session.json.{}.1.tmp", std::process::id()));
    assert!(staged.exists(), "the interrupted replacement left its copy");
    assert_eq!(std::fs::metadata(&staged).unwrap().len(), leak as u64);
    assert!(
        !dir.join("session.json").exists(),
        "a staging copy is never promoted to the manifest"
    );
    assert!(disk_total(&logs) <= cap);

    // The leaked bytes are charged, so the second take cannot be afforded
    // while the first one's reserve is still held; the live bundle is not
    // evicted to make room for it.
    let second = sink.begin(key(2), Mode::Live).unwrap();
    second.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);
    assert_eq!(sink.error(), Some(DiagnosticCode::Quota));
    assert_eq!(session_dirs(&logs).len(), 1, "the leaked bytes still count");
    assert!(!sink.entry_for(key(2)).unwrap().archive_enabled());
    assert!(dir.exists(), "the live bundle was not evicted");
    assert!(staged.exists(), "and the leaked copy was not cleaned up here");
    // Shutting down gives the still-open take its bounded finalization, and
    // even then the copy is not what the manifest becomes.
    sink.shutdown(Duration::from_secs(2));

    let finalized = manifest_of(&dir).expect("the take's own final manifest");
    assert_eq!(finalized.outcome, Some(Outcome::Exit));
    assert_eq!(finalized.diagnostic_error, Some(DiagnosticCode::Io));
    assert!(
        staged.exists(),
        "the staging copy survived the run that leaked it"
    );
    assert_eq!(
        finalized.session_id,
        dir.file_name().unwrap().to_str().unwrap(),
        "the record is the take's own, never the copy's bytes"
    );

    // A later run recognises the staging name, keeps the authoritative record
    // and deletes only the copy.
    let authoritative = std::fs::read(dir.join("session.json")).unwrap();
    let reopened = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    reopened.worker_passes(3);
    assert!(!staged.exists(), "the staging copy is cleaned up");
    assert_eq!(
        std::fs::read(dir.join("session.json")).unwrap(),
        authoritative,
        "the authoritative manifest is kept byte for byte"
    );
    assert_eq!(manifest_of(&dir).unwrap().outcome, Some(Outcome::Exit));
    reopened.shutdown(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// A run with a wall clock from the future
// ---------------------------------------------------------------------------

// (The clamp itself is exercised by
// `a_retention_time_from_a_future_clock_is_clamped_once`, which also proves the
// clamp is history and does not pin the age at zero.)

// ---------------------------------------------------------------------------
// Strangers under the root, and the records of a shared queue
// ---------------------------------------------------------------------------

#[test]
fn a_foreign_child_inside_an_own_bundle_stops_the_sink_without_deleting_it() {
    let scratch = Scratch::new("foreign-child");
    let logs = scratch.logs();
    private_dir(&logs);
    let dir = finished_bundle(&logs, epoch_ms(epoch()), epoch_ms(epoch()), 1, 1);
    let stranger = dir.join("notes.txt");
    private_file(&stranger, b"somebody else's bytes");

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    sink.worker_passes(2);
    assert!(sink.is_sink_disabled());
    assert_eq!(sink.error(), Some(DiagnosticCode::ForeignArchive));
    assert_eq!(
        std::fs::read(&stranger).unwrap(),
        b"somebody else's bytes".to_vec(),
        "a stranger inside a bundle is never deleted"
    );
    assert!(dir.exists(), "the bundle is left alone with it");
    assert!(sink.begin(key(9), Mode::Live).is_none());
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_directory_that_is_not_an_own_bundle_name_stops_the_sink_and_is_kept() {
    let scratch = Scratch::new("foreign-dir");
    let logs = scratch.logs();
    private_dir(&logs);
    let stranger = logs.join("logs.bak");
    private_dir(&stranger);
    private_file(&stranger.join("kept.txt"), b"not ours");

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    sink.worker_passes(2);
    assert!(sink.is_sink_disabled());
    assert_eq!(sink.error(), Some(DiagnosticCode::ForeignArchive));
    assert!(stranger.join("kept.txt").exists(), "nothing foreign is removed");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_linked_child_inside_a_bundle_stops_the_sink_without_being_followed() {
    let scratch = Scratch::new("link-child");
    let logs = scratch.logs();
    private_dir(&logs);
    let dir = finished_bundle(&logs, epoch_ms(epoch()), epoch_ms(epoch()), 2, 1);
    let io = ScriptedIo::new();
    io.fault(|faults| faults.mark_reparse = Some("audio.flac".to_owned()));
    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
        io,
    );
    sink.worker_passes(2);
    assert!(sink.is_sink_disabled());
    assert_eq!(sink.error(), Some(DiagnosticCode::ForeignArchive));
    assert!(dir.join("audio.flac").exists(), "a link is never followed");
    assert!(dir.join("session.json").exists(), "nor is its bundle removed");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_sealed_take_whose_records_are_still_queued_is_not_closed_early() {
    let scratch = Scratch::new("queued");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    sink.worker_pause();
    // Another take fills the shared queue ahead of this one, so several passes
    // drain only that take's records while this one's are still waiting.
    let other = sink.begin(key(2), Mode::Live).unwrap();
    for _ in 0..80 {
        other.event(Event::Phase(Phase {
            phase: CapturePhase::Recording,
        }));
    }
    let sealed = sink.begin(key(1), Mode::Smart).unwrap();
    for _ in 0..30 {
        sealed.event(Event::Phase(Phase {
            phase: CapturePhase::Recording,
        }));
    }
    sealed.end_capture(Outcome::Success);
    sink.end(key(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.advance_all();
    assert!(sink.entry_for(key(1)).unwrap().is_sealed());
    let entry = sink.entry_for(key(1)).unwrap();
    assert!(
        entry.meta_pending() > 0,
        "the take's own records are still in the queue"
    );

    sink.worker_resume();
    // Five passes drain eighty records, all of them the other take's: the
    // sealed take must not be closed over its own queued records.
    for pass in 1..=5 {
        sink.worker_passes(1);
        assert!(
            sink.entry_for(key(1)).is_some(),
            "pass {pass}: closed over queued records"
        );
        let dir = session_dirs(&logs)
            .into_iter()
            .find(|dir| session_name_parts(dir)[4] == "1")
            .expect("its bundle");
        assert!(manifest_of(&dir).unwrap().outcome.is_none());
        assert_eq!(entry.dropped_events(), 0, "no record was lost");
    }

    // Once its records are drained it closes with the real outcome, and every
    // one of them reached the journal.
    sink.worker_passes(8);
    let dir = session_dirs(&logs)
        .into_iter()
        .find(|dir| session_name_parts(dir)[4] == "1")
        .expect("its bundle");
    assert_eq!(manifest_of(&dir).unwrap().outcome, Some(Outcome::Success));
    assert!(has_journal_event(&dir, "session_end"));
    assert!(has_journal_event(&dir, "session_begin"));
    assert!(sink.entry_for(key(1)).is_none(), "sealed and let go");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_take_whose_begin_record_was_dropped_still_gets_its_bundle() {
    let scratch = Scratch::new("dropped-begin");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    sink.worker_pause();
    let other = sink.begin(key(2), Mode::Live).unwrap();
    // The shared queue is filled to its capacity, so the next take's own begin
    // record cannot be queued at all.
    for _ in 0..META_QUEUE - 1 {
        other.event(Event::Phase(Phase {
            phase: CapturePhase::Recording,
        }));
    }
    let dropped = sink.begin(key(1), Mode::Verbatim).unwrap();
    assert_eq!(dropped.key(), key(1));
    let entry = sink.entry_for(key(1)).unwrap();
    assert_eq!(entry.dropped_events(), 1, "the begin record was dropped");
    assert!(session_dirs(&logs).is_empty(), "nothing is on disk yet");

    dropped.end_capture(Outcome::Success);
    sink.end(key(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.advance_all();
    assert!(entry.is_sealed());

    sink.worker_resume();
    sink.worker_passes(16);
    let dir = session_dirs(&logs)
        .into_iter()
        .find(|dir| session_name_parts(dir)[4] == "1")
        .expect("the take with the dropped begin still got a bundle");
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Success));
    assert_eq!(manifest.mode, Mode::Verbatim, "its registration is the source");
    assert_eq!(manifest.generation, 1);
    assert!(has_journal_event(&dir, "session_end"));
    assert!(sink.entry_for(key(1)).is_none());
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_sealed_take_with_a_queued_report_is_not_closed_early() {
    let scratch = Scratch::new("report-queued");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    sink.worker_pause();
    // Another take's records fill the shared queue ahead of this take's one
    // accepted renderer report.
    let other = sink.begin(lab(2), Mode::Live).unwrap();
    for _ in 0..40 {
        other.event(Event::Phase(Phase {
            phase: CapturePhase::Recording,
        }));
    }
    let sealed = sink.begin(lab(1), Mode::Comparison).unwrap();
    assert_eq!(
        sink.admit_renderer_report(super::report(lab(1), 1)),
        ReportAdmission::Accepted,
        "the report is queued and its index committed"
    );
    let entry = sink.entry_for(lab(1)).unwrap();

    sealed.end_capture(Outcome::Success);
    sink.end(lab(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.advance_all();
    assert!(entry.is_sealed());

    sink.worker_resume();
    // The first thirty-two records are the other take's: the sealed take is
    // still holding a report that has not been written, so it stays open.
    for pass in 1..=2 {
        sink.worker_passes(1);
        assert!(
            sink.entry_for(lab(1)).is_some(),
            "pass {pass}: closed over its accepted report"
        );
    }

    // Once the report is drained the take closes with the real outcome, and
    // the report it accepted really reached its journal.
    sink.worker_passes(8);
    assert_eq!(entry.dropped_events(), 0, "the accepted report was written");
    let dir = session_dirs(&logs)
        .into_iter()
        .find(|dir| session_name_parts(dir)[3] == "lab" && session_name_parts(dir)[4] == "1")
        .expect("its bundle");
    assert_eq!(manifest_of(&dir).unwrap().outcome, Some(Outcome::Success));
    assert!(has_journal_event(&dir, "renderer_snapshot"));
    assert!(has_journal_event(&dir, "session_end"));
    assert!(sink.entry_for(lab(1)).is_none(), "sealed and let go");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_retention_tie_is_broken_by_the_name_so_the_older_bundle_goes() {
    let scratch = Scratch::new("tie");
    let logs = scratch.logs();
    let day = 86_400_000u64;
    let now = epoch_ms(epoch());
    // Two bundles the same age: the retention times are identical, so only the
    // name can decide. Both start inside the ten-day age, so nothing is pruned
    // by the age and only the quota makes room.
    let retention = now - day;
    let older = finished_bundle(&logs, now - 5 * day, retention, 1, 1);
    let newer = finished_bundle(&logs, now - 4 * day, retention, 2, 1);
    assert!(older < newer, "the name order follows the start");
    let cap = disk_total(&newer) + 16;
    assert!(disk_total(&logs) > cap, "pruning has to happen");

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), cap),
        epoch(),
    );
    sink.worker_passes(3);

    assert_eq!(
        session_dirs(&logs),
        vec![newer.clone()],
        "the tie is broken by the ordinal name"
    );
    assert!(!older.exists());
    assert_eq!(
        manifest_of(&newer).unwrap().retention_utc_ms,
        retention,
        "the surviving bundle kept its own retention"
    );
    assert_eq!(sink.error(), None);
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_journal_length_that_cannot_be_read_back_keeps_its_bytes_charged() {
    let scratch = Scratch::new("unreadable-tail");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    // The record is written short *and* the file's real length cannot be read
    // back afterwards, so the reconciliation has nothing to measure against.
    io.fault(|faults| {
        faults.short_write = Some(5);
        faults.unreadable_length = Some("events-000001.jsonl".to_owned());
    });
    let cap = SESSION_RESERVE + 4_096;
    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400), cap),
        epoch(),
        io.clone(),
    );
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);

    assert_eq!(sink.error(), Some(DiagnosticCode::Io));
    let entry = sink.entry_for(key(1)).unwrap();
    assert!(!entry.archive_enabled(), "the archive closed");
    let dirs = session_dirs(&logs);
    assert_eq!(dirs.len(), 1);
    let dir = &dirs[0];
    assert!(
        dir.join("events-000001.jsonl").exists(),
        "an unmeasurable file is not deleted either"
    );
    assert!(disk_total(&logs) <= cap);

    // A length that cannot be measured is never handed back as free room: the
    // next take is admitted only if the worst case still fits, and nothing is
    // ever written past the cap while the length stays unreadable.
    let second = sink.begin(key(2), Mode::Live).unwrap();
    second.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);
    assert_eq!(session_dirs(&logs).len(), 1, "no room was invented");
    assert!(!sink.entry_for(key(2)).unwrap().archive_enabled());
    assert!(disk_total(&logs) <= cap);
    sink.shutdown(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// The next write makes its own room
// ---------------------------------------------------------------------------

#[test]
fn an_ordinary_write_evicts_the_oldest_finished_bundle_to_make_room() {
    let scratch = Scratch::new("write-eviction");
    let logs = scratch.logs();
    let day = 86_400_000u64;
    let now = epoch_ms(epoch());
    // Both bundles are young name-wise and inside the age, and the name order
    // is once more the opposite of the retention order: only a write that needs
    // the room can remove one of them, and only the retention can pick it.
    let named_first = finished_bundle(&logs, now - 8 * day, now, 2, 1);
    let named_last = finished_bundle(&logs, now, now - 8 * day, 3, 1);
    assert!(named_first < named_last, "the name order is the opposite one");
    let at_startup = disk_total(&logs);
    assert!(at_startup > SESSION_RESERVE + 16, "there is room to lose");
    // A whole reserve fits beside both bundles with sixteen bytes to spare, and
    // no more: the take's own manifest is what has to prune for itself.
    let cap = at_startup + SESSION_RESERVE + 16;

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), cap),
        epoch(),
    );
    assert_eq!(
        disk_total(&logs),
        at_startup,
        "the startup tree is inside the cap and nothing is pruned for it"
    );
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);

    assert!(
        !named_last.exists(),
        "the oldest retention made room for the write, not the oldest name"
    );
    assert!(named_first.exists(), "and only that one went");
    let live = session_dirs(&logs)
        .into_iter()
        .find(|dir| session_name_parts(dir)[4] == "1")
        .expect("the take opened its bundle");
    assert!(
        manifest_of(&live).is_some(),
        "the manifest that needed the room was written"
    );
    assert!(
        has_journal_event(&live, "session_begin"),
        "and the record after it followed"
    );
    assert!(
        sink.entry_for(key(1)).unwrap().archive_enabled(),
        "the archive stayed open"
    );
    assert_eq!(sink.error(), None);
    assert!(disk_total(&logs) <= cap, "every write stayed inside the cap");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_next_write_breaks_a_retention_tie_by_the_ordinal_name() {
    let scratch = Scratch::new("write-tie");
    let logs = scratch.logs();
    let day = 86_400_000u64;
    let now = epoch_ms(epoch());
    // Two bundles the same retention: only the ordinal name can decide, and the
    // write that needs the room is what has to decide it.
    let retention = now - day;
    let smaller_name = finished_bundle(&logs, now - 5 * day, retention, 2, 1);
    let larger_name = finished_bundle(&logs, now - 4 * day, retention, 3, 1);
    assert!(smaller_name < larger_name, "the name order follows the start");
    let at_startup = disk_total(&logs);
    // Again a whole reserve and sixteen spare bytes, and nothing for the
    // manifest: the write that needs the room is what has to break the tie.
    let cap = at_startup + SESSION_RESERVE + 16;

    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400 * 10), cap),
        epoch(),
    );
    assert_eq!(disk_total(&logs), at_startup, "nothing was pruned for it");
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);

    assert_eq!(
        session_dirs(&logs).len(),
        2,
        "the tie cost exactly one bundle"
    );
    assert!(!smaller_name.exists(), "the tie is broken by the ordinal name");
    assert!(larger_name.exists(), "the later name survived");
    assert_eq!(
        manifest_of(&larger_name).unwrap().retention_utc_ms,
        retention,
        "the survivor kept its own retention"
    );
    assert_eq!(sink.error(), None);
    assert!(disk_total(&logs) <= cap, "every write stayed inside the cap");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn an_active_bundle_is_never_evicted_for_an_ordinary_write() {
    let scratch = Scratch::new("write-protect");
    let logs = scratch.logs();
    let cap = SESSION_RESERVE + 8_000;
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), cap),
        epoch(),
    );
    let live = sink.begin(key(1), Mode::Live).unwrap();
    live.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);
    let live_dir = session_dirs(&logs).remove(0);
    let after_first = disk_total(&logs);
    assert!(after_first > 0 && after_first < cap, "the take is writing");

    // The second take's whole reserve does not fit beside the live bundle, and
    // the live bundle is not a candidate for removal: the answer is memory-only
    // rather than an eviction of what is still being written.
    let blocked = sink.begin(key(2), Mode::Live).unwrap();
    blocked.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);
    assert!(live_dir.exists(), "an active bundle is never a candidate");
    assert_eq!(session_dirs(&logs).len(), 1, "no second bundle was admitted");
    assert!(!sink.entry_for(key(2)).unwrap().archive_enabled());
    assert_eq!(sink.error(), Some(DiagnosticCode::Quota));
    assert!(disk_total(&logs) <= cap);

    // The live take's own ordinary writes are admitted while they fit; once
    // they no longer do there is still no candidate to evict, because the only
    // bundle under the root is the one being written. The write fails with the
    // quota and the bundle stays exactly where it is.
    for _ in 0..64 {
        live.event(Event::Phase(Phase {
            phase: CapturePhase::Recording,
        }));
    }
    sink.worker_passes(8);
    assert!(live_dir.exists(), "the live bundle was never a candidate");
    assert!(
        disk_total(&logs) > after_first,
        "its records were written while they fit"
    );
    assert!(disk_total(&logs) <= cap, "and nothing past the cap");
    assert!(!sink.entry_for(key(1)).unwrap().archive_enabled());
    assert_eq!(sink.error(), Some(DiagnosticCode::Quota));
    sink.shutdown(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// A cleanup that did not finish, and a tree that stopped being the archive's
// ---------------------------------------------------------------------------

#[test]
fn a_failed_bundle_delete_measures_back_what_it_did_remove() {
    let scratch = Scratch::new("partial-delete");
    let logs = scratch.logs();
    let day = 86_400_000u64;
    let now = epoch_ms(epoch());
    let evicted = finished_bundle(&logs, now - 8 * day, now - 8 * day, 2, 1);
    let survivor = finished_bundle(&logs, now - 7 * day, now - 7 * day, 3, 1);
    let io = ScriptedIo::new();
    io.fault(|faults| faults.partial_delete = true);
    let survivor_total = disk_total(&survivor);
    let evicted_total = disk_total(&evicted);
    let taken = std::fs::metadata(evicted.join("audio.flac"))
        .expect("the audio file the interrupted removal takes")
        .len();
    assert!(evicted_total > taken + 16, "the bundle is more than one child");
    // Room for the bundle as the disk really leaves it, the next take's whole
    // reserve and its own first bytes, and nothing at all for the child the
    // interrupted removal did take away.
    let cap = survivor_total + (evicted_total - taken) + SESSION_RESERVE + 3_000;
    assert!(
        disk_total(&logs) <= cap,
        "the startup tree is inside the cap"
    );

    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400 * 10), cap),
        epoch(),
        io.clone(),
    );
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_passes(3);

    // The removal took one child and failed. The bundle stays, and what really
    // is left of it is measured back instead of the ledger still charging the
    // child the disk already took.
    assert!(evicted.exists(), "a failed removal leaves the bundle");
    assert!(
        !evicted.join("audio.flac").exists(),
        "but the child the disk did take is gone"
    );
    assert!(
        evicted.join("session.json").exists()
            && evicted.join("events-000001.jsonl").exists()
            && evicted.join("audio.flac.part").exists(),
        "and nothing else of it was touched"
    );
    assert!(
        sink.entry_for(key(1)).unwrap().archive_enabled(),
        "the room the removal really freed admitted the take"
    );
    let live = session_dirs(&logs)
        .into_iter()
        .find(|dir| session_name_parts(dir)[4] == "1")
        .expect("the take opened its bundle");
    assert!(
        manifest_of(&live).is_some(),
        "and the write that needed the room was written"
    );
    assert_eq!(sink.error(), None, "a refused removal is not a sink failure");
    assert!(disk_total(&logs) <= cap, "every write stayed inside the cap");

    // The disk recovers: what is left of the bundle goes at the next sweep.
    io.fault(|faults| faults.partial_delete = false);
    sink.set_wall_clock(epoch() + Duration::from_secs(86_400 * 11));
    sink.advance_clock(Duration::from_secs(61));
    sink.worker_passes(3);
    assert!(!evicted.exists(), "the rest goes once the disk lets it");
    assert!(disk_total(&logs) <= cap);
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_foreign_child_that_appears_later_stops_the_sink_before_the_next_write() {
    let scratch = Scratch::new("late-foreign");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
        io.clone(),
    );
    sink.worker_pause();
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_resume();
    sink.worker_passes(3);
    let dir = session_dirs(&logs).remove(0);
    let manifest = std::fs::read(dir.join("session.json")).expect("its manifest");

    // A stranger appears in the bundle while the take is still writing and the
    // next write tears. The directory the failure has to measure again is not
    // the archive's own any more: the sink stops instead of writing past bytes
    // nobody measured.
    sink.worker_pause();
    let stranger = dir.join("notes.txt");
    private_file(&stranger, b"not ours");
    io.fault(|faults| faults.short_write = Some(3));
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_resume();
    sink.worker_passes(3);

    assert!(sink.is_sink_disabled());
    assert_eq!(sink.error(), Some(DiagnosticCode::ForeignArchive));
    assert_eq!(
        std::fs::read(&stranger).unwrap(),
        b"not ours".to_vec(),
        "a stranger that appeared later is never removed"
    );
    assert!(dir.exists(), "and its bundle is left alone with it");
    assert_eq!(
        std::fs::read(dir.join("session.json")).unwrap(),
        manifest,
        "no manifest was rewritten over the unmeasured tree"
    );
    assert!(sink.begin(key(2), Mode::Live).is_none(), "no take is opened");
    sink.shutdown(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// The audio archive: what reaches the file, what is claimed, and what a
// failure leaves behind
// ---------------------------------------------------------------------------

/// The samples one capture frame carries, straight from the recorder itself: a
/// test must not invent a second frame size the capture does not use.
const FRAME_SAMPLES: usize = crate::capture::FRAME_SAMPLES;
/// The bytes one capture frame carries.
const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// Deterministic PCM16 for `frames` capture frames of 512 samples. Every
/// sample is exact, and no two frames share a shape: a shifted, duplicated or
/// dropped frame would show in the archive's own check.
fn frame_pcm(frames: u64) -> Vec<u8> {
    let samples = frames as usize * FRAME_SAMPLES;
    let mut bytes = Vec::with_capacity(samples * 2);
    for index in 0..samples {
        let value: i16 = match index % 5 {
            0 => 0,
            1 => i16::MIN,
            2 => i16::MAX,
            3 => ((index % 4_096) as i32 - 2_048) as i16,
            _ => (((index * 31) % 32_768) as i32 - 16_384) as i16,
        };
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// The bundle one generation opened: the generation is the fifth field of the
/// session directory name.
fn bundle(logs: &Path, generation: u64) -> PathBuf {
    session_dirs(logs)
        .into_iter()
        .find(|dir| session_name_parts(dir)[4] == generation.to_string())
        .expect("the take's bundle")
}

/// One take: pushes `frames` frames of the deterministic waveform, finishes
/// its capture with `outcome` and lets the worker settle it. Returns the PCM
/// that was pushed, which is exactly what a complete archive has to hold.
fn recorded_take(
    sink: &Arc<Diagnostics>,
    handle: &SessionHandle,
    frames: u64,
    outcome: Outcome,
) -> Vec<u8> {
    let waveform = frame_pcm(frames);
    let mut archive = handle.capture();
    for frame in 0..frames {
        let at = frame as usize * FRAME_BYTES;
        archive.push(frame + 1, &waveform[at..at + FRAME_BYTES]);
    }
    archive.finish(frames, outcome);
    drop(archive);
    sink.end(handle.key(), outcome);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_passes(12);
    waveform
}

/// A take whose archive is faulted: the bundle is opened first, the fault is
/// armed while the writer is parked, and then the take records five frames
/// (one full block and one short tail) and finishes.
fn take_with_faulted_archive(
    logs: &Path,
    io: &Arc<ScriptedIo>,
    session: SessionKey,
    arm: impl FnOnce(&mut Faults),
) -> (Arc<Diagnostics>, PathBuf) {
    let sink = Diagnostics::with_io(
        logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
        io.clone(),
    );
    sink.worker_pause();
    let handle = sink.begin(session, Mode::Smart).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    sink.worker_resume();
    sink.worker_passes(2);
    // The bundle, its manifest and its journal are on disk; from here the
    // fault can only reach the archive's own writes.
    sink.worker_pause();
    arm(&mut io.faults.lock());
    let waveform = frame_pcm(5);
    let mut archive = handle.capture();
    for frame in 1..=5u64 {
        let at = (frame as usize - 1) * FRAME_BYTES;
        archive.push(frame, &waveform[at..at + FRAME_BYTES]);
    }
    archive.finish(5, Outcome::Success);
    drop(archive);
    sink.end(session, Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_resume();
    sink.worker_passes(12);
    (sink, bundle(logs, session.generation))
}

#[test]
fn a_take_with_a_short_tail_publishes_one_verified_archive() {
    let scratch = Scratch::new("audio-complete");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    // Five frames are 2560 samples: one full block and a short tail of 512.
    let waveform = recorded_take(&sink, &handle, 5, Outcome::Success);

    let dir = bundle(&logs, 1);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Success));
    assert_eq!(manifest.audio_status, AudioStatus::Complete);
    assert_eq!(manifest.audio_file.as_deref(), Some("audio.flac"));
    assert!(manifest.verified);
    assert_eq!(manifest.archived_samples, 5 * 512);
    assert_eq!(manifest.captured_samples, 5 * 512);
    assert_eq!(manifest.dropped_frames, 0);
    assert_eq!(manifest.audio_gap, None);
    assert!(!dir.join("audio.flac.part").exists(), "the partial moved");
    assert!(!dir.join("audio.incomplete.flac").exists());

    // Every sample of the take, in order, straight out of the published file.
    let file = dir.join("audio.flac");
    assert!(
        std::fs::metadata(&file).expect("the archive").len() > 42,
        "a header and its frames"
    );
    flac::verify_pcm(std::fs::File::open(&file).unwrap(), 5 * 512, &waveform)
        .expect("every sample of the take, in order");
    assert!(has_journal_event(&dir, "audio_verified"));
    assert!(!has_journal_event(&dir, "audio_gap"));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_cancelled_take_publishes_the_same_audio_as_a_successful_one() {
    let scratch = Scratch::new("audio-cancel");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    let success = sink.begin(key(1), Mode::Live).unwrap();
    recorded_take(&sink, &success, 3, Outcome::Success);
    let cancelled = sink.begin(key(2), Mode::Live).unwrap();
    recorded_take(&sink, &cancelled, 3, Outcome::Cancelled);

    let first = bundle(&logs, 1);
    let second = bundle(&logs, 2);
    let one = std::fs::read(first.join("audio.flac")).expect("the successful take");
    let two = std::fs::read(second.join("audio.flac")).expect("the cancelled take");
    assert_eq!(one, two, "the outcome changes no sample");
    assert_eq!(manifest_of(&first).unwrap().outcome, Some(Outcome::Success));
    let stopped = manifest_of(&second).expect("the cancelled take's manifest");
    assert_eq!(stopped.outcome, Some(Outcome::Cancelled));
    assert_eq!(
        stopped.audio_status,
        AudioStatus::Complete,
        "a cancelled dictation can still have complete audio"
    );
    assert!(stopped.verified);
    assert_eq!(stopped.audio_file.as_deref(), Some("audio.flac"));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_take_that_captured_nothing_leaves_only_metadata() {
    let scratch = Scratch::new("audio-empty");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Live).unwrap();
    handle.event(Event::Phase(Phase {
        phase: CapturePhase::Recording,
    }));
    handle.end_capture(Outcome::Success);
    sink.end(key(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_passes(12);

    let dir = bundle(&logs, 1);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Success));
    assert_eq!(manifest.audio_status, AudioStatus::None);
    assert_eq!(manifest.audio_file, None);
    assert!(!manifest.verified);
    assert_eq!(manifest.archived_samples, 0);
    assert_eq!(manifest.captured_frames, 0);
    for name in ["audio.flac", "audio.flac.part", "audio.incomplete.flac"] {
        assert!(!dir.join(name).exists(), "{name} was invented");
    }
    assert!(!has_journal_event(&dir, "audio_verified"));
    assert!(!has_journal_event(&dir, "audio_gap"));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn one_lab_comparison_keeps_one_archive_for_its_three_routes() {
    let scratch = Scratch::new("lab-audio");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    let handle = sink.begin(lab(1), Mode::Comparison).unwrap();
    let waveform = frame_pcm(4);
    let mut archive = handle.capture();
    for frame in 1..=4u64 {
        let at = (frame as usize - 1) * FRAME_BYTES;
        archive.push(frame, &waveform[at..at + FRAME_BYTES]);
    }
    // The three routes of one comparison are three requests on one capture:
    // each has its own trace and its own counters, none of them splits the
    // audio.
    for route in [Route::Live, Route::Smart, Route::Verbatim] {
        let trace = handle.request(route).expect("a request slot");
        trace.send_started(SendKind::Audio);
        trace.send_finished_kind(SendKind::Audio, FRAME_BYTES as u64, Duration::from_millis(5), true);
        trace.received(ReceiveFlags {
            interim: true,
            ..ReceiveFlags::default()
        });
        trace.finish(Outcome::Success);
    }
    archive.finish(4, Outcome::Success);
    drop(archive);
    sink.end(lab(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_passes(12);

    let dirs = session_dirs(&logs);
    assert_eq!(dirs.len(), 1, "one take, one bundle, one archive");
    let dir = &dirs[0];
    let manifest = manifest_of(dir).expect("a manifest");
    assert_eq!(manifest.mode, Mode::Comparison);
    assert_eq!(manifest.audio_status, AudioStatus::Complete);
    assert!(manifest.verified);
    assert_eq!(manifest.archived_samples, 4 * 512);
    flac::verify_pcm(
        std::fs::File::open(dir.join("audio.flac")).unwrap(),
        4 * 512,
        &waveform,
    )
    .expect("the one archive of the comparison");
    let requests = journal_records(dir)
        .into_iter()
        .filter(|record| record["event"] == "request_end")
        .count();
    assert_eq!(requests, 3, "three routes, three requests, one audio file");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn an_archive_that_cannot_open_its_file_claims_nothing() {
    let scratch = Scratch::new("audio-create");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let (sink, dir) = take_with_faulted_archive(&logs, &io, key(1), |faults| {
        faults.create_file_error = true;
    });

    // No partial ever existed, and the take is told its archive failed while
    // the dictation itself still succeeded.
    assert!(!dir.join("audio.flac.part").exists(), "no partial was created");
    assert!(!dir.join("audio.flac").exists());
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Success));
    assert_eq!(manifest.audio_status, AudioStatus::Error);
    assert!(!manifest.verified);
    assert_eq!(manifest.audio_file, None);
    assert_eq!(manifest.diagnostic_error, Some(DiagnosticCode::Io));
    assert_eq!(manifest.captured_frames, 5, "the capture is untouched");
    assert_eq!(manifest.archived_samples, 0);
    // All five captured frames really are lost — the archive never held a byte
    // of them — so the hole 1..5 is the truth, with no surviving sample in it.
    let gap = manifest.audio_gap.expect("the hole is recorded");
    assert_eq!(gap.first_sequence, 1);
    assert_eq!(gap.last_sequence, 5);
    assert_eq!(gap.archived_offset_samples, 0);
    assert_eq!(gap.lost_samples, 5 * 512);
    assert!(has_journal_event(&dir, "archive_error"));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_failed_audio_write_keeps_an_unverified_partial() {
    let scratch = Scratch::new("audio-write");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let (sink, dir) = take_with_faulted_archive(&logs, &io, key(1), |faults| {
        faults.short_write = Some(5);
    });

    assert!(dir.join("audio.flac.part").exists(), "the partial stays");
    assert!(
        !dir.join("audio.flac").exists(),
        "a torn write earns no verified name"
    );
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Success));
    assert_eq!(manifest.audio_status, AudioStatus::Error);
    assert!(!manifest.verified);
    assert_eq!(manifest.audio_file, None);
    assert_eq!(manifest.diagnostic_error, Some(DiagnosticCode::Io));
    assert!(sink.error().is_some(), "the class stays in memory too");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_flush_that_fails_before_the_frames_are_down_is_not_an_archive() {
    let scratch = Scratch::new("audio-sync");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let (sink, dir) = take_with_faulted_archive(&logs, &io, key(1), |faults| {
        faults.sync_error = true;
    });

    let part = dir.join("audio.flac.part");
    assert!(part.exists(), "the partial is left where it stopped");
    assert!(!dir.join("audio.flac").exists());
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.audio_status, AudioStatus::Error);
    assert!(!manifest.verified);
    assert_eq!(manifest.audio_file, None);
    assert_eq!(manifest.diagnostic_error, Some(DiagnosticCode::Io));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn an_archive_that_cannot_be_read_back_is_never_published() {
    let scratch = Scratch::new("audio-read");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let (sink, dir) = take_with_faulted_archive(&logs, &io, key(1), |faults| {
        faults.open_read_error = true;
    });

    // The frames, the tail and the patch all reached the file. What failed is
    // the independent check, and an unchecked file keeps no verified name.
    let part = dir.join("audio.flac.part");
    assert!(
        std::fs::metadata(&part).expect("the partial").len() > 42,
        "the frames were written"
    );
    assert!(!dir.join("audio.flac").exists());
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.audio_status, AudioStatus::Error);
    assert!(!manifest.verified);
    assert_eq!(manifest.audio_file, None);
    assert_eq!(manifest.diagnostic_error, Some(DiagnosticCode::Io));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_partial_whose_bytes_changed_is_never_given_a_verified_name() {
    let scratch = Scratch::new("audio-corrupt");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let (sink, dir) = take_with_faulted_archive(&logs, &io, key(1), |faults| {
        faults.corrupt_open_read = Some(("audio.flac.part".to_owned(), 100));
    });

    let part = dir.join("audio.flac.part");
    assert!(part.exists(), "the partial stays");
    assert!(
        !dir.join("audio.flac").exists(),
        "a byte the disk changed is caught before any name is given"
    );
    assert!(!dir.join("audio.incomplete.flac").exists());
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.audio_status, AudioStatus::Error);
    assert!(!manifest.verified);
    assert_eq!(manifest.audio_file, None);
    assert!(
        matches!(
            manifest.diagnostic_error,
            Some(DiagnosticCode::Decode) | Some(DiagnosticCode::HashMismatch)
        ),
        "the decoder's own class: {:?}",
        manifest.diagnostic_error
    );
    assert!(has_journal_event(&dir, "archive_error"));
    assert_eq!(
        manifest.archived_samples,
        5 * 512,
        "the frames really are in the file the check refused"
    );
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn an_archive_whose_header_cannot_be_patched_keeps_the_partial() {
    let scratch = Scratch::new("audio-seek");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let (sink, dir) = take_with_faulted_archive(&logs, &io, key(1), |faults| {
        faults.seek_error = true;
    });

    let part = dir.join("audio.flac.part");
    assert!(part.exists(), "the partial stays");
    assert!(!dir.join("audio.flac").exists());
    // The header on disk is still the one the file was opened with, so the
    // check refuses the file rather than trusting it.
    let bytes = std::fs::read(&part).expect("the partial");
    let info = claxon::FlacReader::new(&bytes[..])
        .expect("a readable header")
        .streaminfo();
    assert!(
        matches!(info.samples, None | Some(0)),
        "the sentinel header is still there"
    );
    assert!(flac::verify(&bytes[..], 5 * 512, &[0u8; 16]).is_err());
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.audio_status, AudioStatus::Error);
    assert!(!manifest.verified);
    assert_eq!(manifest.audio_file, None);
    assert_eq!(manifest.diagnostic_error, Some(DiagnosticCode::Io));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn an_archive_that_cannot_be_renamed_keeps_its_partial() {
    let scratch = Scratch::new("audio-publish");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let (sink, dir) = take_with_faulted_archive(&logs, &io, key(1), |faults| {
        faults.publish_error = true;
    });

    let part = dir.join("audio.flac.part");
    assert!(part.exists(), "the partial stays");
    assert!(!dir.join("audio.flac").exists());
    assert!(!dir.join("audio.incomplete.flac").exists());
    // The bytes themselves are a valid archive: the name is what failed.
    let bytes = std::fs::read(&part).expect("the partial");
    flac::verify_pcm(&bytes[..], 5 * 512, &frame_pcm(5)).expect("the bytes are sound");
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.audio_status, AudioStatus::Error);
    assert!(!manifest.verified);
    assert_eq!(manifest.audio_file, None);
    assert_eq!(manifest.diagnostic_error, Some(DiagnosticCode::Io));
    sink.shutdown(Duration::from_secs(2));
}

/// Pushes every capture sequence in `from..=to` from `waveform`, one frame at
/// a time, exactly the way the capture hands its frames over.
fn push_range(archive: &mut CaptureArchive, waveform: &[u8], from: u64, to: u64) {
    for sequence in from..=to {
        let at = (sequence as usize - 1) * FRAME_BYTES;
        archive.push(sequence, &waveform[at..at + FRAME_BYTES]);
    }
}

#[test]
fn two_separated_holes_and_a_dropped_tail_are_three_holes_with_their_own_samples() {
    // The pool and the queue are 256 buffers each, so a push while the writer
    // is parked succeeds exactly while the pool is not dry. That makes every
    // hole below exact and reproducible: 256 queued, then the drops.
    const BURST: u64 = 256;
    let scratch = Scratch::new("holes-two");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Live).unwrap();
    let entry = sink.entry_for(key(1)).unwrap();
    let total = 779u64;
    let waveform = frame_pcm(total);
    let mut archive = handle.capture();

    // Hole one: the pool is dry after a full burst, so 257..260 never reach the
    // archive. The frame that follows them (261) is what proves the hole ended.
    sink.worker_pause();
    push_range(&mut archive, &waveform, 1, BURST);
    push_range(&mut archive, &waveform, BURST + 1, BURST + 4);
    sink.worker_resume();
    sink.worker_passes(16);

    // Hole two, separated from the first by 256 archived frames.
    sink.worker_pause();
    push_range(&mut archive, &waveform, BURST + 5, 2 * BURST + 4);
    push_range(&mut archive, &waveform, 2 * BURST + 5, 2 * BURST + 8);
    sink.worker_resume();
    sink.worker_passes(24);

    // A last burst, and then a short dropped tail: 777..779 never arrive and
    // there is no surviving frame after them to prove anything — the tail is
    // the hole the archive notes at finalization.
    sink.worker_pause();
    push_range(&mut archive, &waveform, 2 * BURST + 9, 3 * BURST);
    push_range(&mut archive, &waveform, 3 * BURST + 1, total);
    archive.finish(total, Outcome::Success);
    assert_eq!(
        entry.capture.snapshot().dropped_frames,
        4 + 4 + 3,
        "the drops are exactly the frames the dry pool refused"
    );
    assert_eq!(entry.capture.snapshot().dropped_samples, 11 * 512);
    drop(archive);
    sink.end(key(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_resume();
    sink.worker_passes(40);

    let dir = bundle(&logs, 1);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Success));
    assert_eq!(manifest.audio_status, AudioStatus::Incomplete);
    assert!(manifest.verified);
    assert_eq!(manifest.audio_file.as_deref(), Some("audio.incomplete.flac"));
    assert_eq!(manifest.captured_frames, total);
    assert_eq!(manifest.captured_samples, total * 512);
    assert_eq!(manifest.archived_samples, 3 * BURST * 512);
    assert_eq!(manifest.dropped_frames, 11);
    assert_eq!(manifest.dropped_samples, 11 * 512);
    assert_eq!(manifest.last_sequence, Some(total));

    // The manifest keeps the first hole as its one-field summary, and it is the
    // first hole's own numbers — never another hole's or an aggregate.
    let first = manifest.audio_gap.expect("the first hole");
    assert_eq!(first.first_sequence, BURST + 1);
    assert_eq!(first.last_sequence, BURST + 4);
    assert_eq!(first.archived_offset_samples, BURST * 512);
    assert_eq!(first.lost_samples, 4 * 512);

    // Every hole is its own journal record, in order, each with its own offset
    // (including the staged PCM) and its own sample loss.
    let gaps: Vec<serde_json::Value> = journal_records(&dir)
        .into_iter()
        .filter(|record| record["event"] == "audio_gap")
        .collect();
    assert_eq!(gaps.len(), 3, "three separated holes, three records");
    let expected = [
        (BURST + 1, BURST + 4, BURST * 512, 4 * 512),
        (2 * BURST + 5, 2 * BURST + 8, 2 * BURST * 512, 4 * 512),
        (total - 2, total, 3 * BURST * 512, 3 * 512),
    ];
    for (record, (first_sequence, last_sequence, offset, lost)) in
        gaps.iter().zip(expected)
    {
        assert_eq!(record["data"]["first_sequence"], first_sequence);
        assert_eq!(record["data"]["last_sequence"], last_sequence);
        assert_eq!(record["data"]["archived_offset_samples"], offset);
        assert_eq!(record["data"]["lost_samples"], lost);
    }
    let lost_total: u64 = expected.iter().map(|(_, _, _, lost)| lost).sum();
    assert_eq!(
        lost_total,
        manifest.dropped_samples,
        "the holes together account for exactly what the capture lost"
    );

    // What the file really holds, sample for sample: the three surviving
    // bursts, in order and with nothing of any hole in them.
    let mut surviving = Vec::new();
    for (from, to) in [
        (1u64, BURST),
        (BURST + 5, 2 * BURST + 4),
        (2 * BURST + 9, total - 3),
    ] {
        let at = (from as usize - 1) * FRAME_BYTES;
        let end = to as usize * FRAME_BYTES;
        surviving.extend_from_slice(&waveform[at..end]);
    }
    assert_eq!(surviving.len(), 3 * BURST as usize * FRAME_BYTES);
    flac::verify_pcm(
        std::fs::File::open(dir.join("audio.incomplete.flac")).expect("the published subset"),
        3 * BURST * 512,
        &surviving,
    )
    .expect("the surviving bursts, sample for sample");
    sink.shutdown(Duration::from_secs(2));
}

/// The samples of a capture's very last frame when the recorder's flush leaves
/// it short, which is the only frame of a take that is not 512 samples.
const SHORT_TAIL_SAMPLES: usize = 37;

#[test]
fn a_middle_hole_is_priced_in_whole_frames_even_when_the_take_ends_short() {
    // A short final frame must not change what an earlier hole cost: the middle
    // hole pays for whole 512-sample frames, never the take's average frame
    // length, which the short tail would floor.
    const BURST: u64 = 256;
    let scratch = Scratch::new("holes-short-tail");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Live).unwrap();
    let entry = sink.entry_for(key(1)).unwrap();
    let waveform = frame_pcm(BURST + 41);
    let mut archive = handle.capture();

    // A full burst fills the pool and the queue exactly; the four frames after
    // it find the pool dry and never arrive. That is the hole.
    sink.worker_pause();
    push_range(&mut archive, &waveform, 1, BURST);
    push_range(&mut archive, &waveform, BURST + 1, BURST + 4);
    // Let the writer drain, which returns the pool. The surviving frames and
    // then the recorder's short final frame (37 samples, the only frame of the
    // take that is not 512) go into a pool that is full again, so they are all
    // archived.
    sink.worker_resume();
    sink.worker_passes(16);
    push_range(&mut archive, &waveform, BURST + 5, BURST + 40);
    let short_at = (BURST as usize + 40) * FRAME_BYTES;
    let short = &waveform[short_at..short_at + SHORT_TAIL_SAMPLES * 2];
    archive.push(BURST + 41, short);
    archive.finish(BURST + 41, Outcome::Success);
    assert_eq!(entry.capture.snapshot().dropped_frames, 4);
    assert_eq!(entry.capture.snapshot().dropped_samples, 4 * 512);
    assert_eq!(
        entry.capture.snapshot().produced_samples,
        (BURST + 40) * 512 + SHORT_TAIL_SAMPLES as u64
    );
    drop(archive);
    sink.end(key(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_passes(40);

    let dir = bundle(&logs, 1);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.audio_status, AudioStatus::Incomplete);
    assert!(manifest.verified);
    // The hole cost exactly four whole frames, short tail and all.
    let gap = manifest.audio_gap.expect("the hole");
    assert_eq!(gap.first_sequence, BURST + 1);
    assert_eq!(gap.last_sequence, BURST + 4);
    assert_eq!(gap.archived_offset_samples, BURST * 512);
    assert_eq!(gap.lost_samples, 4 * 512, "whole frames, not an average");
    // The short final frame is in the archive, not in any hole.
    let archived = BURST * 512 + 36 * 512 + SHORT_TAIL_SAMPLES as u64;
    assert_eq!(manifest.archived_samples, archived);
    assert_eq!(
        manifest.captured_samples,
        archived + 4 * 512,
        "what the file holds plus what the hole cost is the whole take"
    );

    // And the file really holds the surviving frames and that short tail, in
    // order, with the hole's four frames absent.
    let mut surviving = Vec::new();
    surviving.extend_from_slice(&waveform[..BURST as usize * FRAME_BYTES]);
    surviving.extend_from_slice(
        &waveform[(BURST as usize + 4) * FRAME_BYTES..(BURST as usize + 40) * FRAME_BYTES],
    );
    surviving.extend_from_slice(short);
    assert_eq!(surviving.len() as u64, archived * 2);
    flac::verify_pcm(
        std::fs::File::open(dir.join("audio.incomplete.flac")).expect("the published subset"),
        archived,
        &surviving,
    )
    .expect("the surviving frames and the short tail, sample for sample");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn a_dropped_short_final_frame_is_the_tail_hole_and_pays_only_its_own_samples() {
    // The last frame of a take is the only one a flush may leave short, and
    // losing it must cost exactly its own samples — not a whole nominal frame
    // and not an average.
    // Two full bursts are what the take archives, and the short final frame is
    // the only sequence after the second one.
    const BURST: u64 = 256;
    let total = 2 * BURST + 3;
    let scratch = Scratch::new("holes-dropped-short-tail");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Live).unwrap();
    let entry = sink.entry_for(key(1)).unwrap();
    let waveform = frame_pcm(total);
    let mut archive = handle.capture();

    // A hole of two whole frames, ended by the surviving frame 259.
    sink.worker_pause();
    push_range(&mut archive, &waveform, 1, BURST);
    push_range(&mut archive, &waveform, BURST + 1, BURST + 2);
    sink.worker_resume();
    sink.worker_passes(16);
    // A full second burst empties the pool, and the short final frame then finds
    // it dry: it is lost, and no surviving frame follows it to prove anything.
    sink.worker_pause();
    push_range(&mut archive, &waveform, BURST + 3, total - 1);
    let short_at = (total as usize - 1) * FRAME_BYTES;
    let short = &waveform[short_at..short_at + SHORT_TAIL_SAMPLES * 2];
    archive.push(total, short);
    archive.finish(total, Outcome::Success);
    assert_eq!(entry.capture.snapshot().dropped_frames, 3);
    assert_eq!(
        entry.capture.snapshot().dropped_samples,
        2 * 512 + SHORT_TAIL_SAMPLES as u64,
        "two whole frames and the short one, which really is 37 samples"
    );
    drop(archive);
    sink.end(key(1), Outcome::Success);
    sink.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    sink.worker_resume();
    sink.worker_passes(40);

    let dir = bundle(&logs, 1);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.audio_status, AudioStatus::Incomplete);
    assert!(manifest.verified);
    let archived = 2 * BURST * 512;
    assert_eq!(manifest.archived_samples, archived);
    // The manifest's summary is the first hole, priced in whole frames.
    let first = manifest.audio_gap.expect("the first hole");
    assert_eq!(first.first_sequence, BURST + 1);
    assert_eq!(first.last_sequence, BURST + 2);
    assert_eq!(first.archived_offset_samples, BURST * 512);
    assert_eq!(first.lost_samples, 2 * 512);

    // Two holes: the middle one in whole frames, the dropped short tail in its
    // own exact samples.
    let gaps: Vec<serde_json::Value> = journal_records(&dir)
        .into_iter()
        .filter(|record| record["event"] == "audio_gap")
        .collect();
    assert_eq!(gaps.len(), 2);
    assert_eq!(gaps[0]["data"]["first_sequence"], BURST + 1);
    assert_eq!(gaps[0]["data"]["last_sequence"], BURST + 2);
    assert_eq!(gaps[0]["data"]["archived_offset_samples"], BURST * 512);
    assert_eq!(gaps[0]["data"]["lost_samples"], 2 * 512);
    assert_eq!(gaps[1]["data"]["first_sequence"], total);
    assert_eq!(gaps[1]["data"]["last_sequence"], total);
    assert_eq!(gaps[1]["data"]["archived_offset_samples"], archived);
    assert_eq!(gaps[1]["data"]["lost_samples"], SHORT_TAIL_SAMPLES);
    let lost: u64 = gaps
        .iter()
        .map(|record| record["data"]["lost_samples"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(lost, manifest.dropped_samples);
    assert_eq!(archived + manifest.dropped_samples, manifest.captured_samples);
    let mut surviving = waveform[..BURST as usize * FRAME_BYTES].to_vec();
    surviving.extend_from_slice(
        &waveform[(BURST as usize + 2) * FRAME_BYTES..(total as usize - 1) * FRAME_BYTES],
    );
    flac::verify_pcm(
        std::fs::File::open(dir.join("audio.incomplete.flac")).expect("the published subset"),
        archived,
        &surviving,
    )
    .expect("the two surviving bursts, sample for sample");
    sink.shutdown(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// Content records
// ---------------------------------------------------------------------------
//
// One queue, one writer and one journal carry the content a producer archives
// beside the numeric events. Every test below drives the real sink: the bodies
// are the ones a redacting producer would hand over, and an assertion is about
// what a reader of the bundle finds on disk.

/// The content records of one bundle, in journal order.
fn content_records(dir: &Path) -> Vec<serde_json::Value> {
    journal_records(dir)
        .into_iter()
        .filter(|record| record["event"] == "content")
        .collect()
}

/// The bundle one owner and generation opened: the fourth field of the session
/// directory name is the owner, the fifth the generation.
fn bundle_of(logs: &Path, owner: &str, generation: u64) -> PathBuf {
    session_dirs(logs)
        .into_iter()
        .find(|dir| {
            let parts = session_name_parts(dir);
            parts[3] == owner && parts[4] == generation.to_string()
        })
        .expect("the take's bundle")
}

#[test]
fn a_content_body_far_past_the_bounded_record_size_is_saved_exactly() {
    let scratch = Scratch::new("content-exact");
    let logs = scratch.logs();
    let cap = 1_000_000u64;
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), cap),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Smart).unwrap();

    // Every shape a JSON string has to survive: multi-byte characters, both
    // escapes, a newline and a tab, and the punctuation a URL carries. The
    // body is far past the bound a report is held to, so a record that was
    // shortened anywhere would not come back as itself.
    let mut body = String::from("{\"text\":\"");
    while body.len() < 12_000 {
        body.push_str("éЖ🙂 \"quoted\" back\\slash %41 a&b<c>d\n\ttab ");
    }
    body.push_str("\"}");
    assert!(
        body.len() > RENDERER_REPORT_MAX_BYTES,
        "the body is past the bounded record size"
    );

    assert!(
        handle.content(ContentKind::RuntimeContext, ContentFormat::Text, body.clone()),
        "the record is queued"
    );
    sink.worker_passes(4);

    let dir = bundle(&logs, 1);
    let records = content_records(&dir);
    assert_eq!(records.len(), 1, "one record for the one body");
    let record = &records[0];
    assert_eq!(record["schemaVersion"], 2);
    assert_eq!(record["source"], "shell", "a runtime context is the shell's");
    assert_eq!(record["requestId"], serde_json::Value::Null);
    assert_eq!(record["data"]["kind"], "runtime_context");
    assert_eq!(record["data"]["format"], "text");
    assert_eq!(record["data"]["route"], serde_json::Value::Null);
    assert_eq!(
        record["data"]["body"].as_str(),
        Some(body.as_str()),
        "the body is saved whole and comes back as itself"
    );
    assert!(record["data"]["observedUtcMs"].as_u64().is_some());
    assert!(record["data"]["observedElapsedUs"].as_u64().is_some());
    assert!(disk_total(&logs) <= cap, "the quota stayed bounded");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn queue_pressure_counts_dropped_content_and_never_writes_part_of_a_body() {
    let scratch = Scratch::new("content-pressure");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    // The writer is held, so the shared queue really backs up and overflows.
    sink.worker_pause();
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    let entry = sink.entry_for(key(1)).unwrap();

    let extra = 20usize;
    let mut bodies: Vec<String> = Vec::new();
    let mut refused = 0u64;
    for index in 0..(META_QUEUE + extra) {
        let body = format!("content-record-{index}-{}", "p".repeat(300));
        if !handle.content(ContentKind::CaptureContext, ContentFormat::Json, body.clone()) {
            refused += 1;
        }
        bodies.push(body);
    }
    // The take's own begin record shares this queue, so the exact number of
    // refusals is the queue's business; what this pins is that the queue really
    // overflowed and that every refusal was counted as the drop it is.
    assert!(refused > 0, "the queue really overflowed");
    assert_eq!(
        entry.dropped_events(),
        refused,
        "every record the queue refused is counted as the drop it is"
    );

    sink.worker_resume();
    sink.worker_passes(24);

    let dir = bundle(&logs, 1);
    let written: Vec<String> = content_records(&dir)
        .into_iter()
        .map(|record| record["data"]["body"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        written.len() as u64,
        META_QUEUE as u64 + extra as u64 - refused,
        "exactly the records the queue took were written"
    );
    for body in &written {
        assert!(
            bodies.contains(body),
            "a body was written in part: {} bytes",
            body.len()
        );
    }

    finish_take(&sink, &handle, Outcome::Success);
    let manifest = manifest_of(&bundle(&logs, 1)).expect("a manifest");
    assert_eq!(
        manifest.dropped_events, refused,
        "the drops the queue counted are in the manifest"
    );
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn the_content_of_a_cancelled_take_is_preserved() {
    let scratch = Scratch::new("content-cancelled");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Live).unwrap();
    let interim = "the interim of a dictation that was cancelled".to_owned();
    assert!(handle.content(ContentKind::LiveOutput, ContentFormat::Json, interim.clone()));

    // The terminal rises while the record is still queued: the take is not
    // closed over content it has not written yet, and a cancellation takes
    // nothing that was already produced away.
    finish_take(&sink, &handle, Outcome::Cancelled);

    let dir = bundle(&logs, 1);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Cancelled));
    let records = content_records(&dir);
    assert_eq!(records.len(), 1, "the cancelled take kept its content");
    assert_eq!(records[0]["data"]["kind"], "live_output");
    assert_eq!(records[0]["source"], "relay");
    assert_eq!(records[0]["data"]["body"].as_str(), Some(interim.as_str()));
    assert!(has_journal_event(&dir, "session_end"));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn the_content_of_a_failed_take_is_preserved() {
    let scratch = Scratch::new("content-failed");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 1_000_000),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    let trace = handle.request(Route::Smart).unwrap();
    let sent = format!("{{\"setup\": {{\"model\": \"{}\"}}}}", "m".repeat(5_000));
    assert!(trace.content(ContentKind::ProviderRequest, ContentFormat::Json, sent.clone()));
    trace.cause(RequestCause::UpstreamError);
    trace.finish(Outcome::Error);
    let request_id = trace.request_id();
    drop(trace);
    finish_take(&sink, &handle, Outcome::Error);

    let dir = bundle(&logs, 1);
    let manifest = manifest_of(&dir).expect("a manifest");
    assert_eq!(manifest.outcome, Some(Outcome::Error));
    assert!(
        has_journal_event(&dir, "session_end"),
        "the failed take closed its own journal"
    );
    let records = content_records(&dir);
    assert_eq!(records.len(), 1, "the failed take kept its content");
    assert_eq!(records[0]["source"], "provider");
    assert_eq!(records[0]["data"]["kind"], "provider_request");
    assert_eq!(records[0]["data"]["route"], "smart");
    assert_eq!(records[0]["requestId"].as_u64(), Some(request_id));
    assert_eq!(records[0]["data"]["body"].as_str(), Some(sent.as_str()));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn an_archive_that_cannot_rewrite_its_manifest_counts_the_content_it_cannot_write() {
    let scratch = Scratch::new("content-refused");
    let logs = scratch.logs();
    let io = ScriptedIo::new();
    let sink = Diagnostics::with_io(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
        io.clone(),
    );
    let handle = sink.begin(key(1), Mode::Smart).unwrap();
    let entry = sink.entry_for(key(1)).unwrap();
    let kept = "the request that was already archived".to_owned();
    assert!(handle.content(ContentKind::ProviderRequest, ContentFormat::Json, kept.clone()));
    sink.worker_passes(2);
    let dir = bundle(&logs, 1);
    assert!(has_journal_event(&dir, "content"), "the record is on disk");

    // From here the manifest cannot be rewritten, exactly like a disk that
    // refuses the replacement. The archive closes itself rather than claim a
    // snapshot it could not write, and nothing that follows is written or
    // silently reported as written.
    io.fault(|faults| faults.replace_error = true);
    sink.advance_clock(SNAPSHOT + Duration::from_millis(1));
    sink.worker_passes(2);
    assert_eq!(sink.error(), Some(DiagnosticCode::Io));
    assert!(sink.is_enabled(), "the take failed, not the sink");

    // A closed archive is not a fake success: whichever side refuses these -
    // the take that will not queue them, or the sink that cannot write them -
    // the count below is what the take is owed.
    let dropped = 5u64;
    for index in 0..dropped {
        let _ = handle.content(
            ContentKind::ProviderResponse,
            ContentFormat::Text,
            format!("later-answer-{index}"),
        );
    }
    sink.worker_passes(4);
    assert_eq!(
        entry.dropped_events(),
        dropped,
        "every record the closed archive could not take is counted"
    );
    let records = content_records(&dir);
    assert_eq!(records.len(), 1, "nothing later was written");
    assert_eq!(records[0]["data"]["body"].as_str(), Some(kept.as_str()));
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn two_takes_and_their_requests_keep_their_content_apart() {
    let scratch = Scratch::new("content-separate");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    let dictation = sink.begin(key(1), Mode::Comparison).unwrap();
    let lab = sink.begin(lab(1), Mode::Comparison).unwrap();
    // Two routes of one comparison are two requests on one capture: each has
    // its own trace, its own request id and its own content.
    let live = dictation.request(Route::Live).unwrap();
    let smart = dictation.request(Route::Smart).unwrap();

    let live_body = "the frame the live relay received".to_owned();
    let smart_body = "the answer the smart route received".to_owned();
    let lab_body = "the laboratory's own context".to_owned();
    assert!(live.content(ContentKind::LiveMessage, ContentFormat::Json, live_body.clone()));
    assert!(smart.content(ContentKind::ProviderResponse, ContentFormat::Json, smart_body.clone()));
    assert!(lab.content(ContentKind::RuntimeContext, ContentFormat::Text, lab_body.clone()));
    sink.worker_passes(6);

    let dictation_records = content_records(&bundle_of(&logs, "dictation", 1));
    let lab_records = content_records(&bundle_of(&logs, "lab", 1));
    assert_eq!(dictation_records.len(), 2, "two routes, two records");
    assert_eq!(lab_records.len(), 1, "the lab's own take, its own record");

    let carrying = |records: &[serde_json::Value], body: &str| -> serde_json::Value {
        records
            .iter()
            .find(|record| record["data"]["body"].as_str() == Some(body))
            .unwrap_or_else(|| panic!("no record of this bundle carries {body}"))
            .clone()
    };
    let live_record = carrying(&dictation_records, &live_body);
    assert_eq!(live_record["data"]["kind"], "live_message");
    assert_eq!(live_record["data"]["route"], "live");
    assert_eq!(live_record["source"], "relay");
    assert_eq!(live_record["requestId"].as_u64(), Some(live.request_id()));

    let smart_record = carrying(&dictation_records, &smart_body);
    assert_eq!(smart_record["data"]["kind"], "provider_response");
    assert_eq!(smart_record["data"]["route"], "smart");
    assert_eq!(smart_record["source"], "provider");
    assert_eq!(smart_record["requestId"].as_u64(), Some(smart.request_id()));
    assert_ne!(live.request_id(), smart.request_id());

    // The lab's record is in the lab's bundle only, and it belongs to no
    // request at all.
    let lab_record = carrying(&lab_records, &lab_body);
    assert_eq!(lab_record["source"], "shell");
    assert_eq!(lab_record["requestId"], serde_json::Value::Null);
    assert_eq!(
        manifest_of(&bundle_of(&logs, "lab", 1)).expect("a manifest").mode,
        Mode::Comparison
    );
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn raw_provider_text_is_archived_whole_and_classified_by_its_route() {
    let scratch = Scratch::new("content-provider");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    let handle = sink.begin(key(1), Mode::Comparison).unwrap();
    let smart = handle.request(Route::Smart).unwrap();
    let live = handle.request(Route::Live).unwrap();

    // A malformed answer and a non-2xx body, both past the report bound: what
    // the provider really sent is what has to be readable afterwards.
    let malformed = format!("{{\"candidates\": [{{\"content\": \"{}\"", "x".repeat(6_000));
    let rejected = format!("<html>429 {} </html>", "y".repeat(6_000));
    let relay_error = "the live socket dropped".to_owned();
    assert!(smart.content(
        ContentKind::ProviderResponse,
        ContentFormat::Json,
        malformed.clone()
    ));
    assert!(smart.content(
        ContentKind::TransportError,
        ContentFormat::Text,
        "the smart transport failed".to_owned()
    ));
    assert!(live.content(
        ContentKind::ProviderResponse,
        ContentFormat::Json,
        rejected.clone()
    ));
    assert!(live.content(
        ContentKind::TransportError,
        ContentFormat::Text,
        relay_error.clone()
    ));
    sink.worker_passes(6);

    let dir = bundle(&logs, 1);
    let records = content_records(&dir);
    assert_eq!(records.len(), 4, "one record per body");
    let carrying = |body: &str| -> serde_json::Value {
        records
            .iter()
            .find(|record| record["data"]["body"].as_str() == Some(body))
            .unwrap_or_else(|| panic!("no record carries {} bytes", body.len()))
            .clone()
    };

    let bad = carrying(&malformed);
    assert_eq!(bad["data"]["kind"], "provider_response");
    assert_eq!(bad["data"]["format"], "json");
    assert_eq!(bad["data"]["route"], "smart");
    assert_eq!(bad["source"], "provider");
    assert_eq!(bad["requestId"].as_u64(), Some(smart.request_id()));
    assert_eq!(bad["data"]["body"].as_str(), Some(malformed.as_str()));

    let refused = carrying(&rejected);
    assert_eq!(refused["data"]["kind"], "provider_response");
    assert_eq!(refused["data"]["body"].as_str(), Some(rejected.as_str()));

    // A transport error follows the route it was produced on: the live relay's
    // is the relay's, and everything else is the provider's.
    let relay = carrying(&relay_error);
    assert_eq!(relay["data"]["kind"], "transport_error");
    assert_eq!(relay["source"], "relay");
    assert_eq!(relay["requestId"].as_u64(), Some(live.request_id()));
    let provider_side = carrying("the smart transport failed");
    assert_eq!(provider_side["data"]["kind"], "transport_error");
    assert_eq!(provider_side["source"], "provider");
    sink.shutdown(Duration::from_secs(2));
}

#[test]
fn the_servers_final_result_is_stored_whole_past_the_report_bound() {
    let scratch = Scratch::new("content-result");
    let logs = scratch.logs();
    let sink = Diagnostics::for_test_with_storage(
        &logs,
        policy(Duration::from_secs(86_400), 10_000_000),
        epoch(),
    );
    let take = sink.begin(lab(1), Mode::Comparison).unwrap();

    // The wrapper's own technical report: a long final transcription and one
    // error the script reported beside it. The result is the whole object the
    // wrapper sends - all three mode maps - and it is past the 4096 bytes a
    // report's numbers are held to.
    let final_text = "final transcription ".repeat(400);
    let result_body =
        format!("{{\"results\":{{\"live\":\"{final_text}\"}},\"errors\":{{}},\"errorDetails\":{{}}}}");
    let error_body = "{\"mode\":\"live\",\"message\":\"the socket closed early\"}";
    let mut report = super::report(lab(1), 1);
    report.result = Some(serde_json::from_str(&result_body).expect("the wrapper's result"));
    report.errors = vec![serde_json::from_str(error_body).expect("the wrapper's error")];
    assert_eq!(
        sink.admit_renderer_report(report),
        ReportAdmission::Accepted,
        "the report is queued and its index committed"
    );
    sink.worker_passes(6);

    let dir = bundle_of(&logs, "lab", 1);
    let records = content_records(&dir);
    let result = records
        .iter()
        .find(|record| record["data"]["kind"] == "recorder_result")
        .expect("the result record");
    assert_eq!(result["schemaVersion"], 2);
    assert_eq!(result["source"], "renderer");
    assert_eq!(result["data"]["format"], "json");
    assert_eq!(result["data"]["route"], serde_json::Value::Null);
    assert_eq!(result["requestId"], serde_json::Value::Null);
    let stored = result["data"]["body"].as_str().expect("a body");
    let stored_value: serde_json::Value =
        serde_json::from_str(stored).expect("the stored result is JSON");
    let expected: serde_json::Value =
        serde_json::from_str(&result_body).expect("the wrapper's result");
    assert_eq!(
        stored_value, expected,
        "the whole result is stored exactly as it was sanitized"
    );
    assert!(
        stored.len() > RENDERER_REPORT_MAX_BYTES,
        "the result is past the numeric report bound"
    );
    assert!(
        stored.contains(&final_text),
        "the long final transcription is intact"
    );

    let error = records
        .iter()
        .find(|record| record["data"]["kind"] == "recorder_error")
        .expect("the error record");
    let stored_error = error["data"]["body"].as_str().expect("a body");
    let stored_error: serde_json::Value =
        serde_json::from_str(stored_error).expect("the stored error is JSON");
    let expected_error: serde_json::Value =
        serde_json::from_str(error_body).expect("the wrapper's error");
    assert_eq!(
        stored_error, expected_error,
        "the wrapper's error is stored whole"
    );
    assert_eq!(error["source"], "renderer");
    assert_eq!(take.key(), lab(1));
    sink.shutdown(Duration::from_secs(2));
}
