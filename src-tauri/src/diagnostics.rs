//! Development-only local diagnostics for a take: technical events, counters
//! and the lossless audio archive of the native PCM16 16 kHz mono stream.
//!
//! The whole module exists only in a debug build with the `dev-diagnostics`
//! feature (see the `mod diagnostics` gate and the `compile_error!` in
//! `lib.rs`), so a released executable cannot contain a writer or an asset for
//! this. The FLAC codec the archive writes with is [`crate::flac`], which every
//! build carries.
//!
//! What lives in this file is the producer half:
//!
//! * the identity and vocabulary of a take ([`Owner`], [`SessionKey`],
//!   [`Mode`], [`Outcome`], [`AudioStatus`]) and the fixed event record
//!   ([`Event`]) that the journal is written from,
//! * the counters a producer updates without a lock on its hot path
//!   ([`CaptureCounters`], [`RelayCounters`]): a take's capture counters and
//!   each request's own relay counters,
//! * the registry of open takes ([`Diagnostics`]) with its per-session
//!   admission gate, the ranked terminal outcome, the sealing rule and the
//!   idle fallback,
//! * the three producer handles: [`SessionHandle`], [`CaptureArchive`] and
//!   [`RequestTrace`].
//!
//! The storage half lives in the [`storage`] child module: one named writer
//! thread owns every file under `<config dir>/logs`, the session directories,
//! the rotated journal, the exact length ledger with its per-take finalization
//! reserve, retention, startup recovery and the FLAC encode/verify. The
//! producer side never touches a file: it queues a number or a record and the
//! worker writes it.
//!
//! Counter events remain a closed numeric vocabulary. Development content
//! records also preserve full provider, recorder and paste evidence, redacted
//! at the boundary before it enters this same bounded writer.
mod storage;
mod content;
pub(crate) mod redaction;
pub use content::{ContentFormat, ContentKind, ContentRecord};
pub(crate) use content::{QueuedContent, CONTENT_MAX_BYTES};
use content::{ContentBudget, ContentLease};


#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::flac;
use crate::i18n::MessageId;

/// Audio packets a running take may have in flight before its producer starts
/// counting drops. Sizing matches the pool: a full pool is a full queue.
const AUDIO_QUEUE: usize = 256;
/// Metadata records (events and accepted renderer reports) shared by every
/// open take.
const META_QUEUE: usize = 128;
/// Buffers preallocated per take: one PCM16 frame is 512 samples, 1024 bytes.
const POOL_BUFFERS: usize = 256;
const POOL_CAPACITY: usize = 1024;
/// The largest take count the registry keeps open. Past this, a new take is
/// refused with [`DiagnosticCode::SessionLimit`] and nothing else changes.
const MAX_OPEN_SESSIONS: usize = 64;
/// A renderer report may carry at most this many boundary events.
pub(crate) const RENDERER_EVENTS_MAX: usize = 16;
/// Serialized size of one accepted renderer report.
pub(crate) const RENDERER_REPORT_MAX_BYTES: usize = 4096;
/// Full recorder results share the route but can exceed the technical envelope.
pub(crate) const RENDERER_CONTENT_MAX_BYTES: usize = 1024 * 1024;
/// How long a sealed candidate waits for the last request to end.
const SEAL_GRACE: Duration = Duration::from_secs(2);
/// How long a take whose capture has ended, with no live request, waits for a
/// terminal report before it is closed as abandoned.
const IDLE_DEADLINE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Which window a take belongs to. The native capture keeps the same two
/// kinds, but its generations are its own: a generation without an owner is
/// never a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    Dictation,
    Lab,
}

impl Owner {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dictation => "dictation",
            Self::Lab => "lab",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "dictation" => Some(Self::Dictation),
            "lab" => Some(Self::Lab),
            _ => None,
        }
    }
}

/// One take, as the diagnostics name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub owner: Owner,
    pub generation: u64,
}

impl SessionKey {

    /// Parses one canonical tag: a known owner, a colon and a positive decimal
    /// generation with no sign, no padding and a safe-integer bound. Anything
    /// else is not a tag, and a request that carries one is served as usual
    /// with no trace attached.
    pub fn parse_tag(text: &str) -> Option<Self> {
        let (owner, generation) = text.split_once(':')?;
        Some(Self {
            owner: Owner::from_tag(owner)?,
            generation: parse_generation(generation)?,
        })
    }
}

fn parse_generation(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 19 {
        return None;
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    let generation: u64 = text.parse().ok()?;
    if generation == 0 || generation > 9_007_199_254_740_991 {
        return None;
    }
    Some(generation)
}

/// How a take was recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Live,
    Smart,
    Verbatim,
    /// The laboratory window's side-by-side comparison of the three modes.
    Comparison,
}

/// How a take ended. The variants are ranked, and a terminal outcome may only
/// rise: [`Outcome::Abandoned`] is the fallback for a take nobody closed, and
/// every known outcome outranks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Abandoned,
    Success,
    Error,
    WindowClosed,
    Cancelled,
    Exit,
}

impl Outcome {
    fn rank(self) -> u8 {
        match self {
            Self::Abandoned => 0,
            Self::Success => 1,
            Self::Error => 2,
            Self::WindowClosed | Self::Cancelled => 3,
            Self::Exit => 4,
        }
    }

    /// The outcome a take keeps when two terminals are reported. `Cancelled`
    /// wins its tie with `WindowClosed`; anything lower never displaces
    /// anything higher.
    fn preferred(current: Option<Self>, next: Self) -> Self {
        match current {
            None => next,
            Some(current) => {
                if next.rank() > current.rank() {
                    next
                } else if next == current {
                    current
                } else if next == Self::Cancelled && current == Self::WindowClosed {
                    Self::Cancelled
                } else {
                    current
                }
            }
        }
    }
}

/// How far the audio archive of a take got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioStatus {
    /// Nothing was captured: metadata only, no audio file.
    None,
    Recording,
    /// The writer is encoding, verifying or publishing the archive.
    Verifying,
    /// Every captured sample is in a verified file.
    Complete,
    /// A verified file holds the surviving subset; the gaps are recorded.
    Incomplete,
    /// The archive failed; a `.part` file may be left behind, never a
    /// verified one.
    Error,
}

const AUDIO_STATUS_NONE: u8 = 0;
const AUDIO_STATUS_RECORDING: u8 = 1;
const AUDIO_STATUS_VERIFYING: u8 = 2;
const AUDIO_STATUS_COMPLETE: u8 = 3;
const AUDIO_STATUS_INCOMPLETE: u8 = 4;
const AUDIO_STATUS_ERROR: u8 = 5;

impl AudioStatus {
    pub fn as_byte(self) -> u8 {
        match self {
            Self::None => AUDIO_STATUS_NONE,
            Self::Recording => AUDIO_STATUS_RECORDING,
            Self::Verifying => AUDIO_STATUS_VERIFYING,
            Self::Complete => AUDIO_STATUS_COMPLETE,
            Self::Incomplete => AUDIO_STATUS_INCOMPLETE,
            Self::Error => AUDIO_STATUS_ERROR,
        }
    }

    fn from_byte(byte: u8) -> Self {
        match byte {
            AUDIO_STATUS_RECORDING => Self::Recording,
            AUDIO_STATUS_VERIFYING => Self::Verifying,
            AUDIO_STATUS_COMPLETE => Self::Complete,
            AUDIO_STATUS_INCOMPLETE => Self::Incomplete,
            AUDIO_STATUS_ERROR => Self::Error,
            _ => Self::None,
        }
    }
}

/// The route a request was served on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Live,
    Smart,
    Verbatim,
}

/// What one upstream send carried. Only non-audio sends reach the journal per
/// send; PCM sends stay in the counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendKind {
    Audio,
    Setup,
    ActivityStart,
    ActivityEnd,
}

/// One batch (upload) stage of a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    UploadStart,
    UploadFinalize,
    Interaction,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StagePhase {
    Begin,
    End,
    Error,
}

/// The classes of upstream message one receive step may carry. A composite
/// `serverContent` sets every flag it contains.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiveFlags {
    pub setup_complete: bool,
    pub interim: bool,
    pub r#final: bool,
    pub turn_complete: bool,
    pub go_away: bool,
    pub voice_activity: bool,
    pub usage_metadata: bool,
    pub ping: bool,
    pub pong: bool,
    pub close: bool,
    pub error: bool,
    pub other: bool,
}

impl ReceiveFlags {
    fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// The real reason a request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestCause {
    ClientEnd,
    ClientCancel,
    ReadyTimeout,
    ActivityIncomplete,
    RotateTimeout,
    FinalReceived,
    IdleTimeout,
    HardDeadline,
    SettleTimeout,
    FinalTimeout,
    UpstreamClose,
    UpstreamError,
    SetupFailed,
    MissingTerminal,
    Shutdown,
    Unknown,
}

/// A transport failure, classified without reading its rendered text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Timeout,
    Connect,
    Body,
    Decode,
    Other,
}

/// The only codes an operator may be told about. They are memory-resident as
/// well, so a failure whose own record cannot be written is still classifiable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    Quota,
    Io,
    Acl,
    Codec,
    Decode,
    HashMismatch,
    QueueFull,
    SessionLimit,
    ForeignArchive,
    InvalidReport,
    MissingTerminal,
    Unknown,
}

impl From<flac::FlacError> for DiagnosticCode {
    fn from(error: flac::FlacError) -> Self {
        match error {
            flac::FlacError::Codec => Self::Codec,
            flac::FlacError::Decode => Self::Decode,
            flac::FlacError::HashMismatch => Self::HashMismatch,
        }
    }
}

impl DiagnosticCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::Io => "io",
            Self::Acl => "acl",
            Self::Codec => "codec",
            Self::Decode => "decode",
            Self::HashMismatch => "hash_mismatch",
            Self::QueueFull => "queue_full",
            Self::SessionLimit => "session_limit",
            Self::ForeignArchive => "foreign_archive",
            Self::InvalidReport => "invalid_report",
            Self::MissingTerminal => "missing_terminal",
            Self::Unknown => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// The event vocabulary
// ---------------------------------------------------------------------------

/// The phases of a dictation the shell reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturePhase {
    Idle,
    Arming,
    Recording,
    Finalizing,
    Pasting,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Phase {
    pub phase: CapturePhase,
}

/// One attempt at opening a device, taken before the take is installed.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct CaptureAttempt {
    pub attempt_index: u32,
    pub source_rate: u32,
    pub source_channels: u16,
    pub fallback: bool,
    /// The reason of a failed attempt; `None` for the attempt that recorded.
    pub ui_key: Option<MessageId>,
    pub open_us: u64,
    pub play_us: u64,
    pub first_frame_us: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct CaptureStarted {
    pub source_rate: u32,
    pub source_channels: u16,
    pub open_us: u64,
    pub play_us: u64,
    pub first_frame_us: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct CaptureSnapshot {
    pub produced_frames: u64,
    pub produced_samples: u64,
    pub produced_bytes: u64,
    pub emit_failed_frames: u64,
    pub last_sequence: Option<u64>,
    pub dropped_frames: u64,
    pub dropped_samples: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct CaptureEnd {
    pub last_sequence: Option<u64>,
    pub frames: u64,
    pub samples: u64,
    pub dropped_frames: u64,
    pub dropped_samples: u64,
    /// Every captured frame reached the archive.
    pub complete: bool,
    pub outcome: Outcome,
}

/// The relay counters, written once a second and at every boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RelayCounters {
    pub client_audio_frames: u64,
    pub client_pcm_bytes: u64,
    pub upstream_sent_frames: u64,
    pub upstream_pcm_bytes: u64,
    pub send_failures: u64,
    pub pre_ready_queue_depth: u64,
    pub in_flight_send: bool,
    pub pending_send_us: u64,
    pub max_send_us: u64,
    /// The roundtrip of the last successful PCM send: a special latency
    /// attribute of the audio path, distinct from the maxima above.
    pub last_audio_roundtrip_us: u64,
    pub received_messages: u64,
    pub interim_count: u64,
    pub final_count: u64,
    pub turn_complete_count: u64,
    pub last_client_audio_us: u64,
    pub last_send_start_us: u64,
    pub last_send_success_us: u64,
    pub last_upstream_us: u64,
    pub last_final_us: u64,
}

/// Which class of upstream boundary crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamKind {
    SetupComplete,
    TurnComplete,
    GoAway,
    Close,
    Error,
    Ping,
    Pong,
    Other,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct UpstreamBoundary {
    pub kind: UpstreamKind,
    /// Upstream status or close code when the boundary carried one.
    pub code: Option<u16>,
    /// `goAway.timeLeft` as a parsed duration, never a string.
    pub time_left_us: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RotateReason {
    Pause,
    Limit,
    ServerCut,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ActivityRotate {
    pub(crate) index: u32,
    pub(crate) reason: RotateReason,
    pub(crate) activity_audio_ms: u64,
    pub(crate) pause_ms: u32,
    pub(crate) wait_us: u64,
    pub(crate) held_frames: u32,
    pub(crate) replayed_frames: u32,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct SendBegin {
    pub kind: SendKind,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct SendEnd {
    pub kind: SendKind,
    pub pcm_bytes: u64,
    pub elapsed_us: u64,
    pub success: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct BatchStage {
    pub stage: Stage,
    pub phase: StagePhase,
    pub status: Option<u16>,
    pub timeout: bool,
    pub elapsed_us: Option<u64>,
    pub wav_bytes: Option<u64>,
    pub audio_duration_us: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineTimer {
    Ready,
    Final,
    Rotate,
    Settle,
    Hard,
    Idle,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Deadline {
    pub timer: DeadlineTimer,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RequestEnd {
    pub route: Route,
    pub outcome: Outcome,
    pub cause: RequestCause,
    pub elapsed_us: u64,
    pub status: Option<u16>,
    pub close_code: Option<u16>,
    pub error_class: Option<ErrorClass>,
}

/// The one hole an archive carried: the capture sequences it never held, the
/// samples it had committed when the hole opened and the samples the hole
/// cost. It is written both as a journal record and as the durable field of
/// the manifest, so it carries the derives of both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioGap {
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub archived_offset_samples: u64,
    pub lost_samples: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct AudioVerified {
    pub samples: u64,
    pub verified: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ArchiveError {
    pub code: DiagnosticCode,
}

/// The manifest clock produced a retention time in the future and it was
/// clamped back to the wall clock once.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ClockAdjusted {
    pub retention_utc_ms: u64,
    pub now_utc_ms: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct SessionBegin {
    pub mode: Mode,
    pub started_utc_ms: u64,
    pub retention_utc_ms: u64,
    pub pid: u32,
    pub run_counter: u64,
    pub session_counter: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct SessionEnd {
    pub outcome: Outcome,
    pub dropped_events: u64,
    pub audio_status: AudioStatus,
    pub captured_frames: u64,
    pub captured_samples: u64,
    pub archived_samples: u64,
    pub dropped_frames: u64,
    pub dropped_samples: u64,
    pub verified: bool,
}

/// One journal record. Serialized adjacently as `event` plus `data`, so a
/// consumer never has to guess a shape from a name.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    SessionBegin(SessionBegin),
    Phase(Phase),
    CaptureAttempt(CaptureAttempt),
    CaptureStarted(CaptureStarted),
    CaptureSnapshot(CaptureSnapshot),
    CaptureEnd(CaptureEnd),
    RendererSnapshot(RendererSnapshot),
    RendererBoundary(RendererBoundary),
    RelaySnapshot(RelayCounters),
    UpstreamBoundary(UpstreamBoundary),
    ActivityRotate(ActivityRotate),
    SendBegin(SendBegin),
    SendEnd(SendEnd),
    BatchStage(BatchStage),
    Deadline(Deadline),
    RequestEnd(RequestEnd),
    AudioGap(AudioGap),
    AudioVerified(AudioVerified),
    ArchiveError(ArchiveError),
    ClockAdjusted(ClockAdjusted),
    SessionEnd(SessionEnd),
}

// ---------------------------------------------------------------------------
// The reference clock
// ---------------------------------------------------------------------------

/// The clock every deadline and every manifest timestamp is read from, so a
/// test can drive them without sleeping.
pub(crate) trait Clock: Send + Sync {
    fn instant(&self) -> Instant;
    fn system(&self) -> SystemTime;
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    fn as_any(&self) -> &dyn Any;
}

struct RealClock;

impl Clock for RealClock {
    fn instant(&self) -> Instant {
        Instant::now()
    }

    fn system(&self) -> SystemTime {
        SystemTime::now()
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The quota a sink keeps its whole tree inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub max_age: Duration,
    pub max_bytes: u64,
}

impl Policy {
    /// The development sink keeps ten days and a hundred million bytes.
    pub const DEVELOPMENT: Self = Self {
        max_age: Duration::from_secs(86_400 * 10),
        max_bytes: 100_000_000,
    };
}

// ---------------------------------------------------------------------------
// Producer counters
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CaptureCounters {
    pub produced_frames: u64,
    pub produced_samples: u64,
    pub produced_bytes: u64,
    pub emit_failed_frames: u64,
    pub last_sequence: Option<u64>,
    pub dropped_frames: u64,
    pub dropped_samples: u64,
}

impl CaptureCounters {
    /// True when some produced frame never reached the archive.
    pub fn has_gaps(self) -> bool {
        self.dropped_frames > 0
    }
}

/// The counters a capture updates from its own thread: atomics only, so a
/// frame never waits for a lock.
#[derive(Default)]
struct CaptureAtomics {
    produced_frames: AtomicU64,
    produced_samples: AtomicU64,
    produced_bytes: AtomicU64,
    emit_failed_frames: AtomicU64,
    last_sequence: AtomicU64,
    dropped_frames: AtomicU64,
    dropped_samples: AtomicU64,
}

impl CaptureAtomics {
    fn snapshot(&self) -> CaptureCounters {
        let last = self.last_sequence.load(Ordering::Acquire);
        CaptureCounters {
            produced_frames: self.produced_frames.load(Ordering::Acquire),
            produced_samples: self.produced_samples.load(Ordering::Acquire),
            produced_bytes: self.produced_bytes.load(Ordering::Acquire),
            emit_failed_frames: self.emit_failed_frames.load(Ordering::Acquire),
            last_sequence: (last > 0).then_some(last),
            dropped_frames: self.dropped_frames.load(Ordering::Acquire),
            dropped_samples: self.dropped_samples.load(Ordering::Acquire),
        }
    }

    fn note_emit_failure(&self) {
        self.emit_failed_frames.fetch_add(1, Ordering::Relaxed);
    }

    fn note_produced(&self, sequence: u64, bytes: usize) {
        self.produced_frames.fetch_add(1, Ordering::Relaxed);
        self.produced_samples
            .fetch_add((bytes / 2) as u64, Ordering::Relaxed);
        self.produced_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.last_sequence.fetch_max(sequence, Ordering::Release);
    }

    fn note_dropped(&self, sequence: u64, bytes: usize) {
        self.dropped_frames.fetch_add(1, Ordering::Relaxed);
        self.dropped_samples
            .fetch_add((bytes / 2) as u64, Ordering::Relaxed);
        self.last_sequence.fetch_max(sequence, Ordering::Release);
    }
}

/// The counters one request updates per client frame and per upstream send.
/// They belong to the request, not to the take: a comparison runs three
/// requests at once and their in-flight send, their maxima and their last
/// timestamps are each their own. The relay's `send_finished_kind` passes the
/// kind in, so only an audio send adds PCM frames and bytes.
#[derive(Default)]
struct RelayAtomics {
    client_audio_frames: AtomicU64,
    client_pcm_bytes: AtomicU64,
    upstream_sent_frames: AtomicU64,
    upstream_pcm_bytes: AtomicU64,
    send_failures: AtomicU64,
    pre_ready_queue_depth: AtomicU64,
    in_flight_send: AtomicBool,
    send_start_us: AtomicU64,
    /// The roundtrip of the last successful PCM send: a special latency
    /// attribute of the audio path, distinct from the maxima above.
    last_audio_roundtrip_us: AtomicU64,
    max_send_us: AtomicU64,
    received_messages: AtomicU64,
    interim_count: AtomicU64,
    final_count: AtomicU64,
    turn_complete_count: AtomicU64,
    last_client_audio_us: AtomicU64,
    last_send_start_us: AtomicU64,
    last_send_success_us: AtomicU64,
    last_upstream_us: AtomicU64,
    last_final_us: AtomicU64,
}

impl RelayAtomics {
    /// `now_us` is the sink's monotonic clock, read at the moment of the
    /// snapshot: a send that never returns is aged by the reader, not by the
    /// sender.
    fn snapshot(&self, now_us: u64) -> RelayCounters {
        RelayCounters {
            client_audio_frames: self.client_audio_frames.load(Ordering::Relaxed),
            client_pcm_bytes: self.client_pcm_bytes.load(Ordering::Relaxed),
            upstream_sent_frames: self.upstream_sent_frames.load(Ordering::Relaxed),
            upstream_pcm_bytes: self.upstream_pcm_bytes.load(Ordering::Relaxed),
            send_failures: self.send_failures.load(Ordering::Relaxed),
            pre_ready_queue_depth: self.pre_ready_queue_depth.load(Ordering::Relaxed),
            in_flight_send: self.in_flight_send.load(Ordering::Relaxed),
            pending_send_us: self.pending_send_pending(now_us),
            max_send_us: self.max_send_us.load(Ordering::Relaxed),
            last_audio_roundtrip_us: self.last_audio_roundtrip_us.load(Ordering::Relaxed),
            received_messages: self.received_messages.load(Ordering::Relaxed),
            interim_count: self.interim_count.load(Ordering::Relaxed),
            final_count: self.final_count.load(Ordering::Relaxed),
            turn_complete_count: self.turn_complete_count.load(Ordering::Relaxed),
            last_client_audio_us: self.last_client_audio_us.load(Ordering::Relaxed),
            last_send_start_us: self.last_send_start_us.load(Ordering::Relaxed),
            last_send_success_us: self.last_send_success_us.load(Ordering::Relaxed),
            last_upstream_us: self.last_upstream_us.load(Ordering::Relaxed),
            last_final_us: self.last_final_us.load(Ordering::Relaxed),
        }
    }

    /// How long the send that is in flight right now has been waiting, as of
    /// this read: a send that is awaited and never returns ages with the clock
    /// instead of freezing at its start.
    fn pending_send_pending(&self, now_us: u64) -> u64 {
        if !self.in_flight_send.load(Ordering::Relaxed) {
            return 0;
        }
        now_us.saturating_sub(self.send_start_us.load(Ordering::Relaxed))
    }

    fn note_client_audio(&self, bytes: u64, at_us: u64) {
        self.client_audio_frames.fetch_add(1, Ordering::Relaxed);
        self.client_pcm_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.last_client_audio_us.store(at_us, Ordering::Relaxed);
    }

    fn note_pre_ready_queue(&self, depth: u64) {
        self.pre_ready_queue_depth.store(depth, Ordering::Relaxed);
    }

    /// The send is marked in flight before it is awaited, so an independent
    /// reader sees a send that never returns.
    fn note_send_start(&self, at_us: u64) {
        self.send_start_us.store(at_us, Ordering::Relaxed);
        self.last_send_start_us.store(at_us, Ordering::Relaxed);
        self.in_flight_send.store(true, Ordering::Release);
    }

    /// Only an audio send counts frame and byte totals: a successful setup or
    /// activity message is a send that succeeded, not a frame that carried PCM.
    fn note_send_end(
        &self,
        kind: SendKind,
        pcm_bytes: u64,
        elapsed_us: u64,
        success: bool,
        at_us: u64,
    ) {
        self.in_flight_send.store(false, Ordering::Release);
        self.send_start_us.store(0, Ordering::Relaxed);
        self.max_send_us.fetch_max(elapsed_us, Ordering::Relaxed);
        if !success {
            self.send_failures.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.last_send_success_us.store(at_us, Ordering::Relaxed);
        if matches!(kind, SendKind::Audio) {
            self.upstream_sent_frames.fetch_add(1, Ordering::Relaxed);
            self.upstream_pcm_bytes
                .fetch_add(pcm_bytes, Ordering::Relaxed);
            self.last_audio_roundtrip_us
                .store(elapsed_us, Ordering::Relaxed);
        }
    }


    fn note_received(&self, flags: ReceiveFlags, at_us: u64) {
        self.received_messages.fetch_add(1, Ordering::Relaxed);
        self.last_upstream_us.store(at_us, Ordering::Relaxed);
        if flags.interim {
            self.interim_count.fetch_add(1, Ordering::Relaxed);
        }
        if flags.r#final {
            self.final_count.fetch_add(1, Ordering::Relaxed);
            self.last_final_us.store(at_us, Ordering::Relaxed);
        }
        if flags.turn_complete {
            self.turn_complete_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// The queues
// ---------------------------------------------------------------------------

/// One captured frame on its way to the writer: its 1-based sequence and the
/// PCM16 little-endian samples. The buffer is one of the pool's and travels
/// back to the capture through the free channel.
struct AudioPacket {
    sequence: u64,
    bytes: Vec<u8>,
}

/// A record the writer consumes. A renderer report is admitted whole, so the
/// queue never holds a report whose index was not committed.
#[derive(Clone, Debug)]
pub(crate) enum Meta {
    Event(Event),
    Renderer(RendererReport),
    Content(QueuedContent),
}

#[derive(Clone, Debug)]
pub(crate) struct PendingEvent {
    pub key: SessionKey,
    pub request_id: Option<u64>,
    /// The request's own counters, read at the moment the record was queued: a
    /// relay snapshot is written beside the record it belongs to, with the
    /// value the request had at that instant.
    pub relay: Option<RelayCounters>,
    pub meta: Meta,
}

/// The pool one take's frames are taken from, filled before the first frame.
fn fill_pool(free: &SyncSender<Vec<u8>>) {
    for _ in 0..POOL_BUFFERS {
        if free
            .try_send(Vec::with_capacity(POOL_CAPACITY))
            .is_err()
        {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// The session record
// ---------------------------------------------------------------------------

/// Shared take state. Redaction handles stay in memory; only sanitized evidence
/// reaches the bounded writer, which alone owns paths and files.
struct SessionEntry {
    key: SessionKey,
    mode: Mode,
    clock: Arc<dyn Clock>,
    started: Instant,
    started_utc_ms: u64,
    session_counter: u64,
    audio_tx: SyncSender<AudioPacket>,
    audio_rx: Mutex<Receiver<AudioPacket>>,
    free_tx: SyncSender<Vec<u8>>,
    capture_pool: Mutex<Option<Receiver<Vec<u8>>>>,
    meta_tx: SyncSender<PendingEvent>,
    content_budget: Arc<ContentBudget>,
    /// Borrowed secret handles from every request revision of this take; never serialized.
    redaction_keys: Mutex<Vec<Arc<crate::secrets::SecretKey>>>,
    capture: CaptureAtomics,
    /// This take's own failure class, readable even when its record cannot be
    /// written. The sink keeps its own copy for the operator.
    error: Mutex<Option<DiagnosticCode>>,
    /// The live requests of this take, by their session-monotonic id. The
    /// writer reads each request's own counters from here.
    requests: Mutex<HashMap<u64, Weak<RequestState>>>,
    next_request: AtomicU64,
    live_requests: AtomicU64,
    sealed: AtomicBool,
    /// The capture has published its last sequence: no new frame may enter the
    /// archive queue, whether or not the take is sealed yet.
    capture_closed: AtomicBool,
    /// The worker still accepts this take's records. It clears this when the
    /// archive fails, so a producer stops copying PCM into a queue nobody owns.
    archive_enabled: AtomicBool,
    /// The archive's progress: the capture writes the first frame, the worker
    /// everything after it.
    audio_status: AtomicU8,
    /// Some captured frame never reached the archive queue.
    audio_incomplete: AtomicBool,
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    /// How many frames the writer has taken out of this take's queue. It is the
    /// backpressure watermark a driver outside the capture reads: pushing more
    /// than the pool and the queue can hold would only count drops, so a
    /// fixture waits for this to catch up instead.
    audio_consumed: AtomicU64,
    /// The admission gate: sealing, the accepted report index and the terminal
    /// outcome all move under this one lock.
    admission: Mutex<Admission>,
}

struct Admission {
    sealed: bool,
    capture_finished: bool,
    terminal: Option<Outcome>,
    terminal_at: Option<Instant>,
    /// Set when the capture ended with no live request and no terminal: the
    /// point the idle deadline counts from.
    idle_since: Option<Instant>,
    accepted_report_index: u64,
    missing_terminal: bool,
    dropped_events: u64,
    /// Records of this take that are in the shared queue and not yet taken by
    /// the writer. The take is not done until this is zero again.
    pending_meta: u64,
}

impl SessionEntry {
    fn new(
        key: SessionKey,
        mode: Mode,
        clock: Arc<dyn Clock>,
        started_utc_ms: u64,
        session_counter: u64,
        meta_tx: SyncSender<PendingEvent>,
        content_budget: Arc<ContentBudget>,
    ) -> Self {
        let now = clock.instant();
        let (audio_tx, audio_rx) = sync_channel(AUDIO_QUEUE);
        let (free_tx, free_rx) = sync_channel(AUDIO_QUEUE);
        fill_pool(&free_tx);
        Self {
            key,
            mode,
            clock,
            started: now,
            started_utc_ms,
            session_counter,
            audio_tx,
            audio_rx: Mutex::new(audio_rx),
            free_tx,
            capture_pool: Mutex::new(Some(free_rx)),
            meta_tx,
            content_budget,
            redaction_keys: Mutex::new(Vec::new()),
            capture: CaptureAtomics::default(),
            error: Mutex::new(None),
            requests: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(0),
            live_requests: AtomicU64::new(0),
            sealed: AtomicBool::new(false),
            capture_closed: AtomicBool::new(false),
            archive_enabled: AtomicBool::new(true),
            audio_status: AtomicU8::new(AUDIO_STATUS_NONE),
            audio_incomplete: AtomicBool::new(false),
            #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
            audio_consumed: AtomicU64::new(0),
            admission: Mutex::new(Admission {
                sealed: false,
                capture_finished: false,
                terminal: None,
                terminal_at: None,
                idle_since: None,
                accepted_report_index: 0,
                missing_terminal: false,
                dropped_events: 0,
                pending_meta: 0,
            }),
        }
    }

    /// Microseconds since this take began, on the sink's clock.
    fn elapsed_us(&self) -> u64 {
        self.clock
            .instant()
            .saturating_duration_since(self.started)
            .as_micros() as u64
    }

    fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }

    /// The capture has published its watermark. A frame after this point is a
    /// gap, and the producer reads this without taking the admission gate.
    fn is_capture_closed(&self) -> bool {
        self.capture_closed.load(Ordering::Acquire)
    }

    /// Whether the writer still wants this take's records and PCM.
    fn archive_enabled(&self) -> bool {
        self.archive_enabled.load(Ordering::Acquire)
    }

    fn set_archive_enabled(&self, enabled: bool) {
        self.archive_enabled.store(enabled, Ordering::Release);
    }

    /// A record that was produced and could not be written down.
    fn note_dropped_event(&self) {
        self.admission.lock().dropped_events += 1;
    }

    /// Claims a place in the shared queue for one writer record. A sealed take
    /// admits nothing, and the claim is what keeps the take open until the
    /// writer has taken the record: the whole step happens under the gate the
    /// seal moves under, so a record can never slip in after the seal.
    fn note_meta_queued(&self) -> bool {
        let mut admission = self.admission.lock();
        if admission.sealed {
            return false;
        }
        admission.pending_meta += 1;
        true
    }

    /// Gives the claim back when the record never reached the queue.
    fn note_meta_unqueued(&self) {
        self.note_meta_drained();
    }

    /// One claim the writer has taken out of the shared queue.
    fn note_meta_drained(&self) {
        let mut admission = self.admission.lock();
        admission.pending_meta = admission.pending_meta.saturating_sub(1);
    }

    /// How many of this take's records the writer has not taken yet.
    fn meta_pending(&self) -> u64 {
        self.admission.lock().pending_meta
    }

    /// Hands the writer's record over. A record that does not fit is dropped;
    /// the caller counts it. Nothing here waits. The caller must not hold the
    /// admission gate: this takes it to claim the record's place.
    fn push_meta(&self, request_id: Option<u64>, meta: Meta) -> bool {
        if !self.note_meta_queued() {
            return false;
        }
        let pending = PendingEvent {
            key: self.key,
            request_id,
            relay: None,
            meta,
        };
        if self.meta_tx.try_send(pending).is_ok() {
            return true;
        }
        self.note_meta_unqueued();
        false
    }

    /// The same, carrying the request's own counters so the record is written
    /// without the writer having to read anything back.
    fn push_meta_relay(
        &self,
        request_id: Option<u64>,
        relay: RelayCounters,
        meta: Meta,
    ) -> bool {
        if !self.note_meta_queued() {
            return false;
        }
        let pending = PendingEvent {
            key: self.key,
            request_id,
            relay: Some(relay),
            meta,
        };
        if self.meta_tx.try_send(pending).is_ok() {
            return true;
        }
        self.note_meta_unqueued();
        false
    }
    /// The same, counting a record that could not be queued.
    fn push_event(&self, request_id: Option<u64>, event: Event) -> bool {
        if self.push_meta(request_id, Meta::Event(event)) {
            return true;
        }
        self.note_dropped_event();
        false
    }

    /// A request's last record and its counters, or nothing when the queue is
    /// full: the caller then reports what it produced on its own.
    fn push_event_relay(
        &self,
        request_id: Option<u64>,
        relay: RelayCounters,
        event: Event,
    ) -> bool {
        if self.push_meta_relay(request_id, relay, Meta::Event(event)) {
            return true;
        }
        self.note_dropped_event();
        false
    }

    fn push_content(
        &self,
        request_id: Option<u64>,
        route: Option<Route>,
        kind: ContentKind,
        format: ContentFormat,
        body: String,
    ) -> bool {
        if self.is_sealed() || !self.archive_enabled() {
            self.note_dropped_event();
            return false;
        }
        let Some(lease) = self.content_budget.claim(body.capacity() as u64) else {
            self.note_dropped_event();
            return false;
        };
        let queued = QueuedContent {
            record: ContentRecord {
                kind,
                route,
                format,
                body,
                observed_utc_ms: system_millis(self.clock.system()),
                observed_elapsed_us: self.elapsed_us(),
            },
            _lease: lease,
        };
        if self.push_meta(request_id, Meta::Content(queued)) {
            return true;
        }
        self.note_dropped_event();
        false
    }

    fn note_error_code(&self, code: DiagnosticCode) {
        *self.error.lock() = Some(code);
    }

    fn error_code(&self) -> Option<DiagnosticCode> {
        *self.error.lock()
    }

    /// Every live request's counters, oldest id first. A request that has been
    /// dropped is gone from the registry with it.
    #[allow(dead_code)] // The worker writes one relay snapshot per request.
    fn relay_snapshots(&self) -> Vec<(u64, RelayCounters)> {
        let now_us = self.elapsed_us();
        let mut live: Vec<(u64, Arc<RequestState>)> = self
            .requests
            .lock()
            .iter()
            .filter_map(|(id, weak)| weak.upgrade().map(|state| (*id, state)))
            .collect();
        live.sort_by_key(|(id, _)| *id);
        live.into_iter()
            .map(|(id, state)| (id, state.relay_snapshot(now_us)))
            .collect()
    }

    /// Takes one frame back for the writer, in capture order.
    #[allow(dead_code)] // The writer thread drains this; the tests drain it too.
    fn take_audio(&self, budget: usize) -> Vec<AudioPacket> {
        let receiver = self.audio_rx.lock();
        let mut taken = Vec::new();
        while taken.len() < budget {
            match receiver.try_recv() {
                Ok(packet) => taken.push(packet),
                Err(_) => break,
            }
        }
        #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
        {
            self.audio_consumed
                .fetch_add(taken.len() as u64, Ordering::Release);
        }
        taken
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    /// How many frames the writer has taken out of this take's queue so far.
    fn audio_consumed(&self) -> u64 {
        self.audio_consumed.load(Ordering::Acquire)
    }

    /// The writer's side of the pool: the buffer of a consumed frame goes back
    /// to the capture. A caller that cannot hand it over drops it, which only
    /// costs one allocation later.
    #[allow(dead_code)] // Used by the writer thread.
    fn return_buffer(&self, bytes: Vec<u8>) {
        let _ = self.free_tx.try_send(bytes);
    }

    /// Records one captured frame, whether or not its buffer could be queued.
    /// The frame was produced either way; only a frame the archive never
    /// received is a gap. One captured frame is also what makes the take a
    /// recording take: nothing else may report it, because nothing else knows
    /// whether a sample ever arrived.
    fn note_packet(&self, sequence: u64, bytes: usize, queued: bool) {
        self.capture.note_produced(sequence, bytes);
        // Read first and write only from `None`: the writer's own verdict — a
        // verified file, a failure — is never overwritten here.
        if self.audio_status.load(Ordering::Relaxed) == AUDIO_STATUS_NONE {
            self.audio_status
                .store(AUDIO_STATUS_RECORDING, Ordering::Relaxed);
        }
        if queued {
            return;
        }
        self.capture.note_dropped(sequence, bytes);
        self.audio_incomplete.store(true, Ordering::Relaxed);
    }

    fn mark_capture_incomplete(&self) {
        self.audio_incomplete.store(true, Ordering::Release);
        for current in [AUDIO_STATUS_RECORDING, AUDIO_STATUS_VERIFYING] {
            let _ = self.audio_status.compare_exchange(
                current, AUDIO_STATUS_INCOMPLETE, Ordering::AcqRel, Ordering::Acquire,
            );
        }
    }

    /// The capture is over. The watermark is published once, the archive queue
    /// is closed to new frames at once, and a capture that failed is terminal
    /// on its own.
    fn note_capture_end(&self, last_sequence: Option<u64>, outcome: Outcome, incomplete: bool) {
        let now = self.clock.instant();
        let mut admission = self.admission.lock();
        if admission.capture_finished {
            return;
        }
        if incomplete {
            self.mark_capture_incomplete();
        }
        admission.capture_finished = true;
        if let Some(sequence) = last_sequence {
            self.capture
                .last_sequence
                .fetch_max(sequence, Ordering::Release);
        }
        match outcome {
            Outcome::Error | Outcome::Exit => {
                admission.terminal = Some(Outcome::preferred(admission.terminal, outcome));
                admission.terminal_at = Some(now);
            }
            _ => {}
        }
        if admission.terminal.is_none() && self.live_requests.load(Ordering::Acquire) == 0 {
            admission.idle_since = Some(now);
        }
        self.capture_closed.store(true, Ordering::Release);
        drop(admission);
        let counters = self.capture.snapshot();
        self.push_event(
            None,
            Event::CaptureEnd(CaptureEnd {
                last_sequence: counters.last_sequence,
                frames: counters.produced_frames,
                samples: counters.produced_samples,
                dropped_frames: counters.dropped_frames,
                dropped_samples: counters.dropped_samples,
                complete: !self.audio_incomplete() && !counters.has_gaps(),
                outcome,
            }),
        );
    }

    /// Raises the terminal outcome while the take is unsealed. A take that is
    /// already sealed keeps its history.
    fn raise_terminal(&self, outcome: Outcome, now: Instant) -> bool {
        let mut admission = self.admission.lock();
        if admission.sealed {
            return false;
        }
        let preferred = Outcome::preferred(admission.terminal, outcome);
        if admission.terminal == Some(preferred) {
            return admission.terminal.is_some();
        }
        admission.terminal = Some(preferred);
        admission.terminal_at = Some(now);
        true
    }

    /// Closes the take at once, without the seal grace: the process is going
    /// away, so no later frame and no later report may open it again. The
    /// writer keeps its handle and still gets to flush what it holds — a seal
    /// is never a destruction. A take that is already sealed keeps its
    /// history.
    fn seal_now(&self, terminal: Option<Outcome>) {
        let now = self.clock.instant();
        let mut admission = self.admission.lock();
        if admission.sealed {
            return;
        }
        if let Some(terminal) = terminal {
            admission.terminal = Some(Outcome::preferred(admission.terminal, terminal));
            admission.terminal_at = Some(now);
        }
        if !admission.capture_finished {
            self.mark_capture_incomplete();
            self.capture_closed.store(true, Ordering::Release);
        }
        admission.capture_finished = true;
        admission.sealed = true;
        self.sealed.store(true, Ordering::Release);
    }

    /// One admission step: seals a finished take whose grace has run out, and
    /// closes a take that never heard a terminal.
    fn advance(&self, now: Instant) {
        let mut admission = self.admission.lock();
        if admission.sealed {
            return;
        }
        // Read under the gate: a request takes its slot under this same lock,
        // so a slot cannot appear between this read and the seal.
        let requests = self.live_requests.load(Ordering::Acquire);
        if admission.capture_finished && requests == 0 {
            match (admission.terminal, admission.idle_since) {
                (None, None) => admission.idle_since = Some(now),
                (None, Some(since)) if now.saturating_duration_since(since) >= IDLE_DEADLINE => {
                    admission.terminal = Some(Outcome::Abandoned);
                    admission.terminal_at = Some(now);
                    admission.missing_terminal = true;
                    admission.idle_since = Some(since);
                }
                _ => {}
            }
        } else {
            admission.idle_since = None;
        }
        let due = admission.terminal.is_some()
            && admission.capture_finished
            && requests == 0
            && admission
                .terminal_at
                .map(|at| now.saturating_duration_since(at) >= SEAL_GRACE)
                .unwrap_or(false);
        if due {
            admission.sealed = true;
            self.sealed.store(true, Ordering::Release);
        }
    }

    fn terminal(&self) -> Option<Outcome> {
        self.admission.lock().terminal
    }

    fn missing_terminal(&self) -> bool {
        self.admission.lock().missing_terminal
    }

    fn audio_status(&self) -> AudioStatus {
        AudioStatus::from_byte(self.audio_status.load(Ordering::Relaxed))
    }

    /// The writer's record of the archive's progress.
    fn set_audio_status(&self, status: AudioStatus) {
        self.audio_status.store(status.as_byte(), Ordering::Relaxed);
    }

    fn dropped_events(&self) -> u64 {
        self.admission.lock().dropped_events
    }

    /// Whether some captured frame never reached the archive.
    fn audio_incomplete(&self) -> bool {
        self.audio_incomplete.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// The handles
// ---------------------------------------------------------------------------

/// The shell's handle on one open take. Cloneable, and free of paths and
/// secrets: it holds the registry entry and nothing else.
#[derive(Clone)]
pub struct SessionHandle {
    entry: Arc<SessionEntry>,
}

impl SessionHandle {
    /// Lab requests may use different revisions; keep their redactors until sealing.
    pub(crate) fn remember_keys(&self, keys: &[Arc<crate::secrets::SecretKey>]) {
        let mut known = self.entry.redaction_keys.lock();
        for key in keys {
            if !known.iter().any(|old| Arc::ptr_eq(old, key)) {
                known.push(Arc::clone(key));
            }
        }
    }

    pub(crate) fn redact_renderer_content(&self, report: &mut RendererReport) {
        let keys = self.entry.redaction_keys.lock();
        if let Some(result) = report.result.as_mut() {
            crate::backend::diagnostic_redact_value(result, &keys);
        }
        for error in &mut report.errors {
            crate::backend::diagnostic_redact_value(error, &keys);
        }
    }

    /// Native producers use the same take's remembered request revisions.
    pub fn redacted_content_json(&self, kind: ContentKind, body: &serde_json::Value) -> bool {
        let rendered = {
            let keys = self.entry.redaction_keys.lock();
            crate::backend::diagnostic_json(body, &keys)
        };
        self.entry.push_content(None, None, kind, ContentFormat::Json, rendered)
    }

    /// The caller must redact credentials using the take's pinned boundary.
    pub fn content(&self, kind: ContentKind, format: ContentFormat, body: String) -> bool {
        self.entry.push_content(None, None, kind, format, body)
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub fn key(&self) -> SessionKey {
        self.entry.key
    }


    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn audio_consumed(&self) -> u64 {
        self.entry.audio_consumed()
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn capture_counters(&self) -> CaptureCounters {
        self.entry.capture.snapshot()
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn audio_status(&self) -> AudioStatus {
        AudioStatus::from_byte(self.entry.audio_status.load(Ordering::Acquire))
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn capture_finished(&self) -> bool {
        self.entry.admission.lock().capture_finished
    }

    /// The archive of this take's PCM. The capture takes it once, on its first
    /// frame; a second archive of the same take has no pool and counts what it
    /// is given as dropped, which the counters show.
    pub fn capture(&self) -> CaptureArchive {
        let pool = self.entry.capture_pool.lock().take();
        if !self.entry.admission.lock().capture_finished
            && self.entry.audio_status.load(Ordering::Relaxed) == AUDIO_STATUS_NONE
        {
            self.entry
                .audio_status
                .store(AUDIO_STATUS_RECORDING, Ordering::Relaxed);
        }
        CaptureArchive {
            entry: self.entry.clone(),
            pool,
            finished: false,
        }
    }

    /// Takes a request slot for a route. `None` means the take is sealed (or
    /// has no archive left to write), and the caller instruments nothing.
    pub fn request(&self, route: Route) -> Option<RequestTrace> {
        let id = {
            let mut admission = self.entry.admission.lock();
            if admission.sealed {
                return None;
            }
            let id = self.entry.next_request.fetch_add(1, Ordering::AcqRel) + 1;
            self.entry.live_requests.fetch_add(1, Ordering::AcqRel);
            admission.idle_since = None;
            id
        };
        let state = Arc::new(RequestState {
            entry: self.entry.clone(),
            id,
            route,
            started: self.entry.clock.instant(),
            inner: Mutex::new(RequestInner::default()),
            relay: RelayAtomics::default(),
        });
        self.entry.requests.lock().insert(id, Arc::downgrade(&state));
        Some(RequestTrace { state })
    }

    /// A capture that ended without an archive — a device that never opened,
    /// or a reservation withdrawn before its first frame.
    pub fn end_capture(&self, outcome: Outcome) {
        self.entry.note_capture_end(None, outcome, false);
    }

    /// A stopped capture whose worker did not publish its final watermark.
    /// Closes diagnostics only; native cleanup and the stop result stay unchanged.
    pub fn abandon_capture(&self, outcome: Outcome) {
        self.entry.note_capture_end(None, outcome, true);
    }

    /// The capture's emit step failed: the frame is lost before the archive
    /// ever sees it.
    pub fn emit_failed(&self) {
        self.entry.capture.note_emit_failure();
    }

    /// The shell's own record, for the call sites that are not the capture,
    /// the relay or a renderer report. The writer expands a renderer report
    /// into its snapshot and boundary records.
    pub fn event(&self, event: Event) {
        if self.entry.is_sealed() {
            return;
        }
        self.entry.push_event(None, event);
    }

}

/// The captured PCM of one take. Pushing a frame never waits for a disk: the
/// buffer comes from the pool, the packet goes into the bounded queue and
/// nothing else happens on the capture's thread.
pub struct CaptureArchive {
    entry: Arc<SessionEntry>,
    pool: Option<Receiver<Vec<u8>>>,
    finished: bool,
}

impl CaptureArchive {
    /// One frame of the take, in capture sequence order.
    pub fn push(&mut self, sequence: u64, pcm_le: &[u8]) {
        if self.finished {
            return;
        }
        // The capture's own watermark is enough on its own: a frame that comes
        // after the capture ended is a gap whether or not the take is sealed
        // yet. A take whose archive the worker closed (quota, I/O) counts its
        // frames as gaps too instead of copying them into a queue nobody owns.
        if self.entry.is_sealed()
            || self.entry.is_capture_closed()
            || !self.entry.archive_enabled()
        {
            self.entry.note_packet(sequence, pcm_le.len(), false);
            return;
        }
        let queued = self.queue(sequence, pcm_le);
        self.entry.note_packet(sequence, pcm_le.len(), queued);
    }

    fn queue(&self, sequence: u64, pcm_le: &[u8]) -> bool {
        let Some(pool) = &self.pool else {
            return false;
        };
        let Ok(mut buffer) = pool.try_recv() else {
            // The pool is dry: the writer is behind by a whole queue, and the
            // take would rather record the gap than allocate on this thread.
            return false;
        };
        buffer.clear();
        if buffer.capacity() < pcm_le.len() {
            buffer.reserve(pcm_le.len());
        }
        buffer.extend_from_slice(pcm_le);
        match self.entry.audio_tx.try_send(AudioPacket {
            sequence,
            bytes: buffer,
        }) {
            Ok(()) => true,
            Err(TrySendError::Full(packet)) | Err(TrySendError::Disconnected(packet)) => {
                let _ = self.entry.free_tx.try_send(packet.bytes);
                false
            }
        }
    }

    /// The capture is over: the sequence of its last frame is published and
    /// the writer is told to flush the tail. This never encodes, verifies or
    /// joins anything.
    pub fn finish(&mut self, last_sequence: u64, outcome: Outcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.entry.note_capture_end(Some(last_sequence), outcome, false);
    }
}

impl Drop for CaptureArchive {
    fn drop(&mut self) {
        if !self.finished {
            self.entry
                .note_capture_end(None, Outcome::Abandoned, false);
        }
        if let Some(pool) = self.pool.take() {
            // The pool belongs to the writer once the capture has let it go:
            // the buffers still in the queue come back through the queue.
            drop(pool);
        }
    }
}

#[derive(Default)]
struct RequestInner {
    cause: Option<RequestCause>,
    status: Option<u16>,
    close_code: Option<u16>,
    error_class: Option<ErrorClass>,
    outcome: Option<Outcome>,
    stage_started: [Option<Instant>; 4],
    wav_bytes: Option<u64>,
    audio_duration_us: Option<u64>,
}

/// One request of a take: the live socket, the batch upload or the delete. All
/// clones share one state, and the take loses its request slot only when the
/// last clone is dropped.
pub struct RequestState {
    entry: Arc<SessionEntry>,
    id: u64,
    route: Route,
    started: Instant,
    inner: Mutex<RequestInner>,
    /// This request's own traffic. A comparison's three routes never share a
    /// send in flight, and the writer reads a request's counters when it ends.
    relay: RelayAtomics,
}

impl RequestState {
    fn relay_snapshot(&self, now_us: u64) -> RelayCounters {
        self.relay.snapshot(now_us)
    }

    fn finish(&self, outcome: Outcome, fallback: RequestCause) {
        let entry = &self.entry;
        let (cause, status, close_code, error_class) = {
            let mut inner = self.inner.lock();
            if inner.outcome.is_some() {
                return;
            }
            inner.outcome = Some(outcome);
            (
                inner.cause.unwrap_or(fallback),
                inner.status,
                inner.close_code,
                inner.error_class,
            )
        };
        entry.push_event_relay(
            Some(self.id),
            self.relay_snapshot(entry.elapsed_us()),
            Event::RequestEnd(RequestEnd {
                route: self.route,
                outcome,
                cause,
                elapsed_us: entry.clock.instant()
                    .saturating_duration_since(self.started).as_micros() as u64,
                status,
                close_code,
                error_class,
            }),
        );
    }
}

impl Drop for RequestState {
    fn drop(&mut self) {
        self.finish(Outcome::Abandoned, RequestCause::MissingTerminal);
        self.entry.requests.lock().remove(&self.id);
        self.entry.live_requests.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub struct RequestTrace {
    state: Arc<RequestState>,
}

impl RequestTrace {
    /// Full, already-redacted evidence owned by this request and mode.
    pub fn content(&self, kind: ContentKind, format: ContentFormat, body: String) -> bool {
        self.state.entry.push_content(Some(self.state.id), Some(self.state.route), kind, format, body)
    }

    /// An observation was not captured whole; do not present its archive as complete.
    pub fn note_content_lost(&self) {
        self.state.entry.note_dropped_event();
    }


    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub fn request_id(&self) -> u64 {
        self.state.id
    }

    /// Every client frame the relay took in, including the ones the pre-ready
    /// cap eventually drops.
    pub fn audio_received(&self, bytes: u64) {
        self.state
            .relay
            .note_client_audio(bytes, self.state.entry.elapsed_us());
    }

    /// How many client frames are waiting for the upstream to become ready.
    pub fn pre_ready_queue_depth(&self, depth: u64) {
        self.state.relay.note_pre_ready_queue(depth);
    }

    /// Called before the send is awaited, so a send that never returns is
    /// visible as in flight.
    pub fn send_started(&self, kind: SendKind) {
        let entry = &self.state.entry;
        self.state.relay.note_send_start(entry.elapsed_us());
        if !matches!(kind, SendKind::Audio) {
            entry.push_event(
                Some(self.state.id),
                Event::SendBegin(SendBegin { kind }),
            );
        }
    }

    /// Called after the send returned, whatever it did. PCM sends stay in the
    /// counters; a failure is counted, not averaged into the successes.

    pub fn send_finished_kind(
        &self,
        kind: SendKind,
        pcm_bytes: u64,
        elapsed: Duration,
        success: bool,
    ) {
        let entry = &self.state.entry;
        let elapsed_us = elapsed.as_micros() as u64;
        // The boundary's own record first, then the counters that go with it:
        // a reader of the journal sees the send end before the snapshot that
        // counts it.
        if !matches!(kind, SendKind::Audio) {
            entry.push_event(
                Some(self.state.id),
                Event::SendEnd(SendEnd {
                    kind,
                    pcm_bytes,
                    elapsed_us,
                    success,
                }),
            );
        }
        self.state.relay.note_send_end(
            kind,
            pcm_bytes,
            elapsed_us,
            success,
            entry.elapsed_us(),
        );
    }


    /// One upstream message. Every class it carried is counted; only the
    /// boundaries that end or open something reach the journal.
    pub fn received(&self, flags: ReceiveFlags) {
        self.received_with_details(flags, None, None);
    }

    /// One message's classes and bounded numeric boundary metadata.
    pub fn received_with_details(
        &self,
        flags: ReceiveFlags,
        code: Option<u16>,
        time_left: Option<Duration>,
    ) {
        if flags.is_empty() {
            return;
        }
        self.state
            .relay
            .note_received(flags, self.state.entry.elapsed_us());
        for (kind, present) in [
            (UpstreamKind::SetupComplete, flags.setup_complete),
            (UpstreamKind::TurnComplete, flags.turn_complete),
            (UpstreamKind::GoAway, flags.go_away),
            (UpstreamKind::Close, flags.close),
            (UpstreamKind::Error, flags.error),
        ] {
            if present {
                self.boundary(
                    kind,
                    if matches!(kind, UpstreamKind::Close | UpstreamKind::Error) { code } else { None },
                    if matches!(kind, UpstreamKind::GoAway) { time_left } else { None },
                );
            }
        }
    }

    /// A boundary the counters cannot express: its kind, an upstream status or
    /// close code and, for a `goAway`, the parsed duration it announced.
    pub fn boundary(&self, kind: UpstreamKind, code: Option<u16>, time_left: Option<Duration>) {
        self.state.entry.push_event(
            Some(self.state.id),
            Event::UpstreamBoundary(UpstreamBoundary {
                kind,
                code,
                time_left_us: time_left.map(|left| left.as_micros() as u64),
            }),
        );
    }

    /// A completed activity rotation and the audio held or replayed across it.
    pub(crate) fn activity_rotate(&self, rotation: ActivityRotate) {
        self.state.entry.push_event(Some(self.state.id), Event::ActivityRotate(rotation));
    }

    /// The deadline that actually expired, distinct from its terminal cause.
    pub fn deadline(&self, timer: DeadlineTimer) {
        self.state.entry.push_event(Some(self.state.id), Event::Deadline(Deadline { timer }));
    }

    pub fn batch_audio(&self, wav_bytes: u64, duration: Duration) {
        let mut inner = self.state.inner.lock();
        inner.wav_bytes = Some(wav_bytes);
        inner.audio_duration_us = Some(duration.as_micros().min(u128::from(u64::MAX)) as u64);
    }

    /// One batch stage of the provider: begin, end or failure, with the HTTP
    /// status when the transport got that far and whether it timed out.
    pub fn stage(&self, stage: Stage, phase: StagePhase, status: Option<u16>, timeout: bool) {
        let now = self.state.entry.clock.instant();
        let index = match stage {
            Stage::UploadStart => 0,
            Stage::UploadFinalize => 1,
            Stage::Interaction => 2,
            Stage::Delete => 3,
        };
        let (elapsed_us, wav_bytes, audio_duration_us) = {
            let mut inner = self.state.inner.lock();
            if status.is_some() && !matches!(stage, Stage::Delete) {
                inner.status = status;
            }
            let elapsed_us = if matches!(phase, StagePhase::Begin) {
                inner.stage_started[index] = Some(now);
                None
            } else {
                inner.stage_started[index].take().map(|started| {
                    now.saturating_duration_since(started).as_micros() as u64
                })
            };
            (elapsed_us, inner.wav_bytes, inner.audio_duration_us)
        };
        self.state.entry.push_event(
            Some(self.state.id),
            Event::BatchStage(BatchStage {
                stage,
                phase,
                status,
                timeout,
                elapsed_us,
                wav_bytes,
                audio_duration_us,
            }),
        );
    }

    /// The transport failure, classified before its text was ever rendered.
    pub fn error_class(&self, class: ErrorClass) {
        self.state.inner.lock().error_class = Some(class);
    }

    /// The close code the upstream socket reported.
    pub fn close_code(&self, code: u16) {
        self.state.inner.lock().close_code = Some(code);
    }

    /// The real reason this request ended.
    pub fn cause(&self, cause: RequestCause) {
        self.state.inner.lock().cause = Some(cause);
    }

    /// Ends the request. The take loses its request slot when the last clone
    /// is dropped, not here.
    pub fn finish(&self, outcome: Outcome) {
        self.state.finish(outcome, RequestCause::Unknown);
    }
}

// ---------------------------------------------------------------------------
// The renderer carrier
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderState {
    Idle,
    Arming,
    Recording,
    Stopping,
    Disposed,
}

/// The terminal a renderer may report. The shell remains authoritative for a
/// dictation; a laboratory window's accepted terminal closes its take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererTerminal {
    Success,
    Error,
    Cancelled,
}

impl RendererTerminal {
    fn outcome(self) -> Outcome {
        match self {
            Self::Success => Outcome::Success,
            Self::Error => Outcome::Error,
            Self::Cancelled => Outcome::Cancelled,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererEventKind {
    RecorderStart,
    RecorderStop,
    RecorderIdle,
    RecorderDispose,
    SocketOpen,
    SocketReady,
    SocketError,
    SocketClose,
    SocketDone,
    NativeGap,
    NativeStop,
    BatchBegin,
    BatchEnd,
    BatchError,
}

/// One boundary a renderer observed. The same shape is the report's event and
/// the journal's record; every field is a number, a code or a flag.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RendererBoundary {
    pub event: RendererEventKind,
    pub at_ms: f64,
    #[serde(default)]
    pub last: Option<u64>,
    #[serde(default)]
    pub seen: Option<u64>,
    #[serde(default)]
    pub expected: Option<u64>,
    #[serde(default)]
    pub received: Option<u64>,
    #[serde(default)]
    pub code: Option<u16>,
    #[serde(default)]
    pub damaged: bool,
}

/// The counters of one accepted renderer report, as the journal records them.
/// The field names are the ones the wrapper posts, so a record and the report
/// it came from read alike.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererSnapshot {
    pub state: RecorderState,
    pub client_elapsed_ms: f64,
    pub handed_frames: u64,
    pub handed_samples: u64,
    pub ignored_frames: u64,
    pub sent_frames: u64,
    pub sent_pcm_bytes: u64,
    pub unsent_frames: u64,
    pub buffered_bytes: u64,
    pub ready_count: u64,
    pub interim_count: u64,
    pub final_count: u64,
    pub error_count: u64,
    pub done_count: u64,
    pub events_dropped: u64,
    pub lost_reports: u64,
    pub last_send_ms: Option<f64>,
    pub last_receive_ms: Option<f64>,
    pub terminal_outcome: Option<RendererTerminal>,
}

/// The development wrapper's one report for one take. Every scalar field is
/// required, including nullable send/receive clocks; only the terminal is optional.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RendererReport {
    pub owner: Owner,
    pub generation: u64,
    pub report_index: u64,
    pub client_elapsed_ms: f64,
    pub state: RecorderState,
    pub handed_frames: u64,
    pub handed_samples: u64,
    pub ignored_frames: u64,
    pub sent_frames: u64,
    pub sent_pcm_bytes: u64,
    pub unsent_frames: u64,
    pub buffered_bytes: u64,
    pub ready_count: u64,
    pub interim_count: u64,
    pub final_count: u64,
    pub error_count: u64,
    pub done_count: u64,
    pub events_dropped: u64,
    pub lost_reports: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub last_send_ms: Option<f64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub last_receive_ms: Option<f64>,
    #[serde(default)]
    pub terminal_outcome: Option<RendererTerminal>,
    pub events: Vec<RendererBoundary>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub errors: Vec<serde_json::Value>,
    #[serde(skip)]
    pub(crate) observed_utc_ms: u64,
    #[serde(skip)]
    pub(crate) observed_elapsed_us: u64,
    #[serde(skip)]
    _content_lease: Option<ContentLease>,
}

impl RendererReport {
    pub fn key(&self) -> SessionKey {
        SessionKey {
            owner: self.owner,
            generation: self.generation,
        }
    }

    /// A report whose numbers cannot be the ones the wrapper produced is not a
    /// report: every count is a non-negative safe integer, every clock is a
    /// finite non-negative number and the event list is bounded.
    pub fn validate(&self) -> Result<(), DiagnosticCode> {
        if !is_safe_id(self.generation) || !is_safe_id(self.report_index) {
            return Err(DiagnosticCode::InvalidReport);
        }
        if !is_safe_clock(self.client_elapsed_ms) {
            return Err(DiagnosticCode::InvalidReport);
        }
        let counts = [
            self.handed_frames,
            self.handed_samples,
            self.ignored_frames,
            self.sent_frames,
            self.sent_pcm_bytes,
            self.unsent_frames,
            self.buffered_bytes,
            self.ready_count,
            self.interim_count,
            self.final_count,
            self.error_count,
            self.done_count,
            self.events_dropped,
            self.lost_reports,
        ];
        if counts.iter().any(|count| *count > 9_007_199_254_740_991) {
            return Err(DiagnosticCode::InvalidReport);
        }
        for clock in [self.last_send_ms, self.last_receive_ms] {
            if let Some(clock) = clock {
                if !is_safe_clock(clock) {
                    return Err(DiagnosticCode::InvalidReport);
                }
            }
        }
        if self.events.len() > RENDERER_EVENTS_MAX {
            return Err(DiagnosticCode::InvalidReport);
        }
        if self.result.as_ref().is_some_and(|result| !content::valid_result(result))
            || self.errors.iter().any(|error| !content::valid_error(error))
        {
            return Err(DiagnosticCode::InvalidReport);
        }
        for event in &self.events {
            if !is_safe_clock(event.at_ms) {
                return Err(DiagnosticCode::InvalidReport);
            }
            let numbers = [event.last, event.seen, event.expected, event.received];
            if numbers
                .iter()
                .flatten()
                .any(|number| *number > 9_007_199_254_740_991)
            {
                return Err(DiagnosticCode::InvalidReport);
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> RendererSnapshot {
        RendererSnapshot {
            state: self.state,
            client_elapsed_ms: self.client_elapsed_ms,
            handed_frames: self.handed_frames,
            handed_samples: self.handed_samples,
            ignored_frames: self.ignored_frames,
            sent_frames: self.sent_frames,
            sent_pcm_bytes: self.sent_pcm_bytes,
            unsent_frames: self.unsent_frames,
            buffered_bytes: self.buffered_bytes,
            ready_count: self.ready_count,
            interim_count: self.interim_count,
            final_count: self.final_count,
            error_count: self.error_count,
            done_count: self.done_count,
            events_dropped: self.events_dropped,
            lost_reports: self.lost_reports,
            last_send_ms: self.last_send_ms,
            last_receive_ms: self.last_receive_ms,
            terminal_outcome: self.terminal_outcome,
        }
    }
}

fn is_safe_id(value: u64) -> bool {
    value > 0 && value <= 9_007_199_254_740_991
}

fn is_safe_clock(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

/// What the loopback endpoint answers with for one report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportAdmission {
    /// 202: the report is queued, its index is committed.
    Accepted,
    /// 200: an index that was already accepted, or one below it.
    StaleReport,
    /// 410: no open take with that key.
    StaleSession,
    /// 400: the report itself is not one.
    InvalidReport,
    /// 429: the metadata queue is full; the same index may be retried.
    QueueFull,
    /// 503: the sink is not running.
    Unavailable,
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Registry {
    sessions: HashMap<SessionKey, Arc<SessionEntry>>,
}

impl Registry {
    /// Every entry the writer has not let go of, sealed or not: a sealed take
    /// still has a queue to drain, an archive to finish and a manifest to
    /// write, so it keeps its slot until all of that is done.
    fn unresolved_count(&self) -> usize {
        self.sessions.len()
    }
}

/// The development sink. Disabled is a real state: a disabled sink registers
/// nothing, writes nothing and answers every report as unavailable.
pub struct Diagnostics {
    enabled: bool,
    clock: Arc<dyn Clock>,
    registry: Arc<Mutex<Registry>>,
    meta_tx: SyncSender<PendingEvent>,
    meta_rx: Arc<Mutex<Receiver<PendingEvent>>>,
    content_budget: Arc<ContentBudget>,
    next_session: AtomicU64,
    closed: AtomicBool,
    error: Arc<Mutex<Option<DiagnosticCode>>>,
    /// Set when the root itself cannot be trusted: from then on no take is
    /// registered and no record is written, while everything already counted
    /// stays readable in memory.
    sink_disabled: Arc<AtomicBool>,
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    /// The archive root: `<config dir>/logs` for the resolved profile. The
    /// writer thread creates it.
    root: Option<PathBuf>,
    run_counter: u64,
    /// The writer thread of the storage half. Absent in every build that does
    /// not archive, and absent in the producer fixtures.
    worker: Mutex<Option<storage::Worker>>,
}

impl Diagnostics {
    /// The sink of every build that does not archive: no thread, no directory,
    /// no file. A `Test` runtime uses exactly this.
    pub fn disabled() -> Arc<Self> {
        Self::bare(false, false, Policy::DEVELOPMENT, Arc::new(RealClock), None, 0)
    }

    /// The development sink. `config_dir` is the resolved configuration
    /// directory, and the archive root is its `logs` child — the only place a
    /// session directory may appear. Nothing here touches the filesystem: the
    /// writer thread creates the directory it writes under.
    pub fn start(config_dir: &Path) -> Arc<Self> {
        Self::bare(
            true,
            true,
            Policy::DEVELOPMENT,
            Arc::new(RealClock),
            Some(config_dir.to_path_buf()),
            next_run_counter(),
        )
    }

    /// A sink of the tests' own: an explicit root, an explicit policy and a
    /// clock the test drives. It keeps no writer, so a producer fixture drives
    /// its own queues and no file exists.
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub fn for_test(root: &Path, policy: Policy, now: SystemTime) -> Arc<Self> {
        let clock = Arc::new(FakeClock::new(now));
        Self::bare(true, false, policy, clock, Some(root.to_path_buf()), 0)
    }

    /// The same with the storage half attached: the real writer thread, the
    /// real root and the real files under a directory the test owns.
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub fn for_test_with_storage(root: &Path, policy: Policy, now: SystemTime) -> Arc<Self> {
        Self::with_io(root, policy, now, Arc::new(storage::RealIo))
    }

    /// The same with a scripted filesystem, so a test can make a write short, a
    /// delete fail or a child look like a link without depending on the access
    /// lists of the machine it runs on.
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub fn with_io(
        root: &Path,
        policy: Policy,
        now: SystemTime,
        io: Arc<dyn storage::ArchiveIo>,
    ) -> Arc<Self> {
        let clock = Arc::new(FakeClock::new(now));
        Self::with_storage(policy, clock, root.to_path_buf(), io, 0)
    }

    fn bare(
        enabled: bool,
        storage: bool,
        policy: Policy,
        clock: Arc<dyn Clock>,
        config_dir: Option<PathBuf>,
        run_counter: u64,
    ) -> Arc<Self> {
        match (storage, config_dir) {
            (true, Some(config_dir)) => Self::with_storage(
                policy,
                clock,
                config_dir.join("logs"),
                Arc::new(storage::RealIo),
                run_counter,
            ),
            (true, None) => Self::bare(false, false, policy, clock, None, run_counter),
            (false, _config_dir) => {
                let (meta_tx, meta_rx) = sync_channel(META_QUEUE);
                Arc::new(Self {
                    enabled,
                    clock,
                    registry: Arc::new(Mutex::new(Registry::default())),
                    meta_tx,
                    meta_rx: Arc::new(Mutex::new(meta_rx)),
                    content_budget: Arc::new(ContentBudget::default()),
                    next_session: AtomicU64::new(0),
                    closed: AtomicBool::new(false),
                    error: Arc::new(Mutex::new(None)),
                    sink_disabled: Arc::new(AtomicBool::new(false)),
                    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
                    root: _config_dir,
                    run_counter,
                    worker: Mutex::new(None),
                })
            }
        }
    }

    /// The sink with its writer thread. The registry, the queues, the error
    /// slot and the root are all built here and handed to the thread, which
    /// from then on is the only thing that writes.
    fn with_storage(
        policy: Policy,
        clock: Arc<dyn Clock>,
        root: PathBuf,
        io: Arc<dyn storage::ArchiveIo>,
        run_counter: u64,
    ) -> Arc<Self> {
        let (meta_tx, meta_rx) = sync_channel(META_QUEUE);
        let registry = Arc::new(Mutex::new(Registry::default()));
        let meta_rx = Arc::new(Mutex::new(meta_rx));
        let error = Arc::new(Mutex::new(None));
        let sink_disabled = Arc::new(AtomicBool::new(false));
        let run_id = format!(
            "{}-{}-{}",
            system_millis(clock.system()),
            std::process::id(),
            run_counter
        );
        #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
        let fixture_root = root.clone();
        let worker = storage::spawn(
            io,
            root,
            policy,
            clock.clone(),
            registry.clone(),
            meta_rx.clone(),
            error.clone(),
            sink_disabled.clone(),
            run_id,
            std::process::id(),
            run_counter,
        );
        let diagnostics = Arc::new(Self {
            enabled: true,
            clock,
            registry,
            meta_tx,
            meta_rx,
            content_budget: Arc::new(ContentBudget::default()),
            next_session: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            error,
            sink_disabled,
            #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
            root: Some(fixture_root),
            run_counter,
            worker: Mutex::new(worker),
        });
        if diagnostics.worker.lock().is_none() {
            diagnostics.note_error(DiagnosticCode::Io);
        }
        diagnostics
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }


    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Stops the sink: no new take is registered, no sealed take is reopened,
    /// and a late frame cannot create a file again. An ordinary exit
    /// (`budget > 0`) raises every still-unsealed take to [`Outcome::Exit`];
    /// an installer quit (`budget == ZERO`) leaves the outcome it had, which
    /// the writer then records as abandoned. The storage half joins its writer
    /// within `budget` here; the seal itself never waits.
    pub fn shutdown(&self, budget: Duration) {
        if !self.enabled {
            return;
        }
        self.closed.store(true, Ordering::SeqCst);
        let entries: Vec<Arc<SessionEntry>> =
            self.registry.lock().sessions.values().cloned().collect();
        let raised = (budget > Duration::ZERO).then_some(Outcome::Exit);
        for entry in &entries {
            if entry.is_sealed() {
                continue;
            }
            entry.seal_now(raised);
        }
        // The writer gets the budget to drain its queues, write the last record
        // and the last manifest and close every take. A writer that does not
        // come back in time is left detached: the take it did not finish stays
        // an unverified partial rather than a fabricated result.
        if let Some(worker) = self.worker.lock().as_ref() {
            worker.stop(budget);
        }
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    /// The last failure class kept in memory, whether or not its record could
    /// be written.
    pub fn error(&self) -> Option<DiagnosticCode> {
        *self.error.lock()
    }

    pub fn note_error(&self, code: DiagnosticCode) {
        *self.error.lock() = Some(code);
    }

    /// Opens a take. `None` means the sink is disabled or closed, the key is
    /// already registered, or the open-take limit was reached — the reason is
    /// kept in memory even when a journal cannot be written.
    pub fn begin(&self, key: SessionKey, mode: Mode) -> Option<SessionHandle> {
        if !self.enabled || self.closed.load(Ordering::Acquire) || self.is_sink_disabled() {
            return None;
        }
        let now = self.clock.instant();
        let now_utc_ms = system_millis(self.clock.system());
        let mut registry = self.registry.lock();
        self.advance_locked(&registry, now);
        if registry.sessions.contains_key(&key) {
            return None;
        }
        if registry.unresolved_count() >= MAX_OPEN_SESSIONS {
            drop(registry);
            self.note_error(DiagnosticCode::SessionLimit);
            return None;
        }
        let counter = self.next_session.fetch_add(1, Ordering::AcqRel) + 1;
        let entry = Arc::new(SessionEntry::new(
            key,
            mode,
            self.clock.clone(),
            now_utc_ms,
            counter,
            self.meta_tx.clone(),
            Arc::clone(&self.content_budget),
        ));
        registry.sessions.insert(key, entry.clone());
        entry.push_event(
            None,
            Event::SessionBegin(SessionBegin {
                mode,
                started_utc_ms: now_utc_ms,
                retention_utc_ms: now_utc_ms,
                pid: std::process::id(),
                run_counter: self.run_counter,
                session_counter: counter,
            }),
        );
        Some(SessionHandle { entry })
    }

    /// The registered take behind a key, while it is still open. A sealed take
    /// is history: no new request and no new report is admitted for it.
    pub fn lookup(&self, key: SessionKey) -> Option<SessionHandle> {
        if !self.enabled {
            return None;
        }
        let now = self.clock.instant();
        let mut registry = self.registry.lock();
        self.advance_locked(&mut registry, now);
        registry
            .sessions
            .get(&key)
            .filter(|entry| !entry.is_sealed())
            .map(|entry| SessionHandle {
                entry: entry.clone(),
            })
    }

    /// The shell's terminal for one take. Only a rise is kept, and a sealed
    /// take is never rewritten.
    pub fn end(&self, key: SessionKey, outcome: Outcome) {
        if !self.enabled {
            return;
        }
        let now = self.clock.instant();
        if let Some(entry) = self.registry.lock().sessions.get(&key).cloned() {
            entry.raise_terminal(outcome, now);
            entry.advance(now);
        }
    }

    /// Closes every still-unsealed take of one owner. The laboratory window
    /// closing is the case: takes it was already serving, with no native
    /// reservation left, end as `WindowClosed` all the same.
    pub fn end_owner(&self, owner: Owner, outcome: Outcome) {
        if !self.enabled {
            return;
        }
        let now = self.clock.instant();
        let registry = self.registry.lock();
        for entry in registry.sessions.values() {
            if entry.key.owner == owner && !entry.is_sealed() {
                entry.raise_terminal(outcome, now);
                entry.advance(now);
            }
        }
    }

    /// Admits one renderer report. The whole report is queued before its index
    /// is committed, so a queue that is full costs a retry of the same index
    /// and never a gap in the sequence.
    pub fn admit_renderer_report(&self, mut report: RendererReport) -> ReportAdmission {
        if !self.enabled || self.is_sink_disabled() {
            return ReportAdmission::Unavailable;
        }
        if report.validate().is_err() {
            return ReportAdmission::InvalidReport;
        }
        let key = report.key();
        let entry = match self.lookup(key) {
            Some(handle) => handle.entry,
            None => return ReportAdmission::StaleSession,
        };
        let terminal = report.terminal_outcome;
        let index = report.report_index;
        let accepted = {
            let mut admission = entry.admission.lock();
            if admission.sealed {
                false
            } else if index <= admission.accepted_report_index {
                return ReportAdmission::StaleReport;
            } else {
                let bytes = report.result.as_ref().map_or(0, content::value_bytes)
                    .saturating_add(report.errors.iter().fold(0_u64, |sum, error| sum.saturating_add(content::value_bytes(error))));
                let Some(lease) = self.content_budget.claim(bytes) else {
                    return ReportAdmission::QueueFull;
                };
                report._content_lease = Some(lease);
                report.observed_utc_ms = system_millis(self.clock.system());
                report.observed_elapsed_us = entry.elapsed_us();
                // The gate is already held here, so the record's place is
                // claimed inline instead of through `push_meta`.
                let pending = PendingEvent {
                    key,
                    request_id: None,
                    relay: None,
                    meta: Meta::Renderer(report),
                };
                if entry.meta_tx.try_send(pending).is_err() {
                    return ReportAdmission::QueueFull;
                }
                admission.pending_meta += 1;
                admission.accepted_report_index = index;
                if key.owner == Owner::Lab {
                    if let Some(terminal) = terminal {
                        let preferred = Outcome::preferred(admission.terminal, terminal.outcome());
                        if admission.terminal != Some(preferred) {
                            admission.terminal = Some(preferred);
                            admission.terminal_at = Some(self.clock.instant());
                        }
                    }
                }
                true
            }
        };
        if !accepted {
            return ReportAdmission::StaleSession;
        }
        ReportAdmission::Accepted
    }

    /// The writer's record of a live take. A queue that overflows counts a
    /// dropped event; the take's own records keep flowing.
    #[allow(dead_code)] // Drained by the writer thread.
    pub(crate) fn take_meta(&self, budget: usize) -> Vec<PendingEvent> {
        let receiver = self.meta_rx.lock();
        let mut taken = Vec::new();
        while taken.len() < budget {
            match receiver.try_recv() {
                Ok(pending) => taken.push(pending),
                Err(_) => break,
            }
        }
        taken
    }

    /// One admission step over every open take, so a take nobody talks to any
    /// more still gets sealed and, with no terminal ever reported, closed as
    /// abandoned.
    #[allow(dead_code)] // Called by the writer's pass and by the tests.
    pub(crate) fn advance_all(&self) {
        if !self.enabled {
            return;
        }
        let now = self.clock.instant();
        let registry = self.registry.lock();
        for entry in registry.sessions.values() {
            entry.advance(now);
        }
    }

    fn advance_locked(&self, registry: &Registry, now: Instant) {
        for entry in registry.sessions.values() {
            entry.advance(now);
        }
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn advance_clock(&self, by: Duration) {
        let fake = self
            .clock
            .as_any()
            .downcast_ref::<FakeClock>()
            .expect("the test clock");
        fake.advance(by);
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    fn entry_for(&self, key: SessionKey) -> Option<Arc<SessionEntry>> {
        self.registry.lock().sessions.get(&key).cloned()
    }

    /// The test's hold on the writer: park it, let it go, or wait for one more
    /// completed pass. A test that parks the writer must let it go again, or
    /// its own flush times out.
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn worker_pause(&self) {
        if let Some(worker) = self.worker.lock().as_ref() {
            worker.pause(Duration::from_secs(5));
        }
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn worker_resume(&self) {
        if let Some(worker) = self.worker.lock().as_ref() {
            worker.resume();
        }
    }

    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn worker_flush(&self) {
        if let Some(worker) = self.worker.lock().as_ref() {
            worker.flush(Duration::from_secs(5));
        }
    }

    /// Lets the writer run `count` more passes.
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn worker_passes(&self, count: u32) {
        for _ in 0..count {
            self.worker_flush();
        }
    }

    /// Whether the root itself has been declared unusable: from then on no take
    /// is registered and no record is written, though everything already
    /// counted stays readable.
    fn is_sink_disabled(&self) -> bool {
        self.sink_disabled.load(Ordering::Acquire)
    }

    /// Sets the wall clock alone, so a test can age a bundle without moving the
    /// monotonic clock the deadlines run on.
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub(crate) fn set_wall_clock(&self, now: SystemTime) {
        if let Some(fake) = self.clock.as_any().downcast_ref::<FakeClock>() {
            fake.set_system(now);
        }
    }
}

impl Drop for Diagnostics {
    /// A sink that goes away without an explicit shutdown still stops its
    /// writer, so no thread outlives the root it was writing into.
    fn drop(&mut self) {
        if let Some(worker) = self.worker.lock().take() {
            worker.stop(Duration::from_secs(2));
        }
    }
}

fn system_millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// The sink counter of this process: the first sink of a process is one, and a
/// later sink takes the next number, so two runs never build the same session
/// directory name.
fn next_run_counter() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
struct FakeClock {
    instant: Mutex<Instant>,
    system: Mutex<SystemTime>,
}

#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
impl FakeClock {
    fn new(now: SystemTime) -> Self {
        Self {
            instant: Mutex::new(Instant::now()),
            system: Mutex::new(now),
        }
    }

    fn advance(&self, by: Duration) {
        *self.instant.lock() += by;
        *self.system.lock() += by;
    }

    /// Moves only the wall clock: a test ages a manifest without moving the
    /// monotonic clock the deadlines run on.
    fn set_system(&self, now: SystemTime) {
        *self.system.lock() = now;
    }
}

#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
impl Clock for FakeClock {
    fn instant(&self) -> Instant {
        *self.instant.lock()
    }

    fn system(&self) -> SystemTime {
        *self.system.lock()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
mod tests {
    use super::*;
    use serde_json::json;

    #[path = "storage_tests.rs"]
    mod storage_tests;
    mod development_smoke;
    mod http_tests;

    #[test]
    fn development_archive_smoke() {
        development_smoke::development_archive_smoke_body();
    }

    #[test]
    fn consuming_a_large_content_record_releases_room_for_another_take() {
        let (diagnostics, key) = sink();
        let first = diagnostics.begin(key, Mode::Smart).unwrap();
        let second_key = SessionKey { owner: Owner::Lab, generation: 2 };
        let second = diagnostics.begin(second_key, Mode::Comparison).unwrap();
        assert!(first.content(ContentKind::ProviderResponse, ContentFormat::Text,
            "a".repeat(5 * 1024 * 1024)));
        assert!(!second.content(ContentKind::RecorderResult, ContentFormat::Text,
            "b".repeat(4 * 1024 * 1024)));
        assert_eq!(diagnostics.entry_for(second_key).unwrap().dropped_events(), 1);
        // Removing a record from the queue is not enough: the writer owns its
        // payload until the observation itself is dropped after writing.
        let held = diagnostics.take_meta(META_QUEUE);
        assert!(!second.content(ContentKind::RecorderResult, ContentFormat::Text,
            "b".repeat(4 * 1024 * 1024)));
        drop(held);
        assert!(second.content(ContentKind::RecorderResult, ContentFormat::Text,
            "b".repeat(4 * 1024 * 1024)));
        assert_eq!(diagnostics.entry_for(second_key).unwrap().dropped_events(), 2);
    }

    #[test]
    fn content_pressure_never_commits_a_lab_terminal_before_its_result() {
        let (diagnostics, key) = sink();
        let first = diagnostics.begin(key, Mode::Live).unwrap();
        assert!(first.content(ContentKind::LiveMessage, ContentFormat::Text,
            "a".repeat(7 * 1024 * 1024)));
        let lab_key = SessionKey { owner: Owner::Lab, generation: 2 };
        let lab = diagnostics.begin(lab_key, Mode::Comparison).unwrap();
        let mut terminal = report(lab_key, 1);
        terminal.terminal_outcome = Some(RendererTerminal::Success);
        terminal.result = Some(json!({
            "results": { "smart": "текст".repeat(40_000) },
            "errors": {}, "errorDetails": {}
        }));
        assert_eq!(diagnostics.admit_renderer_report(terminal.clone()), ReportAdmission::QueueFull);
        let entry = diagnostics.entry_for(lab_key).unwrap();
        assert_eq!(entry.terminal(), None);
        assert_eq!(entry.admission.lock().accepted_report_index, 0);
        drop(diagnostics.take_meta(META_QUEUE));
        assert_eq!(diagnostics.admit_renderer_report(terminal), ReportAdmission::Accepted);
        assert_eq!(entry.terminal(), Some(Outcome::Success));
        assert_eq!(entry.admission.lock().accepted_report_index, 1);
        drop(lab);
    }

    fn sink() -> (Arc<Diagnostics>, SessionKey) {
        let root = std::env::temp_dir().join(format!(
            "speechek-diagnostics-test-{}-{}",
            std::process::id(),
            unique()
        ));
        let diagnostics = Diagnostics::for_test(
            &root,
            Policy {
                max_age: Duration::from_secs(600),
                max_bytes: 1_000_000,
            },
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        );
        (diagnostics, SessionKey {
            owner: Owner::Dictation,
            generation: 1,
        })
    }

    fn unique() -> u64 {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::process::id() as u64 * 1_000_000 + COUNTER.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn report(key: SessionKey, index: u64) -> RendererReport {
        RendererReport {
            owner: key.owner,
            generation: key.generation,
            report_index: index,
            client_elapsed_ms: 1_000.0,
            state: RecorderState::Recording,
            handed_frames: 10,
            handed_samples: 5_120,
            ignored_frames: 0,
            sent_frames: 9,
            sent_pcm_bytes: 4_608,
            unsent_frames: 1,
            buffered_bytes: 512,
            ready_count: 1,
            interim_count: 2,
            final_count: 0,
            error_count: 0,
            done_count: 0,
            events_dropped: 0,
            lost_reports: 0,
            last_send_ms: Some(900.0),
            last_receive_ms: None,
            terminal_outcome: None,
            events: Vec::new(),
            result: None,
            errors: Vec::new(),
            observed_utc_ms: 0,
            observed_elapsed_us: 0,
            _content_lease: None,
        }
    }

    #[test]
    fn a_terminal_only_rises_and_cancelled_wins_its_tie() {
        assert_eq!(
            Outcome::preferred(Some(Outcome::WindowClosed), Outcome::Cancelled),
            Outcome::Cancelled
        );
        assert_eq!(
            Outcome::preferred(Some(Outcome::Exit), Outcome::Success),
            Outcome::Exit
        );
        assert_eq!(
            Outcome::preferred(Some(Outcome::Error), Outcome::Success),
            Outcome::Error
        );
        assert_eq!(
            Outcome::preferred(Some(Outcome::Abandoned), Outcome::Success),
            Outcome::Success
        );
        assert_eq!(Outcome::preferred(None, Outcome::Abandoned), Outcome::Abandoned);
    }

    #[test]
    fn a_tag_is_one_canonical_owner_and_generation() {
        assert_eq!(
            SessionKey::parse_tag("dictation:1"),
            Some(SessionKey {
                owner: Owner::Dictation,
                generation: 1
            })
        );
        assert_eq!(
            SessionKey::parse_tag("lab:7500"),
            Some(SessionKey {
                owner: Owner::Lab,
                generation: 7_500
            })
        );
        for bad in [
            "lab:0",
            "lab:01",
            "lab:",
            "lab:1:2",
            "lab:-1",
            "lab: 1",
            "lab:1 ",
            "1",
            "dictation",
            "browser:1",
            "lab:9223372036854775808",
            "lab:99999999999999999999999",
        ] {
            assert_eq!(SessionKey::parse_tag(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_disabled_sink_registers_and_writes_nothing() {
        let diagnostics = Diagnostics::disabled();
        let key = SessionKey {
            owner: Owner::Dictation,
            generation: 1,
        };
        assert!(diagnostics.begin(key, Mode::Smart).is_none());
        assert!(diagnostics.lookup(key).is_none());
        assert_eq!(
            diagnostics.admit_renderer_report(report(key, 1)),
            ReportAdmission::Unavailable
        );
        diagnostics.end(key, Outcome::Success);
        diagnostics.end_owner(Owner::Lab, Outcome::WindowClosed);
        assert_eq!(diagnostics.error(), None);
    }

    #[test]
    fn the_begin_limit_is_reported_as_its_own_code() {
        let (diagnostics, _) = sink();
        for generation in 1..=MAX_OPEN_SESSIONS as u64 {
            let key = SessionKey {
                owner: Owner::Lab,
                generation,
            };
            assert!(diagnostics.begin(key, Mode::Verbatim).is_some(), "{generation}");
        }
        let overflow = SessionKey {
            owner: Owner::Lab,
            generation: MAX_OPEN_SESSIONS as u64 + 1,
        };
        assert!(diagnostics.begin(overflow, Mode::Verbatim).is_none());
        assert_eq!(diagnostics.error(), Some(DiagnosticCode::SessionLimit));

        let already_open = SessionKey {
            owner: Owner::Lab,
            generation: 1,
        };
        assert!(diagnostics.begin(already_open, Mode::Verbatim).is_none());
    }

    #[test]
    fn a_pushed_frame_keeps_its_sequence_and_recycles_its_buffer() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let mut archive = handle.capture();
        for sequence in 1..=4 {
            archive.push(sequence, &[0u8; 1024]);
        }
        let entry = diagnostics.entry_for(key).unwrap();
        let counters = entry.capture.snapshot();
        assert_eq!(counters.produced_frames, 4);
        assert_eq!(counters.produced_samples, 4 * 512);
        assert_eq!(counters.produced_bytes, 4 * 1024);
        assert_eq!(counters.last_sequence, Some(4));
        assert!(!counters.has_gaps());

        let taken = entry.take_audio(2);
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].sequence, 1);
        assert_eq!(taken[1].sequence, 2);
        for packet in taken {
            assert_eq!(packet.bytes.len(), 1024);
            entry.return_buffer(packet.bytes);
        }
        archive.push(5, &[0u8; 1024]);
        assert_eq!(entry.capture.snapshot().dropped_frames, 0);
        assert_eq!(entry.capture.snapshot().last_sequence, Some(5));
    }

    #[test]
    fn a_dry_pool_and_a_full_queue_are_counted_not_waited_on() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Live).unwrap();
        let mut archive = handle.capture();
        for sequence in 1..=(AUDIO_QUEUE as u64 + 44) {
            archive.push(sequence, &[1u8; 1024]);
        }
        let entry = diagnostics.entry_for(key).unwrap();
        let counters = entry.capture.snapshot();
        assert_eq!(counters.produced_frames, AUDIO_QUEUE as u64 + 44);
        assert_eq!(counters.dropped_frames, 44);
        assert_eq!(counters.dropped_samples, 44 * 512);
        assert_eq!(counters.last_sequence, Some(AUDIO_QUEUE as u64 + 44));
        assert!(counters.has_gaps());
        assert_eq!(entry.take_audio(AUDIO_QUEUE + 1).len(), AUDIO_QUEUE);
    }

    #[test]
    fn a_second_archive_of_one_take_has_no_pool() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Live).unwrap();
        let mut first = handle.capture();
        let mut second = handle.capture();
        first.push(1, &[0u8; 1024]);
        second.push(2, &[0u8; 1024]);
        let counters = diagnostics.entry_for(key).unwrap().capture.snapshot();
        assert_eq!(counters.produced_frames, 2);
        assert_eq!(counters.dropped_frames, 1);
        assert_eq!(counters.last_sequence, Some(2));
    }

    #[test]
    fn a_frame_after_the_finish_is_not_archived() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Live).unwrap();
        let mut archive = handle.capture();
        archive.push(1, &[0u8; 1024]);
        archive.finish(1, Outcome::Success);
        archive.push(2, &[0u8; 1024]);
        let entry = diagnostics.entry_for(key).unwrap();
        assert_eq!(entry.capture.snapshot().produced_frames, 1);
        assert_eq!(entry.capture.snapshot().last_sequence, Some(1));
        assert_eq!(entry.take_audio(4).len(), 1);
    }

    #[test]
    fn a_dropped_archive_finishes_as_abandoned() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Live).unwrap();
        {
            let mut archive = handle.capture();
            archive.push(1, &[0u8; 1024]);
        }
        let _entry = diagnostics.entry_for(key).unwrap();
        let seen = diagnostics
            .take_meta(4)
            .into_iter()
            .map(|pending| pending.meta)
            .collect::<Vec<_>>();
        let mut saw_end = false;
        for meta in seen {
            if let Meta::Event(Event::CaptureEnd(end)) = meta {
                assert_eq!(end.outcome, Outcome::Abandoned);
                saw_end = true;
            }
        }
        assert!(saw_end);
    }

    #[test]
    fn sealing_waits_for_the_capture_the_requests_and_the_grace() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let mut archive = handle.capture();
        let trace = handle.request(Route::Smart).unwrap();
        archive.finish(1, Outcome::Success);
        diagnostics.advance_all();
        assert!(diagnostics.lookup(key).is_some(), "a live request holds it open");

        drop(trace);
        diagnostics.end(key, Outcome::Success);
        diagnostics.advance_all();
        assert!(diagnostics.lookup(key).is_some(), "the grace has not run out");

        diagnostics.advance_clock(SEAL_GRACE);
        diagnostics.advance_all();
        assert!(diagnostics.lookup(key).is_none(), "sealed");
        assert!(diagnostics.entry_for(key).unwrap().is_sealed());
        assert_eq!(
            diagnostics.entry_for(key).unwrap().terminal(),
            Some(Outcome::Success)
        );
    }

    #[test]
    fn a_capture_error_takes_no_terminal_from_the_shell() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Live).unwrap();
        let mut archive = handle.capture();
        archive.finish(3, Outcome::Error);
        assert_eq!(
            diagnostics.entry_for(key).unwrap().terminal(),
            Some(Outcome::Error)
        );
        diagnostics.end(key, Outcome::Success);
        assert_eq!(
            diagnostics.entry_for(key).unwrap().terminal(),
            Some(Outcome::Error),
            "a lower rank never displaces a higher one"
        );
        diagnostics.end(key, Outcome::Cancelled);
        assert_eq!(
            diagnostics.entry_for(key).unwrap().terminal(),
            Some(Outcome::Cancelled)
        );
    }

    #[test]
    fn a_sealed_take_keeps_its_history() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let mut archive = handle.capture();
        archive.finish(1, Outcome::Error);
        diagnostics.end(key, Outcome::Error);
        diagnostics.advance_clock(SEAL_GRACE);
        diagnostics.advance_all();
        assert!(diagnostics.entry_for(key).unwrap().is_sealed());
        diagnostics.end(key, Outcome::Exit);
        assert_eq!(
            diagnostics.entry_for(key).unwrap().terminal(),
            Some(Outcome::Error)
        );
        assert!(handle.request(Route::Smart).is_none());
    }

    #[test]
    fn an_idle_take_without_a_terminal_is_closed_as_abandoned() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Verbatim).unwrap();
        let mut archive = handle.capture();
        archive.finish(2, Outcome::Success);
        assert_eq!(diagnostics.entry_for(key).unwrap().terminal(), None);

        diagnostics.advance_clock(IDLE_DEADLINE - Duration::from_millis(1));
        diagnostics.advance_all();
        assert_eq!(diagnostics.entry_for(key).unwrap().terminal(), None);

        diagnostics.advance_clock(Duration::from_millis(2));
        diagnostics.advance_all();
        let entry = diagnostics.entry_for(key).unwrap();
        assert_eq!(entry.terminal(), Some(Outcome::Abandoned));
        assert!(entry.admission.lock().missing_terminal);
    }

    #[test]
    fn a_late_report_never_reopens_a_sealed_take() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let mut archive = handle.capture();
        archive.finish(1, Outcome::Error);
        diagnostics.advance_clock(SEAL_GRACE);
        diagnostics.advance_all();
        assert_eq!(
            diagnostics.admit_renderer_report(report(key, 1)),
            ReportAdmission::StaleSession
        );
    }

    #[test]
    fn a_report_commits_its_index_once() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        assert_eq!(
            diagnostics.admit_renderer_report(report(key, 1)),
            ReportAdmission::Accepted
        );
        assert_eq!(
            diagnostics.admit_renderer_report(report(key, 2)),
            ReportAdmission::Accepted
        );
        assert_eq!(
            diagnostics.admit_renderer_report(report(key, 2)),
            ReportAdmission::StaleReport
        );
        assert_eq!(
            diagnostics.admit_renderer_report(report(key, 1)),
            ReportAdmission::StaleReport
        );

        let unknown = SessionKey {
            owner: Owner::Lab,
            generation: 99,
        };
        assert_eq!(
            diagnostics.admit_renderer_report(report(unknown, 1)),
            ReportAdmission::StaleSession
        );

        let queued = diagnostics.take_meta(META_QUEUE);
        let reports = queued
            .iter()
            .filter(|pending| matches!(pending.meta, Meta::Renderer(_)))
            .count();
        assert_eq!(reports, 2, "one record per accepted index");
        drop(handle);
    }

    #[test]
    fn an_accepted_lab_terminal_closes_its_take() {
        let (diagnostics, _) = sink();
        let key = SessionKey {
            owner: Owner::Lab,
            generation: 7,
        };
        let _handle = diagnostics.begin(key, Mode::Comparison).unwrap();
        let mut accepted = report(key, 1);
        accepted.terminal_outcome = Some(RendererTerminal::Success);
        assert_eq!(
            diagnostics.admit_renderer_report(accepted),
            ReportAdmission::Accepted
        );
        assert_eq!(
            diagnostics.entry_for(key).unwrap().terminal(),
            Some(Outcome::Success)
        );
    }

    #[test]
    fn a_dictation_terminal_in_a_report_is_ignored_by_the_sink() {
        let (diagnostics, key) = sink();
        let _handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let mut accepted = report(key, 1);
        accepted.terminal_outcome = Some(RendererTerminal::Cancelled);
        assert_eq!(
            diagnostics.admit_renderer_report(accepted),
            ReportAdmission::Accepted
        );
        assert_eq!(diagnostics.entry_for(key).unwrap().terminal(), None);
    }

    #[test]
    fn a_queue_that_is_full_does_not_commit_the_index() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        // Fill the shared metadata queue to its capacity, then try a report.
        for _ in 0..META_QUEUE {
            handle.event(Event::Phase(Phase {
                phase: CapturePhase::Recording,
            }));
        }
        assert_eq!(
            diagnostics.admit_renderer_report(report(key, 5)),
            ReportAdmission::QueueFull
        );
        assert_eq!(
            diagnostics.entry_for(key).unwrap().admission.lock().accepted_report_index,
            0
        );
        let _ = diagnostics.take_meta(META_QUEUE * 2);
        assert_eq!(
            diagnostics.admit_renderer_report(report(key, 5)),
            ReportAdmission::Accepted,
            "the same index is retried and accepted once there is room"
        );
    }

    #[test]
    fn a_report_that_is_not_one_is_refused() {
        let (diagnostics, key) = sink();
        assert_eq!(
            diagnostics.admit_renderer_report(report(key, 1)),
            ReportAdmission::StaleSession,
            "the key is validated before the session is looked up"
        );
        let _handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let mut oversized = report(key, 1);
        oversized.events = vec![
            RendererBoundary {
                event: RendererEventKind::SocketReady,
                at_ms: 1.0,
                last: None,
                seen: None,
                expected: None,
                received: None,
                code: None,
                damaged: false,
            };
            RENDERER_EVENTS_MAX + 1
        ];
        assert_eq!(
            diagnostics.admit_renderer_report(oversized),
            ReportAdmission::InvalidReport
        );
        let mut negative = report(key, 1);
        negative.client_elapsed_ms = -1.0;
        assert_eq!(
            diagnostics.admit_renderer_report(negative),
            ReportAdmission::InvalidReport
        );
        let mut out_of_range = report(key, 1);
        out_of_range.sent_pcm_bytes = u64::MAX;
        assert_eq!(
            diagnostics.admit_renderer_report(out_of_range),
            ReportAdmission::InvalidReport
        );
        let mut no_index = report(key, 0);
        no_index.report_index = 0;
        assert_eq!(
            diagnostics.admit_renderer_report(no_index),
            ReportAdmission::InvalidReport
        );
        let mut bad_event = report(key, 1);
        bad_event.events = vec![RendererBoundary {
            event: RendererEventKind::NativeGap,
            at_ms: f64::INFINITY,
            last: None,
            seen: None,
            expected: None,
            received: None,
            code: None,
            damaged: true,
        }];
        assert_eq!(
            diagnostics.admit_renderer_report(bad_event),
            ReportAdmission::InvalidReport
        );
    }

    #[test]
    fn a_report_carries_no_unknown_field() {
        let text = json!({
            "owner": "dictation",
            "generation": 1,
            "reportIndex": 1,
            "clientElapsedMs": 1.0,
            "state": "recording",
            "handedFrames": 1,
            "handedSamples": 512,
            "ignoredFrames": 0,
            "sentFrames": 1,
            "sentPcmBytes": 1024,
            "unsentFrames": 0,
            "bufferedBytes": 0,
            "readyCount": 1,
            "interimCount": 0,
            "finalCount": 0,
            "errorCount": 0,
            "doneCount": 0,
            "eventsDropped": 0,
            "lostReports": 0,
            "lastSendMs": null,
            "lastReceiveMs": null,
            "events": [],
            "message": "a string the journal must never hold"
        })
        .to_string();
        assert!(serde_json::from_str::<RendererReport>(&text).is_err());

        let clean = json!({
            "owner": "lab",
            "generation": 2,
            "reportIndex": 3,
            "clientElapsedMs": 2000.0,
            "state": "stopping",
            "handedFrames": 10,
            "handedSamples": 5120,
            "ignoredFrames": 0,
            "sentFrames": 10,
            "sentPcmBytes": 10240,
            "unsentFrames": 0,
            "bufferedBytes": 0,
            "readyCount": 1,
            "interimCount": 3,
            "finalCount": 1,
            "errorCount": 0,
            "doneCount": 1,
            "eventsDropped": 0,
            "lostReports": 0,
            "lastSendMs": null,
            "lastReceiveMs": null,
            "terminalOutcome": "success",
            "events": [{"event": "native_stop", "atMs": 1900.0, "last": 10, "seen": 10, "damaged": false}]
        })
        .to_string();
        let parsed: RendererReport = serde_json::from_str(&clean).unwrap();
        assert_eq!(parsed.validate(), Ok(()));
    }

    #[test]
    fn request_clones_share_one_slot() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let entry = diagnostics.entry_for(key).unwrap();
        let trace = handle.request(Route::Smart).unwrap();
        assert_eq!(entry.live_requests.load(Ordering::Acquire), 1);
        assert_eq!(trace.request_id(), 1);
        {
            let clone = trace.clone();
            assert_eq!(clone.request_id(), 1);
            drop(clone);
            assert_eq!(entry.live_requests.load(Ordering::Acquire), 1);
        }
        drop(trace);
        assert_eq!(entry.live_requests.load(Ordering::Acquire), 0);

        let next = handle.request(Route::Smart).unwrap();
        assert_eq!(next.request_id(), 2);
    }

    #[test]
    fn each_request_keeps_its_own_sends_and_only_audio_counts_frames() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let entry = diagnostics.entry_for(key).unwrap();
        let first = handle.request(Route::Smart).unwrap();
        let second = handle.request(Route::Verbatim).unwrap();
        assert_eq!(first.request_id(), 1);
        assert_eq!(second.request_id(), 2);

        fn counters(entry: &Arc<SessionEntry>, id: u64) -> RelayCounters {
            entry
                .relay_snapshots()
                .into_iter()
                .find(|(request, _)| *request == id)
                .map(|(_, counters)| counters)
                .expect("a live request")
        }

        // A send that never returns is in flight for its own request alone, and
        // it ages with the sink's clock instead of freezing at its start.
        first.send_started(SendKind::Audio);
        diagnostics.advance_clock(Duration::from_millis(4));
        let first_now = counters(&entry, 1);
        assert!(first_now.in_flight_send);
        assert_eq!(first_now.pending_send_us, 4_000);
        let second_now = counters(&entry, 2);
        assert!(!second_now.in_flight_send);
        assert_eq!(second_now.pending_send_us, 0);

        // Finishing one request's send leaves the other's state alone, and only
        // an audio send adds frames and bytes.
        first.send_finished_kind(SendKind::Audio, 1_024, Duration::from_millis(5), true);
        first.send_started(SendKind::Audio);
        first.send_finished_kind(SendKind::Audio, 0, Duration::from_millis(6), false);
        second.send_started(SendKind::Setup);
        second.send_finished_kind(SendKind::Setup, 0, Duration::from_millis(2), true);
        second.send_started(SendKind::ActivityStart);
        second.send_finished_kind(SendKind::ActivityStart, 0, Duration::from_millis(1), true);
        second.audio_received(1_024);
        second.audio_received(1_024);
        second.pre_ready_queue_depth(7);

        let first_counters = counters(&entry, 1);
        assert_eq!(first_counters.upstream_sent_frames, 1);
        assert_eq!(first_counters.upstream_pcm_bytes, 1_024);
        assert_eq!(first_counters.send_failures, 1);
        assert_eq!(first_counters.max_send_us, 6_000);
        assert_eq!(first_counters.last_audio_roundtrip_us, 5_000);
        assert!(!first_counters.in_flight_send);
        assert_eq!(first_counters.pending_send_us, 0);
        assert_eq!(first_counters.client_audio_frames, 0);

        let second_counters = counters(&entry, 2);
        assert_eq!(
            second_counters.upstream_sent_frames, 0,
            "a setup and an activity message are sends, not audio frames"
        );
        assert_eq!(second_counters.upstream_pcm_bytes, 0);
        assert_eq!(second_counters.send_failures, 0);
        assert_eq!(second_counters.client_audio_frames, 2);
        assert_eq!(second_counters.client_pcm_bytes, 2_048);
        assert_eq!(second_counters.pre_ready_queue_depth, 7);

        first.received(ReceiveFlags {
            interim: true,
            r#final: true,
            turn_complete: true,
            ..ReceiveFlags::default()
        });
        let first_counters = counters(&entry, 1);
        assert_eq!(first_counters.received_messages, 1);
        assert_eq!(first_counters.interim_count, 1);
        assert_eq!(first_counters.final_count, 1);
        assert_eq!(first_counters.turn_complete_count, 1);
        assert_eq!(counters(&entry, 2).received_messages, 0);

        // A request that is gone takes its counters with it.
        first.finish(Outcome::Success);
        drop(first);
        assert_eq!(entry.live_requests.load(Ordering::Acquire), 1);
        assert!(entry
            .relay_snapshots()
            .iter()
            .all(|(request, _)| *request == 2));
    }


    #[test]
    fn an_unfinished_request_ends_only_when_its_last_clone_is_dropped() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let trace = handle.request(Route::Smart).unwrap();
        let clone = trace.clone();
        trace.audio_received(1024);
        drop(trace);
        assert!(!diagnostics.take_meta(META_QUEUE).into_iter()
            .any(|pending| matches!(pending.meta, Meta::Event(Event::RequestEnd(_)))));
        drop(clone);
        let mut ends = diagnostics.take_meta(META_QUEUE).into_iter().filter_map(|pending| {
            match pending.meta {
                Meta::Event(Event::RequestEnd(end)) => Some((end, pending.relay)),
                _ => None,
            }
        });
        let (end, relay) = ends.next().expect("the dropped request has a terminal record");
        assert_eq!(end.outcome, Outcome::Abandoned);
        assert_eq!(end.cause, RequestCause::MissingTerminal);
        assert_eq!(relay.unwrap().client_pcm_bytes, 1024);
        assert!(ends.next().is_none(), "the request ends exactly once");
    }

    #[test]
    fn delete_failure_does_not_replace_the_interaction_status_or_cause() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Verbatim).unwrap();
        let trace = handle.request(Route::Verbatim).unwrap();
        trace.stage(Stage::Interaction, StagePhase::Error, Some(429), false);
        trace.cause(RequestCause::UpstreamError);
        trace.stage(Stage::Delete, StagePhase::Error, Some(503), true);
        trace.finish(Outcome::Error);
        drop(trace);
        let end = diagnostics.take_meta(META_QUEUE).into_iter().find_map(|pending| {
            match pending.meta {
                Meta::Event(Event::RequestEnd(end)) => Some(end),
                _ => None,
            }
        }).expect("the request end");
        assert_eq!(end.status, Some(429));
        assert_eq!(end.cause, RequestCause::UpstreamError);
        assert_eq!(end.outcome, Outcome::Error);
    }

    #[test]
    fn batch_stages_keep_their_own_elapsed_times_and_actual_audio_size() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let trace = handle.request(Route::Smart).unwrap();
        trace.batch_audio(1068, Duration::from_micros(32_000));
        trace.stage(Stage::Interaction, StagePhase::Begin, None, false);
        diagnostics.advance_clock(Duration::from_millis(25));
        trace.stage(Stage::Delete, StagePhase::Begin, None, false);
        diagnostics.advance_clock(Duration::from_millis(7));
        trace.stage(Stage::Interaction, StagePhase::End, Some(200), false);
        trace.stage(Stage::Delete, StagePhase::Error, Some(503), true);
        let stages = diagnostics.take_meta(META_QUEUE).into_iter().filter_map(|pending| {
            match pending.meta {
                Meta::Event(Event::BatchStage(stage)) if !matches!(stage.phase, StagePhase::Begin) => Some(stage),
                _ => None,
            }
        }).collect::<Vec<_>>();
        let interaction = stages.iter().find(|stage| matches!(stage.stage, Stage::Interaction)).unwrap();
        let delete = stages.iter().find(|stage| matches!(stage.stage, Stage::Delete)).unwrap();
        assert_eq!(interaction.elapsed_us, Some(32_000));
        assert_eq!(delete.elapsed_us, Some(7_000));
        assert_eq!(interaction.wav_bytes, Some(1068));
        assert_eq!(interaction.audio_duration_us, Some(32_000));
        assert_eq!(delete.status, Some(503));
        assert!(delete.timeout);
    }

    #[test]
    fn a_request_end_keeps_the_cause_the_status_and_the_close_code() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Verbatim).unwrap();
        let _entry = diagnostics.entry_for(key).unwrap();
        let trace = handle.request(Route::Verbatim).unwrap();
        trace.stage(Stage::UploadStart, StagePhase::Begin, None, false);
        trace.stage(
            Stage::UploadFinalize,
            StagePhase::Error,
            Some(503),
            true,
        );
        trace.error_class(ErrorClass::Timeout);
        trace.close_code(1006);
        trace.cause(RequestCause::UpstreamError);
        trace.finish(Outcome::Error);

        let ended = diagnostics
            .take_meta(META_QUEUE)
            .into_iter()
            .find_map(|pending| match pending.meta {
                Meta::Event(Event::RequestEnd(end)) => Some(end),
                _ => None,
            })
            .expect("one request_end");
        assert_eq!(ended.route, Route::Verbatim);
        assert_eq!(ended.outcome, Outcome::Error);
        assert_eq!(ended.cause, RequestCause::UpstreamError);
        assert_eq!(ended.status, Some(503));
        assert_eq!(ended.close_code, Some(1006));
        assert_eq!(ended.error_class, Some(ErrorClass::Timeout));

        trace.finish(Outcome::Success);
        assert_eq!(
            diagnostics
                .take_meta(META_QUEUE)
                .into_iter()
                .filter(|pending| matches!(
                    pending.meta,
                    Meta::Event(Event::RequestEnd(_))
                ))
                .count(),
            0,
            "a request ends once"
        );
    }

    #[test]
    fn a_boundary_is_recorded_where_a_count_is_not_enough() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let _entry = diagnostics.entry_for(key).unwrap();
        let trace = handle.request(Route::Smart).unwrap();
        trace.boundary(
            UpstreamKind::GoAway,
            Some(1001),
            Some(Duration::from_millis(500)),
        );
        let boundary = diagnostics
            .take_meta(META_QUEUE)
            .into_iter()
            .find_map(|pending| match pending.meta {
                Meta::Event(Event::UpstreamBoundary(boundary)) => Some(boundary),
                _ => None,
            })
            .expect("one boundary");
        assert_eq!(boundary.kind, UpstreamKind::GoAway);
        assert_eq!(boundary.code, Some(1001));
        assert_eq!(boundary.time_left_us, Some(500_000));
    }

    #[test]
    fn an_owner_close_takes_every_unsealed_take_of_that_owner() {
        let (diagnostics, _) = sink();
        let first = SessionKey {
            owner: Owner::Lab,
            generation: 1,
        };
        let second = SessionKey {
            owner: Owner::Lab,
            generation: 2,
        };
        let dictation = SessionKey {
            owner: Owner::Dictation,
            generation: 1,
        };
        let _a = diagnostics.begin(first, Mode::Verbatim).unwrap();
        let _b = diagnostics.begin(second, Mode::Verbatim).unwrap();
        let _c = diagnostics.begin(dictation, Mode::Smart).unwrap();
        diagnostics.end_owner(Owner::Lab, Outcome::WindowClosed);
        assert_eq!(
            diagnostics.entry_for(first).unwrap().terminal(),
            Some(Outcome::WindowClosed)
        );
        assert_eq!(
            diagnostics.entry_for(second).unwrap().terminal(),
            Some(Outcome::WindowClosed)
        );
        assert_eq!(diagnostics.entry_for(dictation).unwrap().terminal(), None);
    }

    #[test]
    fn an_overflowing_metadata_queue_counts_its_drops() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Live).unwrap();
        let entry = diagnostics.entry_for(key).unwrap();
        let trace = handle.request(Route::Live).unwrap();
        assert_eq!(
            diagnostics.entry_for(key).unwrap().audio_status(),
            AudioStatus::None
        );
        // Start from an empty queue, then overflow it by a known amount: the
        // queue holds META_QUEUE records and every one past that is counted.
        let _ = diagnostics.take_meta(META_QUEUE);
        let dropped = 24;
        for _ in 0..(META_QUEUE + dropped) {
            trace.stage(Stage::Interaction, StagePhase::Begin, None, false);
        }
        assert_eq!(entry.dropped_events(), dropped as u64);
        let queued = diagnostics.take_meta(META_QUEUE * 2);
        assert_eq!(queued.len(), META_QUEUE, "the queue stayed bounded");
    }

    #[test]
    fn a_capture_takes_its_status_from_its_first_frame() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        assert_eq!(
            diagnostics.entry_for(key).unwrap().audio_status(),
            AudioStatus::None
        );
        let mut archive = handle.capture();
        archive.push(1, &[0u8; 1024]);
        assert_eq!(
            diagnostics.entry_for(key).unwrap().audio_status(),
            AudioStatus::Recording
        );
    }


    #[test]
    fn an_ordinary_exit_raises_an_unsealed_take_and_closes_admission() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Smart).unwrap();
        let mut archive = handle.capture();
        archive.push(1, &[0u8; 1024]);
        archive.finish(1, Outcome::Success);

        diagnostics.shutdown(Duration::from_millis(500));

        let entry = diagnostics.entry_for(key).unwrap();
        assert!(entry.is_sealed());
        assert_eq!(entry.terminal(), Some(Outcome::Exit));
        assert_eq!(entry.audio_status(), AudioStatus::Recording);
        assert_eq!(entry.capture.snapshot().last_sequence, Some(1));
        assert!(diagnostics.lookup(key).is_none(), "sealed is history");

        let other = SessionKey {
            owner: Owner::Lab,
            generation: 5,
        };
        assert!(
            diagnostics.begin(other, Mode::Verbatim).is_none(),
            "shutdown closes admission"
        );
        assert!(
            handle.request(Route::Smart).is_none(),
            "a sealed take takes no request"
        );
    }

    #[test]
    fn an_installer_quit_invents_no_outcome_and_refuses_late_frames() {
        let (diagnostics, key) = sink();
        let handle = diagnostics.begin(key, Mode::Verbatim).unwrap();
        let mut archive = handle.capture();
        archive.push(1, &[0u8; 1024]);

        diagnostics.shutdown(Duration::ZERO);

        let entry = diagnostics.entry_for(key).unwrap();
        assert!(entry.is_sealed());
        assert_eq!(entry.terminal(), None, "no outcome was invented");

        archive.push(2, &[0u8; 1024]);
        let counters = entry.capture.snapshot();
        assert_eq!(counters.produced_frames, 2);
        assert_eq!(counters.last_sequence, Some(2));
        assert_eq!(counters.dropped_frames, 1, "the late frame is a gap");
        assert!(entry.audio_incomplete());
    }
}
