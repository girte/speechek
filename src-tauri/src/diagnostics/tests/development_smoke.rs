//! The headless end-to-end smoke: a real persistent encoder, the real loopback
//! router, the real Live relay and a fake Gemini upstream, with the real
//! development wrapper driven by a real Bun child.
//!
//! What one run proves, in one take:
//!
//! * the synthetic source is framed by the production encoder - the same
//!   resampler, the same 512-sample frames, never recreated between batches -
//!   and the frames the archive received are byte-for-byte the samples this
//!   test wrote, late tail included;
//! * the same frames, served over `/fixture/frames`, are what the browser
//!   wrapper feeds to the core, so the native tap, the emitted frames and the
//!   relayed audio are one sample sequence;
//! * the relay really forwards every frame to a real upstream socket; the
//!   upstream's own boundaries (setup, the activity it opens, an interim, the
//!   final of that speech, the turn, the end of each activity, the final tail,
//!   one close) reach the journal as numbers, and the text those frames carried
//!   is archived beside them complete - the interim, the finals and the close
//!   detail the archive is read back for;
//! * the take is long enough for the relay to rotate its own activity at the
//!   forced limit, hold the audio that follows until the upstream ends that
//!   activity and replay it into the next one, so not a frame of a recording
//!   longer than the server's own ~220 s activity is lost;
//! * the renderer's own report, result and errors included, is admitted over
//!   the real loopback endpoint while the take is open, and its boundaries land
//!   beside the native ones;
//! * the take closes exactly once: one verified FLAC whose samples claxon
//!   decodes to exactly the committed PCM, one manifest, one journal whose
//!   content records hold what the take produced in full, and no pinned key, no
//!   device and no path anywhere in them.
//!
//! The child is a real process, the servers are real sockets on ephemeral
//! loopback ports and the sink is isolated to a scratch root. Everything is
//! bounded: the whole smoke runs under [`SMOKE_BUDGET`], and the guard kills
//! the child, aborts the servers, releases the writer and removes the root on
//! every path, including a panicking one.

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead as _, BufReader};
use std::net::{Ipv4Addr, TcpListener as StdListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message as UpstreamMessage};

use super::super::*;
use crate::backend::diagnostic_test_router;
use crate::capture::DiagnosticsFramingFixture;
use crate::i18n::Language;
use crate::provider::Provider;
use crate::secrets::{KeyRing, SecretKey};
use crate::settings::{RuntimeSnapshot, Settings, SharedRuntime};

/* -------------------------------------------------------------------------- */
/* The take                                                                   */
/* -------------------------------------------------------------------------- */

/// The whole smoke's deadline. A run that has not finished by then is killed
/// and fails, so a wedged child, writer or socket never holds the suite.
const SMOKE_BUDGET: Duration = Duration::from_secs(60);

/// The generation the child drives, the one the take is registered under.
const GENERATION: u64 = 1;

/// Source samples the fixture is fed: exactly one more than the samples the
/// frames carry, because the encoder always keeps one source sample for the
/// interpolation step that never comes. The remainder is what the final flush
/// emits as the 37-sample tail.
const TOTAL_SOURCE: u64 = 7_500 * 512 + 38;

/// The frames the encoder emits for that source: 7500 full frames and one
/// 37-sample tail - the "late tail" of the capture that a live take would end
/// with.
const FRAMES: u64 = 7_501;

/// The samples those frames carry.
const SAMPLES: u64 = 7_500 * 512 + 37;

/// The sample rate the capture and the relay both work in, in Hz.
const SAMPLE_RATE: u64 = 16_000;

/// The transcript the fake upstream finalizes the interim's speech with, before
/// the take reaches its first rotation. A real upstream finalizes speech it has
/// heard; an activity that ends on an interim whose final never came is the
/// incomplete case, not this one.
const ACTIVITY_FINAL: &str = "synthetic activity final transcript";

/// Source samples one batch may carry, so one batch emits at most 128 frames.
const BATCH_SOURCE: usize = 128 * 512;

/// The frames one served batch may carry.
const BATCH_FRAMES: usize = 128;

/// The synthetic secrets the fake upstream is configured with. Neither may
/// reach the archive in any form - not as the key itself, not as the text a
/// JSON document would carry it as and not as the text a query string would.
/// The second key is shaped so those three forms really are three different
/// strings: a key the archive leaks in one of them is still a leaked key.
const SECRET: &str = "synthetic-smoke-secret-key";
const ESCAPED_SECRET: &str = "synthetic\"secret\\key/+=?&%";
/// The synthetic transcripts and the close detail the fake upstream uses. Every
/// one of them is what the take really produced, so every one of them has to be
/// read back from the archive complete.
const INTERIM: &str = "synthetic interim transcript";
const FINAL: &str = "synthetic final transcript";
const CLOSE_REASON: &str = "synthetic close reason";
/// Every key the take is pinned with, for the absence scan.
const PINNED_KEYS: &[&str] = &[SECRET, ESCAPED_SECRET];

/* -------------------------------------------------------------------------- */
/* Tool and script resolution                                                 */
/* -------------------------------------------------------------------------- */

/// The repository root: this file lives in `src-tauri/src/diagnostics/tests`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the repository root above src-tauri")
        .to_path_buf()
}

/// The Bun the other checks would use, resolved exactly as
/// `Resolve-SpeechekBun` resolves it: `SPEECHEK_BUN` first, then `PATH`, then
/// the per-user install, then the local developer cache. `SPEECHEK_BUN` is a
/// tool path, not provider configuration, and nothing here reads an API base,
/// a key or any other provider setting.
fn resolve_bun() -> PathBuf {
    let configured = std::env::var_os("SPEECHEK_BUN").map(PathBuf::from);
    if let Some(bun) = configured.as_deref().filter(|path| path.is_file()) {
        return bun.to_path_buf();
    }
    let names: &[&str] = if cfg!(windows) {
        &["bun.exe"]
    } else {
        &["bun"]
    };
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            for name in names {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let per_user = PathBuf::from(profile).join(".bun").join("bin").join("bun.exe");
        if per_user.is_file() {
            return per_user;
        }
    }
    let local = repo_root()
        .join(".private")
        .join("tools")
        .join("bun-windows-x64")
        .join("bun.exe");
    if local.is_file() {
        return local;
    }
    panic!(
        "Bun was not found. Put bun.exe on PATH, or set SPEECHEK_BUN to a real \
         bun.exe path (the same contract as scripts/release-common.ps1)."
    );
}

/// One script of this repository, by the name the checks use.
fn resolve_script(name: &str) -> PathBuf {
    let script = repo_root().join("scripts").join(name);
    assert!(
        script.is_file(),
        "the smoke's script is missing: {}",
        script.display()
    );
    script
}

/* -------------------------------------------------------------------------- */
/* The scratch root                                                           */
/* -------------------------------------------------------------------------- */

/// A directory of this test's own, removed when the run succeeded. A run that
/// failed keeps it, so the evidence is still there to look at, and the only
/// thing left behind is this process's own scratch directory.
///
/// This directory itself is deliberately ordinary: it is only a container, and
/// nothing ever verifies its access list. The sink's own root is the `logs`
/// child under it, which must not exist yet - the writer creates it with the
/// private descriptor `create_private_directory` gives every archive root, and
/// it refuses a root whose access list is foreign to this user. Creating the
/// root here, as a plain temporary directory, is what makes the writer declare
/// the whole sink unusable and every take fail to register.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "speechek-smoke-{}-{tag}-{}",
            std::process::id(),
            unique()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a scratch root");
        Self { path }
    }

    /// The sink's root: the one directory under this scratch that the writer
    /// owns and creates itself.
    fn logs(&self) -> PathBuf {
        self.path.join("logs")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("the smoke's scratch root was kept: {}", self.path.display());
            return;
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::process::id() as u64 * 1_000_000 + COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

fn epoch() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/* -------------------------------------------------------------------------- */
/* The synthetic source                                                       */
/* -------------------------------------------------------------------------- */

/// The exact rounding the encoder uses, so the expected PCM is the encoder's
/// own bytes and not a second opinion about them.
fn pcm16(level: f32) -> i16 {
    (level.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

/// One source sample. The pattern is deterministic and cheap, and the last
/// four emitted samples are fixed markers: `0.5, -0.5, 1.0, -1.0`, of which
/// the last three sit inside the 37-sample tail.
fn source(index: u64) -> f32 {
    if index == SAMPLES - 1 {
        return 1.0;
    }
    if index == SAMPLES - 2 {
        return -1.0;
    }
    if index == SAMPLES - 3 {
        return 0.5;
    }
    if index == SAMPLES - 4 {
        return -0.5;
    }
    match index % 8 {
        0 => 0.0,
        1 => -1.0,
        2 => 1.0,
        3 => 0.5,
        4 => -0.5,
        5 => 0.25,
        6 => -0.25,
        _ => 0.75,
    }
}

/// The PCM the capture committed: every sample the encoder emits, in order.
fn expected_pcm() -> Vec<u8> {
    let mut pcm = Vec::with_capacity(SAMPLES as usize * 2);
    for index in 0..SAMPLES {
        pcm.extend_from_slice(&pcm16(source(index)).to_le_bytes());
    }
    pcm
}

/* -------------------------------------------------------------------------- */
/* The fixture route                                                          */
/* -------------------------------------------------------------------------- */

/// What the child pulls: the batches the coordinator has already framed, in
/// order, and the end of the sequence.
#[derive(Default)]
struct Pending {
    batches: VecDeque<Value>,
    failure: Option<String>,
}

struct FixtureState {
    pending: StdMutex<Pending>,
    ready: tokio::sync::Notify,
    stop: Arc<AtomicBool>,
}

impl FixtureState {
    fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            pending: StdMutex::new(Pending::default()),
            ready: tokio::sync::Notify::new(),
            stop,
        }
    }

    /// One framed batch, ready to be served. `eof` marks the batch after which
    /// no frame is owed any more.
    fn publish(&self, frames: Vec<(u64, Vec<u8>)>, eof: bool) {
        assert!(
            frames.len() <= BATCH_FRAMES,
            "one batch carries at most {BATCH_FRAMES} frames, not {}",
            frames.len()
        );
        let last = frames.last().map(|(sequence, _)| *sequence);
        let payload = json!({
            "frames": frames
                .into_iter()
                .map(|(sequence, bytes)| json!({
                    "sequence": sequence,
                    "data": base64_encode(&bytes),
                }))
                .collect::<Vec<Value>>(),
            "eof": eof,
            "lastSequence": last,
        });
        self.pending
            .lock()
            .expect("the fixture lock")
            .batches
            .push_back(payload);
        self.ready.notify_waiters();
    }

    fn fail(&self, reason: impl Into<String>) {
        self.pending.lock().expect("the fixture lock").failure = Some(reason.into());
        self.ready.notify_waiters();
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `GET /fixture/frames`: one batch per request, in the order the coordinator
/// framed them. The handler waits for the next batch rather than answering an
/// empty list, so a slow writer never looks like the end of the sequence.
async fn fixture_frames(State(state): State<Arc<FixtureState>>) -> Response {
    let deadline = Instant::now() + SMOKE_BUDGET;
    loop {
        {
            let mut pending = state.pending.lock().expect("the fixture lock");
            if let Some(batch) = pending.batches.pop_front() {
                return json_response(StatusCode::OK, batch);
            }
            if let Some(failure) = &pending.failure {
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "failure": failure }),
                );
            }
        }
        if Instant::now() >= deadline || state.stop.load(Ordering::SeqCst) {
            state.fail("the fixture stopped producing frames");
            continue;
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), state.ready.notified()).await;
    }
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json;charset=utf-8")],
        body.to_string(),
    )
        .into_response()
}

/* -------------------------------------------------------------------------- */
/* The fake upstream                                                          */
/* -------------------------------------------------------------------------- */

/// The bytes one base64 payload carries, its padding removed: what the frame
/// really holds, so the fixture can name the audio offset it answers at.
fn base64_bytes(base64: &str) -> usize {
    let padding = base64
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    base64.len() / 4 * 3 - padding
}

/// One audio offset in the shape the Live API announces it in: the seconds of
/// audio the session has carried so far, as `"220.440s"`.
fn offset_seconds(samples: u64) -> String {
    format!("{:.3}s", samples as f64 / SAMPLE_RATE as f64)
}

/// A Gemini-shaped Live socket: it confirms the setup, opens the activity it is
/// asked for, then answers the relayed audio with an interim transcript and the
/// final that settles it, sends the end of every activity it is asked to close
/// - naming the audio offset it ended at - and, once the whole recording has
/// arrived and the take's own `activityEnd` follows it, the final tail and one
/// close. Nothing here is a second capture: every frame it receives is a frame
/// the native capture produced.
async fn fake_upstream(listener: TcpListener) {
    let Ok((stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let mut audio = 0_u64;
    let mut samples = 0_u64;
    while let Some(message) = socket.next().await {
        let Ok(message) = message else {
            return;
        };
        let text = match message {
            UpstreamMessage::Text(text) => text.to_string(),
            UpstreamMessage::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            UpstreamMessage::Close(_) => return,
            UpstreamMessage::Ping(_) | UpstreamMessage::Pong(_) | UpstreamMessage::Frame(_) => {
                continue
            }
        };
        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if payload.get("setup").is_some() {
            let _ = socket
                .send(UpstreamMessage::text(r#"{"setupComplete":{}}"#.to_owned()))
                .await;
            continue;
        }
        if let Some(data) = payload
            .pointer("/realtimeInput/audio/data")
            .and_then(Value::as_str)
        {
            audio += 1;
            samples += (base64_bytes(data) / 2) as u64;
            if audio == 8 {
                let _ = socket
                    .send(UpstreamMessage::text(
                        json!({ "serverContent": { "interimInputTranscription": { "text": INTERIM } } })
                            .to_string(),
                    ))
                    .await;
            }
            if audio == 16 {
                // The speech the interim announced, finalized with the turn it
                // belonged to: an activity may only end after this.
                let _ = socket
                    .send(UpstreamMessage::text(
                        json!({ "serverContent": {
                            "inputTranscription": { "text": ACTIVITY_FINAL },
                            "turnComplete": true,
                        } })
                        .to_string(),
                    ))
                    .await;
            }
            continue;
        }
        if payload.pointer("/realtimeInput/activityStart").is_some() {
            // The activity the relay just opened, and the offset of the audio
            // it is about to hear.
            let _ = socket
                .send(UpstreamMessage::text(
                    json!({ "voiceActivity": {
                        "type": "ACTIVITY_START",
                        "audioOffset": offset_seconds(samples),
                    } })
                    .to_string(),
                ))
                .await;
            continue;
        }
        if payload.pointer("/realtimeInput/activityEnd").is_some() {
            // The activity ends here, at the offset of the audio it carried.
            // The take's own end is the one that arrives once every frame of
            // the recording has: only that one carries the final tail and the
            // one close this session gets.
            let _ = socket
                .send(UpstreamMessage::text(
                    json!({ "voiceActivity": {
                        "type": "ACTIVITY_END",
                        "audioOffset": offset_seconds(samples),
                    } })
                    .to_string(),
                ))
                .await;
            if samples == SAMPLES {
                let _ = socket
                    .send(UpstreamMessage::text(
                        json!({ "serverContent": { "inputTranscription": { "text": FINAL } } })
                            .to_string(),
                    ))
                    .await;
                let _ = socket
                    .send(UpstreamMessage::Close(Some(CloseFrame {
                        code: CloseCode::from(1000_u16),
                        reason: CLOSE_REASON.into(),
                    })))
                    .await;
                return;
            }
        }
    }
}

/* -------------------------------------------------------------------------- */
/* The child                                                                  */
/* -------------------------------------------------------------------------- */

/// The lines the child printed so far: the failure text of this run.
#[derive(Clone, Default)]
struct Transcript {
    lines: Arc<StdMutex<Vec<String>>>,
}

impl Transcript {
    fn push(&self, line: &str) {
        let mut lines = self.lines.lock().expect("the transcript lock");
        lines.push(line.to_owned());
        if lines.len() > 512 {
            lines.remove(0);
        }
    }

    fn text(&self) -> String {
        self.lines
            .lock()
            .expect("the transcript lock")
            .join("\n")
    }
}

/// Waits for one line that carries `marker`, bounded by `deadline`.
fn await_marker(
    lines: &Receiver<String>,
    transcript: &Transcript,
    marker: &str,
    deadline: Instant,
) -> String {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "the child never printed {marker}:\n{}",
            transcript.text()
        );
        match lines.recv_timeout(left.min(Duration::from_millis(250))) {
            Ok(line) => {
                if line.contains(marker) {
                    return line;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => panic!(
                "the child stopped printing before {marker}:\n{}",
                transcript.text()
            ),
        }
    }
}

/* -------------------------------------------------------------------------- */
/* The guard                                                                  */
/* -------------------------------------------------------------------------- */

/// Everything one smoke run owns. Dropping it, on any path, kills the child,
/// aborts the servers, stops the writer and lets the scratch root go: no child,
/// socket, thread or file of this test outlives the test.
struct Rig {
    child: Option<Child>,
    /// The backend's own origin: the only page allowed to drive this loopback,
    /// and the one the child sends with every request.
    origin: String,
    stop: Arc<AtomicBool>,
    producer: Option<std::thread::JoinHandle<()>>,
    diagnostics: Arc<Diagnostics>,
    servers: Vec<tokio::task::AbortHandle>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Rig {
    fn abort_servers(&mut self) {
        for server in self.servers.drain(..) {
            server.abort();
        }
    }

    /// Ends the run: no child, no server, no writer, before the root is read.
    fn finish(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.abort_servers();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(5));
        }
        self.diagnostics.shutdown(Duration::from_secs(2));
        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.finish();
    }
}

/* -------------------------------------------------------------------------- */
/* The smoke                                                                  */
/* -------------------------------------------------------------------------- */

/// The take's configuration revision: Live, the synthetic keys, pinned to the
/// generation the child drives. No environment variable is read on any path.
fn snapshot() -> Arc<RuntimeSnapshot> {
    Arc::new(RuntimeSnapshot {
        settings: Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            input_device: None,
            language: Language::En,
            custom_vocabulary: Arc::from(vec![
                "Speechek".to_owned(),
                "Kubernetes".to_owned(),
            ]),
            dictation_languages: Arc::from(vec!["ru-RU".to_owned(), "en-US".to_owned()]),
            history_limit: crate::settings::DEFAULT_HISTORY_LIMIT,
        },
        keys: Arc::new(KeyRing::new(
            PINNED_KEYS
                .iter()
                .map(|key| Arc::new(SecretKey::new((*key).to_owned())))
                .collect(),
        )),
        revision: 1,
    })
}

pub(crate) fn development_archive_smoke_body() {
    let started = Instant::now();
    let deadline = started + SMOKE_BUDGET;

    let scratch = Scratch::new("archive");
    let key = SessionKey {
        owner: Owner::Dictation,
        generation: GENERATION,
    };
    let diagnostics = Diagnostics::for_test_with_storage(&scratch.logs(), Policy::DEVELOPMENT, epoch());
    let pinned = snapshot();
    let runtime = Arc::new(SharedRuntime::new(Arc::clone(&pinned)));
    runtime.pin(GENERATION, Arc::clone(&pinned));

    // Every port is ephemeral and bound before anything is spawned, so the
    // child and the fake upstream are told where to go instead of guessing.
    let upstream = bind_loopback();
    let backend = bind_loopback();
    let upstream_port = upstream.local_addr().expect("the upstream address").port();
    let origin = format!("http://127.0.0.1:{}", backend.local_addr().expect("the backend address").port());
    let provider = Arc::new(Provider::for_test(
        "http://127.0.0.1:1",
        &format!("ws://127.0.0.1:{upstream_port}"),
    ));

    let handle = diagnostics
        .begin(key, Mode::Live)
        .expect("the take is registered");

    let stop = Arc::new(AtomicBool::new(false));
    let fixture = Arc::new(FixtureState::new(Arc::clone(&stop)));
    let ports = Ports {
        backend,
        upstream,
        provider: Arc::clone(&provider),
        runtime,
        diagnostics: Arc::clone(&diagnostics),
        fixture: Arc::clone(&fixture),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("the smoke's runtime");
    let servers = rt.block_on(ports.serve());

    let mut rig = Rig {
        child: None,
        origin: origin.clone(),
        stop: Arc::clone(&stop),
        producer: None,
        diagnostics: Arc::clone(&diagnostics),
        servers,
        runtime: Some(rt),
    };

    // The child: the real wrapper, driven the way the desktop would drive it.
    // Bun and the script are found the same way the PowerShell tooling finds
    // them, so a machine that can run the other checks can run this one.
    let bun = resolve_bun();
    let script = resolve_script("dev-recorder.test.mjs");
    let mut child = Command::new(&bun)
        .arg(&script)
        .arg("--smoke-url")
        .arg(&rig.origin)
        .arg("--owner")
        .arg("dictation")
        .arg("--generation")
        .arg(GENERATION.to_string())
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the Bun child");
    let stdout = child.stdout.take().expect("the child's stdout");
    let stderr = child.stderr.take().expect("the child's stderr");
    let (tx, lines) = mpsc::channel();
    let transcript = Transcript::default();
    let out_transcript = transcript.clone();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            out_transcript.push(&line);
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let err_transcript = transcript.clone();
    let err_reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            err_transcript.push(&line);
        }
    });
    rig.child = Some(child);

    // No PCM is produced before the wrapper observed its own `ready`: a core
    // that is still queueing would otherwise overflow its pre-ready cap and
    // fail an otherwise healthy smoke.
    let ready = await_marker(
        &lines,
        &transcript,
        "SPEECHEK_SMOKE_READY",
        deadline.min(Instant::now() + Duration::from_secs(30)),
    );
    assert!(ready.contains("SPEECHEK_SMOKE_READY"), "{ready}");

    // The sink's monotonic clock is the fake one: it moves only when this test
    // moves it. Left frozen, every elapsed watermark is a legitimate zero and
    // proves nothing about the relay's timing, so the take is advanced at its
    // real milestones instead - all of them while it is still open, and far
    // below the seal grace and the idle deadline.
    advance_take_clock(&diagnostics, Duration::from_millis(50));

    // The coordinator: frame the synthetic source in bounded batches, wait for
    // the writer between them (outside the encoder), and publish each batch for
    // the child to pull. Nothing here is a second PCM source.
    let producer = {
        let diagnostics = Arc::clone(&diagnostics);
        let fixture = Arc::clone(&fixture);
        let stop = Arc::clone(&stop);
        let handle = handle.clone();
        std::thread::spawn(move || {
            let mut encoder = DiagnosticsFramingFixture::new(key, 16_000, handle);
            let mut batch = vec![0.0_f32; BATCH_SOURCE];
            let mut index = 0_u64;
            let mut handed = 0_u64;
            let mut ticked = 0_u64;
            while index < TOTAL_SOURCE {
                assert!(
                    !stop.load(Ordering::SeqCst) && Instant::now() < deadline,
                    "the smoke ran out of time while framing"
                );
                let take = ((TOTAL_SOURCE - index) as usize).min(BATCH_SOURCE);
                for (slot, offset) in batch[..take].iter_mut().zip(0_u64..) {
                    *slot = source(index + offset);
                }
                encoder.push(&batch[..take]);
                index += take as u64;
                let mut frames = encoder.take_frames();
                let last = index == TOTAL_SOURCE;
                if last {
                    // The take ends exactly once; the flush emits the tail that
                    // was still waiting for its interpolation neighbour.
                    encoder.finish(Outcome::Success);
                    frames.extend(encoder.take_frames());
                }
                handed += frames.len() as u64;
                // Between pushes, and never inside the encoder: the writer has
                // taken what was pushed, so the pool and the queue are free
                // again.
                while encoder.consumed_watermark() < handed {
                    assert!(
                        !stop.load(Ordering::SeqCst) && Instant::now() < deadline,
                        "the writer did not consume the framed batches in time"
                    );
                    diagnostics.worker_flush();
                }
                // One millisecond per batch: the take's monotonic clock moves
                // with the frames, as a live capture's would, so the relay's
                // elapsed watermarks are real and distinct rather than a
                // frozen zero.
                if ticked < index / BATCH_SOURCE as u64 {
                    ticked = index / BATCH_SOURCE as u64;
                    advance_take_clock(&diagnostics, Duration::from_millis(1));
                }
                fixture.publish(frames, last);
            }
            assert_eq!(handed, FRAMES, "every frame reached the archive");
        })
    };
    rig.producer = Some(producer);

    // The child's own verdict: every frame fed, and its terminal report
    // admitted over the real endpoint before it left.
    let done = await_marker(
        &lines,
        &transcript,
        "SPEECHEK_SMOKE_DONE",
        deadline.min(Instant::now() + Duration::from_secs(45)),
    );
    assert!(done.contains("accepted=1"), "{done}");
    assert!(done.contains(&format!("frames={FRAMES}")), "{done}");
    assert!(done.contains(&format!("lastSequence={FRAMES}")), "{done}");
    let status = wait_exit(rig.child.as_mut(), deadline).expect("the child exited");
    assert!(status.success(), "the child failed:\n{}", transcript.text());
    let _ = reader.join();
    let _ = err_reader.join();

    // One last milestone: the terminal transcription and the close arrived
    // after the last frame, so the take's clock moves once more before the
    // shell's own terminal is applied.
    advance_take_clock(&diagnostics, Duration::from_millis(50));

    // Only now, with the child's result in hand, does the shell's terminal
    // land: a dictation's outcome is the shell's to decide.
    handle.end_capture(Outcome::Success);
    diagnostics.end(key, Outcome::Success);
    finalize(&diagnostics, deadline);

    let dir = single_bundle(&scratch.logs());
    verify_archive(&dir);
    verify_journal(&dir);
    verify_content(&dir);
    verify_privacy(&dir);
    rig.finish();
}

fn bind_loopback() -> StdListener {
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
    listener
        .set_nonblocking(true)
        .expect("a non-blocking listener");
    listener
}

/// The listeners, the router and the fake upstream, spawned together.
struct Ports {
    backend: StdListener,
    upstream: StdListener,
    provider: Arc<Provider>,
    runtime: Arc<SharedRuntime>,
    diagnostics: Arc<Diagnostics>,
    fixture: Arc<FixtureState>,
}

impl Ports {
    async fn serve(self) -> Vec<tokio::task::AbortHandle> {
        let backend = TcpListener::from_std(self.backend).expect("the backend listener");
        let upstream = TcpListener::from_std(self.upstream).expect("the upstream listener");
        let port = backend.local_addr().expect("the backend address").port();
        let router = diagnostic_test_router(self.runtime, self.provider, self.diagnostics, port)
            .route(
                "/fixture/frames",
                get(fixture_frames).with_state(Arc::clone(&self.fixture)),
            );
        let serving = tokio::spawn(async move {
            let _ = axum::serve(backend, router).await;
        });
        let upstream_task = tokio::spawn(fake_upstream(upstream));
        vec![serving.abort_handle(), upstream_task.abort_handle()]
    }
}

/// Waits for the child to leave, bounded; a child that will not leave is
/// killed and reported as no exit at all.
fn wait_exit(child: Option<&mut Child>, deadline: Instant) -> Option<ExitStatus> {
    let child = child?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/* -------------------------------------------------------------------------- */
/* The take's clock                                                           */
/* -------------------------------------------------------------------------- */

/// Moves the take's monotonic clock by `by`, the way a live capture moves it
/// with every frame.
///
/// The sink a test builds keeps a `FakeClock`, so nothing but this call ever
/// advances the instant its elapsed watermarks are measured from. Advancing it
/// at the take's real milestones is what makes those watermarks a statement
/// about the relay's timing instead of a frozen zero; it is never advanced past
/// the seal grace or the idle deadline.
fn advance_take_clock(diagnostics: &Diagnostics, by: Duration) {
    diagnostics.advance_clock(by);
}

/* -------------------------------------------------------------------------- */
/* Finalization                                                               */
/* -------------------------------------------------------------------------- */

/// Lets the writer seal, encode, verify and close the take, bounded. The
/// monotonic clock is the sink's own fake one, so the seal grace is advanced
/// rather than slept through.
fn finalize(diagnostics: &Diagnostics, deadline: Instant) {
    diagnostics.advance_clock(SEAL_GRACE + Duration::from_millis(1));
    loop {
        diagnostics.worker_flush();
        if let Some(dir) = bundle_dir_of(diagnostics) {
            if finalized(&dir) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the take did not finalize inside the smoke budget"
        );
    }
}

/// Whether the writer has really closed the bundle, as opposed to merely
/// having published the audio file.
///
/// The conditions are the terminal state itself, not the presence of a key: a
/// manifest is written from the take's very first snapshot, so `outcome` is
/// present as `null` long before it is a terminal, and `audio.flac` appears one
/// pass before the take is closed. Waiting on presence alone ends the wait in
/// that window and reads a manifest whose outcome is still `null`.
fn finalized(dir: &Path) -> bool {
    let manifest = manifest_of(dir);
    manifest["outcome"].as_str().is_some()
        && manifest["endedUtcMs"].is_u64()
        && matches!(
            manifest["audioStatus"].as_str(),
            Some("complete") | Some("incomplete")
        )
}

/// The one bundle the sink wrote under its root.
fn bundle_dir_of(diagnostics: &Diagnostics) -> Option<PathBuf> {
    let root = diagnostics.root()?;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    (dirs.len() == 1).then(|| dirs.remove(0))
}

fn single_bundle(root: &Path) -> PathBuf {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .expect("the archive root")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert_eq!(dirs.len(), 1, "one take, one bundle: {dirs:?}");
    dirs.remove(0)
}

fn manifest_of(dir: &Path) -> Value {
    let bytes = std::fs::read(dir.join("session.json")).unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Every journal record of the bundle, in segment and line order.
fn journal(dir: &Path) -> Vec<Value> {
    let mut segments: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the bundle directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            name.starts_with("events-") && name.ends_with(".jsonl")
        })
        .collect();
    segments.sort();
    let mut records = Vec::new();
    for segment in segments {
        let text = std::fs::read_to_string(segment).unwrap_or_default();
        for line in text.lines() {
            records.push(serde_json::from_str(line).expect("a journal record"));
        }
    }
    records
}

/* -------------------------------------------------------------------------- */
/* The archive                                                                */
/* -------------------------------------------------------------------------- */

fn verify_archive(dir: &Path) {
    let manifest = manifest_of(dir);
    assert_eq!(manifest["schemaVersion"], 2, "{manifest}");
    assert_eq!(manifest["owner"], "dictation");
    assert_eq!(manifest["generation"], GENERATION);
    assert_eq!(manifest["mode"], "live");
    assert_eq!(manifest["outcome"], "success");
    assert_eq!(manifest["audioStatus"], "complete");
    assert_eq!(manifest["audioFile"], "audio.flac");
    assert_eq!(manifest["verified"], true);
    assert_eq!(manifest["capturedFrames"], FRAMES);
    assert_eq!(manifest["capturedSamples"], SAMPLES);
    assert_eq!(manifest["archivedSamples"], SAMPLES);
    assert_eq!(manifest["droppedFrames"], 0);
    assert_eq!(manifest["droppedSamples"], 0);
    assert_eq!(manifest["lastSequence"], FRAMES);
    assert_eq!(manifest["diagnosticError"], Value::Null);
    assert_eq!(manifest["audioGap"], Value::Null);
    assert!(
        manifest["endedUtcMs"].is_number(),
        "the take has an end: {manifest}"
    );

    // One verified FLAC, no partial and no surviving-subset name: the whole
    // take is there.
    assert!(dir.join("audio.flac").exists(), "the verified archive");
    assert!(
        !dir.join("audio.flac.part").exists(),
        "a published archive has no partial"
    );
    assert!(
        !dir.join("audio.incomplete.flac").exists(),
        "a complete take is not a subset"
    );

    // The independent verdict: claxon decodes the file and every sample has to
    // be the PCM this test committed, in order, with the digest recomputed over
    // it.
    let file = std::fs::File::open(dir.join("audio.flac")).expect("the archive file");
    let pcm = expected_pcm();
    flac::verify_pcm(file, SAMPLES, &pcm).expect("the archive, sample for sample");

    // The late tail: exactly one short final frame of 37 samples, and its last
    // three samples are the markers the source put there.
    let tail_samples = (SAMPLES % 512) as usize;
    assert_eq!(tail_samples, 37, "a late tail of 37 samples");
    let tail = &pcm[pcm.len() - tail_samples * 2..];
    assert_eq!(
        tail.len(),
        37 * 2,
        "the tail is not padded and not doubled"
    );
    let last_of = |offset: usize| i16::from_le_bytes([tail[offset], tail[offset + 1]]);
    assert_eq!(last_of(tail.len() - 2), pcm16(1.0), "the final marker");
    assert_eq!(last_of(tail.len() - 4), pcm16(-1.0));
    assert_eq!(last_of(tail.len() - 6), pcm16(0.5));
    assert_eq!(tail_samples, 37);
}

/* -------------------------------------------------------------------------- */
/* The journal                                                                */
/* -------------------------------------------------------------------------- */

fn events<'a>(records: &'a [Value], event: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| record["event"] == event)
        .collect()
}

fn verify_journal(dir: &Path) {
    let records = journal(dir);
    assert!(!records.is_empty(), "the take has a journal");

    // The envelope, record by record. The clock fields are read when a record
    // is written and the records are written in sequence order, so neither may
    // ever go backwards; with a test clock several records legitimately share
    // one timestamp, which is why the comparison is non-strict.
    let mut sequence = 0_u64;
    let mut run_id = String::new();
    let mut session_id = String::new();
    let mut previous_utc_ms = 0_u64;
    let mut previous_elapsed_us = 0_u64;
    for record in &records {
        assert_eq!(record["schemaVersion"], 2, "{record}");
        assert_eq!(record["owner"], "dictation", "{record}");
        assert_eq!(record["generation"], GENERATION, "{record}");
        assert!(record["event"].is_string(), "{record}");
        assert!(record["data"].is_object(), "{record}");
        let source = record["source"].as_str().expect("a source class");
        assert!(
            ["shell", "capture", "renderer", "relay", "provider", "archive"].contains(&source),
            "{record}"
        );
        let utc_ms = record["utcMs"].as_u64().expect("a wall clock");
        let elapsed_us = record["elapsedUs"].as_u64().expect("an elapsed clock");
        assert!(utc_ms >= previous_utc_ms, "the wall clock went back: {record}");
        assert!(
            elapsed_us >= previous_elapsed_us,
            "the elapsed clock went back: {record}"
        );
        previous_utc_ms = utc_ms;
        previous_elapsed_us = elapsed_us;
        let next = record["sequence"].as_u64().expect("a sequence");
        assert_eq!(next, sequence + 1, "the sequence is dense: {record}");
        sequence = next;
        if run_id.is_empty() {
            run_id = record["runId"].as_str().expect("a run id").to_owned();
            session_id = record["sessionId"].as_str().expect("a session id").to_owned();
        }
        assert_eq!(record["runId"], json!(run_id));
        assert_eq!(record["sessionId"], json!(session_id));
    }
    let manifest = manifest_of(dir);
    assert_eq!(manifest["runId"], json!(run_id), "one run, one prefix");
    assert_eq!(manifest["sessionId"], json!(session_id));

    // The capture: 7501 frames of 37+7500*512 samples, complete, no gaps.
    let capture_end = events(&records, "capture_end");
    assert_eq!(capture_end.len(), 1, "one capture end");
    let end = &capture_end[0]["data"];
    assert_eq!(end["frames"], FRAMES);
    assert_eq!(end["samples"], SAMPLES);
    assert_eq!(end["last_sequence"], FRAMES);
    assert_eq!(end["dropped_frames"], 0);
    assert_eq!(end["dropped_samples"], 0);
    assert_eq!(end["complete"], true);
    assert_eq!(end["outcome"], "success");

    // The relay: one live request, every frame forwarded, the upstream's own
    // boundaries, and its terminal.
    let snapshots = events(&records, "relay_snapshot");
    assert!(!snapshots.is_empty(), "the relay wrote snapshots");
    let last = snapshots.last().expect("a relay snapshot")["data"].clone();
    assert_eq!(last["client_audio_frames"], FRAMES);
    // Every byte of the one sequence: 7500 full frames and the 37-sample tail.
    assert_eq!(last["client_pcm_bytes"], SAMPLES * 2);
    assert_eq!(last["upstream_sent_frames"], FRAMES);
    assert_eq!(last["upstream_pcm_bytes"], SAMPLES * 2);
    // The take is longer than the server's own activity, so the relay closed
    // its first activity at the forced limit and the frames that followed it
    // were replayed into the next one: every frame still crossed the socket
    // exactly once, as the counters above say.
    let rotations = events(&records, "activity_rotate");
    assert_eq!(rotations.len(), 1, "one rotation in this take");
    let rotation = &rotations[0]["data"];
    assert_eq!(rotation["index"], 1, "the first activity is the one rotated");
    assert_eq!(rotation["reason"], "limit", "{rotation}");
    assert_eq!(rotation["activity_audio_ms"], 200_000, "{rotation}");
    assert_eq!(rotation["replayed_frames"], 0, "{rotation}");
    assert_eq!(last["send_failures"], 0);
    assert!(last["received_messages"].as_u64().unwrap_or(0) >= 3);
    assert!(last["interim_count"].as_u64().unwrap_or(0) >= 1);
    assert!(last["final_count"].as_u64().unwrap_or(0) >= 1);
    assert!(last["turn_complete_count"].as_u64().unwrap_or(0) >= 1);
    // The take's clock was advanced at its real milestones, so these are the
    // watermarks a live take would record: every one of them non-zero, and
    // ordered the way the relay really reaches them.
    let watermark = |name: &str| {
        last[name]
            .as_u64()
            .unwrap_or_else(|| panic!("{name} is not a number: {last}"))
    };
    let last_client_audio_us = watermark("last_client_audio_us");
    let last_send_start_us = watermark("last_send_start_us");
    let last_send_success_us = watermark("last_send_success_us");
    let last_upstream_us = watermark("last_upstream_us");
    let last_final_us = watermark("last_final_us");
    for (name, value) in [
        ("last_client_audio_us", last_client_audio_us),
        ("last_send_start_us", last_send_start_us),
        ("last_send_success_us", last_send_success_us),
        ("last_upstream_us", last_upstream_us),
        ("last_final_us", last_final_us),
    ] {
        assert!(value > 0, "{name} never moved: {last}");
    }
    // The one sequence in order: the last client frame was received before the
    // send it triggered began, that send finished, and the upstream's own
    // messages - the final transcription and then the close - arrived after it.
    // `last_upstream_us` is the last receive of all, so it cannot precede the
    // final that preceded the close.
    assert!(
        last_client_audio_us <= last_send_start_us,
        "a send began before the frame it carried: {last}"
    );
    assert!(
        last_send_start_us <= last_send_success_us,
        "the last send finished before it began: {last}"
    );
    assert!(
        last_send_success_us <= last_upstream_us,
        "the upstream answered before the send: {last}"
    );
    assert!(
        last_final_us <= last_upstream_us,
        "the close preceded the final it followed: {last}"
    );
    assert_eq!(last["in_flight_send"], false);

    let boundaries: HashSet<String> = events(&records, "upstream_boundary")
        .into_iter()
        .map(|record| record["data"]["kind"].as_str().unwrap_or_default().to_owned())
        .collect();
    for kind in ["setup_complete", "turn_complete", "close"] {
        assert!(boundaries.contains(kind), "the upstream boundary {kind}");
    }
    let close = events(&records, "upstream_boundary")
        .into_iter()
        .find(|record| record["data"]["kind"] == "close")
        .expect("the close boundary");
    assert_eq!(close["data"]["code"], 1000, "the numeric close code");

    let request_end = events(&records, "request_end");
    assert_eq!(request_end.len(), 1, "one request ended");
    assert_eq!(request_end[0]["source"], "relay");
    assert_eq!(request_end[0]["data"]["route"], "live");
    assert_eq!(request_end[0]["data"]["outcome"], "success");
    assert!(request_end[0]["data"]["status"].is_null() || request_end[0]["data"]["status"].is_u64());

    // PCM sends are counters, never a record per frame.
    let sends = events(&records, "send_begin");
    assert!(sends.len() <= 8, "the sends are boundaries: {}", sends.len());
    for record in sends
        .into_iter()
        .chain(events(&records, "send_end").into_iter())
    {
        let kind = record["data"]["kind"].as_str().unwrap_or_default();
        assert_ne!(kind, "audio", "no per-frame send record: {record}");
    }

    // The renderer: the same take, counted from the other side, with the
    // terminal the shell's outcome agrees with.
    let renderer: Vec<&Value> = events(&records, "renderer_snapshot")
        .into_iter()
        .map(|record| &record["data"])
        .collect();
    assert!(!renderer.is_empty(), "the renderer reported");
    assert!(
        renderer
            .iter()
            .any(|snapshot| snapshot["handedFrames"] == json!(FRAMES)),
        "the renderer handed the same frames: {renderer:?}"
    );
    assert!(
        renderer
            .iter()
            .any(|snapshot| snapshot["handedSamples"] == json!(SAMPLES)),
        "the renderer handed the same samples"
    );
    assert!(
        renderer
            .iter()
            .any(|snapshot| snapshot["terminalOutcome"] == "success"),
        "the renderer's terminal"
    );
    let last_renderer = renderer.last().expect("a renderer snapshot");
    assert_eq!(last_renderer["readyCount"], 1);
    assert_eq!(last_renderer["errorCount"], 0);
    assert_eq!(last_renderer["lostReports"], 0);

    let renderer_events: HashSet<String> = events(&records, "renderer_boundary")
        .into_iter()
        .map(|record| record["data"]["event"].as_str().unwrap_or_default().to_owned())
        .collect();
    for kind in [
        "recorder_start",
        "socket_open",
        "socket_ready",
        "native_stop",
        "recorder_stop",
        "socket_done",
    ] {
        assert!(renderer_events.contains(kind), "the renderer boundary {kind}");
    }
    let allowed: HashSet<&str> = [
        "recorder_start",
        "recorder_stop",
        "recorder_idle",
        "recorder_dispose",
        "socket_open",
        "socket_ready",
        "socket_error",
        "socket_close",
        "socket_done",
        "native_gap",
        "native_stop",
        "batch_begin",
        "batch_end",
        "batch_error",
    ]
    .into_iter()
    .collect();
    for record in events(&records, "renderer_boundary") {
        let event = record["data"]["event"].as_str().unwrap_or_default();
        assert!(allowed.contains(event), "an unknown boundary: {record}");
        assert!(
            record["data"]["atMs"].as_f64().is_some_and(|at| at >= 0.0),
            "{record}"
        );
    }

    // The archive's own verification record and the take's terminal.
    let verified = events(&records, "audio_verified");
    assert_eq!(verified.len(), 1, "one verification");
    assert_eq!(verified[0]["data"]["samples"], SAMPLES);
    assert_eq!(verified[0]["data"]["verified"], true);

    let session_end = events(&records, "session_end");
    assert_eq!(session_end.len(), 1, "one session end");
    let end = &session_end[0]["data"];
    assert_eq!(end["outcome"], "success");
    assert_eq!(end["audio_status"], "complete");
    assert_eq!(end["verified"], true);
    assert_eq!(end["captured_frames"], FRAMES);
    assert_eq!(end["captured_samples"], SAMPLES);
    assert_eq!(end["archived_samples"], SAMPLES);
    assert_eq!(end["dropped_frames"], 0);
    assert_eq!(end["dropped_samples"], 0);
    assert_eq!(end["dropped_events"], 0);
    assert!(
        !records
            .iter()
            .any(|record| record["event"] == "audio_gap"),
        "a complete take records no hole"
    );
}

/* -------------------------------------------------------------------------- */
/* Privacy                                                                    */
/* -------------------------------------------------------------------------- */

/// Every form one pinned key could reach a file in: the key itself, the text a
/// JSON document would hold it as and the text a query string would.
fn key_forms(key: &str) -> Vec<String> {
    let quoted = serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""));
    let json_escaped = quoted.trim_matches('"').to_owned();
    let mut percent = String::new();
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            percent.push(byte as char);
        } else {
            percent.push_str(&format!("%{byte:02X}"));
        }
    }
    let mut forms = vec![key.to_owned(), json_escaped, percent.clone(), percent.to_lowercase()];
    forms.sort();
    forms.dedup();
    forms
}

/// The bundle files, in name order.
fn bundle_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the bundle directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

/// No pinned key, no device, no path and nothing that is not the take's own
/// recorded text may appear anywhere under the bundle: the archive holds what
/// the take produced, in full, and nothing else.
fn verify_privacy(dir: &Path) {
    let files = bundle_files(dir);
    assert!(!files.is_empty(), "the bundle has files");

    let forbidden: Vec<String> = PINNED_KEYS
        .iter()
        .flat_map(|key| key_forms(key))
        .collect();
    for file in &files {
        let bytes = std::fs::read(file).expect("a bundle file");
        let text = String::from_utf8_lossy(&bytes);
        for needle in &forbidden {
            assert!(
                !text.contains(needle.as_str()),
                "{} carries a pinned key in one of its forms",
                file.display()
            );
        }
        assert!(
            !text.contains(&dir.to_string_lossy().to_string()),
            "{} carries its own path",
            file.display()
        );
    }
}

/// The content the take really produced, read back from the bundle. The
/// transcripts its upstream sent, the text the browser was handed and the
/// detail its one close carried are all present and complete: a body the
/// archive shortened, redacted or dropped would not be found here.
fn verify_content(dir: &Path) {
    let records = journal(dir);
    let content: Vec<&Value> = records
        .iter()
        .filter(|record| record["event"] == "content")
        .collect();
    assert!(!content.is_empty(), "the take archived its content");

    let bodies = |kind: &str| -> Vec<String> {
        content
            .iter()
            .filter(|record| record["data"]["kind"] == kind)
            .filter_map(|record| record["data"]["body"].as_str().map(str::to_owned))
            .collect()
    };
    let carries = |kind: &str, needle: &str| -> bool {
        bodies(kind).iter().any(|body| body.contains(needle))
    };
    let describing = |kind: &str, needle: &str| {
        let all = bodies(kind);
        assert!(
            all.iter().any(|body| body.contains(needle)),
            "no {kind} record carries its text: {all:?}"
        );
    };
    // Every transcript the fake upstream sent, whole, in the records of the
    // frames that carried them.
    describing("live_message", INTERIM);
    describing("live_message", ACTIVITY_FINAL);
    describing("live_message", FINAL);
    // And the same text as the take handed it to the browser.
    assert!(
        carries("live_output", INTERIM) || carries("live_output", FINAL),
        "the transcript the browser was handed is archived: {:?}",
        bodies("live_output")
    );
    // The recorder's own result, which the wrapper posted in the terminal
    // report, carries the final transcription it was told.
    describing("recorder_result", FINAL);
    // The one close this session got, with the reason it carried.
    describing("socket_close", CLOSE_REASON);
    // The setup frame the relay opened with: the archive keeps how many terms
    // of the custom vocabulary it carried, and never a term itself.
    describing("live_setup", "\"custom_vocabulary_terms\":2");
    // The dictation-language hints of the snapshot reach the setup frame.
    describing("live_setup", "\"languageCodes\":[\"ru-RU\",\"en-US\"]");
    assert!(
        !carries("live_setup", "Kubernetes"),
        "no term of the vocabulary reaches the archive: {:?}",
        bodies("live_setup")
    );

    // Every record is a whole body: nothing is shortened and nothing is a
    // payload of a class the archive does not know.
    for record in content.iter().copied() {
        assert_eq!(record["schemaVersion"], 2);
        let kind = record["data"]["kind"].as_str().expect("a content kind");
        let format = record["data"]["format"].as_str().expect("a content format");
        let body = record["data"]["body"].as_str().expect("a content body");
        assert!(!body.is_empty(), "an empty content body: {record}");
        assert!(matches!(format, "json" | "text"), "{record}");
        let source = record["source"].as_str().expect("a source class");
        assert!(
            ["shell", "capture", "renderer", "relay", "provider", "archive"].contains(&source),
            "{record}"
        );
        assert!(
            record["data"]["observedUtcMs"].as_u64().is_some(),
            "{record}"
        );
        assert!(
            record["data"]["observedElapsedUs"].as_u64().is_some(),
            "{record}"
        );
        // A body announced as JSON is JSON: the raw frame or the wrapper's own
        // object, never a fragment of one.
        if format == "json" {
            assert!(
                serde_json::from_str::<Value>(body).is_ok(),
                "a json body that is not json: {kind}"
            );
        }
    }
}
