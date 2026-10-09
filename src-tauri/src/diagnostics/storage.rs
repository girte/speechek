//! The storage half of the development diagnostics.
//!
//! One named thread (`speechek-diagnostics`) owns every write. Producers only
//! leave a number or a queued record behind; the thread creates the session
//! directory under the archive root (`<config dir>/logs`), writes the compact
//! `session.json`, appends the rotated `events-NNNNNN.jsonl`, keeps the whole
//! tree inside the policy with a length ledger, prunes what has aged out and
//! recovers what an earlier run left behind.
//!
//! What else this thread owns:
//!
//! * the take's PCM queue. It is drained here, one fixed 2048-sample block at a
//!   time, into the private `audio.flac.part`; once the capture has published
//!   its watermark and the queue is empty the short tail is encoded, the header
//!   is patched in place and an independent decoder checks the file before it
//!   earns a verified name. No whole recording is ever held in memory, nothing
//!   is ever invented on disk, and a frame the archive could not take is
//!   counted as the hole it is rather than silently dropped.
//! * it never guesses an outcome. A take the shell never closed is sealed and
//!   recorded as [`Outcome::Abandoned`] with [`DiagnosticCode::MissingTerminal`]
//!   in its manifest, and a manifest an earlier process left open is rewritten
//!   as abandoned on the next startup, never as a success.
//!
//! The quota is the sum of the real lengths of every file the sink owns under
//! the root — journal segments, partials, staging copies and the 8192-byte
//! finalization reserve of every open take. Every write is admitted against
//! that sum before a byte is produced; an ordinary write of a live take first
//! makes room for itself by removing the oldest finished bundles, while a
//! recovery record is simply left undone. After a failed write or a failed
//! delete the lengths are read back from the disk rather than assumed, and a
//! tree that appeared under the root and is not the archive's own stops the
//! sink instead of being written past.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::flac::{self, FlacEncoder, FrameCommit};

use super::{
    system_millis, ArchiveError, AudioGap, AudioPacket, AudioStatus, AudioVerified,
    CaptureCounters, CaptureSnapshot, Clock, ClockAdjusted, DiagnosticCode, Event, Meta, Mode,
    Outcome, Owner, PendingEvent, Policy, Registry, SessionBegin, SessionEnd, SessionEntry,
    SessionKey,
};

// ---------------------------------------------------------------------------
// The audio archive
// ---------------------------------------------------------------------------

/// One hole the archive recorded: the capture sequences it never received, the
/// samples it had accepted when the hole opened, the last sequence it covers and
/// the samples that hole alone cost.
#[derive(Clone, Copy)]
struct GapSpan {
    first_sequence: u64,
    /// Every sample of every frame the archive accepted before the hole: what
    /// is already encoded *and* the block still being staged when it opened.
    started_at_samples: u64,
    /// The last capture sequence the hole covers. A hole in the middle of a
    /// take ends where the next surviving frame begins.
    last_lost: u64,
    /// The samples this hole alone cost, from the capture's own counter.
    lost_samples: u64,
}

impl GapSpan {
    fn to_gap(self) -> AudioGap {
        AudioGap {
            first_sequence: self.first_sequence,
            last_sequence: self.last_lost,
            archived_offset_samples: self.started_at_samples,
            lost_samples: self.lost_samples,
        }
    }
}

/// How far one take's audio archive got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioStage {
    /// No frame has reached the writer yet: nothing exists on disk.
    Idle,
    /// The header is on disk and encoded frames are being appended.
    Recording,
    /// Finalization has been attempted: the archive is as verified as it gets,
    /// and no frame is appended after this.
    Done,
}

/// The FLAC side of one take the writer is working for. It owns the streaming
/// encoder, the file the frames are appended to and the counters the journal
/// and the manifest report. Nothing here is a path the producer can see.
struct AudioArchive {
    /// `audio.flac.part` while the take is being written.
    part: PathBuf,
    /// `None` until the first frame, and after a failure.
    encoder: Option<FlacEncoder>,
    /// The writable handle: the appends, and the in-place header patch.
    file: Option<File>,
    /// The header the file was opened with, so the patched one can be checked
    /// to be exactly as long.
    header_len: usize,
    stage: AudioStage,
    /// Inter-channel samples the archive holds: encoded frames plus the block
    /// being staged. This is the offset a hole is measured from.
    accepted_samples: u64,
    committed_samples: u64,
    committed_frames: u64,
    /// The last capture sequence that really reached the archive.
    last_sequence: Option<u64>,
    /// The queue was proved empty by the last audio pass: a short read, not a
    /// full budget. The take is neither closed nor released until this is true.
    drained: bool,
    /// One hole that may still grow: it is flushed the moment a surviving frame
    /// proves it ended, and at the end of the take. Never a list — one pending
    /// span plus exact per-hole sample counts is all the archive keeps.
    pending_gap: Option<GapSpan>,
    /// Holes the take has already recorded and the samples they were charged.
    /// The manifest carries the first, the journal every one of them, and what
    /// the last hole pays is what is left after both.
    gaps_recorded: u64,
    charged_samples: u64,
    /// The first hole of the take, for the manifest's one-field summary.
    first_gap: Option<AudioGap>,
    audio_file: Option<String>,
    verified: bool,
    /// The take has been told what the archive ended up as. Finalization
    /// happens once, whether it succeeded or failed.
    reported: bool,
}

impl AudioArchive {
    /// A take the quota or the root never admitted: there is nothing to write.
    fn unavailable() -> Self {
        Self {
            part: PathBuf::new(),
            encoder: None,
            file: None,
            header_len: 0,
            stage: AudioStage::Done,
            accepted_samples: 0,
            committed_samples: 0,
            committed_frames: 0,
            last_sequence: None,
            // A take that never had a queue is drained from the start, so it is
            // never held open by an audio pass that cannot come.
            drained: true,
            pending_gap: None,
            gaps_recorded: 0,
            charged_samples: 0,
            first_gap: None,
            audio_file: None,
            verified: false,
            reported: false,
        }
    }

    fn new(part: PathBuf) -> Self {
        Self {
            part,
            ..Self::unavailable()
        }
        .at_stage(AudioStage::Idle)
    }

    fn at_stage(mut self, stage: AudioStage) -> Self {
        self.stage = stage;
        self
    }

    fn done(&self) -> bool {
        self.stage == AudioStage::Done
    }

    /// What the take's own status has to say: a verified file is Complete when
    /// every captured sample is in it and Incomplete when it holds the
    /// surviving subset; an unverified archive that has bytes is an error; and
    /// a take nothing reached stays None.
    fn status(&self) -> AudioStatus {
        match self.stage {
            AudioStage::Idle => AudioStatus::None,
            AudioStage::Recording => AudioStatus::Recording,
            AudioStage::Done => {
                if self.verified {
                    if self.audio_file.as_deref() == Some(AUDIO_NAME) {
                        AudioStatus::Complete
                    } else {
                        AudioStatus::Incomplete
                    }
                } else if self.committed_frames > 0 {
                    AudioStatus::Error
                } else {
                    AudioStatus::None
                }
            }
        }
    }

    /// Starts or extends the one hole that may still be growing. The capture's
    /// sequences start at one, so a take whose very first frame was lost
    /// reports one. A hole only extends when the sequence is contiguous with
    /// it; two separated holes are two separate spans.
    fn note_gap(&mut self, first_sequence: u64, last_lost: u64) {
        match &mut self.pending_gap {
            Some(gap) if first_sequence <= gap.last_lost.saturating_add(1) => {
                gap.last_lost = gap.last_lost.max(last_lost);
            }
            Some(_) => {
                // A new, separated hole without a flush is not something the
                // writer ever produces: the surviving frame that revealed the
                // first one flushes it before this can happen. Record what is
                // certain rather than swallowing the second hole.
                self.pending_gap = Some(GapSpan {
                    first_sequence,
                    started_at_samples: self.accepted_samples,
                    last_lost,
                    lost_samples: 0,
                });
            }
            None => {
                self.pending_gap = Some(GapSpan {
                    first_sequence,
                    started_at_samples: self.accepted_samples,
                    last_lost,
                    lost_samples: 0,
                });
            }
        }
    }

    /// Closes the pending hole and hands over its own numbers. The caller says
    /// exactly what that hole cost, so no hole is ever charged another's loss:
    /// a middle hole pays for the whole frames in its own span, and the last
    /// one pays whatever the take lost that no earlier hole has claimed.
    fn take_pending_gap(&mut self, lost: u64) -> Option<GapSpan> {
        let gap = self.pending_gap.take()?;
        self.charged_samples = self.charged_samples.saturating_add(lost);
        self.gaps_recorded += 1;
        Some(GapSpan {
            lost_samples: lost,
            ..gap
        })
    }

    /// Records every hole the take knows so far, each priced by `loss`, and
    /// remembers the first for the manifest summary.
    fn flush_gaps(&mut self, mut loss: impl FnMut(&GapSpan) -> u64) -> Vec<AudioGap> {
        let mut gaps = Vec::new();
        while let Some(span) = self.pending_gap {
            let lost = loss(&span);
            let Some(gap) = self.take_pending_gap(lost) else {
                break;
            };
            let gap = gap.to_gap();
            if self.first_gap.is_none() {
                self.first_gap = Some(gap);
            }
            gaps.push(gap);
        }
        gaps
    }

    /// Whether this archive can receive nothing at all: the take was refused a
    /// directory, or its bundle was removed by the sweep.
    fn refuses_writes(&self) -> bool {
        self.stage == AudioStage::Done
            && self.file.is_none()
            && self.encoder.is_none()
            && self.committed_frames == 0
    }
}

/// The one place an encoded frame becomes a byte of the file: the exact growth
/// is admitted against the quota *before* it is produced, the frame is written,
/// and only a complete write moves the ledger forward. The encoder calls this
/// before it counts the frame, so the take never reports a frame the file does
/// not hold.
struct FileCommit<'a> {
    state: &'a mut State,
    file: &'a mut File,
    path: PathBuf,
}

impl FrameCommit for FileCommit<'_> {
    fn write_frame(&mut self, encoded: &[u8]) -> Result<(), DiagnosticCode> {
        let wanted = encoded.len() as u64;
        let relative = self.path.parent().map(Path::to_path_buf);
        let state = &mut *self.state;
        state.admit(Admission::Grow, wanted)?;
        match state.io.write(&mut *self.file, encoded) {
            Ok(written) if written as u64 == wanted => {
                state.ledger.commit_growth(&self.path, wanted);
                Ok(())
            }
            Ok(_) => Err(state.reconcile(relative.as_deref(), wanted, DiagnosticCode::Io)),
            Err(code) => Err(state.reconcile(relative.as_deref(), wanted, code)),
        }
    }
}

/// How often the worker looks at its queues.
const WAKE: Duration = Duration::from_millis(100);
/// How often an open take's counters reach its journal.
const SNAPSHOT: Duration = Duration::from_secs(1);
/// How often retention and the quota sweep the tree.
const SWEEP: Duration = Duration::from_secs(60);
/// Metadata records one pass takes from the shared queue outside shutdown.
const META_PER_PASS: usize = 16;
/// What one open take keeps for its own finalization: one bounded final record
/// and one bounded staged manifest.
pub(crate) const SESSION_RESERVE: u64 = 8_192;
/// The largest final record the reserve has to cover.
const FINAL_RECORD_MAX: usize = 4_096;
/// The largest manifest the reserve has to cover.
const MANIFEST_MAX_BYTES: usize = 2_048;
/// Schema version of `session.json` and of every journal record.
const SCHEMA_VERSION: u32 = 1;
/// A journal segment is rotated before a record would pass this length.
const JSONL_MAX_BYTES: u64 = 1_000_000;

const MANIFEST_NAME: &str = "session.json";
const AUDIO_NAME: &str = "audio.flac";
const AUDIO_PART_NAME: &str = "audio.flac.part";
const AUDIO_INCOMPLETE_NAME: &str = "audio.incomplete.flac";
/// Captured frames one pass drains from a take's queue into its encoder. The
/// queue holds 256 and one pass moves 32: the writer keeps up with far more
/// than real time without ever holding the registry lock for a whole take.
const AUDIO_PER_PASS: usize = 32;
/// What one pass of a shutdown asks for: more than any take's queue can hold,
/// so the pass that finalizes a take while the sink is closing really has its
/// whole tail instead of the last 32 frames of it.
const AUDIO_DRAIN_ALL: usize = 1_024;

// ---------------------------------------------------------------------------
// The archive root
// ---------------------------------------------------------------------------

/// One directory entry, as the scan sees it. A reparse point is never followed
/// and never removed.
pub(crate) struct ListedEntry {
    pub name: OsString,
    pub dir: bool,
    pub reparse: bool,
}

/// What the worker needs from the filesystem, in one seam: the real Windows
/// implementation delegates to the private helpers in `settings`, and the tests
/// script failures on top of the same real calls.
pub(crate) trait ArchiveIo: Send + Sync + 'static {
    fn create_dir(&self, path: &Path) -> Result<(), DiagnosticCode>;
    fn create_file(&self, path: &Path) -> Result<File, DiagnosticCode>;
    fn open_read(&self, path: &Path) -> Result<File, DiagnosticCode>;
    fn open_append(&self, path: &Path) -> Result<File, DiagnosticCode>;
    fn write(&self, file: &mut File, bytes: &[u8]) -> Result<usize, DiagnosticCode>;
    /// Moves the file's write position. The FLAC header patch is the only
    /// caller: it overwrites the 42 bytes the file was opened with.
    fn seek(&self, file: &mut File, position: u64) -> Result<(), DiagnosticCode>;
    fn sync(&self, file: &mut File) -> Result<(), DiagnosticCode>;
    fn replace(&self, path: &Path, bytes: &[u8]) -> Result<(), DiagnosticCode>;
    fn publish(&self, source: &Path, destination: &Path) -> Result<(), DiagnosticCode>;
    fn remove_file(&self, path: &Path) -> Result<(), DiagnosticCode>;
    fn remove_tree(&self, dir: &Path) -> Result<(), DiagnosticCode>;
    fn list(&self, dir: &Path) -> Result<Vec<ListedEntry>, DiagnosticCode>;
    fn length(&self, path: &Path) -> Result<Option<u64>, DiagnosticCode>;
    fn verify(&self, path: &Path) -> Result<(), DiagnosticCode>;
}

/// The real root: private directories, private files and atomic replacements
/// from `crate::settings`, standard file handles for everything else.
pub(crate) struct RealIo;

impl ArchiveIo for RealIo {
    fn create_dir(&self, path: &Path) -> Result<(), DiagnosticCode> {
        crate::settings::create_private_directory(path).map_err(|_| DiagnosticCode::Acl)
    }

    fn create_file(&self, path: &Path) -> Result<File, DiagnosticCode> {
        crate::settings::create_private_file(path).map_err(|_| DiagnosticCode::Io)
    }

    fn open_read(&self, path: &Path) -> Result<File, DiagnosticCode> {
        File::open(path).map_err(|_| DiagnosticCode::Io)
    }

    fn open_append(&self, path: &Path) -> Result<File, DiagnosticCode> {
        // The name is checked first: appending never writes through a link and
        // never widens what the file already grants.
        <RealIo as ArchiveIo>::verify(self, path)?;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|_| DiagnosticCode::Io)
    }

    fn write(&self, file: &mut File, bytes: &[u8]) -> Result<usize, DiagnosticCode> {
        use std::io::Write;
        file.write_all(bytes).map_err(|_| DiagnosticCode::Io)?;
        Ok(bytes.len())
    }

    fn sync(&self, file: &mut File) -> Result<(), DiagnosticCode> {
        file.sync_all().map_err(|_| DiagnosticCode::Io)
    }

    fn seek(&self, file: &mut File, position: u64) -> Result<(), DiagnosticCode> {
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(position))
            .map_err(|_| DiagnosticCode::Io)?;
        Ok(())
    }

    fn replace(&self, path: &Path, bytes: &[u8]) -> Result<(), DiagnosticCode> {
        crate::settings::atomic_replace(path, bytes, true).map_err(|_| DiagnosticCode::Io)
    }

    fn publish(&self, source: &Path, destination: &Path) -> Result<(), DiagnosticCode> {
        crate::settings::publish_private_file(source, destination).map_err(|_| DiagnosticCode::Io)
    }

    fn remove_tree(&self, dir: &Path) -> Result<(), DiagnosticCode> {
        std::fs::remove_dir_all(dir).map_err(|_| DiagnosticCode::Io)
    }

    fn remove_file(&self, path: &Path) -> Result<(), DiagnosticCode> {
        std::fs::remove_file(path).map_err(|_| DiagnosticCode::Io)
    }

    fn list(&self, dir: &Path) -> Result<Vec<ListedEntry>, DiagnosticCode> {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let mut listed = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(|_| DiagnosticCode::Io)? {
            let entry = entry.map_err(|_| DiagnosticCode::Io)?;
            let metadata =
                std::fs::symlink_metadata(entry.path()).map_err(|_| DiagnosticCode::Io)?;
            let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            listed.push(ListedEntry {
                name: entry.file_name(),
                dir: metadata.is_dir() && !reparse,
                reparse,
            });
        }
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(listed)
    }

    fn length(&self, path: &Path) -> Result<Option<u64>, DiagnosticCode> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => Ok(None),
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(DiagnosticCode::Io),
        }
    }

    fn verify(&self, path: &Path) -> Result<(), DiagnosticCode> {
        crate::settings::verify_private_path(path).map_err(|_| DiagnosticCode::Acl)
    }
}

// ---------------------------------------------------------------------------
// Own names
// ---------------------------------------------------------------------------

/// One child name the archive owns. Anything else under the root is somebody
/// else's and stops the sink instead of being touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owned {
    Manifest,
    Events(u32),
    Audio,
    AudioPart,
    AudioIncomplete,
    /// `session.json.<pid>.<counter>.tmp`: the staging copy of
    /// `crate::settings::atomic_replace`, recognised by its exact grammar.
    StagingManifest,
}

fn owned_child(name: &str) -> Option<Owned> {
    if name == MANIFEST_NAME {
        return Some(Owned::Manifest);
    }
    if name == AUDIO_NAME {
        return Some(Owned::Audio);
    }
    if name == AUDIO_PART_NAME {
        return Some(Owned::AudioPart);
    }
    if name == AUDIO_INCOMPLETE_NAME {
        return Some(Owned::AudioIncomplete);
    }
    if let Some(rest) = name.strip_prefix("events-") {
        if let Some((digits, extension)) = rest.split_once('.') {
            if extension == "jsonl" && digits.len() == 6 {
                if let Ok(index) = digits.parse::<u32>() {
                    return Some(Owned::Events(index));
                }
            }
        }
        return None;
    }
    if let Some(rest) = name.strip_prefix("session.json.") {
        if let Some(rest) = rest.strip_suffix(".tmp") {
            if let Some((pid, counter)) = rest.split_once('.') {
                if !pid.is_empty()
                    && !counter.is_empty()
                    && pid.bytes().all(|byte| byte.is_ascii_digit())
                    && counter.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Some(Owned::StagingManifest);
                }
            }
        }
    }
    None
}

/// The six fields of one session directory name, or `None` for a stranger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionName {
    started_utc_ms: u64,
    pid: u32,
    run_counter: u64,
    owner: Owner,
    generation: u64,
    session_counter: u64,
}

fn parse_session_name(name: &str) -> Option<SessionName> {
    let mut parts = name.split('-');
    let started_utc_ms = parts.next()?.parse::<u64>().ok()?;
    let pid = parts.next()?.parse::<u32>().ok()?;
    let run_counter = parts.next()?.parse::<u64>().ok()?;
    let owner = match parts.next()? {
        "dictation" => Owner::Dictation,
        "lab" => Owner::Lab,
        _ => return None,
    };
    let generation = parts.next()?.parse::<u64>().ok()?;
    let session_counter = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(SessionName {
        started_utc_ms,
        pid,
        run_counter,
        owner,
        generation,
        session_counter,
    })
}

fn session_dir_name(run_id: &str, key: SessionKey, session_counter: u64) -> String {
    format!(
        "{run_id}-{}-{}-{session_counter}",
        key.owner.as_str(),
        key.generation
    )
}

fn events_name(index: u32) -> String {
    format!("events-{index:06}.jsonl")
}

/// The newest segment of a bundle, if it has one.
fn newest_segment(io: &dyn ArchiveIo, dir: &Path) -> Option<u32> {
    let mut newest: Option<u32> = None;
    for child in io.list(dir).ok()? {
        if child.dir || child.reparse {
            continue;
        }
        if let Some(Owned::Events(index)) = child.name.to_str().and_then(owned_child) {
            newest = Some(match newest {
                Some(current) if current >= index => current,
                _ => index,
            });
        }
    }
    newest
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

/// `session.json`, the one record of a take that survives its journal being
/// pruned later. Compact, bounded and free of paths, secrets and text.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Manifest {
    pub schema_version: u32,
    pub run_id: String,
    pub session_id: String,
    pub owner: Owner,
    pub generation: u64,
    pub mode: Mode,
    pub package_version: String,
    pub started_utc_ms: u64,
    pub retention_utc_ms: u64,
    pub ended_utc_ms: Option<u64>,
    pub outcome: Option<Outcome>,
    pub audio_status: AudioStatus,
    pub audio_file: Option<String>,
    pub captured_frames: u64,
    pub captured_samples: u64,
    pub archived_samples: u64,
    pub dropped_frames: u64,
    pub dropped_samples: u64,
    pub dropped_events: u64,
    pub last_sequence: Option<u64>,
    pub verified: bool,
    pub diagnostic_error: Option<DiagnosticCode>,
    /// The one hole the archive carried: the capture sequences it never
    /// received and the samples they cost. A manifest an earlier run wrote
    /// has no such field; it decodes as none rather than failing the whole
    /// record, so a bundle is never abandoned over a field it predates.
    #[serde(default)]
    pub audio_gap: Option<AudioGap>,
}

impl Manifest {
    fn new(run_id: &str, session_id: &str, key: SessionKey, begin: &SessionBegin) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            run_id: run_id.to_owned(),
            session_id: session_id.to_owned(),
            owner: key.owner,
            generation: key.generation,
            mode: begin.mode,
            package_version: env!("CARGO_PKG_VERSION").to_owned(),
            started_utc_ms: begin.started_utc_ms,
            retention_utc_ms: begin.retention_utc_ms,
            ended_utc_ms: None,
            outcome: None,
            audio_status: AudioStatus::None,
            audio_gap: None,
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
        }
    }

    fn encode(&self) -> Result<Vec<u8>, DiagnosticCode> {
        let bytes = serde_json::to_vec(self).map_err(|_| DiagnosticCode::Io)?;
        if bytes.len() > MANIFEST_MAX_BYTES {
            return Err(DiagnosticCode::Io);
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MANIFEST_MAX_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }

    /// Mirrors the counters the take keeps in memory. A take that captured
    /// audio but has no verified file stays `recording`: the archive is not
    /// done, and nothing here fabricates a file for it.
    fn observe(&mut self, counters: CaptureCounters, status: AudioStatus, dropped_events: u64) {
        self.captured_frames = counters.produced_frames;
        self.captured_samples = counters.produced_samples;
        self.dropped_frames = counters.dropped_frames;
        self.dropped_samples = counters.dropped_samples;
        self.last_sequence = counters.last_sequence;
        self.dropped_events = dropped_events;
        if status == AudioStatus::None && counters.produced_frames > 0 {
            self.audio_status = AudioStatus::Recording;
        } else {
            self.audio_status = status;
        }
    }

    /// Settles what the manifest may claim about its files after a recovery: a
    /// file is only ever verified when the audio file itself is there.
    fn settle_files(&mut self, has_audio: bool, has_part: bool, has_incomplete: bool) -> bool {
        let before = self.clone();
        // A verified name is only ever kept when the file it names is really
        // there: `audio.flac` for a whole take, `audio.incomplete.flac` for a
        // verified subset. A `.part` is never promoted by a restart.
        self.verified = self.verified && (has_audio || has_incomplete);
        if self.verified {
            let name = if has_audio { AUDIO_NAME } else { AUDIO_INCOMPLETE_NAME };
            self.audio_file = Some(name.to_owned());
            self.audio_status = if has_audio {
                AudioStatus::Complete
            } else {
                AudioStatus::Incomplete
            };
        } else if has_incomplete {
            self.audio_file = Some(AUDIO_INCOMPLETE_NAME.to_owned());
            self.audio_status = AudioStatus::Incomplete;
        } else if has_part {
            self.audio_file = None;
            self.audio_status = AudioStatus::Incomplete;
        } else if matches!(
            self.audio_status,
            AudioStatus::Complete | AudioStatus::Incomplete
        ) {
            self.audio_file = None;
            self.audio_status = AudioStatus::Recording;
        }
        *self != before
    }
}

/// The class an archive failed with, on the manifest that outlives the journal
/// segments. The first class stands: the archive that is already closed does
/// not get a new reason from every write that trips over it.
fn note_manifest_error(manifest: &mut Manifest, code: DiagnosticCode) {
    if manifest.diagnostic_error.is_none() {
        manifest.diagnostic_error = Some(code);
    }
}

/// The class an archive failed with, for a take the writer is holding its own
/// `Active` for. The finalization takes the take out of `sessions`, so the class
/// cannot be looked up by key: it goes onto that take's manifest (the record
/// that outlives the journal segments), into its memory (so the journal record
/// is classified even when nothing else could be written) and once into the log.
/// The first class stands.
fn note_archive_error(
    active: &mut Active,
    entry: &Arc<SessionEntry>,
    state: &mut State,
    code: DiagnosticCode,
) {
    note_manifest_error(&mut active.manifest, code);
    entry.note_error_code(code);
    state.note_error(code);
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

/// The exact length accounting of the whole tree. `actual` is always the sum
/// of `files`; `staged` is growth that has been admitted and not yet written
/// (including the staging copy of an atomic replacement); `reserves` is what
/// every open take keeps for its own finalization. An unreadable length is
/// never counted as zero: the last known length stays charged.
struct Ledger {
    cap: u64,
    actual: u64,
    staged: u64,
    reserves: HashMap<SessionKey, u64>,
    files: HashMap<PathBuf, u64>,
}

impl Ledger {
    fn new(cap: u64) -> Self {
        Self {
            cap,
            actual: 0,
            staged: 0,
            reserves: HashMap::new(),
            files: HashMap::new(),
        }
    }

    fn total(&self) -> u64 {
        let reserves: u64 = self.reserves.values().copied().sum();
        self.actual
            .saturating_add(self.staged)
            .saturating_add(reserves)
    }

    fn room(&self, bytes: u64) -> bool {
        self.total().saturating_add(bytes) <= self.cap
    }

    /// Admits exactly `bytes` of growth before any of it is produced.
    fn admit(&mut self, bytes: u64) -> bool {
        if !self.room(bytes) {
            return false;
        }
        self.staged += bytes;
        true
    }

    /// Gives back growth that was admitted and never written.
    fn release(&mut self, bytes: u64) {
        self.staged = self.staged.saturating_sub(bytes);
    }

    /// A finished append of `bytes` to `path`.
    fn commit_growth(&mut self, path: &Path, bytes: u64) {
        self.staged = self.staged.saturating_sub(bytes);
        let entry = self.files.entry(path.to_path_buf()).or_insert(0);
        *entry += bytes;
        self.actual += bytes;
    }

    /// A finished replacement of `path`, whose new length is `bytes`.
    fn commit_replace(&mut self, path: &Path, bytes: u64) {
        self.staged = self.staged.saturating_sub(bytes);
        if let Some(old) = self.files.remove(path) {
            self.actual = self.actual.saturating_sub(old);
        }
        self.files.insert(path.to_path_buf(), bytes);
        self.actual += bytes;
    }

    fn note_file(&mut self, path: &Path, bytes: u64) {
        if let Some(old) = self.files.insert(path.to_path_buf(), bytes) {
            self.actual = self.actual.saturating_sub(old);
        }
        self.actual += bytes;
    }

    fn forget_file(&mut self, path: &Path) {
        if let Some(old) = self.files.remove(path) {
            self.actual = self.actual.saturating_sub(old);
        }
    }

    fn forget_tree(&mut self, dir: &Path) {
        let gone: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|path| path.parent() == Some(dir))
            .cloned()
            .collect();
        for path in gone {
            self.forget_file(&path);
        }
    }

    /// The bytes of a file that only changed its name — the partial that was
    /// published as the verified archive. The length is exactly what it was.
    fn rename_file(&mut self, from: &Path, to: &Path) {
        if let Some(length) = self.files.remove(from) {
            self.files.insert(to.to_path_buf(), length);
        }
    }

    fn reserve(&mut self, key: SessionKey) -> bool {
        if self.reserves.contains_key(&key) {
            return true;
        }
        if !self.room(SESSION_RESERVE) {
            return false;
        }
        self.reserves.insert(key, SESSION_RESERVE);
        true
    }

    fn release_reserve(&mut self, key: SessionKey) {
        self.reserves.remove(&key);
    }

    /// Emergency finalization: the bytes come out of the take's own reserve and
    /// the reserve is never refilled.
    fn spend_reserve(&mut self, key: SessionKey, bytes: u64) -> bool {
        let Some(reserve) = self.reserves.get_mut(&key) else {
            return false;
        };
        if *reserve < bytes {
            return false;
        }
        *reserve -= bytes;
        self.staged += bytes;
        true
    }
}


// ---------------------------------------------------------------------------
// Journal records
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Shell,
    Capture,
    Renderer,
    Relay,
    Provider,
    Archive,
}

fn source_of(event: &Event) -> Source {
    match event {
        Event::SessionBegin(_) | Event::Phase(_) => Source::Shell,
        Event::CaptureAttempt(_)
        | Event::CaptureStarted(_)
        | Event::CaptureSnapshot(_)
        | Event::CaptureEnd(_) => Source::Capture,
        Event::RendererSnapshot(_) | Event::RendererBoundary(_) => Source::Renderer,
        Event::RelaySnapshot(_)
        | Event::UpstreamBoundary(_)
        | Event::SendBegin(_)
        | Event::SendEnd(_)
        | Event::Deadline(_) => Source::Relay,
        Event::RequestEnd(end) => if matches!(end.route, super::Route::Live) {
            Source::Relay
        } else {
            Source::Provider
        },
        Event::BatchStage(_) => Source::Provider,
        Event::AudioGap(_)
        | Event::AudioVerified(_)
        | Event::ArchiveError(_)
        | Event::ClockAdjusted(_)
        | Event::SessionEnd(_) => Source::Archive,
    }
}

/// One typed envelope flattened with the event's `event` and `data` fields.
/// Serializes borrowed identifiers directly, without intermediate maps or values.
#[allow(clippy::too_many_arguments)]
fn record_line(
    run_id: &str,
    session_id: &str,
    key: SessionKey,
    sequence: u64,
    request_id: Option<u64>,
    utc_ms: u64,
    elapsed_us: u64,
    event: &Event,
) -> Result<Vec<u8>, DiagnosticCode> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Record<'a> {
        schema_version: u32,
        sequence: u64,
        run_id: &'a str,
        session_id: &'a str,
        owner: Owner,
        generation: u64,
        request_id: Option<u64>,
        utc_ms: u64,
        elapsed_us: u64,
        source: Source,
        #[serde(flatten)]
        event: &'a Event,
    }
    let record = Record {
        schema_version: SCHEMA_VERSION,
        sequence,
        run_id,
        session_id,
        owner: key.owner,
        generation: key.generation,
        request_id,
        utc_ms,
        elapsed_us,
        source: source_of(event),
        event,
    };
    let mut line = serde_json::to_vec(&record).map_err(|_| DiagnosticCode::Io)?;
    line.push(b'\n');
    Ok(line)
}

// ---------------------------------------------------------------------------
// Gates and the worker handle
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Signal {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Signal {
    fn set(&self) {
        *self.flag.lock() = true;
        self.cv.notify_all();
    }

    fn wait(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        let mut flag = self.flag.lock();
        while !*flag {
            if self.cv.wait_until(&mut flag, deadline).timed_out() {
                return false;
            }
        }
        true
    }
}

#[derive(Default)]
struct GateInner {
    stop: bool,
    pause: bool,
    /// Set while the writer is parked at its parking point, so only a parked
    /// writer is ever reported as parked.
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    paused: bool,
    beat_requested: u64,
    beat_done: u64,
}

/// The writer's parking point: it stops on request, parks while a test holds
/// it, and reports every pass so a caller can wait for one.
#[derive(Default)]
struct Gate {
    inner: Mutex<GateInner>,
    cv: Condvar,
}

impl Gate {
    fn request_stop(&self) {
        self.inner.lock().stop = true;
        self.cv.notify_all();
    }

    fn stop_requested(&self) -> bool {
        self.inner.lock().stop
    }

    fn begin_pass(&self) {
        let mut inner = self.inner.lock();
        while inner.pause && !inner.stop {
            #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
            {
                inner.paused = true;
            }
            self.cv.notify_all();
            self.cv.wait(&mut inner);
        }
        #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
        {
            inner.paused = false;
        }
    }

    fn end_pass(&self) {
        let mut inner = self.inner.lock();
        inner.beat_done = inner.beat_requested;
        self.cv.notify_all();
    }

    fn sleep(&self) {
        let mut inner = self.inner.lock();
        if !inner.stop {
            self.cv.wait_for(&mut inner, WAKE);
        }
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    fn pause(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        let mut inner = self.inner.lock();
        inner.pause = true;
        self.cv.notify_all();
        while !inner.paused {
            if self.cv.wait_until(&mut inner, deadline).timed_out() {
                return;
            }
        }
    }

    fn resume(&self) {
        self.inner.lock().pause = false;
        self.cv.notify_all();
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    /// Waits for one more completed pass. A parked writer has to be resumed
    /// first: the caller owns that.
    fn flush(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        let mut inner = self.inner.lock();
        inner.beat_requested += 1;
        let wanted = inner.beat_requested;
        self.cv.notify_all();
        while inner.beat_done < wanted {
            if self.cv.wait_until(&mut inner, deadline).timed_out() {
                return;
            }
        }
    }
}

/// The handle the sink keeps on its writer thread. A stopped writer that did
/// not come back inside the shutdown budget is left detached, never waited on.
pub(crate) struct Worker {
    gate: Arc<Gate>,
    done: Arc<Signal>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Worker {
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn pause(&self, budget: Duration) {
        self.gate.pause(budget);
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn resume(&self) {
        self.gate.resume();
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn flush(&self, budget: Duration) {
        self.gate.flush(budget);
    }

    pub(crate) fn stop(&self, budget: Duration) {
        self.gate.request_stop();
        self.gate.resume();
        if self.done.wait(budget) {
            if let Some(handle) = self.handle.lock().take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.gate.request_stop();
        self.gate.resume();
    }
}

// ---------------------------------------------------------------------------
// The writer's own state
// ---------------------------------------------------------------------------

/// One bundle that exists on disk: its directory, the retention time its
/// manifest carries and whether a registered take still owns it.
struct Bundle {
    dir: PathBuf,
    retention_utc_ms: u64,
    active: bool,
}

/// One take the worker is writing for.
struct Active {
    key: SessionKey,
    session_id: String,
    /// `None` for a take the quota never admitted a directory for.
    dir: Option<PathBuf>,
    manifest: Manifest,
    jsonl: Option<File>,
    jsonl_len: u64,
    jsonl_index: u32,
    sequence: u64,
    last_snapshot: Instant,
    /// An ordinary quota or I/O failure closed the archive; only the bounded
    /// final record and the bounded final manifest are still written.
    disable_writes: bool,
    /// The one bounded emergency finalization has been used.
    emergency_spent: bool,
    /// Passes in a row that found the take sealed and fully drained.
    settled: u8,
    /// The FLAC archive of this take: the encoder, the file it appends to and
    /// everything the journal says about it.
    audio: AudioArchive,
}

impl Active {
    fn jsonl_path(&self, dir: &Path) -> PathBuf {
        dir.join(events_name(self.jsonl_index))
    }

    /// The manifest's view of the archive: whether the file is verified, which
    /// of its two names it earned and how many samples really reached it.
    fn observe_audio(&mut self) {
        self.manifest.verified = self.audio.verified;
        self.manifest.audio_file = self.audio.audio_file.clone();
        self.manifest.archived_samples = self.audio.committed_samples;
    }

    /// Appends one record, rotating the segment first when the record would
    /// pass the segment limit.
    fn emit(
        &mut self,
        state: &mut State,
        request_id: Option<u64>,
        event: &Event,
        utc_ms: u64,
        elapsed_us: u64,
    ) -> Result<(), DiagnosticCode> {
        let Some(dir) = self.dir.clone() else {
            return Err(DiagnosticCode::Quota);
        };
        let line = record_line(
            &state.run_id,
            &self.session_id,
            self.key,
            self.sequence + 1,
            request_id,
            utc_ms,
            elapsed_us,
            event,
        )?;
        if self.jsonl.is_some() && self.jsonl_len + line.len() as u64 > JSONL_MAX_BYTES {
            if let Some(file) = self.jsonl.as_mut() {
                let _ = state.io.sync(file);
            }
            self.jsonl = None;
            self.jsonl_index += 1;
            self.jsonl_len = 0;
        }
        let path = self.jsonl_path(&dir);
        if self.jsonl.is_none() {
            let file = state.io.create_file(&path)?;
            state.ledger.note_file(&path, 0);
            self.jsonl = Some(file);
        }
        let file = self.jsonl.as_mut().ok_or(DiagnosticCode::Io)?;
        state.append_checked(Admission::Grow, file, &path, &line)?;
        self.jsonl_len += line.len() as u64;
        self.sequence += 1;
        Ok(())
    }

    fn flush_manifest(&mut self, state: &mut State) -> Result<(), DiagnosticCode> {
        let Some(dir) = self.dir.clone() else {
            return Err(DiagnosticCode::Quota);
        };
        let bytes = self.manifest.encode()?;
        let path = dir.join(MANIFEST_NAME);
        state.replace_checked(Admission::Grow, &path, &bytes)
    }

    /// The last of a take the quota closed: one bounded final record and one
    /// bounded final manifest, both out of the take's own reserve.
    fn finalize_from_reserve(
        &mut self,
        state: &mut State,
        event: &Event,
        utc_ms: u64,
        elapsed_us: u64,
    ) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        if self.emergency_spent {
            return;
        }
        self.emergency_spent = true;
        if let Ok(line) = record_line(
            &state.run_id,
            &self.session_id,
            self.key,
            self.sequence + 1,
            None,
            utc_ms,
            elapsed_us,
            event,
        ) {
            if line.len() <= FINAL_RECORD_MAX {
                // A segment is rotated before a record would pass its limit,
                // exactly as the ordinary path does: the final record is no
                // reason to let one segment grow past the size.
                if self.jsonl.is_some() && self.jsonl_len + line.len() as u64 > JSONL_MAX_BYTES {
                    if let Some(file) = self.jsonl.as_mut() {
                        let _ = state.io.sync(file);
                    }
                    self.jsonl = None;
                    self.jsonl_index += 1;
                    self.jsonl_len = 0;
                }
                let path = self.jsonl_path(&dir);
                if self.jsonl.is_none() {
                    if let Ok(file) = state.io.create_file(&path) {
                        state.ledger.note_file(&path, 0);
                        self.jsonl = Some(file);
                    }
                }
                if let Some(file) = self.jsonl.as_mut() {
                    if state.reserve_append(self.key, file, &path, &line).is_ok() {
                        self.jsonl_len += line.len() as u64;
                        self.sequence += 1;
                    }
                }
            }
        }
        if let Ok(bytes) = self.manifest.encode() {
            let path = dir.join(MANIFEST_NAME);
            let _ = state.reserve_replace(self.key, &path, &bytes);
        }
    }
}

/// How an ordinary write is admitted.
enum Admission {
    /// A live take's write makes room for itself first, the way the periodic
    /// sweep would: the oldest finished bundles are removed until it fits.
    Grow,
    /// A recovery record is charged exactly what fits and is left undone when
    /// it does not: a bundle is never removed to write about another one.
    Exact,
}

struct State {
    io: Arc<dyn ArchiveIo>,
    root: PathBuf,
    policy: Policy,
    clock: Arc<dyn Clock>,
    registry: Arc<Mutex<Registry>>,
    meta_rx: Arc<Mutex<Receiver<PendingEvent>>>,
    error: Arc<Mutex<Option<DiagnosticCode>>>,
    sink_disabled: Arc<AtomicBool>,
    gate: Arc<Gate>,
    run_id: String,
    pid: u32,
    run_counter: u64,
    process_start: Instant,

    started: bool,
    ledger: Ledger,
    sessions: HashMap<SessionKey, Active>,
    bundles: Vec<Bundle>,
    last_sweep: Instant,
    warned: bool,
}

/// Spawns the named writer. `None` means the thread could not be created; the
/// sink then records an I/O failure and stays memory-only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    io: Arc<dyn ArchiveIo>,
    root: PathBuf,
    policy: Policy,
    clock: Arc<dyn Clock>,
    registry: Arc<Mutex<Registry>>,
    meta_rx: Arc<Mutex<Receiver<PendingEvent>>>,
    error: Arc<Mutex<Option<DiagnosticCode>>>,
    sink_disabled: Arc<AtomicBool>,
    run_id: String,
    pid: u32,
    run_counter: u64,
) -> Option<Worker> {
    let now = clock.instant();
    let cap = policy.max_bytes;
    let gate = Arc::new(Gate::default());
    let done = Arc::new(Signal::default());
    let state = State {
        io,
        root,
        policy,
        clock,
        registry,
        meta_rx,
        error,
        sink_disabled,
        gate: gate.clone(),
        run_id,
        pid,
        run_counter,
        process_start: now,
        started: false,
        ledger: Ledger::new(cap),
        sessions: HashMap::new(),
        bundles: Vec::new(),
        last_sweep: now,
        warned: false,
    };
    let thread_gate = gate.clone();
    let thread_done = done.clone();
    let handle = std::thread::Builder::new()
        .name("speechek-diagnostics".to_owned())
        .spawn(move || {
            let mut state = state;
            loop {
                thread_gate.begin_pass();
                state.pass();
                thread_gate.end_pass();
                if thread_gate.stop_requested() {
                    break;
                }
                thread_gate.sleep();
            }
            state.retire();
            thread_done.set();
        })
        .ok()?;
    Some(Worker {
        gate,
        done,
        handle: Mutex::new(Some(handle)),
    })
}

impl State {
    // -- helpers ----------------------------------------------------------

    fn elapsed_us(&self) -> u64 {
        u64::try_from(
            self.clock
                .instant()
                .saturating_duration_since(self.process_start)
                .as_micros(),
        )
        .unwrap_or(u64::MAX)
    }

    fn utc_ms(&self) -> u64 {
        system_millis(self.clock.system())
    }

    fn shutting_down(&self) -> bool {
        self.gate.stop_requested()
    }

    fn entry_of(&self, key: SessionKey) -> Option<Arc<SessionEntry>> {
        self.registry.lock().sessions.get(&key).cloned()
    }

    fn disable_sink(&mut self, code: DiagnosticCode) {
        self.sink_disabled.store(true, Ordering::Release);
        self.note_error(code);
    }

    fn note_error(&mut self, code: DiagnosticCode) {
        *self.error.lock() = Some(code);
        if !self.warned {
            self.warned = true;
            eprintln!("speechek-diagnostics: {}", code.as_str());
        }
    }

    /// A record the archive could not write. The take's own counter is raised,
    /// so the number is in its manifest and in memory even though no file got
    /// the record.
    fn record_lost(&mut self, key: SessionKey) {
        if let Some(entry) = self.entry_of(key) {
            entry.note_dropped_event();
        }
    }

    // -- the writes --------------------------------------------------------

    /// Charges exactly `bytes` of growth before a byte of it is produced. An
    /// ordinary write of a live take first makes room for itself; a recovery
    /// record is only charged what fits. An active take is never a candidate
    /// for removal, a refusal is never a reason to refill a reserve, and once
    /// the sink was stopped by a tree that is not the archive's own, nothing is
    /// written past it any more.
    fn admit(&mut self, admission: Admission, bytes: u64) -> Result<(), DiagnosticCode> {
        if let Some(code) = self.stopped_class() {
            return Err(code);
        }
        if self.ledger.admit(bytes) {
            return Ok(());
        }
        if matches!(admission, Admission::Grow) {
            self.prune_to_fit(bytes);
            if let Some(code) = self.stopped_class() {
                // The room was to be made by removing a bundle the archive no
                // longer fully owns: the write stops with the sink.
                return Err(code);
            }
            if self.ledger.admit(bytes) {
                return Ok(());
            }
        }
        Err(DiagnosticCode::Quota)
    }

    /// The class the sink was stopped with, when it was stopped: the root is
    /// not the archive's own any more, so no record and no recovery write
    /// follows it.
    fn stopped_class(&self) -> Option<DiagnosticCode> {
        if self.sink_disabled.load(Ordering::Acquire) {
            Some((*self.error.lock()).unwrap_or(DiagnosticCode::Io))
        } else {
            None
        }
    }

    /// Appends `bytes` to `file`, admitting the exact growth first. A write
    /// that fails or is short is followed by a full re-measurement of the
    /// directory; the admission is only handed back once every child was
    /// measured.
    fn append_checked(
        &mut self,
        admission: Admission,
        file: &mut File,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), DiagnosticCode> {
        let wanted = bytes.len() as u64;
        self.admit(admission, wanted)?;
        match self.io.write(file, bytes) {
            Ok(written) if written as u64 == wanted => {
                self.ledger.commit_growth(path, wanted);
                Ok(())
            }
            Ok(_) => Err(self.reconcile(path.parent(), wanted, DiagnosticCode::Io)),
            Err(code) => {
                let code = self.reconcile(path.parent(), wanted, code);
                Err(code)
            }
        }
    }

    /// Replaces `path` with `bytes` in one step, staging copy included. A
    /// failed replacement can leave its own staging copy behind, so the
    /// directory is measured again instead of trusting the destination alone.
    fn replace_checked(
        &mut self,
        admission: Admission,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), DiagnosticCode> {
        let wanted = bytes.len() as u64;
        self.admit(admission, wanted)?;
        match self.io.replace(path, bytes) {
            Ok(()) => {
                self.ledger.commit_replace(path, wanted);
                Ok(())
            }
            Err(code) => {
                let code = self.reconcile(path.parent(), wanted, code);
                Err(code)
            }
        }
    }

    /// The same two writes, out of a take's own reserve: nothing is pruned
    /// here and the reserve is never refilled.
    fn reserve_append(
        &mut self,
        key: SessionKey,
        file: &mut File,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), DiagnosticCode> {
        let wanted = bytes.len() as u64;
        if !self.ledger.spend_reserve(key, wanted) {
            return Err(DiagnosticCode::Quota);
        }
        match self.io.write(file, bytes) {
            Ok(written) if written as u64 == wanted => {
                self.ledger.commit_growth(path, wanted);
                Ok(())
            }
            Ok(_) => Err(self.reconcile(path.parent(), wanted, DiagnosticCode::Io)),
            Err(code) => {
                let code = self.reconcile(path.parent(), wanted, code);
                Err(code)
            }
        }
    }

    fn reserve_replace(
        &mut self,
        key: SessionKey,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), DiagnosticCode> {
        let wanted = bytes.len() as u64;
        if !self.ledger.spend_reserve(key, wanted) {
            return Err(DiagnosticCode::Quota);
        }
        match self.io.replace(path, bytes) {
            Ok(()) => {
                self.ledger.commit_replace(path, wanted);
                Ok(())
            }
            Err(code) => {
                let code = self.reconcile(path.parent(), wanted, code);
                Err(code)
            }
        }
    }

    /// The shared failure path of every write: measure the directory again, and
    /// hand the admitted growth back only when the whole directory was
    /// measured. When it was not, the growth stays charged and nothing can pass
    /// the cap again until a scan reads the real lengths. The class that stops
    /// the sink outranks the write that happened to notice a tree that is not
    /// the archive's own any more.
    fn reconcile(
        &mut self,
        dir: Option<&Path>,
        admitted: u64,
        code: DiagnosticCode,
    ) -> DiagnosticCode {
        if self.rescan_dir(dir) {
            self.ledger.release(admitted);
            return code;
        }
        if let Some(code) = self.stopped_class() {
            return code;
        }
        code
    }

    /// Measures one session directory again after a failed write. Every child
    /// of the archive's own is charged its real length and every file that is
    /// gone is let go; a child whose length cannot be read keeps the length it
    /// already had. A child that is not the archive's — a directory, a reparse
    /// point, a name the archive never writes — stops the sink instead of being
    /// forgotten: its bytes are not measurable, and nothing may be written past
    /// an unmeasured tree. `false` means the directory was not fully measured,
    /// so nothing may be handed back.
    fn rescan_dir(&mut self, dir: Option<&Path>) -> bool {
        let Some(dir) = dir else {
            return false;
        };
        let Ok(children) = self.io.list(dir) else {
            return false;
        };
        let mut present: HashSet<PathBuf> = HashSet::new();
        for child in children {
            if child.dir || child.reparse || child.name.to_str().and_then(owned_child).is_none() {
                self.disable_sink(DiagnosticCode::ForeignArchive);
                return false;
            }
            let path = dir.join(&child.name);
            present.insert(path.clone());
            match self.io.length(&path) {
                Ok(Some(len)) => self.ledger.note_file(&path, len),
                // An unreadable length is never counted as zero.
                Ok(None) | Err(_) => return false,
            }
        }
        let gone: Vec<PathBuf> = self
            .ledger
            .files
            .keys()
            .filter(|path| path.parent() == Some(dir) && !present.contains(*path))
            .cloned()
            .collect();
        for path in gone {
            self.ledger.forget_file(&path);
        }
        true
    }

    /// What a cleanup that did not finish leaves behind: the real lengths of
    /// the tree are read back, so a child the disk did remove is no longer
    /// charged. A tree the disk will not even list back keeps every byte
    /// charged rather than being released on a maybe.
    fn reconcile_tree(&mut self, dir: &Path) {
        if self.io.list(dir).is_ok() {
            self.rescan_dir(Some(dir));
        }
    }

    // -- a pass -----------------------------------------------------------

    fn pass(&mut self) {
        if !self.started {
            self.started = true;
            if !self.startup() {
                return;
            }
        }
        if self.sink_disabled.load(Ordering::Acquire) {
            return;
        }
        let now = self.clock.instant();
        self.advance_takes(now);
        let entries: Vec<Arc<SessionEntry>> =
            self.registry.lock().sessions.values().cloned().collect();
        // Every registered take gets its writer state from what the registry
        // already knows, not only from the `session_begin` record: a record the
        // shared queue had to drop must not leave a take without a bundle, a
        // snapshot or a close.
        for entry in &entries {
            self.ensure_active(entry.key, now);
        }
        let touched = self.drain_meta();
        // The PCM pass: up to 32 packets per take per pass, encoded frame by
        // frame by the very thread that writes the journals, and the pool
        // buffer handed straight back to the capture. A take whose capture has
        // published its watermark is finished in the same pass: the tail, the
        // header patch and the independent check.
        for entry in &entries {
            self.write_audio(entry, now);
        }
        for entry in &entries {
            self.snapshot_take(entry, now);
        }
        for entry in &entries {
            self.settle_take(entry, now, &touched);
        }
        if now.saturating_duration_since(self.last_sweep) >= SWEEP {
            self.last_sweep = now;
            self.sweep();
        }
    }

    fn retire(&mut self) {
        for (_, mut active) in self.sessions.drain() {
            if let Some(file) = active.jsonl.as_mut() {
                let _ = self.io.sync(file);
            }
        }
    }

    // -- startup ----------------------------------------------------------

    /// Creates the root, counts every byte the sink already owns, prunes what
    /// the policy refuses and rewrites the manifests an earlier process left
    /// open. A stranger under the root disables the sink and is never touched.
    fn startup(&mut self) -> bool {
        if let Err(code) = self.io.create_dir(&self.root) {
            self.disable_sink(code);
            return false;
        }
        let listed = match self.io.list(&self.root) {
            Ok(listed) => listed,
            Err(code) => {
                self.disable_sink(code);
                return false;
            }
        };
        let mut recovery: Vec<(PathBuf, SessionKey)> = Vec::new();
        for entry in listed {
            if entry.reparse || !entry.dir {
                self.disable_sink(DiagnosticCode::ForeignArchive);
                return false;
            }
            let Some(parsed) = entry.name.to_str().and_then(parse_session_name) else {
                self.disable_sink(DiagnosticCode::ForeignArchive);
                return false;
            };
            let dir = self.root.join(&entry.name);
            if !self.count_bundle(&dir, parsed, &mut recovery) {
                return false;
            }
        }
        let now = self.utc_ms();
        self.prune_age(now);
        // A tree that is already over the cap gives way before a recovery
        // writes anything: the oldest finished bundles go first, and a bundle
        // that cannot be made room for keeps its bytes untouched.
        self.prune_to_fit(0);
        if !self.clean_staging() {
            return false;
        }
        for (dir, key) in recovery {
            // A bundle the prune already removed is gone for good: its recovery
            // is skipped instead of failing over a directory that no longer is.
            if self.bundles.iter().any(|bundle| bundle.dir == dir) {
                self.recover_bundle(&dir, key, now);
            }
        }
        true
    }

    /// Counts one existing bundle and decides whether its manifest has to be
    /// rewritten. Unknown children are never removed: they stop the sink.
    fn count_bundle(
        &mut self,
        dir: &Path,
        parsed: SessionName,
        recovery: &mut Vec<(PathBuf, SessionKey)>,
    ) -> bool {
        let children = match self.io.list(dir) {
            Ok(children) => children,
            Err(code) => {
                self.disable_sink(code);
                return false;
            }
        };
        // A bundle whose own access list was broadened is not ours any more:
        // nothing is read out of it, nothing in it is ever deleted.
        if self.io.verify(dir).is_err() {
            self.disable_sink(DiagnosticCode::Acl);
            return false;
        }
        let mut manifest: Option<Vec<u8>> = None;
        let mut has_audio = false;
        let mut has_part = false;
        let mut has_incomplete = false;
        for child in &children {
            if child.reparse {
                self.disable_sink(DiagnosticCode::ForeignArchive);
                return false;
            }
            let owned = match child.name.to_str().and_then(owned_child) {
                Some(owned) if !child.dir => owned,
                _ => {
                    self.disable_sink(DiagnosticCode::ForeignArchive);
                    return false;
                }
            };
            let path = dir.join(&child.name);
            if self.io.verify(&path).is_err() {
                self.disable_sink(DiagnosticCode::Acl);
                return false;
            }
            let opened = match owned {
                Owned::Manifest => Some(self.io.open_read(&path)),
                Owned::Audio => None,
                Owned::AudioPart => None,
                Owned::AudioIncomplete => None,
                Owned::Events(_) | Owned::StagingManifest => None,
            };
            match owned {
                Owned::Audio => has_audio = true,
                Owned::AudioPart => has_part = true,
                Owned::AudioIncomplete => has_incomplete = true,
                Owned::Manifest => match opened {
                    Some(Ok(mut file)) => {
                        let mut bytes = Vec::new();
                        if file
                            .by_ref()
                            .take(MANIFEST_MAX_BYTES as u64 + 1)
                            .read_to_end(&mut bytes)
                            .is_err()
                        {
                            self.disable_sink(DiagnosticCode::Io);
                            return false;
                        }
                        manifest = Some(bytes);
                    }
                    Some(Err(code)) => {
                        self.disable_sink(code);
                        return false;
                    }
                    None => {}
                },
                Owned::Events(_) | Owned::StagingManifest => {}
            }
            match self.io.length(&path) {
                Ok(Some(len)) => self.ledger.note_file(&path, len),
                // A length that cannot be read is never counted as zero: the
                // bytes stay charged as they were.
                Ok(None) => {}
                Err(code) => {
                    self.disable_sink(code);
                    return false;
                }
            }
        }

        let key = SessionKey {
            owner: parsed.owner,
            generation: parsed.generation,
        };
        let decoded = manifest.as_deref().and_then(Manifest::decode);
        let mut retention = parsed.started_utc_ms;
        let mut needs_write = decoded.is_none();
        if let Some(mut next) = decoded {
            if next.outcome.is_none() {
                next.outcome = Some(Outcome::Abandoned);
                next.ended_utc_ms = next.ended_utc_ms.or(Some(parsed.started_utc_ms));
                next.diagnostic_error = Some(DiagnosticCode::MissingTerminal);
                needs_write = true;
            }
            if next.settle_files(has_audio, has_part, has_incomplete) {
                needs_write = true;
            }
            // A retention time from a clock that has since gone backwards is
            // clamped back to the wall clock once; the start stays historical.
            let now = self.utc_ms();
            if next.retention_utc_ms > now {
                next.retention_utc_ms = now;
                needs_write = true;
            }
            retention = next.retention_utc_ms;
        }
        self.bundles.push(Bundle {
            dir: dir.to_path_buf(),
            retention_utc_ms: retention,
            active: false,
        });
        if needs_write {
            recovery.push((dir.to_path_buf(), key));
        }
        true
    }

    /// Writes the manifest an earlier process never finished. The bytes are
    /// admitted like any other write; a bundle the quota cannot afford is left
    /// exactly as it is and only classified in memory.
    fn recover_bundle(&mut self, dir: &Path, key: SessionKey, now: u64) {
        let io = Arc::clone(&self.io);
        if io.verify(dir).is_err() {
            self.disable_sink(DiagnosticCode::Acl);
            return;
        }
        let children = match io.list(dir) {
            Ok(children) => children,
            Err(_) => return,
        };
        let mut manifest: Option<Manifest> = None;
        let mut has_audio = false;
        let mut has_part = false;
        let mut has_incomplete = false;
        for child in &children {
            let Some(name) = child.name.to_str() else {
                return;
            };
            match owned_child(name) {
                Some(Owned::Manifest) => {
                    if let Ok(mut file) = io.open_read(&dir.join(&child.name)) {
                        let mut raw = Vec::new();
                        if file
                            .by_ref()
                            .take(MANIFEST_MAX_BYTES as u64 + 1)
                            .read_to_end(&mut raw)
                            .is_ok()
                        {
                            manifest = Manifest::decode(&raw);
                        }
                    }
                }
                Some(Owned::Audio) => has_audio = true,
                Some(Owned::AudioPart) => has_part = true,
                Some(Owned::AudioIncomplete) => has_incomplete = true,
                _ => {}
            }
        }
        let mut next = match manifest {
            Some(manifest) => manifest,
            None => {
                let session_id = dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned();
                let start = dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(parse_session_name)
                    .map(|parsed| parsed.started_utc_ms)
                    .unwrap_or(now);
                Manifest {
                    schema_version: SCHEMA_VERSION,
                    run_id: self.run_id.clone(),
                    session_id,
                    owner: key.owner,
                    generation: key.generation,
                    mode: Mode::Live,
                    package_version: env!("CARGO_PKG_VERSION").to_owned(),
                    started_utc_ms: start,
                    retention_utc_ms: start.min(now),
                    ended_utc_ms: Some(now),
                    outcome: Some(Outcome::Abandoned),
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
                    diagnostic_error: Some(DiagnosticCode::MissingTerminal),
                    audio_gap: None,
                }
            }
        };
        next.settle_files(has_audio, has_part, has_incomplete);
        if next.outcome.is_none() {
            next.outcome = Some(Outcome::Abandoned);
            next.ended_utc_ms = Some(now);
            next.diagnostic_error = Some(DiagnosticCode::MissingTerminal);
        }
        let clamped = next.retention_utc_ms > now;
        if clamped {
            next.retention_utc_ms = now;
        }
        if next.package_version != env!("CARGO_PKG_VERSION") {
            next.package_version = env!("CARGO_PKG_VERSION").to_owned();
        }
        let Ok(bytes) = next.encode() else {
            return;
        };
        let path = dir.join(MANIFEST_NAME);
        if self.replace_checked(Admission::Exact, &path, &bytes).is_err() {
            return;
        }
        let retention = next.retention_utc_ms;
        if let Some(bundle) = self.bundles.iter_mut().find(|bundle| bundle.dir == dir) {
            bundle.retention_utc_ms = retention;
        }
        if clamped {
            self.recover_note(dir, key, retention, now);
        }
    }

    /// The one numeric record of a clock clamp, appended to the bundle's newest
    /// segment. `sequence` is zero here: this record is written by a later run
    /// for a bundle whose own record sequence that run never saw.
    fn recover_note(&mut self, dir: &Path, key: SessionKey, retention: u64, now: u64) {
        let io = Arc::clone(&self.io);
        let session_id = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let index = newest_segment(&*io, dir).unwrap_or(1);
        let path = dir.join(events_name(index));
        let existing = match io.length(&path) {
            Ok(existing) => existing,
            Err(_) => None,
        };
        let mut file = match existing {
            Some(len) => {
                self.ledger.note_file(&path, len);
                match io.open_append(&path) {
                    Ok(file) => file,
                    Err(_) => return,
                }
            }
            None => match io.create_file(&path) {
                Ok(file) => {
                    self.ledger.note_file(&path, 0);
                    file
                }
                Err(_) => return,
            },
        };
        let Ok(line) = record_line(
            &self.run_id,
            &session_id,
            key,
            0,
            None,
            now,
            self.elapsed_us(),
            &Event::ClockAdjusted(ClockAdjusted {
                retention_utc_ms: retention,
                now_utc_ms: now,
            }),
        ) else {
            return;
        };
        if self.append_checked(Admission::Exact, &mut file, &path, &line).is_ok() {
            let _ = io.sync(&mut file);
        }
    }

    /// Removes the manifest staging copies an earlier run left behind. The
    /// authoritative name is always `session.json`, so a copy is never
    /// promoted: each one is verified first, then deleted, and the bytes of a
    /// copy that could not be deleted stay charged.
    fn clean_staging(&mut self) -> bool {
        let dirs: Vec<PathBuf> = self
            .bundles
            .iter()
            .map(|bundle| bundle.dir.clone())
            .collect();
        for dir in dirs {
            let children = match self.io.list(&dir) {
                Ok(children) => children,
                Err(code) => {
                    self.disable_sink(code);
                    return false;
                }
            };
            for child in children {
                if child.name.to_str().and_then(owned_child) != Some(Owned::StagingManifest) {
                    continue;
                }
                if child.dir || child.reparse {
                    self.disable_sink(DiagnosticCode::ForeignArchive);
                    return false;
                }
                let path = dir.join(&child.name);
                if self.io.verify(&path).is_err() {
                    self.disable_sink(DiagnosticCode::Acl);
                    return false;
                }
                if self.io.remove_file(&path).is_ok() {
                    self.ledger.forget_file(&path);
                } else {
                    // A delete that did not finish leaves the ledger to the
                    // disk: what really is left is measured back before
                    // anything else is written.
                    self.reconcile_tree(&dir);
                    if self.sink_disabled.load(Ordering::Acquire) {
                        return false;
                    }
                }
            }
        }
        true
    }

    // -- registration and snapshots ---------------------------------------

    fn advance_takes(&self, now: Instant) {
        let registry = self.registry.lock();
        for entry in registry.sessions.values() {
            entry.advance(now);
        }
    }

    /// Admits a take's directory and its reserve, or leaves it memory-only.
    fn ensure_active(&mut self, key: SessionKey, now: Instant) -> bool {
        if self.sessions.contains_key(&key) {
            return true;
        }
        let Some(entry) = self.entry_of(key) else {
            return false;
        };
        let begin = SessionBegin {
            mode: entry.mode,
            started_utc_ms: entry.started_utc_ms,
            retention_utc_ms: entry.started_utc_ms,
            pid: self.pid,
            run_counter: self.run_counter,
            session_counter: entry.session_counter,
        };
        let session_id = session_dir_name(&self.run_id, key, entry.session_counter);
        let mut failure = DiagnosticCode::Quota;
        // Make room before the reserve is taken: the oldest finished bundles
        // give way, and a take the cap still cannot afford stays memory-only.
        self.prune_to_fit(SESSION_RESERVE);
        if self.ledger.reserve(key) {
            let dir = self.root.join(&session_id);
            if self.io.create_dir(&dir).is_ok() {
                let mut active = Active {
                    key,
                    session_id: session_id.clone(),
                    dir: Some(dir.clone()),
                    manifest: Manifest::new(&self.run_id, &session_id, key, &begin),
                    jsonl: None,
                    jsonl_len: 0,
                    jsonl_index: 1,
                    sequence: 0,
                    last_snapshot: now,
                    disable_writes: false,
                    emergency_spent: false,
                    settled: 0,
                    audio: AudioArchive::new(dir.join(AUDIO_PART_NAME)),
                };
                active.manifest.observe(
                    entry.capture.snapshot(),
                    entry.audio_status(),
                    entry.dropped_events(),
                );
                active.observe_audio();
                let path = dir.join(MANIFEST_NAME);
                if let Ok(bytes) = active.manifest.encode() {
                    if self.replace_checked(Admission::Grow, &path, &bytes).is_err() {
                        // The archive never opened a journal; its one bounded
                        // finalization stays for the take's own terminal.
                        active.disable_writes = true;
                        active.manifest.diagnostic_error = Some(DiagnosticCode::Io);
                        self.note_error(DiagnosticCode::Io);
                    }
                }
                self.sessions.insert(key, active);
                self.bundles.push(Bundle {
                    dir,
                    retention_utc_ms: entry.started_utc_ms,
                    active: true,
                });
                return true;
            }
            self.ledger.release_reserve(key);
            failure = DiagnosticCode::Io;
        }
        // No room, or no directory: memory-only. The producer is told so its
        // frames are counted rather than copied into a queue nobody owns.
        entry.set_archive_enabled(false);
        self.sessions.insert(
            key,
            Active {
                key,
                session_id: session_id.clone(),
                dir: None,
                manifest: Manifest::new(&self.run_id, &session_id, key, &begin),
                jsonl: None,
                jsonl_len: 0,
                jsonl_index: 1,
                sequence: 0,
                last_snapshot: now,
                disable_writes: true,
                emergency_spent: false,
                settled: 0,
                // Nothing of this take ever reaches a file: no directory, no
                // journal and no partial. Its archive refuses every write.
                audio: AudioArchive::unavailable(),
            },
        );
        self.note_error(failure);
        false
    }

    fn snapshot_take(&mut self, entry: &Arc<SessionEntry>, now: Instant) {
        let key = entry.key;
        let due = match self.sessions.get(&key) {
            Some(active) if active.dir.is_some() && !active.disable_writes => {
                now.saturating_duration_since(active.last_snapshot) >= SNAPSHOT
            }
            _ => return,
        };
        if !due {
            return;
        }
        if let Some(active) = self.sessions.get_mut(&key) {
            active.last_snapshot = now;
            active.manifest.observe(
                entry.capture.snapshot(),
                entry.audio_status(),
                entry.dropped_events(),
            );
            active.observe_audio();
        }
        let counters = entry.capture.snapshot();
        self.emit_event(
            key,
            None,
            &Event::CaptureSnapshot(CaptureSnapshot {
                produced_frames: counters.produced_frames,
                produced_samples: counters.produced_samples,
                produced_bytes: counters.produced_bytes,
                emit_failed_frames: counters.emit_failed_frames,
                last_sequence: counters.last_sequence,
                dropped_frames: counters.dropped_frames,
                dropped_samples: counters.dropped_samples,
            }),
        );
        for (request_id, counters) in entry.relay_snapshots() {
            self.emit_event(key, Some(request_id), &Event::RelaySnapshot(counters));
        }
        self.write_manifest_of(key);
    }

    fn write_manifest_of(&mut self, key: SessionKey) {
        let Some(mut active) = self.sessions.remove(&key) else {
            return;
        };
        let failure = if active.dir.is_some() && !active.disable_writes {
            active.flush_manifest(self).err()
        } else {
            None
        };
        self.sessions.insert(key, active);
        if let Some(code) = failure {
            self.disable_take(key, code);
        }
    }

    // -- metadata ---------------------------------------------------------

    /// Writes what the queues hold and reports which takes were touched. On
    /// shutdown the queue is emptied completely, so a closed take is never
    /// closed over a record that was already queued for it.
    fn drain_meta(&mut self) -> HashSet<SessionKey> {
        let budget = if self.shutting_down() {
            usize::MAX
        } else {
            META_PER_PASS
        };
        let mut touched = HashSet::new();
        let now = self.clock.instant();
        let records: Vec<PendingEvent> = {
            let receiver = self.meta_rx.lock();
            let mut taken = Vec::new();
            while taken.len() < budget {
                match receiver.try_recv() {
                    Ok(pending) => taken.push(pending),
                    Err(_) => break,
                }
            }
            taken
        };
        for pending in records {
            touched.insert(pending.key);
            self.route(pending, now);
        }
        touched
    }

    fn route(&mut self, pending: PendingEvent, now: Instant) {
        let key = pending.key;
        if let Some(entry) = self.entry_of(key) {
            entry.note_meta_drained();
        }
        match pending.meta {
            Meta::Event(event) => {
                if matches!(&event, Event::SessionBegin(_)) {
                    self.ensure_active(key, now);
                }
                if !self.sessions.contains_key(&key) {
                    self.record_lost(key);
                    return;
                }
                let relay = pending.relay;
                let request_id = pending.request_id;
                self.emit_event(key, request_id, &event);
                if let Some(counters) = relay {
                    self.emit_event(key, request_id, &Event::RelaySnapshot(counters));
                }
            }
            Meta::Renderer(report) => {
                if !self.sessions.contains_key(&key) {
                    self.record_lost(key);
                    return;
                }
                let snapshot = Event::RendererSnapshot(report.snapshot());
                self.emit_event(key, None, &snapshot);
                for boundary in &report.events {
                    self.emit_event(key, None, &Event::RendererBoundary(*boundary));
                }
            }
        }
    }

    fn emit_event(&mut self, key: SessionKey, request_id: Option<u64>, event: &Event) {
        let utc_ms = self.utc_ms();
        let elapsed_us = self.elapsed_us();
        let Some(mut active) = self.sessions.remove(&key) else {
            return;
        };
        if active.dir.is_none() || active.disable_writes {
            self.sessions.insert(key, active);
            self.record_lost(key);
            return;
        }
        let result = active.emit(self, request_id, event, utc_ms, elapsed_us);
        self.sessions.insert(key, active);
        if let Err(code) = result {
            self.disable_take(key, code);
        }
    }

    /// An ordinary failure closes the archive for everything but its own
    /// finalization: the producer stops copying PCM and every other record is
    /// dropped. The one bounded final record and the bounded final manifest
    /// are *not* spent here — the terminal the take still owes is what the
    /// reserve is for, and nothing may consume it while the outcome is not
    /// known yet.
    fn disable_take(&mut self, key: SessionKey, code: DiagnosticCode) {
        let Some(entry) = self.entry_of(key) else {
            return;
        };
        entry.set_archive_enabled(false);
        entry.note_error_code(code);
        self.note_error(code);
        if let Some(active) = self.sessions.get_mut(&key) {
            if active.dir.is_some() {
                active.disable_writes = true;
                active.manifest.diagnostic_error = Some(code);
                active.manifest.observe(
                    entry.capture.snapshot(),
                    entry.audio_status(),
                    entry.dropped_events(),
                );
                active.observe_audio();
            }
        }
    }

    // -- the audio archive -------------------------------------------------

    /// Drains this take's queued PCM into its encoder and, once the capture has
    /// published its watermark and the queue is empty, finalizes the archive.
    /// No registry lock is held here and no producer ever waits on anything.
    fn write_audio(&mut self, entry: &Arc<SessionEntry>, _now: Instant) {
        let Some(active) = self.sessions.get(&entry.key) else {
            return;
        };
        if active.audio.done() && !entry.is_capture_closed() {
            return;
        }
        let budget = if self.shutting_down() {
            // The sink is closing: this is the last chance the queued frames
            // have, so the pass takes the whole queue rather than its first 32.
            AUDIO_DRAIN_ALL
        } else {
            AUDIO_PER_PASS
        };
        let packets = entry.take_audio(budget);
        // A full budget does not prove the queue is empty; a short one does.
        let drained = packets.len() < budget;
        // The take's own record of that proof: a sealed take is not let go, and
        // its reserve is not released, while its queued PCM is still waiting.
        self.with_audio(entry.key, |audio| audio.drained = drained);
        if !packets.is_empty() {
            let outcome = self.append_packets(entry.key, &packets);
            for packet in packets {
                entry.return_buffer(packet.bytes);
            }
            if let Err(code) = outcome {
                self.fail_audio(entry.key, code);
            }
        }
        if !entry.is_capture_closed() || !drained {
            return;
        }
        let done = self
            .sessions
            .get(&entry.key)
            .is_some_and(|active| active.audio.done());
        if done {
            // The archive is already closed — a refused bundle, a failed
            // write — and only now is the capture over with its queue empty.
            // The take is told its numbers here rather than at the moment of
            // the failure, so what it reports is what it really captured.
            self.finish_audio(entry, entry.capture.snapshot().last_sequence);
        } else {
            self.finalize_audio(entry);
        }
    }

    /// Encodes one pass worth of packets in capture order. The pool buffer goes
    /// back to the capture as soon as its bytes are staged; the encoder keeps
    /// nothing but its own fixed block. A packet the archive never received —
    /// a dry pool, a full queue, a capture that outran the writer, or an
    /// archive that is already closed — is a hole in the sequence, and the
    /// first one is what the journal records.
    fn append_packets(
        &mut self,
        key: SessionKey,
        packets: &[AudioPacket],
    ) -> Result<(), DiagnosticCode> {
        let mut expected = match self.sessions.get(&key) {
            Some(active) => active.audio.last_sequence.map_or(1, |last| last + 1),
            None => return Ok(()),
        };
        for packet in packets {
            if packet.sequence != expected {
                // A hole in the middle of a take runs up to the frame before
                // this surviving one: it never claims the frames that arrived,
                // and this frame is what proves the hole ended.
                let last_lost = packet.sequence - 1;
                self.with_audio(key, |audio| audio.note_gap(expected, last_lost));
                self.flush_gaps(key);
            }
            expected = packet.sequence + 1;
            self.append_pcm(key, packet.sequence, &packet.bytes)?;
        }
        Ok(())
    }

    /// One packet into the encoder: the header and the private partial come
    /// from the first frame, and every frame is admitted against the quota
    /// before a byte of it is written. A packet whose commit failed is counted
    /// nowhere, so what the take claims is exactly what the file holds.
    fn append_pcm(
        &mut self,
        key: SessionKey,
        sequence: u64,
        pcm: &[u8],
    ) -> Result<(), DiagnosticCode> {
        if self
            .sessions
            .get(&key)
            .is_none_or(|active| active.audio.done())
        {
            return Ok(());
        }
        self.ensure_audio_file(key)?;
        let Some(mut active) = self.sessions.remove(&key) else {
            return Ok(());
        };
        let (Some(encoder), Some(mut file)) =
            (active.audio.encoder.as_mut(), active.audio.file.take())
        else {
            self.sessions.insert(key, active);
            return Ok(());
        };
        let mut commit = FileCommit {
            state: self,
            file: &mut file,
            path: active.audio.part.clone(),
        };
        let result = encoder.push_pcm(pcm, &mut commit);
        // The encoder counts a frame only after its bytes are on disk, so its
        // own totals are the ones the archive reports; the sequence the file
        // really reached is the one the packet carried. `accepted_samples`
        // counts the whole packet too, staged block included: that is the
        // offset a hole opening right after it must be measured from.
        active.audio.committed_samples = encoder.samples();
        active.audio.committed_frames = encoder.frames();
        if result.is_ok() {
            active.audio.accepted_samples +=
                (pcm.len() / (flac::BYTES_PER_SAMPLE * flac::CHANNELS)) as u64;
            active.audio.last_sequence = Some(sequence);
        }
        active.audio.file = Some(file);
        self.sessions.insert(key, active);
        result
    }

    /// The first audio bytes of a take: the private `audio.flac.part` and the
    /// header an interrupted run can still be read through.
    fn ensure_audio_file(&mut self, key: SessionKey) -> Result<(), DiagnosticCode> {
        let part = match self.sessions.get(&key) {
            Some(active) if active.audio.encoder.is_none() => active.audio.part.clone(),
            _ => return Ok(()),
        };
        let encoder = FlacEncoder::new()?;
        let header = encoder.initial_header().to_vec();
        let wanted = header.len() as u64;
        let relative = part.parent().map(Path::to_path_buf);
        self.admit(Admission::Grow, wanted)?;
        let mut file = match self.io.create_file(&part) {
            Ok(file) => file,
            Err(code) => return Err(self.reconcile(relative.as_deref(), wanted, code)),
        };
        self.ledger.note_file(&part, 0);
        match self.io.write(&mut file, &header) {
            Ok(written) if written as u64 == wanted => {
                self.ledger.commit_growth(&part, wanted);
            }
            Ok(_) => return Err(self.reconcile(relative.as_deref(), wanted, DiagnosticCode::Io)),
            Err(code) => return Err(self.reconcile(relative.as_deref(), wanted, code)),
        }
        if let Err(code) = self.io.sync(&mut file) {
            // The header write already committed its admission, so nothing is
            // staged any more: reconciling with `wanted` here would hand back
            // bytes this file no longer owes and undercharge an unrelated
            // staged charge from an earlier failed rescan.
            return Err(self.reconcile(relative.as_deref(), 0, code));
        }
        if let Some(active) = self.sessions.get_mut(&key) {
            active.audio.header_len = header.len();
            active.audio.file = Some(file);
            active.audio.encoder = Some(encoder);
            active.audio.stage = AudioStage::Recording;
        }
        Ok(())
    }

    /// The capture is over and its queue is empty: the short tail, the header
    /// patch and the check by the independent decoder. A capture that produced
    /// a frame the archive never received leaves a hole, so the file holds a
    /// subset and may only be published as incomplete. Nothing is appended
    /// after this, whether it worked or not.
    fn finalize_audio(&mut self, entry: &Arc<SessionEntry>) {
        let key = entry.key;
        let Some(mut active) = self.sessions.remove(&key) else {
            return;
        };
        active.audio.stage = AudioStage::Done;
        // The tail of the take is a hole: the capture published a watermark
        // past the last frame the file holds. It is noted before the encoder is
        // borrowed, because the archive record and the encoder are both fields
        // of the same take.
        let counters = entry.capture.snapshot();
        if let Some(last) = counters.last_sequence {
            if active.audio.last_sequence != Some(last) {
                // The capture went past the last frame the file holds: the hole
                // runs to the capture's own watermark.
                let next = active.audio.last_sequence.map_or(1, |seen| seen + 1);
                active.audio.note_gap(next, last);
            }
        }
        let part = active.audio.part.clone();
        let relative = part.parent().map(Path::to_path_buf);
        let (Some(encoder), Some(mut file)) =
            (active.audio.encoder.as_mut(), active.audio.file.take())
        else {
            // Nothing was ever received: metadata only, and no empty FLAC.
            self.sessions.insert(key, active);
            return;
        };
        // The tail block, the digest and the patched header. The commit borrows
        // the writer only for as long as the encoder is running.
        let encoded = {
            let mut commit = FileCommit {
                state: self,
                file: &mut file,
                path: part.clone(),
            };
            encoder.finish(&mut commit)
        };
        // The encoder's own totals are what the file really holds, whether the
        // finalization worked or not: a tail block that reached the disk before
        // a later step failed is part of the file and has to be reported.
        active.audio.committed_samples = encoder.samples();
        active.audio.committed_frames = encoder.frames();
        active.audio.encoder = None;
        // The verdict the check may act on. Every later failure clears it: a
        // header that is not the one on disk, or bytes that may not have
        // reached the disk at all, is not a verified archive.
        let mut finished = match encoded {
            Ok(finished) => Some(finished),
            Err(code) => {
                // The take is out of `self.sessions` for this whole
                // finalization, so the class is written onto its own manifest
                // here rather than looked up by key.
                note_archive_error(&mut active, entry, self, code);
                None
            }
        };
        // The patch is exactly as long as the header the file was opened with,
        // so it overwrites it in place: the length does not change, the bytes
        // are already charged to the ledger, and nothing new is admitted. The
        // check that the header really is that long lives in the codec, which
        // refuses to hand back one of another length.
        if let Some(header) = finished.as_ref().map(|finished| finished.header.clone()) {
            let wanted = header.len() as u64;
            let patch = self
                .io
                .seek(&mut file, 0)
                .and_then(|()| match self.io.write(&mut file, &header) {
                    Ok(written) if written as u64 == wanted => Ok(()),
                    Ok(_) => Err(DiagnosticCode::Io),
                    Err(code) => Err(code),
                });
            if let Err(code) = patch {
                let code = self.reconcile(relative.as_deref(), 0, code);
                note_archive_error(&mut active, entry, self, code);
                // The header on disk is still the sentinel one, so the file no
                // longer agrees with what the encoder committed.
                finished = None;
            }
        }
        if let Err(code) = self.io.sync(&mut file) {
            note_archive_error(&mut active, entry, self, code);
            // Bytes that may not have reached the disk cannot be published.
            finished = None;
        }
        // The writable handle is closed here, before anything reads the file
        // back: the handle was opened with read sharing only, so the move that
        // gives the archive its verified name could not take the name while it
        // is still open.
        drop(file);
        active.audio.file = None;
        self.sessions.insert(key, active);
        self.verify_audio(key, part, finished);
        self.finish_audio(entry, None);
    }

    /// The independent check and the one name the file earned. A `.part` whose
    /// bytes the decoder rejects, whose digest does not recompute or whose
    /// header is not the one on disk stays a `.part`: a verified name is never
    /// given to an unchecked file.
    fn verify_audio(&mut self, key: SessionKey, part: PathBuf, finished: Option<flac::Finished>) {
        let Some(finished) = finished else {
            self.with_audio(key, |audio| audio.verified = false);
            return;
        };
        let checked = self
            .io
            .open_read(&part)
            .and_then(|reader| flac::verify(reader, finished.samples, &finished.md5));
        if let Err(code) = checked {
            self.note_audio_error(key, code);
            self.with_audio(key, |audio| audio.verified = false);
            return;
        }
        // A take that recorded any hole at all holds a subset, however few
        // samples the hole cost — and a hole the tail has only just noted is
        // already a hole, even though its record is written at finalization.
        let complete = self.sessions.get(&key).is_some_and(|active| {
            active.audio.gaps_recorded == 0 && active.audio.pending_gap.is_none()
        });
        let destination = part.with_file_name(if complete {
            AUDIO_NAME
        } else {
            AUDIO_INCOMPLETE_NAME
        });
        match self.io.publish(&part, &destination) {
            Ok(()) => {
                self.ledger.rename_file(&part, &destination);
                let name = destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned();
                self.with_audio(key, |audio| {
                    audio.verified = true;
                    audio.audio_file = Some(name);
                });
            }
            Err(code) => {
                self.note_audio_error(key, code);
                self.with_audio(key, |audio| {
                    audio.verified = false;
                    audio.audio_file = None;
                });
            }
        }
    }

    /// The archive cannot be appended to any more: a failed write, a quota the
    /// take cannot afford, or a bundle the sweep removed. The producer stops
    /// copying PCM, the queued frames are counted as the holes they are, and
    /// the take is told exactly what it has.
    fn fail_audio(&mut self, key: SessionKey, code: DiagnosticCode) {
        self.note_audio_error(key, code);
        // The archive cannot take another frame, but nothing is reported and no
        // reserve is released here: the capture may still be producing and its
        // queue may still hold frames, so a report now would freeze the take's
        // numbers at whatever the failure interrupted. `write_audio` finalizes
        // once the capture has published its watermark and the queue is empty,
        // and the take's slot and reserve are held until then.
        self.with_audio(key, |audio| audio.stage = AudioStage::Done);
        if let Some(entry) = self.entry_of(key) {
            entry.set_archive_enabled(false);
        }
    }

    /// The class an archive failed with, kept everywhere it can outlive the
    /// write that noticed it: in the take's memory (so the journal record is
    /// classified even when nothing else could be written), in the take's
    /// manifest (the record that survives the segments) and once in the log.
    /// The first class stands: a later write that trips over the same closed
    /// archive does not rewrite the reason.
    fn note_audio_error(&mut self, key: SessionKey, code: DiagnosticCode) {
        if let Some(active) = self.sessions.get_mut(&key) {
            note_manifest_error(&mut active.manifest, code);
        }
        if let Some(entry) = self.entry_of(key) {
            entry.note_error_code(code);
        }
        self.note_error(code);
    }

    /// Takes the audio archive off every take the sink refused: a memory-only
    /// take, and every bundle the sweep removed. The producer stops copying
    /// PCM into a queue nobody owns, the queue is drained so the pool is not
    /// left dry, and the frames that never reached the archive stay counted.
    fn refuse_open_archives(&mut self, entries: &[Arc<SessionEntry>]) {
        for entry in entries {
            let refused = match self.sessions.get(&entry.key) {
                Some(active) => active.audio.refuses_writes(),
                None => true,
            };
            if !refused {
                continue;
            }
            entry.set_archive_enabled(false);
            // Nothing is reported here: the capture may still be producing and
            // the queue still holds frames, so the take's final numbers are
            // reported by `write_audio` once its capture has published its
            // watermark and the queue is empty — which is also why the take
            // keeps its slot and its reserve until then.
        }
    }

    /// The take's own record of what its archive ended up as: the hole with the
    /// samples it cost, the check, and the one status the manifest and the
    /// shell are told. `last` is the capture's watermark when the archive was
    /// refused outright: everything past the frame the file holds is a hole.
    fn finish_audio(&mut self, entry: &Arc<SessionEntry>, last: Option<u64>) {
        let key = entry.key;
        let Some(mut active) = self.sessions.remove(&key) else {
            return;
        };
        if active.audio.reported {
            self.sessions.insert(key, active);
            return;
        }
        active.audio.reported = true;
        if let Some(last) = last {
            let next = active.audio.last_sequence.map_or(1, |seen| seen + 1);
            if last >= next {
                active.audio.note_gap(next, last);
            }
        }
        let counters = entry.capture.snapshot();
        // The last hole the take can have is the tail. What it pays is what is
        // left after every earlier hole was charged: the capture produced this
        // much and the file holds that much, which covers the tail and any frame
        // a closed archive had to discard, with no sample counted twice.
        let remaining = counters
            .produced_samples
            .saturating_sub(active.audio.committed_samples)
            .saturating_sub(active.audio.charged_samples);
        let gaps = active.audio.flush_gaps(|_| remaining);
        active.manifest.audio_gap = active.audio.first_gap;
        // A take that captured audio and holds no verified file is an archive
        // that failed, however little of it reached the disk: a take that
        // captured nothing at all is the only one with no audio status.
        let status = match active.audio.status() {
            AudioStatus::None if counters.produced_frames > 0 => AudioStatus::Error,
            status => status,
        };
        let verified = active.audio.verified;
        // What the file really holds, which is what the verified record reports.
        let samples = active.audio.committed_samples;
        active.observe_audio();
        entry.set_audio_status(status);
        // The archive is the writer's own, so these records go straight into
        // the take's journal: the take may already be sealed by the time its
        // tail is encoded, and a sealed take admits nothing through the
        // producer queue. A journal that cannot take them keeps the numbers in
        // the manifest, which is the record that outlives the segments.
        let utc_ms = self.utc_ms();
        let elapsed_us = self.elapsed_us();
        let mut records: Vec<Event> = gaps.into_iter().map(Event::AudioGap).collect();
        if status == AudioStatus::Error {
            // The class the archive really failed with — quota, I/O, codec,
            // decoder or a mismatched digest — never a generic one.
            let code = entry.error_code().unwrap_or(DiagnosticCode::Io);
            records.push(Event::ArchiveError(ArchiveError { code }));
        }
        // A take that never had audio has nothing to verify: it gets no
        // audio_verified record, so the journal cannot claim a file that was
        // never written.
        if verified || samples > 0 {
            records.push(Event::AudioVerified(AudioVerified { samples, verified }));
        }
        for record in &records {
            let _ = active.emit(self, None, record, utc_ms, elapsed_us);
        }
        self.sessions.insert(key, active);
    }

    /// Records every hole the take has so far, each with its own losses, and
    /// writes each one into the take's own journal. The take may already be
    /// sealed, so it cannot go through the producer queue; a take with no
    /// directory keeps the record in its manifest instead.
    fn flush_gaps(&mut self, key: SessionKey) {
        // A middle hole covers whole frames, and every frame of a take is the
        // recorder's own 512 samples except the very last one, which a flush may
        // leave short. Pricing from the counters' average would floor the moment
        // that short tail existed and would poison every earlier hole, so a
        // middle hole pays the nominal frame; the tail pays the exact remainder
        // instead. The frames a hole covers are exactly what its own sequence
        // span names, so no later hole's drops are charged to it.
        let frame_samples = crate::capture::FRAME_SAMPLES as u64;
        let mut gaps = Vec::new();
        if let Some(active) = self.sessions.get_mut(&key) {
            gaps = active.audio.flush_gaps(|span| {
                (span.last_lost - span.first_sequence + 1).saturating_mul(frame_samples)
            });
        }
        for gap in gaps {
            self.emit_archive(key, Event::AudioGap(gap));
        }
    }

    /// Writes one archive record straight into the take's own journal.
    fn emit_archive(&mut self, key: SessionKey, event: Event) {
        let Some(mut active) = self.sessions.remove(&key) else {
            return;
        };
        if active.dir.is_some() && !active.disable_writes {
            let utc_ms = self.utc_ms();
            let elapsed_us = self.elapsed_us();
            let _ = active.emit(self, None, &event, utc_ms, elapsed_us);
        }
        self.sessions.insert(key, active);
    }

    /// Applies one change to a take's archive record, if the writer still has
    /// that take. Nothing else of the writer's state is visible to the change.
    fn with_audio(&mut self, key: SessionKey, change: impl FnOnce(&mut AudioArchive)) -> bool {
        match self.sessions.get_mut(&key) {
            Some(active) => {
                change(&mut active.audio);
                true
            }
            None => false,
        }
    }

    // -- settling ---------------------------------------------------------

    fn settle_take(
        &mut self,
        entry: &Arc<SessionEntry>,
        now: Instant,
        touched: &HashSet<SessionKey>,
    ) {
        let key = entry.key;
        let sealed = entry.is_sealed();
        // Drained means this take's own records are out of the queue, not just
        // that this pass happened to leave them alone: a shared queue that
        // still holds them keeps the take open.
        let drained = !touched.contains(&key) && entry.meta_pending() == 0;
        // The audio is part of the take's own state and not only of the
        // journal: a sealed take whose queued PCM has not been encoded yet
        // keeps its slot and its reserve, or the tail would never be written
        // and nothing would ever publish the file it holds. Done is not enough
        // on its own — a failed archive is marked done at once, while its queue
        // may still hold hundreds of frames — so the queue-empty proof of the
        // last audio pass is part of the condition. While the sink is closing
        // the pass has drained the whole queue, so what is left is the archive's
        // final answer and the take is let go: a capture that never published
        // its watermark leaves an unverified partial, not a take that is never
        // closed.
        let audio_done = self.shutting_down()
            || match self.sessions.get(&key) {
                Some(active) => active.audio.done() && active.audio.drained,
                None => true,
            };
        // On shutdown nothing can be produced any more and the pass emptied the
        // queue, so one settled pass is enough; while the sink lives, a second
        // pass proves no record was queued at the instant of the seal.
        let needed = if self.shutting_down() { 1 } else { 2 };
        let ready = match self.sessions.get_mut(&key) {
            Some(active) => {
                if !sealed || !drained || !audio_done {
                    active.settled = 0;
                    false
                } else {
                    active.settled = active.settled.saturating_add(1);
                    active.settled >= needed
                }
            }
            None => false,
        };
        if ready {
            self.close_take(key, now);
        }
    }

    /// The take is sealed and fully drained: its last record and its last
    /// manifest are written, the registry lets it go and the retention pass may
    /// remove the bundle from then on.
    fn close_take(&mut self, key: SessionKey, now: Instant) {
        let Some(entry) = self.entry_of(key) else {
            self.forget_active(key);
            return;
        };
        let Some(mut active) = self.sessions.remove(&key) else {
            return;
        };
        let counters = entry.capture.snapshot();
        active
            .manifest
            .observe(counters, entry.audio_status(), entry.dropped_events());
        active.observe_audio();
        let outcome = entry.terminal().unwrap_or(Outcome::Abandoned);
        active.manifest.outcome = Some(outcome);
        active.manifest.ended_utc_ms = Some(self.utc_ms());
        if entry.missing_terminal() && active.manifest.diagnostic_error.is_none() {
            active.manifest.diagnostic_error = Some(DiagnosticCode::MissingTerminal);
        }
        let end = Event::SessionEnd(SessionEnd {
            outcome,
            dropped_events: entry.dropped_events(),
            audio_status: active.manifest.audio_status,
            captured_frames: counters.produced_frames,
            captured_samples: counters.produced_samples,
            archived_samples: active.manifest.archived_samples,
            dropped_frames: counters.dropped_frames,
            dropped_samples: counters.dropped_samples,
            verified: active.manifest.verified,
        });
        let utc_ms = self.utc_ms();
        let elapsed_us = self.elapsed_us();
        if active.dir.is_some() {
            if active.disable_writes {
                active.finalize_from_reserve(self, &end, utc_ms, elapsed_us);
            } else {
                let written = active.emit(self, None, &end, utc_ms, elapsed_us).is_ok()
                    && active.flush_manifest(self).is_ok();
                if !written {
                    active.disable_writes = true;
                    active.finalize_from_reserve(self, &end, utc_ms, elapsed_us);
                }
            }
            if let Some(file) = active.jsonl.as_mut() {
                let _ = self.io.sync(file);
            }
        }
        let dir = active.dir.clone();
        self.ledger.release_reserve(key);
        if let Some(dir) = dir {
            if let Some(bundle) = self.bundles.iter_mut().find(|bundle| bundle.dir == dir) {
                bundle.active = false;
            }
        }
        self.registry.lock().sessions.remove(&key);
        // The take no longer protects its bundle, so the quota is pruned right
        // here instead of waiting for the next minute; the sweep clock is
        // stamped because this is exactly the work a sweep would do.
        self.last_sweep = now;
        self.prune_to_fit(0);
    }

    fn forget_active(&mut self, key: SessionKey) {
        self.ledger.release_reserve(key);
        if let Some(active) = self.sessions.remove(&key) {
            if let Some(dir) = active.dir {
                if let Some(bundle) = self.bundles.iter_mut().find(|bundle| bundle.dir == dir) {
                    bundle.active = false;
                }
            }
        }
    }

    // -- retention and the quota -------------------------------------------

    fn sweep(&mut self) {
        let now_ms = self.utc_ms();
        self.prune_age(now_ms);
        // The sweep just removed bundles, and the takes it could not afford
        // may have no bundle at all: their producers are told to stop copying
        // PCM, which is also what keeps a closed pool from drying out.
        let entries: Vec<Arc<SessionEntry>> =
            self.registry.lock().sessions.values().cloned().collect();
        self.refuse_open_archives(&entries);
        self.prune_to_fit(0);
    }

    fn prune_age(&mut self, now_ms: u64) {
        let max_age_ms = u64::try_from(self.policy.max_age.as_millis()).unwrap_or(u64::MAX);
        let expired: Vec<PathBuf> = self
            .bundles
            .iter()
            .filter(|bundle| !bundle.active)
            .filter(|bundle| now_ms.saturating_sub(bundle.retention_utc_ms) >= max_age_ms)
            .map(|bundle| bundle.dir.clone())
            .collect();
        for dir in expired {
            self.delete_bundle(&dir);
        }
    }

    /// Deletes finished bundles, the oldest retention first, until `needed`
    /// bytes fit. An active take is never a candidate, and a bundle whose
    /// removal did not finish is not asked twice in one pass: the bytes it
    /// really lost were measured back from the disk, and the next candidate is
    /// the one that may still make the room.
    fn prune_to_fit(&mut self, needed: u64) {
        if self.ledger.room(needed) {
            return;
        }
        for dir in self.finished_by_age() {
            if self.ledger.room(needed) || self.sink_disabled.load(Ordering::Acquire) {
                return;
            }
            self.delete_bundle(&dir);
        }
    }

    /// Every finished bundle, the oldest retention first and a retention tie
    /// broken by the ordinal name.
    fn finished_by_age(&self) -> Vec<PathBuf> {
        let mut candidates: Vec<&Bundle> =
            self.bundles.iter().filter(|bundle| !bundle.active).collect();
        candidates.sort_by(|a, b| {
            a.retention_utc_ms
                .cmp(&b.retention_utc_ms)
                .then_with(|| a.dir.cmp(&b.dir))
        });
        candidates
            .into_iter()
            .map(|bundle| bundle.dir.clone())
            .collect()
    }

    /// Removes one whole bundle. Every name is re-checked and every file is
    /// verified first, so a stranger or a broadened child that appeared
    /// meanwhile stops the sink instead of being deleted.
    fn delete_bundle(&mut self, dir: &Path) -> bool {
        if self.io.verify(dir).is_err() {
            self.disable_sink(DiagnosticCode::Acl);
            return false;
        }
        let children = match self.io.list(dir) {
            Ok(children) => children,
            Err(_) => return false,
        };
        for child in &children {
            let owned = child.name.to_str().and_then(owned_child);
            if child.reparse || child.dir || owned.is_none() {
                self.disable_sink(DiagnosticCode::ForeignArchive);
                return false;
            }
            if self.io.verify(&dir.join(&child.name)).is_err() {
                self.disable_sink(DiagnosticCode::Acl);
                return false;
            }
        }
        if self.io.remove_tree(dir).is_err() {
            // The bytes stay charged until the delete actually succeeds. A
            // removal that did not finish may still have taken part of the tree
            // with it, so what really is left is measured back.
            self.reconcile_tree(dir);
            return false;
        }
        self.ledger.forget_tree(dir);
        self.bundles.retain(|bundle| bundle.dir != dir);
        true
    }
}
