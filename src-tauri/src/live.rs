//! `/api/live`: the loopback WebSocket relay to the Gemini Live API.
//!
//! The overlay streams 16 kHz mono PCM16 frames as base64 JSON; the relay forwards
//! them to the Live API over a connection that carries the API key server-side only.
//! The key never reaches the browser, and no error, log line or message sent back to
//! the browser ever contains it (`backend::redact`).
//!
//! One accepted socket binds exactly one key for the whole session, and the browser
//! sees exactly five messages: `ready` once the upstream confirms its setup,
//! `interim` and `final` for the input transcription, `error` for a terminal
//! failure, and exactly one `done` that closes the session. Interim transcripts are
//! never promoted: only a transcript the Live API itself marked as final becomes
//! `final`.
//!
//! Timing contract: 15 s to confirm the setup, 15 s for the post-end
//! transcript, 5 s for the server to end an activity the relay closed for
//! rotation, 900 ms/600 ms settle windows, an 11-minute session cap, a
//! 10-minute audio cap and a 700 s idle timeout. Deadlines are absolute here,
//! so a stalled environment cannot move them.
//!
//! Development diagnostics may also archive the session's content: the setup
//! frame as it was sent together with the pinned rotation entry this socket
//! bound - its 1-based ordinal and the ring's key count, never the key - every
//! frame that arrived from the upstream - parsed or not - every message written
//! back to the browser, the upstream's close and every transport failure. Each
//! record is already redacted against the ring the socket was bound to, and
//! every hook is gated on
//! `all(debug_assertions, feature = "dev-diagnostics")` and on a traced request,
//! so a production build and an untraced take build nothing. Nothing here reads
//! audio: the recording itself is archived as PCM16 by the capture side, never
//! again as the base64 this relay forwards.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as ClientMessage, WebSocket};
use base64::Engine as _;
use futures_util::future::OptionFuture;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::Message as UpstreamMessage;

use crate::backend::{redact, redact_ui};
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use crate::backend::{diagnostic_bytes, diagnostic_json, diagnostic_text};
use crate::i18n::{render, Language, MessageId, UiMessage};
use crate::provider::Provider;
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use crate::provider::key_ordinal;
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use crate::diagnostics::{
    ActivityRotate, ContentFormat, ContentKind, DeadlineTimer, Outcome, ReceiveFlags, RequestCause,
    RequestTrace, RotateReason, SendKind, UpstreamKind,
};
use crate::secrets::{KeyRing, SecretKey};

/* -------------------------------------------------------------------------- */
/* Contract                                                                   */
/* -------------------------------------------------------------------------- */

/// The Live model the relay drives; the value is the exact model name sent in
/// the setup frame.
const LIVE_MODEL: &str = "gemini-3.5-transcribe-live";

/// The recorder's audio contract: 16 kHz mono PCM16.
const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u64 = 1;
const BYTES_PER_SAMPLE: u64 = 2;

/// 10 minutes: the documented Live session limit, and the audio cap with it.
const MAX_RECORDING_SECONDS: u64 = 600;
const MAX_AUDIO_BYTES: u64 =
    SAMPLE_RATE as u64 * CHANNELS * BYTES_PER_SAMPLE * MAX_RECORDING_SECONDS;

/// Setup confirmation deadline, measured from `start`.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Post-end transcript deadline. Kept below the browser's own 20 s budget so the UI
/// shows our message rather than its own timeout.
const FINAL_TIMEOUT: Duration = Duration::from_secs(15);
/// The earliest an activity may be rotated, and the latest: the Live API ends
/// one activity by itself after roughly 220 s, so a rotation has to be
/// completed before that.
const ROTATE_EARLIEST_MS: u64 = 150_000;
/// The activity length at which a rotation is forced, quiet or not.
const ROTATE_FORCE_MS: u64 = 200_000;
/// How long the server has to end an activity the relay closed for rotation.
const ROTATE_TIMEOUT: Duration = Duration::from_secs(5);
/// How much forwarded audio a server-side cut can replay: the frames that
/// follow a cut point have to be still here when the cut arrives.
const REPLAY_WINDOW_SAMPLES: u64 = 15 * SAMPLE_RATE as u64;
/// Session cap: the 10-minute audio cap plus a minute of slack for the tail.
const HARD_LIMIT: Duration = Duration::from_secs(MAX_RECORDING_SECONDS + 60);
/// Quiet period after the last final transcript before the session is completed.
const FINAL_SETTLE: Duration = Duration::from_millis(900);
/// Quiet period after the upstream's turn boundary before the session is completed.
const TURN_SETTLE: Duration = Duration::from_millis(600);
/// Sentinel for the loop's deadline branch while no timer is armed: far enough out
/// that it can never fire before a real deadline is set.
const IDLE_PARK: Duration = Duration::from_secs(24 * 60 * 60);
/// The idle timeout for the browser socket: a socket the browser stops feeding
/// for 700 s is dropped. The 11-minute cap is shorter than it, so a session in
/// progress never reaches this deadline.
const IDLE_TIMEOUT: Duration = Duration::from_secs(700);

/* -------------------------------------------------------------------------- */
/* Session state                                                              */
/* -------------------------------------------------------------------------- */

/// The upstream socket `connect_async` returns for a `wss://` Live URL.
type Upstream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The two transcription modes, mapped to the Live API's enum values.
#[derive(Clone, Copy)]
enum Mode {
    Smart,
    Verbatim,
}

impl Mode {
    fn from_json(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "smart" => Some(Self::Smart),
            "verbatim" => Some(Self::Verbatim),
            _ => None,
        }
    }

    fn live_value(self) -> &'static str {
        match self {
            Self::Smart => "SMART",
            Self::Verbatim => "VERBATIM",
        }
    }
}

/// The relay's timers as absolute deadlines, so a late wake-up still knows which
/// one expired.
#[derive(Clone, Copy)]
enum Timer {
    /// Setup was not confirmed in time.
    Ready,
    /// The post-end transcript is overdue.
    Final,
    /// The server did not end an activity the relay closed for rotation.
    Rotate,
    /// The upstream went quiet after its last transcript or turn boundary.
    Settle,
    /// The session ran into the 11-minute cap.
    Hard,
    /// The browser stopped sending anything for the idle timeout.
    Idle,
}

/// Which side of a rotation the current activity is on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivityPhase {
    /// Audio is forwarded into the activity as it arrives.
    Open,
    /// Our `activityEnd` was written: audio is held until the server ends the
    /// activity, because an `activityStart` sent before it is ignored.
    Closing,
}

/// Everything one accepted socket tracks, mirroring `LiveState`.
struct Relay {
    /// The one key bound to this socket; it only ever appears inside the URL
    /// [`Provider::live_url`] builds, never in a message.
    key: Arc<SecretKey>,
    /// The ring this socket was pinned to. Every key in it - the one above
    /// included - is what each outgoing message is redacted against, so an
    /// upstream error that quotes a key cannot reach the browser.
    ring: Arc<KeyRing>,
    /// The custom vocabulary of the revision this socket was bound to: the
    /// terms the upstream setup frame biases recognition toward. Empty means
    /// the setup frame carries no vocabulary field at all.
    vocabulary: Arc<[String]>,
    /// The dictation-language hints of the bound revision; empty means the
    /// setup frame carries no `languageCodes` field.
    languages: Arc<[String]>,
    mode: Option<Mode>,
    started: bool,
    end_requested: bool,
    upstream_ready: bool,
    done: bool,
    /// Bytes of audio accepted so far, against the 10-minute cap.
    bytes: u64,
    /// Audio that arrived before the upstream confirmed its setup, in order.
    queue: Vec<String>,
    /// Which side of a rotation the current activity is on.
    activity: ActivityPhase,
    /// The 1-based ordinal of the current activity.
    activity_index: u32,
    /// Samples forwarded into the current activity, replays and flushed held
    /// frames included.
    activity_samples: u64,
    /// `sent_samples` at the moment the current `activityStart` was written:
    /// the origin the upstream's own audio offsets are measured from.
    activity_start_sample: u64,
    /// The `audioOffset` the upstream announced with the current activity's
    /// `ACTIVITY_START`, in seconds.
    server_start_offset: Option<f64>,
    /// Whether this activity announced boundaries, even without a valid offset.
    server_activity_started: bool,
    /// The last activity was acknowledged after our final activityEnd.
    confirmed: bool,
    /// Every sample forwarded upstream, replays and flushed held frames
    /// included.
    sent_samples: u64,
    /// The frames of the last [`REPLAY_WINDOW_SAMPLES`] forwarded samples, in
    /// order, with the sample each one started at: what a cut the server makes
    /// on its own is replayed from.
    recent: VecDeque<(u64, u32, String)>,
    /// The frames that arrived between our `activityEnd` and the server's
    /// `ACTIVITY_END`, in order.
    held: Vec<(String, u32)>,
    /// Whether any held frame carried speech: a rotation that holds only
    /// silence is not worth continuing.
    held_voice: bool,
    /// Whether the upstream sent interim text whose final has not arrived.
    pending_speech: bool,
    /// The pause detector of the current activity.
    pauses: PauseDetector,
    ready_at: Option<Instant>,
    final_at: Option<Instant>,
    /// Deadline for the server to end the activity the relay closed.
    rotate_at: Option<Instant>,
    settle_at: Option<Instant>,
    hard_at: Option<Instant>,
    /// Deadline for the next client frame, refreshed on every one that arrives.
    idle_at: Option<Instant>,
    /// The development diagnostics of this socket, when the wrapper tagged it.
    /// Everything it is told is a number, a boolean or a fixed enum.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    trace: Option<RequestTrace>,
    /// The request's terminal, armed for as long as this relay lives: whatever
    /// path leaves the loop, the request ends exactly once.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    terminal: Option<RequestTerminal>,
    /// When our `activityEnd` was written, for the diagnostics' wait time.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    closing_since: Option<Instant>,
    /// Why the current activity is closing, for the diagnostics.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    closing_reason: Option<RotateReason>,
}

impl Relay {
    fn new(
        key: Arc<SecretKey>,
        ring: Arc<KeyRing>,
        vocabulary: Arc<[String]>,
        languages: Arc<[String]>,
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] trace: Option<RequestTrace>,
    ) -> Self {
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        let terminal = trace.clone().map(RequestTerminal::new);
        Self {
            key,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            trace,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            terminal,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            closing_since: None,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            closing_reason: None,
            ring,
            vocabulary,
            languages,
            mode: None,
            started: false,
            end_requested: false,
            upstream_ready: false,
            done: false,
            bytes: 0,
            queue: Vec::new(),
            activity: ActivityPhase::Open,
            activity_index: 1,
            activity_samples: 0,
            activity_start_sample: 0,
            server_start_offset: None,
            server_activity_started: false,
            confirmed: false,
            sent_samples: 0,
            recent: VecDeque::new(),
            held: Vec::new(),
            held_voice: false,
            pending_speech: false,
            pauses: PauseDetector::new(),
            ready_at: None,
            final_at: None,
            rotate_at: None,
            settle_at: None,
            hard_at: None,
            idle_at: Some(Instant::now() + IDLE_TIMEOUT),
        }
    }

    /// The earliest armed deadline; the loop sleeps to it and re-checks the rest.
    fn next_deadline(&self) -> Option<Instant> {
        [
            self.ready_at,
            self.final_at,
            self.rotate_at,
            self.settle_at,
            self.hard_at,
            self.idle_at,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Clears every timer, the undelivered audio and the audio held across a
    /// rotation.
    fn clear_timers(&mut self) {
        self.ready_at = None;
        self.final_at = None;
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        {
            self.closing_since = None;
            self.closing_reason = None;
        }
        self.rotate_at = None;
        self.settle_at = None;
        self.hard_at = None;
        self.idle_at = None;
        self.queue.clear();
        self.held.clear();
        self.held_voice = false;
        self.recent.clear();
    }

    /// One client frame arrived: the idle deadline starts over.
    fn touch(&mut self) {
        self.idle_at = Some(Instant::now() + IDLE_TIMEOUT);
    }
}

/* -------------------------------------------------------------------------- */
/* Activity rotation                                                          */
/* -------------------------------------------------------------------------- */

/// The level of one PCM16 frame in dBFS: the RMS of its little-endian samples
/// against full scale, `-120.0` for silence and for a frame with no sample in
/// it. This is what tells a pause from speech; nothing here keeps the audio.
fn frame_level_db(pcm: &[u8]) -> f32 {
    let mut sum = 0.0_f64;
    let mut samples = 0_u64;
    for chunk in pcm.chunks_exact(BYTES_PER_SAMPLE as usize) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]) as f64;
        sum += sample * sample;
        samples += 1;
    }
    if samples == 0 {
        return -120.0;
    }
    let rms = (sum / samples as f64).sqrt();
    if rms <= 0.0 {
        return -120.0;
    }
    (20.0 * (rms / 32_768.0).log10()) as f32
}

/// The noise floor of one activity and how long it has been quiet: a rotation
/// is only allowed at a pause, and a pause is only a pause relative to what
/// this microphone's own floor turned out to be.
struct PauseDetector {
    /// The slow-moving floor of the activity, in dBFS.
    floor_db: f32,
    /// How many milliseconds of audio in a row have been quiet.
    quiet_ms: u32,
}

impl PauseDetector {
    fn new() -> Self {
        Self {
            floor_db: -60.0,
            quiet_ms: 0,
        }
    }

    /// Feeds one frame and answers whether it was quiet. The floor follows a
    /// quieter frame down at once and rises by a twentieth of a decibel per
    /// frame otherwise, so a pause in a quiet room and one in a noisy one are
    /// both seen; the threshold sits 12 dB above the floor, clamped to the
    /// range a human pause lives in.
    fn observe(&mut self, level_db: f32, frame_ms: u32) -> bool {
        self.floor_db = if level_db < self.floor_db {
            level_db
        } else {
            self.floor_db + 0.05
        };
        self.floor_db = self.floor_db.clamp(-120.0, -35.0);
        let threshold = (self.floor_db + 12.0).clamp(-60.0, -30.0);
        let quiet = level_db <= threshold;
        self.quiet_ms = if quiet { self.quiet_ms + frame_ms } else { 0 };
        quiet
    }
}

/// Whether the current activity should be rotated here: the earlier the
/// activity, the longer the pause it takes, and from [`ROTATE_FORCE_MS`] on
/// the rotation happens at the next frame whatever the level is. The Live API
/// ends an activity by itself after roughly 220 s, so waiting for a perfect
/// pause is not an option.
fn rotation_due(activity_ms: u64, quiet_ms: u32) -> bool {
    if activity_ms >= ROTATE_FORCE_MS {
        true
    } else if activity_ms >= 190_000 {
        quiet_ms >= 150
    } else if activity_ms >= 175_000 {
        quiet_ms >= 300
    } else if activity_ms >= ROTATE_EARLIEST_MS {
        quiet_ms >= 600
    } else {
        false
    }
}

/// The audio offset the upstream announced, in seconds: `"220.440s"`, `"0s"`.
/// Anything else - another shape, a negative or an unusable number - is no
/// offset at all, never a reason to abort the relay.
fn parse_audio_offset(value: &Value) -> Option<f64> {
    let seconds = value
        .as_str()?
        .trim()
        .strip_suffix('s')?
        .trim()
        .parse::<f64>()
        .ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(seconds)
}

/// The relay's one request, ended exactly once.
///
/// The relay is a loop with several `return`s, so the terminal state is
/// recorded by the path that reaches it ([`finish`], [`fail`], [`cancel`]) and
/// this guard is the safety net: a path nobody instrumented - an unwind, or a
/// `return` added later - still ends the request, and says so honestly with
/// [`RequestCause::MissingTerminal`] instead of inventing a success.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
struct RequestTerminal {
    trace: RequestTrace,
    outcome: Option<Outcome>,
}

#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
impl RequestTerminal {
    fn new(trace: RequestTrace) -> Self {
        Self {
            trace,
            outcome: None,
        }
    }

    /// The first terminal a path reached decides; a `fail` that then calls
    /// [`finish`] keeps its `Error` rather than being rewritten to a success.
    fn reached(&mut self, outcome: Outcome) {
        if self.outcome.is_none() {
            self.outcome = Some(outcome);
        }
    }
}

#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
impl Drop for RequestTerminal {
    fn drop(&mut self) {
        match self.outcome {
            Some(outcome) => self.trace.finish(outcome),
            None => {
                self.trace.cause(RequestCause::MissingTerminal);
                self.trace.finish(Outcome::Abandoned);
            }
        }
    }
}

/// The terminal state this relay reached, for the request's own record.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_terminal(state: &mut Relay, outcome: Outcome) {
    if let Some(terminal) = state.terminal.as_mut() {
        terminal.reached(outcome);
    }
}

/// The real reason this request ended, recorded before the terminal itself.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_cause(state: &Relay, cause: RequestCause) {
    if let Some(trace) = state.trace.as_ref() {
        trace.cause(cause);
    }
}

/// A deadline that fired, written the moment it did.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_deadline(state: &Relay, timer: DeadlineTimer) {
    if let Some(trace) = state.trace.as_ref() {
        trace.deadline(timer);
    }
}

/// One completed rotation: the activity that ended, why it ended and the audio
/// that was held or replayed across the boundary.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_rotation(state: &Relay, rotation: ActivityRotate) {
    if let Some(trace) = state.trace.as_ref() {
        trace.activity_rotate(rotation);
    }
}

/// One packet that arrived but never became a message: a frame this relay could
/// not decode or parse still crossed the socket, so it moves the receive
/// counters exactly once, as the `other` class. A frame that does become a
/// message is counted where it is parsed instead; this is only for the paths
/// that used to leave the packet uncounted.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_received_other(state: &Relay) {
    if let Some(trace) = state.trace.as_ref() {
        trace.received(ReceiveFlags {
            other: true,
            ..ReceiveFlags::default()
        });
    }
}

/// One upstream message, classified into every class it carried: a composite
/// `serverContent` sets each of its flags, and a message that is none of the
/// known shapes is `other` so it still moves the receive counters.
///
/// `ping`/`pong` never surface here (the socket layer answers them) and `close`
/// arrives as a socket frame, so both are counted where they are observed.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn receive_flags(message: &Value) -> ReceiveFlags {
    let content = message.get("serverContent");
    let setup_complete = message.get("setupComplete").is_some();
    let interim = content
        .and_then(|content| content.get("interimInputTranscription"))
        .and_then(text_of)
        .is_some();
    let r#final = content
        .and_then(|content| content.get("inputTranscription"))
        .and_then(text_of)
        .is_some();
    let turn_complete = json_truthy(content.and_then(|content| content.get("turnComplete")));
    let go_away = message.get("goAway").is_some();
    // The activity boundaries arrive at the top level of the frame, outside any
    // `serverContent`; a frame carrying one is voice activity like the signal
    // inside `serverContent` is.
    let voice_activity = message.get("voiceActivity").is_some()
        || content
            .map(|content| {
                content.get("voiceActivityDetectionSignal").is_some()
                    || content.get("voiceActivity").is_some()
            })
            .unwrap_or(false);
    let usage_metadata = message.get("usageMetadata").is_some();
    let error = message.get("error").is_some();
    let known = setup_complete
        || interim
        || r#final
        || turn_complete
        || go_away
        || voice_activity
        || usage_metadata
        || error;
    ReceiveFlags {
        setup_complete,
        interim,
        r#final,
        turn_complete,
        go_away,
        voice_activity,
        usage_metadata,
        ping: false,
        pong: false,
        close: false,
        error,
        other: !known,
    }
}

/// The `goAway.timeLeft` the upstream announced, as a duration: the API sends
/// seconds, as a number or as `"<n>s"`. Anything else is no duration at all.
/// This is the numeric half of the metadata only; the frame's own text is
/// archived as content where it arrives.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn go_away_time_left(message: &Value) -> Option<Duration> {
    let value = message.get("goAway")?.get("timeLeft")?;
    let seconds = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().trim_end_matches('s').parse::<f64>().ok()?,
        _ => return None,
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    // A finite number can still be out of `Duration`'s range (`1e30`, say):
    // an unusable announcement is no metadata, never an abort of the relay.
    Duration::try_from_secs_f64(seconds).ok()
}

/// Which kind of upstream send one payload is. The payload shape is the relay's
/// own ([`setup_payload`], [`audio_payload`], the two activity frames), so this
/// reads the same key the sender wrote.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn send_kind_of(payload: &Value) -> SendKind {
    let realtime = payload.get("realtimeInput");
    if realtime
        .and_then(|input| input.get("activityStart"))
        .is_some()
    {
        SendKind::ActivityStart
    } else if realtime
        .and_then(|input| input.get("activityEnd"))
        .is_some()
    {
        SendKind::ActivityEnd
    } else if realtime.and_then(|input| input.get("audio")).is_some() {
        SendKind::Audio
    } else {
        SendKind::Setup
    }
}

/// The PCM16 bytes one upstream send carried, from the base64 length the frame
/// already announced. A non-audio payload carries none.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn send_pcm_bytes(payload: &Value) -> u64 {
    if !matches!(send_kind_of(payload), SendKind::Audio) {
        return 0;
    }
    payload
        .get("realtimeInput")
        .and_then(|input| input.get("audio"))
        .and_then(|audio| audio.get("data"))
        .and_then(Value::as_str)
        .map(|data| base64_byte_length(data).max(0) as u64)
        .unwrap_or(0)
}

/* -------------------------------------------------------------------------- */
/* Content evidence                                                           */
/* -------------------------------------------------------------------------- */
/*
 * What the archive is told, and in what shape. Every one of these is gated the
 * same way and computes nothing when the request has no trace: with production
 * or an untraced take, none of the strings below is even built.
 *
 * Redaction always borrows the pinned ring - the whole revision the socket was
 * bound to, not only the key this socket chose - so a key another member of the
 * ring could be quoted with is masked in its raw, JSON-escaped and
 * percent-encoded spellings alike.
 */

/// One frame from the upstream, archived as the exact text that arrived and
/// before the relay parses, uses or discards it: `setupComplete`, usage, an
/// unknown shape, an error payload and a frame that is not JSON at all are all
/// evidence, and a malformed frame is exactly the one worth keeping. A frame
/// this relay could not parse is recorded as text, not as a JSON value: the
/// bytes that arrived are the body, whatever they are.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_upstream_frame(state: &Relay, raw: &str) {
    let Some(trace) = state.trace.as_ref() else {
        return;
    };
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => {
            trace.content(
                ContentKind::LiveMessage,
                ContentFormat::Json,
                diagnostic_json(&value, state.ring.keys()),
            );
        }
        Err(_) => {
            trace.content(
                ContentKind::LiveMessage,
                ContentFormat::Text,
                diagnostic_text(raw, state.ring.keys()),
            );
        }
    }
}

/// One frame from the upstream that is not UTF-8 text. What went wrong is the
/// encoding, so the record carries the exact bytes that arrived rather than a
/// byte count: they are redacted where they are valid UTF-8 - using the same
/// pinned ring as every other record - and then base64-encoded, because the
/// bytes as a whole are not text and must not be offered as if they were.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_undecodable_frame(state: &Relay, bytes: &[u8]) {
    let Some(trace) = state.trace.as_ref() else {
        return;
    };
    trace.content(
        ContentKind::LiveMessage,
        ContentFormat::Json,
        opaque_bytes_record("binary", bytes, state.ring.keys()),
    );
}

/// One control frame's payload, archived as its own kind: a `ping`/`pong`
/// carries bytes too, and they are evidence of what the socket layer exchanged.
/// Small by construction, redacted and encoded exactly like an undecodable
/// frame. Observing it never sends anything back.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_control_frame(state: &Relay, kind: &str, payload: &[u8]) {
    let Some(trace) = state.trace.as_ref() else {
        return;
    };
    trace.content(
        ContentKind::LiveMessage,
        ContentFormat::Json,
        opaque_bytes_record(kind, payload, state.ring.keys()),
    );
}

/// The one shape every byte-carrying observation uses: the frame's kind, the
/// transport encoding and the redacted bytes as base64, with the original
/// length beside them. The bytes are redacted before they are encoded, so a key
/// that was valid UTF-8 inside them is masked while an octet that is not text
/// at all survives exactly. The record is serialized directly - never put
/// through the JSON redactor a second time, which could mask an encoded span by
/// coincidence.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn opaque_bytes_record(kind: &str, bytes: &[u8], keys: &[Arc<SecretKey>]) -> String {
    let safe = diagnostic_bytes(bytes, keys);
    json!({
        "frame": kind,
        "encoding": "base64",
        "data": base64::engine::general_purpose::STANDARD.encode(safe.as_ref()),
        "bytes": bytes.len(),
    })
    .to_string()
}

/// One message the relay wrote to the browser, archived as the exact JSON text
/// it sent: `ready`, `interim`, `final`, `error` and `done`. What the browser
/// received is therefore reconstructible without a second recorder.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_browser_message(state: &Relay, payload: &Value) {
    let Some(trace) = state.trace.as_ref() else {
        return;
    };
    trace.content(
        ContentKind::LiveOutput,
        ContentFormat::Json,
        diagnostic_json(payload, state.ring.keys()),
    );
}

/// What the Live API was opened with: the setup frame as it was sent, field
/// for field, redacted the same way, plus which pinned rotation entry this
/// socket bound - a 1-based ordinal and the ring's key count, never the key
/// itself. The send is already counted numerically; this is the configuration
/// those counts belong to, and the entry a 401/429 on this socket came from.
/// The terms of the custom vocabulary are never archived: the frame keeps only
/// how many of them the setup carried, because a term is user text. The
/// dictation-language hints (`languageCodes`) are configuration and stay.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_setup(state: &Relay, setup: &Value) {
    let Some(trace) = state.trace.as_ref() else {
        return;
    };
    let mut frame = setup.clone();
    if let Some(config) = frame
        .pointer_mut("/setup/inputAudioTranscription")
        .and_then(Value::as_object_mut)
    {
        config.remove("customVocabulary");
    }
    let ring = state.ring.keys();
    trace.content(
        ContentKind::LiveSetup,
        ContentFormat::Json,
        diagnostic_json(
            &json!({
                "frame": frame,
                "custom_vocabulary_terms": state.vocabulary.len(),
                "key_ordinal": key_ordinal(&state.key, ring),
                "ring_keys": ring.len(),
            }),
            ring,
        ),
    );
}

/// The upstream's close, archived at the frame that carried it and before any
/// UI wording is derived from it: the code and the reason text as they arrived.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_close(state: &Relay, code: u16, reason: &str) {
    let Some(trace) = state.trace.as_ref() else {
        return;
    };
    trace.content(
        ContentKind::SocketClose,
        ContentFormat::Json,
        diagnostic_json(&json!({ "code": code, "reason": reason }), state.ring.keys()),
    );
}

/// One transport failure, in full and redacted: the whole rendered source chain
/// - a top message such as a socket error often hides its actual cause - with
/// every URL it names masked before anything else, because those URLs carry the
/// key in their query, and with every spelling of every key of the pinned ring
/// redacted. No header ever reaches this call.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_transport_error(state: &Relay, detail: &str) {
    let Some(trace) = state.trace.as_ref() else {
        return;
    };
    trace.content(
        ContentKind::TransportError,
        ContentFormat::Text,
        diagnostic_text(&mask_urls(detail), state.ring.keys()),
    );
}

/// Replaces every `ws://`/`wss://` URL inside one diagnostic with `[url]`: a
/// socket error may quote the address the relay dialed, and that address holds
/// the key. The scan stops at whitespace or a quote, so it never swallows the
/// wording around the URL.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn mask_urls(detail: &str) -> String {
    const SCHEMES: [&str; 2] = ["wss://", "ws://"];

    let mut masked = String::with_capacity(detail.len());
    let mut rest = detail;
    loop {
        let next = SCHEMES
            .iter()
            .filter_map(|scheme| rest.find(*scheme))
            .min();
        let Some(start) = next else {
            masked.push_str(rest);
            return masked;
        };
        masked.push_str(&rest[..start]);
        masked.push_str("[url]");
        let url = &rest[start..];
        let end = url
            .find(|character: char| character.is_whitespace() || character == '"')
            .unwrap_or(url.len());
        rest = &url[end..];
    }
}

/// What woke the loop. The sockets are borrowed by the select's futures only, so the
/// handlers can take them again by `&mut`.
enum Event {
    Client(Option<Result<ClientMessage, axum::Error>>),
    Upstream(Option<Result<UpstreamMessage, tokio_tungstenite::tungstenite::Error>>),
    Deadline,
}

/* -------------------------------------------------------------------------- */
/* Relay loop                                                                 */
/* -------------------------------------------------------------------------- */

/// Relay one accepted `/api/live` socket to the Live API.
///
/// The caller has already checked the Origin, refused a plain HTTP request and
/// selected `key` from `ring`; nothing here picks another key or retries a failed
/// session. Every message this relay sends or logs is redacted against `ring`,
/// which holds that same key.
pub async fn serve(
    socket: WebSocket,
    provider: Arc<Provider>,
    key: Arc<SecretKey>,
    ring: Arc<KeyRing>,
    vocabulary: Arc<[String]>,
    languages: Arc<[String]>,
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] trace: Option<RequestTrace>,
) {
    // Redaction is only ever as complete as the ring: the socket's own key has to
    // be part of it, and it is, because it was selected from it.
    debug_assert!(ring
        .keys()
        .iter()
        .any(|candidate| Arc::ptr_eq(candidate, &key)));

    let mut socket = socket;
    let mut upstream: Option<Upstream> = None;
    let mut state = Relay::new(
        key,
        ring,
        vocabulary,
        languages,
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        trace,
    );

    loop {
        let next = state.next_deadline();
        let armed = next.is_some();
        let deadline = match next {
            Some(deadline) => tokio::time::sleep_until(deadline),
            None => tokio::time::sleep(IDLE_PARK),
        };

        let event = tokio::select! {
            message = socket.recv() => Event::Client(message),
            message = OptionFuture::from(upstream.as_mut().map(|live| live.next())), if upstream.is_some() => {
                Event::Upstream(message.flatten())
            }
            _ = deadline, if armed => Event::Deadline,
        };

        // Any frame from the browser proves it is still there and pushes the idle
        // deadline out.
        if matches!(event, Event::Client(_)) {
            state.touch();
        }

        match event {
            Event::Client(Some(Ok(ClientMessage::Text(text)))) => {
                let raw = text.as_str().to_owned();
                handle_client(&mut socket, &mut upstream, &mut state, &raw, &provider).await;
            }
            Event::Client(Some(Ok(ClientMessage::Binary(data)))) => {
                match std::str::from_utf8(&data) {
                    Ok(raw) => {
                        let raw = raw.to_owned();
                        handle_client(&mut socket, &mut upstream, &mut state, &raw, &provider)
                            .await;
                    }
                    // The browser only ever sends text frames; binary frames are still
                    // decoded as UTF-8, and undecodable bytes end the session.
                    Err(_) => {
                        // A binary frame the browser never should have sent: the
                        // client ended the request by being unreadable.
                        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                        note_cause(&state, RequestCause::ClientEnd);
                        cancel(&mut socket, &mut upstream, &mut state).await;
                        return;
                    }
                }
            }
            // The browser went away, or its socket failed. Only the upstream is left to
            // close: nothing could be delivered on a socket that is already gone.
            Event::Client(Some(Ok(ClientMessage::Close(_))))
            | Event::Client(None)
            | Event::Client(Some(Err(_))) => {
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                note_cause(&state, RequestCause::ClientCancel);
                cancel(&mut socket, &mut upstream, &mut state).await;
                return;
            }
            Event::Client(Some(Ok(ClientMessage::Ping(_) | ClientMessage::Pong(_)))) => {}
            Event::Upstream(message) => {
                handle_upstream(&mut socket, &mut upstream, &mut state, message).await
            }
            Event::Deadline => handle_deadline(&mut socket, &mut upstream, &mut state).await,
        }

        if state.done {
            return;
        }
    }
}

/* -------------------------------------------------------------------------- */
/* Client messages                                                            */
/* -------------------------------------------------------------------------- */

/// Dispatch one text frame from the browser.
async fn handle_client(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    raw: &str,
    provider: &Provider,
) {
    if state.done {
        return;
    }
    let Some(message) = parse_json_object(raw) else {
        fail(socket, upstream, state, UiMessage::new(MessageId::LiveMalformedJson)).await;
        return;
    };
    let message_type = message.get("type");
    match message_type.and_then(Value::as_str) {
        Some("start") => start(socket, upstream, state, &message, provider).await,
        Some("audio") => push_audio(socket, upstream, state, &message).await,
        Some("end") => end(socket, upstream, state).await,
        _ => {
            // The `String(type)` spelling, for the shapes JSON can deliver.
            let described = match message_type {
                None => "undefined".to_owned(),
                Some(Value::Null) => "null".to_owned(),
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
            };
            fail(
                socket,
                upstream,
                state,
                UiMessage::new(MessageId::LiveUnknownMessageType).with_arg("type", json!(described)),
            )
            .await;
        }
    }
}

/// `start`: bind the mode and open the Live session.
async fn start(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    message: &Value,
    provider: &Provider,
) {
    if state.started {
        return;
    }
    let Some(mode) = message.get("mode").and_then(Mode::from_json) else {
        fail(
            socket,
            upstream,
            state,
            UiMessage::new(MessageId::LiveModeRequired),
        )
        .await;
        return;
    };
    state.started = true;
    state.mode = Some(mode);
    open_upstream(socket, upstream, state, provider).await;
}

/// Opens the Live socket and sends the setup frame.
///
/// The setup payload asks for TEXT responses, automatic activity detection off
/// (the relay drives `activityStart`/`activityEnd` itself) and the mode's input
/// transcription.
async fn open_upstream(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    provider: &Provider,
) {
    let Some(mode) = state.mode else {
        return;
    };

    // Both timers are armed before the handshake finishes, so the setup deadline
    // also bounds the connection attempt and initial setup write.
    let now = Instant::now();
    let ready_at = now + READY_TIMEOUT;
    state.ready_at = Some(ready_at);
    state.hard_at = Some(now + HARD_LIMIT);

    // The key is appended by the provider, server-side only; it never reaches the browser.
    let mut live = match tokio::time::timeout_at(
        ready_at,
        tokio_tungstenite::connect_async(provider.live_url(state.key.as_str())),
    )
    .await
    {
        Ok(Ok((live, _response))) => live,
        Ok(Err(error)) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_cause(state, RequestCause::SetupFailed);
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_transport_error(state, &crate::provider::transport_chain_text(&error));
            let ui = UiMessage::new(MessageId::LiveConnectFailed)
                .with_arg("detail", json!(error.to_string()));
            fail(socket, upstream, state, ui).await;
            return;
        }
        Err(_) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_cause(state, RequestCause::ReadyTimeout);
            fail(
                socket,
                upstream,
                state,
                UiMessage::new(MessageId::LiveSetupTimeout),
            )
            .await;
            return;
        }
    };

    // The setup frame is a send like any other: it is marked in flight before
    // it is awaited, so a setup write that never returns is visible too. The
    // payload is built once here, so the configuration the archive keeps is the
    // exact one this send writes.
    let setup = setup_payload(mode, &state.vocabulary, &state.languages);
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    note_setup(state, &setup);
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(trace) = state.trace.as_ref() {
        trace.send_started(SendKind::Setup);
    }
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    let sent_at = Instant::now();
    match tokio::time::timeout_at(
        ready_at,
        live.send(UpstreamMessage::text(setup.to_string())),
    )
    .await
    {
        Ok(Ok(())) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(trace) = state.trace.as_ref() {
                trace.send_finished_kind(SendKind::Setup, 0, sent_at.elapsed(), true);
            }
        }
        Ok(Err(error)) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(trace) = state.trace.as_ref() {
                trace.send_finished_kind(SendKind::Setup, 0, sent_at.elapsed(), false);
            }
            // A close event or the ready deadline reports a setup that never confirms.
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_transport_error(state, &crate::provider::transport_chain_text(&error));
            warn(
                state.ring.keys(),
                &format!("Could not write to the Live API socket: {error}"),
            );
        }
        Err(_) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(trace) = state.trace.as_ref() {
                trace.send_finished_kind(SendKind::Setup, 0, sent_at.elapsed(), false);
            }
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_cause(state, RequestCause::ReadyTimeout);
            fail(
                socket,
                upstream,
                state,
                UiMessage::new(MessageId::LiveSetupTimeout),
            )
            .await;
            return;
        }
    }

    *upstream = Some(live);
}

/// `audio`: one base64 PCM16 frame, queued until the setup is confirmed.
async fn push_audio(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    message: &Value,
) {
    if !state.started {
        fail(
            socket,
            upstream,
            state,
            UiMessage::new(MessageId::LiveAudioBeforeStart),
        )
        .await;
        return;
    }
    if state.end_requested {
        // The recording is already finalizing.
        return;
    }

    let Some(data) = message
        .get("data")
        .and_then(Value::as_str)
        .filter(|data| !data.is_empty())
    else {
        fail(
            socket,
            upstream,
            state,
            UiMessage::new(MessageId::LiveAudioDataRequired),
        )
        .await;
        return;
    };
    let byte_length = base64_byte_length(data);
    if byte_length <= 0 || byte_length % BYTES_PER_SAMPLE as i64 != 0 {
        fail(
            socket,
            upstream,
            state,
            UiMessage::new(MessageId::LiveAudioPcmRequired),
        )
        .await;
        return;
    }
    if state.bytes + byte_length as u64 > MAX_AUDIO_BYTES {
        fail(
            socket,
            upstream,
            state,
            UiMessage::new(MessageId::LiveAudioTooLong)
                .with_arg("minutes", json!(MAX_RECORDING_SECONDS / 60)),
        )
        .await;
        return;
    }
    state.bytes += byte_length as u64;
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(trace) = state.trace.as_ref() {
        // Counted before the queue decision: every client frame the relay took
        // in is one, including a frame the pre-ready cap eventually drops.
        trace.audio_received(byte_length as u64);
    }

    // The frame's own level is what tells a pause from speech, so the PCM is
    // decoded here in every build, not only where diagnostics run.
    let Ok(pcm) = base64::engine::general_purpose::STANDARD.decode(data) else {
        fail(
            socket,
            upstream,
            state,
            UiMessage::new(MessageId::LiveAudioPcmRequired),
        )
        .await;
        return;
    };
    let samples = (pcm.len() / BYTES_PER_SAMPLE as usize) as u32;
    let frame_ms = samples * 1_000 / SAMPLE_RATE;
    let quiet = state.pauses.observe(frame_level_db(&pcm), frame_ms);

    if !state.upstream_ready {
        // Setup is still in flight: hold the frames and replay them in order.
        state.queue.push(data.to_owned());
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        if let Some(trace) = state.trace.as_ref() {
            trace.pre_ready_queue_depth(state.queue.len() as u64);
        }
        return;
    }

    if state.activity == ActivityPhase::Closing {
        // Our `activityEnd` was written: an `activityStart` sent now would be
        // ignored, so the audio waits for the server's own `ACTIVITY_END`.
        state.held.push((data.to_owned(), samples));
        state.held_voice |= !quiet;
        return;
    }

    forward_frame(upstream, state, data, samples).await;
    let activity_ms = state.activity_samples * 1_000 / SAMPLE_RATE as u64;
    if !state.end_requested && rotation_due(activity_ms, state.pauses.quiet_ms) {
        // The activity is closed at a pause, well before the ~220 s the Live
        // API would end it on its own.
        write_upstream(
            upstream,
            state,
            &json!({ "realtimeInput": { "activityEnd": {} } }),
        )
        .await;
        let now = Instant::now();
        state.activity = ActivityPhase::Closing;
        state.rotate_at = Some(now + ROTATE_TIMEOUT);
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        {
            state.closing_since = Some(now);
            state.closing_reason = Some(if activity_ms >= ROTATE_FORCE_MS {
                RotateReason::Limit
            } else {
                RotateReason::Pause
            });
        }
    }
}

/// `end`: stop the recording and wait for the final transcript.
async fn end(socket: &mut WebSocket, upstream: &mut Option<Upstream>, state: &mut Relay) {
    if state.done {
        return;
    }
    if !state.started {
        fail(socket, upstream, state, UiMessage::new(MessageId::LiveEndBeforeStart)).await;
        return;
    }
    if state.end_requested {
        return;
    }
    state.end_requested = true;
    // The browser asked for the end of the recording: that is the reason this
    // request stops taking audio, whatever the upstream does afterwards.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    note_cause(state, RequestCause::ClientEnd);

    if state.bytes == 0 && state.queue.is_empty() {
        // Nothing was recorded, so there is nothing to finalize.
        finish(socket, upstream, state).await;
        return;
    }
    if state.upstream_ready {
        if state.activity == ActivityPhase::Closing {
            // A rotation is already in flight: its `activityEnd` is written,
            // and the hold resolution below writes ours and arms the deadline
            // once the server ends the activity.
            return;
        }
        write_upstream(
            upstream,
            state,
            &json!({ "realtimeInput": { "activityEnd": {} } }),
        )
        .await;
        arm_final_timeout(state);
    }
}

/* -------------------------------------------------------------------------- */
/* Upstream messages                                                          */
/* -------------------------------------------------------------------------- */

/// Handles one message or close from the Live socket.
///
/// The control frames' payloads are bound here only to feed the archive, so a
/// build without development diagnostics has nothing to read them.
async fn handle_upstream(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    message: Option<Result<UpstreamMessage, tokio_tungstenite::tungstenite::Error>>,
) {
    if state.done {
        return;
    }
    match message {
        Some(Ok(UpstreamMessage::Text(text))) => {
            let raw = text.as_str().to_owned();
            // Archived before the message is parsed, used or dropped: a frame
            // this relay cannot make sense of is evidence of itself.
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_upstream_frame(state, &raw);
            handle_upstream_message(socket, upstream, state, &raw).await;
        }
        Some(Ok(UpstreamMessage::Close(frame))) => {
            let (code, reason) = match frame {
                Some(frame) => (u16::from(frame.code), frame.reason.as_str().to_owned()),
                None => (1005, String::new()),
            };
            // The code is recorded as a number by `upstream_closed`, and the
            // whole close - code and reason as they arrived, redacted - is
            // archived here, before any UI wording is derived from it.
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(trace) = state.trace.as_ref() {
                trace.received_with_details(
                    ReceiveFlags {
                        close: true,
                        ..ReceiveFlags::default()
                    },
                    Some(code),
                    None,
                );
            }
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_close(state, code, &reason);
            upstream_closed(socket, upstream, state, code, &reason).await;
        }
        Some(Ok(UpstreamMessage::Binary(data))) => {
            // Binary frames are decoded as UTF-8 and parsed the same way;
            // undecodable bytes are not JSON either, so they are dropped. The
            // archive keeps the text when there is any, and the exact bytes -
            // redacted and encoded - when there is not, so the frame is never
            // silently gone and never reduced to a length.
            match std::str::from_utf8(&data) {
                Ok(raw) => {
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    note_upstream_frame(state, raw);
                    let raw = raw.to_owned();
                    handle_upstream_message(socket, upstream, state, &raw).await;
                }
                Err(_) => {
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    note_undecodable_frame(state, &data);
                    // The packet crossed the socket even though it is not
                    // decodable text, so it counts as one off-shape receive.
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    note_received_other(state);
                }
            }
        }
        // Ping/Pong are answered by the socket layer, but they are still
        // traffic from the upstream: they are counted, and their payload is
        // archived as the bytes it is. Observing them changes nothing about how
        // the socket answers them.
        Some(Ok(UpstreamMessage::Ping(_payload))) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(trace) = state.trace.as_ref() {
                trace.received(ReceiveFlags {
                    ping: true,
                    ..ReceiveFlags::default()
                });
            }
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_control_frame(state, "ping", _payload.as_ref());
        }
        Some(Ok(UpstreamMessage::Pong(_payload))) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(trace) = state.trace.as_ref() {
                trace.received(ReceiveFlags {
                    pong: true,
                    ..ReceiveFlags::default()
                });
            }
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_control_frame(state, "pong", _payload.as_ref());
        }
        // The message crate documents `Frame` as a value the read path does not
        // produce; the arm exists for the counter, and if one ever does arrive
        // its payload is archived as the bytes it is rather than dropped.
        Some(Ok(UpstreamMessage::Frame(_frame))) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(trace) = state.trace.as_ref() {
                trace.received(ReceiveFlags {
                    other: true,
                    ..ReceiveFlags::default()
                });
            }
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_control_frame(state, "frame", _frame.payload());
        }
        Some(Err(error)) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(trace) = state.trace.as_ref() {
                trace.received_with_details(
                    ReceiveFlags {
                        error: true,
                        ..ReceiveFlags::default()
                    },
                    None,
                    None,
                );
            }
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_transport_error(state, &crate::provider::transport_chain_text(&error));
            // This only warns; the close event (or the ready deadline) reports the
            // failure, and a terminated stream yields `None` next.
            warn(
                state.ring.keys(),
                &format!("Live API socket reported an error: {error}"),
            );
        }
        // End of stream without a close frame: the browser API reports that as 1006.
        None => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            if let Some(trace) = state.trace.as_ref() {
                // Nothing arrived here, so this is not a received message and no
                // receive count moves; the known code and its single close
                // boundary are still recorded.
                trace.boundary(UpstreamKind::Close, Some(1006), None);
            }
            // No close frame arrived, so the archive records the 1006 this
            // end of stream means with no reason at all - the same code the
            // numeric boundary above carries.
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_close(state, 1006, "");
            upstream_closed(socket, upstream, state, 1006, "").await;
        }
    }
}

/// Handles one JSON message from the Live socket.
async fn handle_upstream_message(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    raw: &str,
) {
    if state.done {
        return;
    }
    if raw.is_empty() {
        // An empty frame is still a packet that arrived: it is archived, and
        // counted once, even though no message comes out of it.
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        note_received_other(state);
        return;
    }
    let Some(message) = parse_json_object(raw) else {
        // Not JSON: the frame did arrive, so it is counted once as off-shape -
        // exactly the receive the parsed path below would have counted.
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        note_received_other(state);
        return;
    };

    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(trace) = state.trace.as_ref() {
        // One call per message, whatever it turned out to be: every class it
        // carried is counted once, and a message of none of the known shapes is
        // counted as `other` instead of vanishing. A `goAway` contributes the
        // duration it announced, as a number; the frame itself is archived as
        // content before this runs.
        trace.received_with_details(
            receive_flags(&message),
            None,
            go_away_time_left(&message),
        );
    }

    if message.get("setupComplete").is_some() {
        state.ready_at = None;
        state.upstream_ready = true;
        write_client(socket, state, &json!({ "type": "ready" })).await;
        open_activity(upstream, state).await;
        flush_audio_queue(upstream, state).await;
        if state.end_requested {
            // `end` arrived while the setup was still in flight.
            write_upstream(
                upstream,
                state,
                &json!({ "realtimeInput": { "activityEnd": {} } }),
            )
            .await;
            arm_final_timeout(state);
        }
        return;
    }

    if let Some(failure) = upstream_error_message(&message) {
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        note_cause(state, RequestCause::UpstreamError);
        fail(
            socket,
            upstream,
            state,
            UiMessage::new(MessageId::LiveApiError).with_arg("detail", json!(failure)),
        )
        .await;
        return;
    }

    if let Some(content) = message.get("serverContent") {
        if let Some(interim) = content.get("interimInputTranscription").and_then(text_of) {
            // Speech the upstream has heard but not yet finalized: an activity that
            // ends now would lose it.
            state.pending_speech = true;
            write_client(
                socket,
                state,
                &json!({ "type": "interim", "text": interim }),
            )
            .await;
        }

        if let Some(final_text) = content.get("inputTranscription").and_then(text_of) {
            write_client(
                socket,
                state,
                &json!({ "type": "final", "text": final_text }),
            )
            .await;
            state.pending_speech = false;
            if state.end_requested && state.activity != ActivityPhase::Closing {
                if !state.server_activity_started {
                    state.confirmed = true;
                }
                if state.confirmed {
                    state.settle_at = Some(Instant::now() + FINAL_SETTLE);
                }
            }
        }

        // `input_transcription` is not guaranteed to arrive before the turn boundary, so
        // turnComplete only shortens the wait; the settle timer still guarantees `done`.
        if json_truthy(content.get("turnComplete")) && state.end_requested
            && state.activity != ActivityPhase::Closing && state.confirmed
        {
            state.settle_at = Some(Instant::now() + TURN_SETTLE);
        }
    }
    // The Live API's own activity boundaries are top-level, outside
    // `serverContent`, and the whole rotation rests on them: an activity it
    // ended by itself, and one the relay closed at a pause, are resolved here.
    if let Some(voice_activity) = message.get("voiceActivity") {
        match voice_activity.get("type").and_then(Value::as_str) {
            Some("ACTIVITY_START") => {
                state.server_activity_started = true;
                state.server_start_offset =
                    parse_audio_offset(&voice_activity["audioOffset"]);
            }
            Some("ACTIVITY_END") => {
                let cut = activity_cut(state, &voice_activity["audioOffset"]);
                if state.end_requested {
                    state.confirmed = true;
                }
                if state.activity == ActivityPhase::Closing {
                    if state.pending_speech {
                        // The activity the relay closed had interim text whose
                        // final never arrived: its tail is lost, and only the
                        // recording can still recover it.
                        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                        note_cause(state, RequestCause::ActivityIncomplete);
                        fail(
                            socket,
                            upstream,
                            state,
                            UiMessage::new(MessageId::LiveIncomplete),
                        )
                        .await;
                        return;
                    }
                    state.rotate_at = None;
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    let rotation = ActivityRotate {
                        index: state.activity_index,
                        reason: state.closing_reason.unwrap_or(RotateReason::Pause),
                        activity_audio_ms: state.activity_samples * 1_000 / SAMPLE_RATE as u64,
                        pause_ms: state.pauses.quiet_ms,
                        wait_us: state.closing_since
                            .map(|since| since.elapsed().as_micros() as u64)
                            .unwrap_or(0),
                        held_frames: state.held.len() as u32,
                        replayed_frames: 0,
                    };
                    if state.end_requested && !state.held_voice {
                        // The dictation is over and the rotation held nothing
                        // but silence: the silent tail is dropped, not replayed
                        // into one more activity.
                        state.activity = ActivityPhase::Open;
                        state.held.clear();
                        state.held_voice = false;
                        state.settle_at = Some(Instant::now() + TURN_SETTLE);
                    } else {
                        let held = std::mem::take(&mut state.held);
                        open_activity(upstream, state).await;
                        for (data, samples) in held {
                            forward_frame(upstream, state, &data, samples).await;
                        }
                        state.held_voice = false;
                        state.activity_index += 1;
                        if state.end_requested {
                            // `end` arrived while the rotation was in flight:
                            // its own `activityEnd` was deferred to here.
                            write_upstream(
                                upstream,
                                state,
                                &json!({ "realtimeInput": { "activityEnd": {} } }),
                            )
                            .await;
                            arm_final_timeout(state);
                        }
                    }
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    note_rotation(state, rotation);
                } else if !state.end_requested
                    || cut.is_some_and(|cut| state.sent_samples.saturating_sub(cut) > 3_200)
                {
                    // The server ended the activity on its own, which it does
                    // after roughly 220 s: the audio it never transcribed is
                    // replayed from the offset it named.
                    if state.pending_speech {
                        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                        note_cause(state, RequestCause::ActivityIncomplete);
                        fail(
                            socket,
                            upstream,
                            state,
                            UiMessage::new(MessageId::LiveIncomplete),
                        )
                        .await;
                        return;
                    }
                    // The same offset calculation applies to cuts arriving
                    // before or after the browser's end request.
                    let replay: Vec<(String, u32)> = match cut {
                        None => Vec::new(),
                        Some(cut) => state
                            .recent
                            .iter()
                            .filter(|(start, samples, _)| start + u64::from(*samples) > cut)
                            .map(|(_, samples, data)| (data.clone(), *samples))
                            .collect(),
                    };
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    let rotation = ActivityRotate {
                        index: state.activity_index,
                        reason: RotateReason::ServerCut,
                        activity_audio_ms: state.activity_samples * 1_000 / SAMPLE_RATE as u64,
                        pause_ms: state.pauses.quiet_ms,
                        wait_us: 0,
                        held_frames: 0,
                        replayed_frames: replay.len() as u32,
                    };
                    open_activity(upstream, state).await;
                    state.activity_index += 1;
                    for (data, samples) in replay {
                        forward_frame(upstream, state, &data, samples).await;
                    }
                    if state.end_requested {
                        write_upstream(
                            upstream,
                            state,
                            &json!({ "realtimeInput": { "activityEnd": {} } }),
                        ).await;
                        arm_final_timeout(state);
                    }
                    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                    note_rotation(state, rotation);
                } else {
                    // The activity the relay ended for the dictation's own end:
                    // the turn boundary is the last thing expected here.
                    state.settle_at = Some(Instant::now() + TURN_SETTLE);
                }
            }
            _ => {}
        }
    }
}

/// The Live socket closed.
async fn upstream_closed(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    code: u16,
    reason: &str,
) {
    if state.done {
        return;
    }
    let detail = describe_close(code, reason);
    // The socket closed on us: whether that is the expected end or a failure
    // is decided below, and the cause is recorded before the terminal.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    note_cause(state, RequestCause::UpstreamClose);
    // The numeric code this close carried - a close frame's own, or the 1006 an
    // end of stream reports. Recorded here for every caller of this handler.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(trace) = state.trace.as_ref() {
        trace.close_code(code);
    }
    if !state.upstream_ready {
        let ui = match detail {
            None => UiMessage::new(MessageId::LiveSetupClosedBeforeReady),
            Some(detail) => UiMessage::new(MessageId::LiveSetupClosedWithDetail)
                .with_arg("detail", detail),
        };
        fail(
            socket,
            upstream,
            state,
            ui,
        )
        .await;
        return;
    }
    if state.end_requested {
        if !state.confirmed || state.pending_speech || state.activity == ActivityPhase::Closing {
            // The recording ended, but the upstream had speech it never
            // finalized, or an activity the relay closed was never ended by the
            // server: the transcript this session has is not the whole one.
            fail(
                socket,
                upstream,
                state,
                UiMessage::new(MessageId::LiveIncomplete),
            )
            .await;
            return;
        }
        finish(socket, upstream, state).await;
        return;
    }
    let ui = match detail {
        None => UiMessage::new(MessageId::LiveConnectionClosed),
        Some(detail) => UiMessage::new(MessageId::LiveConnectionClosedWithDetail)
            .with_arg("detail", detail),
    };
    fail(
        socket,
        upstream,
        state,
        ui,
    )
    .await;
}

/// Human-readable close reason: Google carries an `{"error":{"message":...}}`
/// payload in the close reason, anything else is text.
fn describe_close(code: u16, reason: &str) -> Option<Value> {
    let reason = reason.trim();
    if !reason.is_empty() {
        let message = parse_json_object(reason)
            .as_ref()
            .and_then(upstream_error_message)
            .unwrap_or_else(|| reason.to_owned());
        return Some(json!(message));
    }
    (code != 1000).then(|| json!(UiMessage::new(MessageId::CloseCode)
        .with_arg("code", json!(code))))
}

/* -------------------------------------------------------------------------- */
/* Timers                                                                     */
/* -------------------------------------------------------------------------- */

/// The earliest expired deadline decides, exactly like a timer that was scheduled first.
async fn handle_deadline(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
) {
    if state.done {
        return;
    }
    let now = Instant::now();
    let mut fired: Option<(Instant, Timer)> = None;
    for (deadline, timer) in [
        (state.ready_at, Timer::Ready),
        (state.final_at, Timer::Final),
        (state.rotate_at, Timer::Rotate),
        (state.settle_at, Timer::Settle),
        (state.hard_at, Timer::Hard),
        (state.idle_at, Timer::Idle),
    ] {
        if let Some(deadline) = deadline {
            if deadline <= now && fired.map_or(true, |(earliest, _)| deadline < earliest) {
                fired = Some((deadline, timer));
            }
        }
    }

    match fired.map(|(_, timer)| timer) {
        Some(Timer::Ready) => {
            state.ready_at = None;
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_deadline(state, DeadlineTimer::Ready);
            if !state.upstream_ready {
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                note_cause(state, RequestCause::ReadyTimeout);
                fail(
                    socket,
                    upstream,
                    state,
                    UiMessage::new(MessageId::LiveSetupTimeout),
                )
                .await;
            }
        }
        Some(Timer::Final) => {
            state.final_at = None;
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_deadline(state, DeadlineTimer::Final);
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_cause(state, RequestCause::FinalTimeout);
            // The upstream never sent the transcript the recording was ended
            // for: whatever arrived is a prefix, and only the recording can
            // still be transcribed whole.
            fail(
                socket,
                upstream,
                state,
                UiMessage::new(MessageId::LiveIncomplete),
            )
            .await;
        }
        Some(Timer::Rotate) => {
            state.rotate_at = None;
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_deadline(state, DeadlineTimer::Rotate);
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_cause(state, RequestCause::RotateTimeout);
            // The server never ended the activity the relay closed: the audio
            // held behind it is unrecoverable upstream.
            fail(
                socket,
                upstream,
                state,
                UiMessage::new(MessageId::LiveIncomplete),
            )
            .await;
        }
        Some(Timer::Settle) => {
            state.settle_at = None;
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_deadline(state, DeadlineTimer::Settle);
            if !state.confirmed || state.pending_speech || state.activity == ActivityPhase::Closing {
                // The upstream heard speech and never finalized it: this is not
                // a completed dictation.
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                note_cause(state, RequestCause::ActivityIncomplete);
                fail(
                    socket,
                    upstream,
                    state,
                    UiMessage::new(MessageId::LiveIncomplete),
                )
                .await;
            } else {
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                note_cause(state, RequestCause::SettleTimeout);
                finish(socket, upstream, state).await;
            }
        }
        Some(Timer::Hard) => {
            state.hard_at = None;
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_deadline(state, DeadlineTimer::Hard);
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_cause(state, RequestCause::HardDeadline);
            fail(
                socket,
                upstream,
                state,
                UiMessage::new(MessageId::LiveSessionTooLong)
                    .with_arg("minutes", json!(MAX_RECORDING_SECONDS / 60)),
            )
            .await;
        }
        Some(Timer::Idle) => {
            // No verdict to report: the socket itself timed out, so the browser sees
            // the close and nothing else.
            state.idle_at = None;
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_deadline(state, DeadlineTimer::Idle);
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_cause(state, RequestCause::IdleTimeout);
            cancel(socket, upstream, state).await;
        }
        // Nothing was actually due: a wake that arrived after the timer was cleared.
        None => {}
    }
}

/// After `activityEnd` only the spoken tail is outstanding; the whole deadline
/// is armed whatever the transcript state is, because an activity that was not
/// confirmed as final has to be reported as incomplete.
fn arm_final_timeout(state: &mut Relay) {
    state.final_at = Some(Instant::now() + FINAL_TIMEOUT);
}

/* -------------------------------------------------------------------------- */
/* Terminal states                                                            */
/* -------------------------------------------------------------------------- */

/// Terminal failure: report it, then always finish with `done` so the UI cannot hang.
async fn fail(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    mut ui: UiMessage,
) {
    if state.done {
        return;
    }
    // The terminal that decides this request's outcome: a failure reports
    // `Error` even though it still closes with the same `done` as a success.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    note_terminal(state, Outcome::Error);
    redact_ui(&mut ui, state.ring.keys());
    let safe = redact(&render(Language::En, &ui), state.ring.keys());
    write_client(
        socket,
        state,
        &json!({ "type": "error", "message": safe, "ui": ui }),
    )
    .await;
    finish(socket, upstream, state).await;
}

/// Terminal success: exactly one `done`, then both sockets close.
async fn finish(socket: &mut WebSocket, upstream: &mut Option<Upstream>, state: &mut Relay) {
    if state.done {
        return;
    }
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    note_terminal(state, Outcome::Success);
    state.done = true;
    state.clear_timers();
    write_client(socket, state, &json!({ "type": "done" })).await;
    close_upstream(upstream).await;
    let _ = socket
        .send(ClientMessage::Close(Some(CloseFrame {
            code: 1000,
            reason: "done".into(),
        })))
        .await;
}

/// The browser socket is gone: no more messages to it, but the Live session is
/// closed so the provider does not hold the upstream open.
async fn cancel(socket: &mut WebSocket, upstream: &mut Option<Upstream>, state: &mut Relay) {
    state.done = true;
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    note_terminal(state, Outcome::Cancelled);
    state.clear_timers();
    close_upstream(upstream).await;
    let _ = socket.send(ClientMessage::Close(None)).await;
}

/// Closes the Live socket once; taking it out of the option makes that structural.
async fn close_upstream(upstream: &mut Option<Upstream>) {
    if let Some(mut live) = upstream.take() {
        let _ = live.close(None).await;
    }
}

/* -------------------------------------------------------------------------- */
/* Socket writes                                                              */
/* -------------------------------------------------------------------------- */

/// Best-effort write: a browser socket that went away is noticed by the read half
/// of the loop, not by this call.
///
/// The payload is archived as the exact text about to be written, so every
/// transformation between the provider's message and what the browser receives
/// - a filtered interim, a promoted final, a mapped error - is visible next to
/// the frame it came from.
async fn write_client(socket: &mut WebSocket, state: &Relay, payload: &Value) {
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    note_browser_message(state, payload);
    if let Err(error) = socket.send(ClientMessage::text(payload.to_string())).await {
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        note_transport_error(state, &crate::provider::transport_chain_text(&error));
        warn(
            state.ring.keys(),
            &format!("Could not write to the browser socket: {error}"),
        );
    }
}

/// Best-effort write: a lost Live socket is reported by the close event (or the
/// ready deadline), not by this call.
async fn write_upstream(upstream: &mut Option<Upstream>, state: &Relay, payload: &Value) {
    let Some(live) = upstream.as_mut() else {
        return;
    };
    if !state.upstream_ready {
        return;
    }
    // In flight before the await, duration and result after: an independent
    // reader of the counters sees a send that never returned.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(trace) = state.trace.as_ref() {
        trace.send_started(send_kind_of(payload));
    }
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    let sent_at = Instant::now();
    let sent = live.send(UpstreamMessage::text(payload.to_string())).await;
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(trace) = state.trace.as_ref() {
        trace.send_finished_kind(
            send_kind_of(payload),
            send_pcm_bytes(payload),
            sent_at.elapsed(),
            sent.is_ok(),
        );
    }
    if let Err(error) = sent {
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        note_transport_error(state, &crate::provider::transport_chain_text(&error));
        warn(
            state.ring.keys(),
            &format!("Could not write to the Live API socket: {error}"),
        );
    }
}

/// Replays the audio that arrived while the setup was still in flight, in order.
async fn flush_audio_queue(upstream: &mut Option<Upstream>, state: &mut Relay) {
    if state.queue.is_empty() {
        return;
    }
    let frames = std::mem::take(&mut state.queue);
    for frame in frames {
        let samples = (base64_byte_length(&frame) / BYTES_PER_SAMPLE as i64) as u32;
        forward_frame(upstream, state, &frame, samples).await;
    }
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(trace) = state.trace.as_ref() {
        // The queue is empty now, so the last depth a reader sees is zero.
        trace.pre_ready_queue_depth(0);
    }
}

/// The one path every frame that goes upstream takes: the audio write, the
/// counters and the replay window move together, so a frame that was forwarded
/// is always one a server-side cut can replay.
async fn forward_frame(
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    data: &str,
    samples: u32,
) {
    write_upstream(upstream, state, &audio_payload(data)).await;
    let start = state.sent_samples;
    state.sent_samples += samples as u64;
    state.activity_samples += samples as u64;
    state.recent.push_back((start, samples, data.to_owned()));
    // Only what a cut can still need is kept: everything older than the replay
    // window is gone.
    while let Some((begin, _, _)) = state.recent.front() {
        if state.sent_samples - begin >= REPLAY_WINDOW_SAMPLES {
            state.recent.pop_front();
        } else {
            break;
        }
    }
}

fn activity_cut(state: &Relay, end_offset: &Value) -> Option<u64> {
    let end = parse_audio_offset(end_offset)?;
    let start = state.server_start_offset?;
    if end < start {
        return None;
    }
    let cut = state.activity_start_sample
        + ((end - start) * SAMPLE_RATE as f64).round() as u64;
    state.recent.front()
        .filter(|(oldest, _, _)| cut >= *oldest)
        .map(|_| cut)
}

/// Opens a new activity and resets what belongs to one: the audio the next
/// activity starts at, its own pause detector and the offset the upstream has
/// not announced yet.
async fn open_activity(upstream: &mut Option<Upstream>, state: &mut Relay) {
    write_upstream(
        upstream,
        state,
        &json!({ "realtimeInput": { "activityStart": {} } }),
    )
    .await;
    state.activity_start_sample = state.sent_samples;
    state.activity_samples = 0;
    state.server_start_offset = None;
    state.server_activity_started = false;
    state.confirmed = false;
    state.settle_at = None;
    state.final_at = None;
    state.activity = ActivityPhase::Open;
    state.pauses = PauseDetector::new();
}

/// Local diagnostics only, never a key: a windowed EXE has no console, so these
/// go to stderr in debug builds only.
fn warn(keys: &[Arc<SecretKey>], message: &str) {
    if cfg!(debug_assertions) {
        eprintln!("live: {}", redact(message, keys));
    }
}

/* -------------------------------------------------------------------------- */
/* Payloads and JSON narrowing                                                */
/* -------------------------------------------------------------------------- */

/// The exact setup frame the Live API expects, field for field. A non-empty
/// `vocabulary` rides in `inputAudioTranscription` as `customVocabulary` and
/// non-empty dictation-language hints as `languageCodes`; empty ones leave the
/// frame exactly as it was before either existed (automatic detection).
fn setup_payload(mode: Mode, vocabulary: &[String], languages: &[String]) -> Value {
    let mut transcription = json!({ "mode": mode.live_value() });
    if !vocabulary.is_empty() {
        transcription["customVocabulary"] = json!(vocabulary);
    }
    if !languages.is_empty() {
        transcription["languageCodes"] = json!(languages);
    }
    json!({
        "setup": {
            "model": format!("models/{LIVE_MODEL}"),
            "generationConfig": { "responseModalities": ["TEXT"] },
            "realtimeInputConfig": { "automaticActivityDetection": { "disabled": true } },
            "inputAudioTranscription": transcription,
        }
    })
}

/// One audio frame in the Live API's realtime input shape.
fn audio_payload(data: &str) -> Value {
    json!({
        "realtimeInput": {
            "audio": { "data": data, "mimeType": format!("audio/pcm;rate={SAMPLE_RATE}") },
        }
    })
}

/// Untrusted JSON boundary: null, arrays and unparsable text are rejected, plain
/// objects pass through.
fn parse_json_object(raw: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => Some(value),
        _ => None,
    }
}

/// Reads a nested `{ text: string }` transcription node, ignoring blank payloads.
fn text_of(value: &Value) -> Option<String> {
    let text = value.get("text")?.as_str()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Reads the OpenAI-style `{ error: { message } }` node Google returns.
fn upstream_error_message(payload: &Value) -> Option<String> {
    let message = payload.get("error")?.get("message")?.as_str()?;
    let trimmed = message.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// JavaScript truthiness for the flags the Live API sends, so a `false` flag does not
/// read like a set one.
fn json_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

/// Byte length of a base64 string without decoding it.
fn base64_byte_length(data: &str) -> i64 {
    let padding: i64 = if data.ends_with("==") {
        2
    } else if data.ends_with('=') {
        1
    } else {
        0
    };
    let length = data.len() as i64;
    // Padding never exceeds the four-character quantum the length is counted in.
    length * 3 / 4 - padding
}

/* -------------------------------------------------------------------------- */
/* Tests                                                                      */
/* -------------------------------------------------------------------------- */

/// The relay's own instrumentation, driven through real loopback sockets: a
/// real `serve` task behind an ephemeral port, a real fake Live socket and the
/// real writer of the diagnostics sink.
///
/// What is asserted is the journal the sink actually wrote - the deadline that
/// fired, the cause of the terminal, the counters of the stall - and that no
/// synthetic secret of the upstream ever reaches it. Nothing here reads the
/// process environment, touches a real API key or changes a production path.
#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
mod relay_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::SystemTime;

    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    use crate::diagnostics::{
        Diagnostics, Mode as DiagMode, Owner, Policy, Route, SessionKey,
    };

    /// One synthetic key. It is a constant of the test, not a secret of any
    /// account, and it is exactly what the privacy assertions look for.
    const SYNTHETIC_KEY: &str = "speechek-synthetic-key-9f3b7c1d";
    const SYNTHETIC_SECRET: &str = "speechek-synthetic-secret-4e5f6a7b";

    /// A root of this test's own, under the temporary directory.
    fn scratch(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "speechek-live-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// A sink with its real writer, and one open Live take in it.
    fn take(root: &Path) -> (Arc<Diagnostics>, RequestTrace) {
        let diagnostics = Diagnostics::for_test_with_storage(
            root,
            Policy {
                max_age: Duration::from_secs(600),
                max_bytes: 100_000_000,
            },
            SystemTime::now(),
        );
        let key = SessionKey {
            owner: Owner::Dictation,
            generation: 1,
        };
        let handle = diagnostics.begin(key, DiagMode::Live).expect("an open take");
        let trace = handle.request(Route::Live).expect("a request slot");
        (diagnostics, trace)
    }

    /// A fake Live socket on its own ephemeral port. It answers the setup with
    /// every message in `reply` and then, once the relay has sent its first
    /// audio frame, sends `after_audio` (an upstream error, say) and closes
    /// with `close`. Every frame the relay sends is forwarded to the test.
    ///
    /// Tying those replies to the audio is what makes the test deterministic:
    /// what the test wants to observe can never race the frames the relay is
    /// still receiving.
    async fn fake_upstream(
        reply: Vec<Value>,
        after_audio: Option<Value>,
        close: Option<(u16, String)>,
    ) -> (u16, mpsc::Receiver<Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            // The setup frame; its shape is pinned by the payload tests.
            let _ = ws.next().await;
            for message in reply {
                if ws.send(WsMessage::text(message.to_string())).await.is_err() {
                    return;
                }
            }
            if after_audio.is_none() {
                if let Some((code, reason)) = &close {
                    let _ = ws
                        .send(WsMessage::Close(Some(CloseFrame {
                            code: (*code).into(),
                            reason: reason.as_str().into(),
                        })))
                        .await;
                }
            }
            let mut after_audio = after_audio;
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    WsMessage::Text(text) => {
                        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                            continue;
                        };
                        let is_audio = value["realtimeInput"]["audio"].is_object();
                        if tx.send(value).await.is_err() {
                            return;
                        }
                        if !is_audio {
                            continue;
                        }
                        if let Some(extra) = after_audio.take() {
                            if ws.send(WsMessage::text(extra.to_string())).await.is_err() {
                                return;
                            }
                        }
                        if let Some((code, reason)) = &close {
                            let _ = ws
                                .send(WsMessage::Close(Some(CloseFrame {
                                    code: (*code).into(),
                                    reason: reason.as_str().into(),
                                })))
                                .await;
                        }
                    }
                    WsMessage::Close(_) => return,
                    _ => {}
                }
            }
        });
        (port, rx)
    }

    /// A fake Live socket that answers the setup with `reply` and, once the
    /// relay has sent its first audio frame, drops the TCP connection without a
    /// close frame - the transport drop the browser reports as 1006.
    async fn abrupt_upstream(reply: Vec<Value>) -> (u16, mpsc::Receiver<Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let _ = ws.next().await;
            for message in reply {
                if ws.send(WsMessage::text(message.to_string())).await.is_err() {
                    return;
                }
            }
            while let Some(Ok(message)) = ws.next().await {
                let WsMessage::Text(text) = message else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                let is_audio = value["realtimeInput"]["audio"].is_object();
                if tx.send(value).await.is_err() {
                    return;
                }
                if is_audio {
                    // Drop the socket here: no close frame follows.
                    return;
                }
            }
        });
        (port, rx)
    }

    /// The real `serve` behind an ephemeral port, reached exactly like the
    /// backend reaches it.
    async fn relay(
        upstream_port: u16,
        vocabulary: Arc<[String]>,
        trace: Option<RequestTrace>,
    ) -> (u16, Arc<SecretKey>, Arc<KeyRing>) {
        let provider = Arc::new(Provider::for_test(
            "http://127.0.0.1:1",
            &format!("ws://127.0.0.1:{upstream_port}"),
        ));
        let key = Arc::new(SecretKey::new(SYNTHETIC_KEY.to_string()));
        let ring = Arc::new(KeyRing::new(vec![Arc::clone(&key)]));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let serve_key = Arc::clone(&key);
        let serve_ring = Arc::clone(&ring);
        let serve_vocabulary = vocabulary;
        let app = axum::Router::new().route(
            "/api/live",
            axum::routing::any(move |upgrade: axum::extract::ws::WebSocketUpgrade| {
                let provider = Arc::clone(&provider);
                let key = Arc::clone(&serve_key);
                let ring = Arc::clone(&serve_ring);
                let vocabulary = Arc::clone(&serve_vocabulary);
                let trace = trace.clone();
                async move {
                    upgrade.on_upgrade(move |socket| async move {
                        serve(socket, provider, key, ring, vocabulary, Arc::default(), trace)
                            .await;
                    })
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, key, ring)
    }

    /// A browser socket on the relay, and the first frame it reads.
    async fn client(port: u16) -> tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    > {
        let (client, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{port}/api/live"
        ))
        .await
        .expect("the relay accepts");
        client
    }

    /// Waits for the frame the upstream was sent that satisfies `done`.
    async fn upstream_until(
        received: &mut mpsc::Receiver<Value>,
        mut done: impl FnMut(&Value) -> bool,
    ) -> Value {
        loop {
            let value = received.recv().await.expect("the upstream saw a frame");
            if done(&value) {
                return value;
            }
        }
    }

    /// One audio frame of 512 silent samples, base64 as the browser sends it.
    fn audio_frame() -> Value {
        let pcm = vec![0u8; 512 * 2];
        json!({
            "type": "audio",
            "data": base64::engine::general_purpose::STANDARD.encode(&pcm),
        })
    }

    /// One audio frame of 512 samples, all at `level`, base64 as the browser
    /// sends it: the payload itself is what a replay has to reproduce.
    fn level_frame(level: i16) -> Value {
        let mut pcm = Vec::with_capacity(512 * 2);
        for _ in 0..512 {
            pcm.extend_from_slice(&level.to_le_bytes());
        }
        json!({
            "type": "audio",
            "data": base64::engine::general_purpose::STANDARD.encode(&pcm),
        })
    }

    /// The base64 payload of one audio frame the relay sent upstream.
    fn sent_audio(value: &Value) -> Option<&str> {
        value
            .pointer("/realtimeInput/audio/data")
            .and_then(Value::as_str)
    }

    /// Every message the browser received until the relay's one `done`.
    async fn browser_until_done(
        client: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Vec<Value> {
        let mut messages = Vec::new();
        loop {
            match client.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    let done = value["type"] == "done";
                    messages.push(value);
                    if done {
                        return messages;
                    }
                }
                Some(Ok(_)) => continue,
                other => panic!("the relay never finished: {other:?}"),
            }
        }
    }

    /// A fake Live socket that answers every frame the relay sends with the
    /// messages `answer` returns for it and forwards any frame the test injects
    /// into it. Every frame the relay sends is appended to the shared log, so a
    /// test can assert exactly what crossed the socket and in what order -
    /// including what did not cross it.
    /// An injected JSON null closes the fake socket instead of sending a frame.
    async fn scripted_upstream(
        mut answer: impl FnMut(&Value) -> Vec<Value> + Send + 'static,
    ) -> (u16, mpsc::UnboundedSender<Value>, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let upstream_log = Arc::clone(&log);
        let (inject, mut injected) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            loop {
                tokio::select! {
                    message = ws.next() => {
                        let Some(Ok(message)) = message else {
                            return;
                        };
                        let WsMessage::Text(text) = message else {
                            if matches!(message, WsMessage::Close(_)) {
                                return;
                            }
                            continue;
                        };
                        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                            continue;
                        };
                        upstream_log
                            .lock()
                            .expect("the upstream log lock")
                            .push(value.clone());
                        for reply in answer(&value) {
                            if ws.send(WsMessage::text(reply.to_string())).await.is_err() {
                                return;
                            }
                        }
                    }
                    frame = injected.recv() => {
                        let Some(frame) = frame else {
                            return;
                        };
                        // Null is the fixture's explicit upstream-close command.
                        if frame.is_null() {
                            let _ = ws.send(WsMessage::Close(None)).await;
                            return;
                        }
                        if ws.send(WsMessage::text(frame.to_string())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        (port, inject, log)
    }

    async fn end_boundary_case(announced: bool, final_text: bool, boundary: bool) -> Vec<Value> {
        let (upstream_port, inject, log) = scripted_upstream(move |frame| {
            if frame.get("setup").is_some() {
                return vec![json!({ "setupComplete": {} })];
            }
            if announced && frame.pointer("/realtimeInput/activityStart").is_some() {
                return vec![json!({ "voiceActivity": { "type": "ACTIVITY_START", "audioOffset": "0s" } })];
            }
            Vec::new()
        }).await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), None).await;
        let mut client = client(port).await;
        client.send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string())).await.expect("start");
        assert!(client.next().await.expect("ready").expect("ready text").to_text().expect("text").contains("ready"));
        // Exercise upstream finalization, not the empty-recording fast path.
        client.send(WsMessage::text(audio_frame().to_string())).await.expect("silent audio");
        client.send(WsMessage::text(json!({ "type": "end" }).to_string())).await.expect("end");
        let rows = log_when(&log, 300, |rows| rows.iter().any(|row| row.pointer("/realtimeInput/activityEnd").is_some())).await;
        assert!(rows.iter().any(|row| row.pointer("/realtimeInput/activityEnd").is_some()));
        assert_eq!(rows.iter().filter_map(sent_audio).count(), 1);
        if final_text {
            inject.send(json!({ "serverContent": { "inputTranscription": { "text": "tail" } } })).expect("final");
        }
        if boundary {
            inject.send(json!({ "voiceActivity": { "type": "ACTIVITY_END", "audioOffset": "0.032s" } })).expect("boundary");
        }
        inject.send(Value::Null).expect("close");
        tokio::time::timeout(Duration::from_secs(5), browser_until_done(&mut client)).await.expect("terminal messages")
    }

    #[tokio::test]
    async fn close_after_end_without_confirmation_is_incomplete() {
        let messages = end_boundary_case(false, false, false).await;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["ui"]["key"], "LiveIncomplete");
        assert_eq!(messages.last().expect("done")["type"], "done");
    }

    #[tokio::test]
    async fn announced_activity_final_without_end_is_incomplete() {
        let messages = end_boundary_case(true, true, false).await;
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0], json!({ "type": "final", "text": "tail" }));
        assert_eq!(messages[1]["ui"]["key"], "LiveIncomplete");
        assert_eq!(messages.last().expect("done")["type"], "done");
    }

    #[tokio::test]
    async fn announced_activity_final_and_end_close_successfully() {
        let messages = end_boundary_case(true, true, true).await;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], json!({ "type": "final", "text": "tail" }));
        assert_eq!(messages.last().expect("done")["type"], "done");
    }

    #[tokio::test]
    async fn confirmed_silent_boundary_closes_successfully() {
        let messages = end_boundary_case(true, false, true).await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages.last().expect("done")["type"], "done");
    }

    #[tokio::test]
    async fn server_cut_after_browser_end_replays_tail_then_ends_successor() {
        let (upstream_port, inject, log) = scripted_upstream(|frame| {
            if frame.get("setup").is_some() { return vec![json!({ "setupComplete": {} })]; }
            if frame.pointer("/realtimeInput/activityStart").is_some() {
                return vec![json!({ "voiceActivity": { "type": "ACTIVITY_START", "audioOffset": "0s" } })];
            }
            Vec::new()
        }).await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), None).await;
        let mut client = client(port).await;
        client.send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string())).await.expect("start");
        assert!(client.next().await.expect("ready").expect("text").to_text().expect("text").contains("ready"));
        let frames: Vec<Value> = (0..100).map(|index| level_frame(8_000 + index)).collect();
        for frame in &frames {
            client.send(WsMessage::text(frame.to_string())).await.expect("audio");
        }
        client.send(WsMessage::text(json!({ "type": "end" }).to_string())).await.expect("end");
        let before = log_when(&log, 500, |rows| rows.last().is_some_and(|row| row.pointer("/realtimeInput/activityEnd").is_some())).await;
        assert_eq!(before.iter().filter_map(sent_audio).count(), 100);
        inject.send(json!({ "voiceActivity": { "type": "ACTIVITY_END", "audioOffset": "1.6s" } })).expect("cut");
        let after = log_when(&log, 500, |rows| rows.len() >= before.len() + 52).await;
        let tail = &after[before.len()..];
        assert_eq!(tail.len(), 52);
        assert!(tail[0].pointer("/realtimeInput/activityStart").is_some());
        let replay: Vec<&str> = tail.iter().filter_map(sent_audio).collect();
        let expected: Vec<&str> = frames[50..].iter().map(|frame| frame["data"].as_str().expect("base64")).collect();
        assert_eq!(replay, expected);
        assert!(tail[51].pointer("/realtimeInput/activityEnd").is_some());
        inject.send(json!({ "serverContent": { "inputTranscription": { "text": "successor" } }, "voiceActivity": { "type": "ACTIVITY_END", "audioOffset": "1.6s" } })).expect("successor end");
        let messages = tokio::time::timeout(Duration::from_secs(5), browser_until_done(&mut client)).await.expect("done");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["text"], "successor");
        assert_eq!(messages[1]["type"], "done");
    }

    #[tokio::test]
    async fn composite_rotation_final_waits_for_held_voice_successor() {
        let (upstream_port, inject, log) = scripted_upstream(|frame| {
            if frame.get("setup").is_some() { return vec![json!({ "setupComplete": {} })]; }
            if frame.pointer("/realtimeInput/activityStart").is_some() {
                return vec![json!({ "voiceActivity": { "type": "ACTIVITY_START", "audioOffset": "0s" } })];
            }
            Vec::new()
        }).await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), None).await;
        let mut client = client(port).await;
        client.send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string())).await.expect("start");
        assert!(client.next().await.expect("ready").expect("text").to_text().expect("text").contains("ready"));
        for _ in 0..4_688 {
            client.send(WsMessage::text(level_frame(8_000).to_string())).await.expect("speech");
        }
        for _ in 0..19 {
            client.send(WsMessage::text(level_frame(0).to_string())).await.expect("pause");
        }
        let closed = log_when(&log, 500, |rows| rows.last().is_some_and(|row| row.pointer("/realtimeInput/activityEnd").is_some())).await;
        assert!(closed.last().expect("rotation end").pointer("/realtimeInput/activityEnd").is_some());
        let held = level_frame(8_100);
        client.send(WsMessage::text(held.to_string())).await.expect("held voice");
        client.send(WsMessage::text(json!({ "type": "end" }).to_string())).await.expect("end");
        // Let the relay consume held voice and end while its old activity is closing.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(log.lock().expect("log").len(), closed.len());
        inject.send(json!({ "serverContent": { "inputTranscription": { "text": "old final" } }, "voiceActivity": { "type": "ACTIVITY_END", "audioOffset": "150.624s" } })).expect("composite old end");
        let first = tokio::time::timeout(Duration::from_secs(5), client.next()).await.expect("old final deadline").expect("old final").expect("text");
        let first: Value = serde_json::from_str(first.to_text().expect("text")).expect("json");
        assert_eq!(first, json!({ "type": "final", "text": "old final" }));
        let successor = log_when(&log, 500, |rows| rows.len() >= closed.len() + 3).await;
        let tail = &successor[closed.len()..];
        assert_eq!(tail.len(), 3);
        assert!(tail[0].pointer("/realtimeInput/activityStart").is_some());
        assert_eq!(sent_audio(&tail[1]), held["data"].as_str());
        assert!(tail[2].pointer("/realtimeInput/activityEnd").is_some());
        assert!(tokio::time::timeout(FINAL_SETTLE + Duration::from_millis(200), client.next()).await.is_err(), "no done before the successor is confirmed");
        inject.send(json!({ "serverContent": { "inputTranscription": { "text": "new final" } }, "voiceActivity": { "type": "ACTIVITY_END", "audioOffset": "0.032s" } })).expect("successor final and end");
        let messages = tokio::time::timeout(Duration::from_secs(5), browser_until_done(&mut client)).await.expect("done");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], json!({ "type": "final", "text": "new final" }));
        assert_eq!(messages[1]["type"], "done");
    }


    /// The upstream log, once `done` accepts it: the frames that crossed the
    /// socket so far. Polled, because the fake task appends to it as it reads.
    async fn log_when(
        log: &Mutex<Vec<Value>>,
        ticks: usize,
        done: impl Fn(&[Value]) -> bool,
    ) -> Vec<Value> {
        for _ in 0..ticks {
            {
                let rows = log.lock().expect("the upstream log lock");
                if done(&rows) {
                    return rows.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        log.lock().expect("the upstream log lock").clone()
    }

    /// The one `activity_rotate` record of the journal, if it is written.
    fn rotation(records: &[Value]) -> Option<&Value> {
        records
            .iter()
            .find(|record| record["event"] == "activity_rotate")
            .map(|record| &record["data"])
    }

    /// Every journal line of the one take under `root`.
    fn journal(root: &Path) -> String {
        let mut text = String::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return text;
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().to_string();
                if name.starts_with("events-") && name.ends_with(".jsonl") {
                    if let Ok(line) = std::fs::read_to_string(file.path()) {
                        text.push_str(&line);
                    }
                }
            }
        }
        text
    }

    /// Every journal record of the one take under `root`, parsed. The segments
    /// are read in name order, so the records are in the order they were
    /// written, and a line the writer is still appending is skipped.
    fn records(root: &Path) -> Vec<Value> {
        let mut segments: Vec<std::path::PathBuf> = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().to_string();
                if name.starts_with("events-") && name.ends_with(".jsonl") {
                    segments.push(file.path());
                }
            }
        }
        segments.sort();
        let mut rows = Vec::new();
        for segment in segments {
            let Ok(text) = std::fs::read_to_string(&segment) else {
                continue;
            };
            for line in text.lines() {
                if let Ok(record) = serde_json::from_str::<Value>(line) {
                    rows.push(record);
                }
            }
        }
        rows
    }

    /// The journal, once `done` accepts the records written so far. The real
    /// records are re-read every 100 ms for at most `ticks` reads (the writer's
    /// own pass interval), so a wait inside a running deadline can be bounded to
    /// less than that deadline; a condition that never holds returns the records
    /// as they are instead of hanging.
    async fn journal_when(
        root: &Path,
        ticks: usize,
        done: impl Fn(&[Value]) -> bool,
    ) -> Vec<Value> {
        let mut rows = records(root);
        for _ in 0..ticks {
            if done(&rows) {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            rows = records(root);
        }
        rows
    }

    /// The same wait with the fixture's frozen clock moved on between reads.
    ///
    /// The writer's periodic snapshot is gated on that clock: one is written
    /// once the clock has passed a whole interval since the take's last one,
    /// and the take's watermark is set on the pass that registers it. A
    /// fixture that advances the clock exactly once therefore races that pass:
    /// when the writer registers the take after the advance, the gate can never
    /// open again and no counter is ever written. Advancing again on every read
    /// that is still short of `done` takes that race out, and the one behind it
    /// too: a snapshot written before the relay counted the frame is followed
    /// by another one written after it, instead of the gate closing on a count
    /// that is one short.
    async fn journal_when_advancing(
        root: &Path,
        diagnostics: &Diagnostics,
        ticks: usize,
        done: impl Fn(&[Value]) -> bool,
    ) -> Vec<Value> {
        let mut rows = records(root);
        for _ in 0..ticks {
            if done(&rows) {
                return rows;
            }
            // Past the writer's one-second gate, then one writer pass.
            diagnostics.advance_clock(Duration::from_secs(2));
            tokio::time::sleep(Duration::from_millis(100)).await;
            rows = records(root);
        }
        rows
    }

    /// The one terminal record of the request, once it is written.
    fn terminal(records: &[Value]) -> Option<&Value> {
        records
            .iter()
            .find(|record| record["event"] == "request_end")
            .map(|record| &record["data"])
    }

    /// Every boundary of one kind, in the order they were written.
    fn boundaries<'a>(records: &'a [Value], kind: &str) -> Vec<&'a Value> {
        records
            .iter()
            .filter(|record| record["event"] == "upstream_boundary")
            .filter(|record| record["data"]["kind"] == kind)
            .map(|record| &record["data"])
            .collect()
    }
    /// Every content record of the take, in the order they were written. The
    /// body is the record's own, exactly as the producer handed it over.
    fn contents<'a>(records: &'a [Value]) -> Vec<&'a Value> {
        records
            .iter()
            .filter(|record| record["event"] == "content")
            .map(|record| &record["data"])
            .collect()
    }

    /// The bodies of the content records of one kind.
    fn content_bodies<'a>(records: &'a [Value], kind: &str) -> Vec<&'a str> {
        contents(records)
            .into_iter()
            .filter(|record| record["kind"] == kind)
            .filter_map(|record| record["body"].as_str())
            .collect()
    }

    /// Whether the journal names one deadline as the timer that fired.
    fn has_deadline(records: &[Value], timer: &str) -> bool {
        records
            .iter()
            .any(|record| record["event"] == "deadline" && record["data"]["timer"] == timer)
    }

    /// The counters of the newest relay snapshot. The writer emits one with
    /// every request terminal, so once the request ended this is exactly the
    /// counters it ended with - no periodic snapshot has to be raced for.
    fn counters(records: &[Value]) -> Option<&Value> {
        records
            .iter()
            .rev()
            .find(|record| record["event"] == "relay_snapshot")
            .map(|record| &record["data"])
    }

    /// One counter of the newest relay snapshot.
    fn counter(records: &[Value], field: &str) -> Option<u64> {
        counters(records)?.get(field)?.as_u64()
    }

    fn cleanup(diagnostics: Arc<Diagnostics>, root: &Path) {
        diagnostics.shutdown(Duration::from_millis(500));
        let _ = std::fs::remove_dir_all(root);
    }

    /// The browser going away ends the request as a cancel, and the counters of
    /// what the relay really did are in the journal with it.
    #[tokio::test]
    async fn client_close_records_the_cancel_and_the_relay_counters() {
        let root = scratch("cancel");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, mut received) = fake_upstream(
            vec![json!({ "setupComplete": {} })],
            None,
            None,
        )
        .await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        let ready = client.next().await.expect("a frame").expect("text");
        assert!(ready.to_text().expect("text").contains("ready"));

        for _ in 0..2 {
            client
                .send(WsMessage::text(audio_frame().to_string()))
                .await
                .expect("an audio frame");
        }
        // Both frames really reached the upstream before the socket is dropped,
        // so the counters below are a handshake and not a race.
        for _ in 0..2 {
            upstream_until(&mut received, |value| {
                value["realtimeInput"]["audio"].is_object()
            })
            .await;
        }
        drop(client);

        // The journal is snake_case; the terminal record and the snapshot the
        // writer emits with it are what the test reads, not a rendered line.
        let records = journal_when(&root, 300, |rows| {
            terminal(rows).is_some_and(|end| end["cause"] == "client_cancel")
        })
        .await;
        let end = terminal(&records).expect("the cancel is the request's terminal");
        assert_eq!(end["cause"], "client_cancel", "the cancel is the cause");
        assert_eq!(
            end["outcome"], "cancelled",
            "a browser that went away is a cancelled request"
        );
        assert_eq!(
            counter(&records, "client_audio_frames"),
            Some(2),
            "both client frames are counted"
        );
        assert_eq!(
            counter(&records, "client_pcm_bytes"),
            Some(2_048),
            "both frames' PCM is counted"
        );
        assert_eq!(
            counter(&records, "upstream_sent_frames"),
            Some(2),
            "both frames were sent upstream"
        );
        cleanup(diagnostics, &root);
    }

    /// A silent upstream after `end` is not a success: the transcript the
    /// session has is only the part that arrived before the deadline, so the
    /// relay reports the dictation as incomplete and the browser falls back to
    /// transcribing the recording.
    #[tokio::test]
    async fn a_silent_upstream_after_end_is_incomplete() {
        let root = scratch("silent-after-end");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, mut received) = fake_upstream(
            vec![
                json!({ "setupComplete": {} }),
                json!({ "serverContent": { "inputTranscription": { "text": "the tail" } } }),
            ],
            None,
            None,
        )
        .await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        for _ in 0..3 {
            client
                .send(WsMessage::text(audio_frame().to_string()))
                .await
                .expect("an audio frame");
        }
        for _ in 0..3 {
            upstream_until(&mut received, |value| {
                value["realtimeInput"]["audio"].is_object()
            })
            .await;
        }
        client
            .send(WsMessage::text(json!({ "type": "end" }).to_string()))
            .await
            .expect("the end frame");
        upstream_until(&mut received, |value| {
            value["realtimeInput"]["activityEnd"].is_object()
        })
        .await;

        // The upstream stays silent from here on: the post-end deadline is the
        // only thing that can end this request, and what it reports is an
        // incomplete dictation, never a success.
        let messages = browser_until_done(&mut client).await;
        let error = messages
            .iter()
            .find(|message| message["type"] == "error")
            .expect("the browser is told the transcript is incomplete");
        assert_eq!(
            error["ui"]["key"], "LiveIncomplete",
            "the incomplete transcript has its own message: {error}"
        );
        assert_eq!(
            messages.last().expect("the one done")["type"],
            "done",
            "the session still closes with its one `done`"
        );

        let records = journal_when(&root, 400, |rows| {
            terminal(rows).is_some_and(|end| end["cause"] == "final_timeout")
        })
        .await;
        let end = terminal(&records).expect("the deadline ends the request");
        assert_eq!(
            end["cause"], "final_timeout",
            "the post-end deadline is what ended it"
        );
        assert_eq!(
            end["outcome"], "error",
            "an incomplete dictation is not a success"
        );
        assert!(
            has_deadline(&records, "final"),
            "the final timer is the deadline that fired"
        );
        cleanup(diagnostics, &root);
    }

    /// The rotation the relay starts on its own: the activity is closed at a
    /// pause, the audio that follows is held until the server ends that
    /// activity, and only then is it replayed into the next one, in order.
    #[tokio::test]
    async fn rotation_holds_audio_until_the_server_ends_the_activity() {
        let root = scratch("rotation-hold");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, inject, log) = scripted_upstream(|frame| {
            if frame.get("setup").is_some() {
                return vec![json!({ "setupComplete": {} })];
            }
            if frame.pointer("/realtimeInput/activityStart").is_some() {
                return vec![json!({
                    "voiceActivity": { "type": "ACTIVITY_START", "audioOffset": "0s" }
                })];
            }
            if frame.pointer("/realtimeInput/activityEnd").is_some() {
                // The activity's tail: a final, so the rotation's hold is
                // resolved rather than reported as speech left unfinished.
                return vec![json!({
                    "serverContent": { "inputTranscription": { "text": "up to the pause" } }
                })];
            }
            Vec::new()
        })
        .await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        // 4688 frames of speech: 150.016 s of audio, past the earliest rotation.
        const SPOKEN: usize = 4_688;
        const SILENT: usize = 19;
        for _ in 0..SPOKEN {
            client
                .send(WsMessage::text(level_frame(8_000).to_string()))
                .await
                .expect("a spoken frame");
        }
        let spoken = log_when(&log, 500, |rows| {
            rows.iter().filter_map(sent_audio).count() == SPOKEN
        })
        .await;
        assert_eq!(
            spoken.iter().filter_map(sent_audio).count(),
            SPOKEN,
            "every spoken frame was forwarded"
        );

        // 19 frames of silence: 608 ms, the pause the rotation is allowed to
        // take at this point of the activity.
        for _ in 0..SILENT {
            client
                .send(WsMessage::text(level_frame(0).to_string()))
                .await
                .expect("a silent frame");
        }
        let rotated = log_when(&log, 500, |rows| {
            rows.last()
                .is_some_and(|last| last.pointer("/realtimeInput/activityEnd").is_some())
        })
        .await;
        assert_eq!(
            rotated.iter().filter_map(sent_audio).count(),
            SPOKEN + SILENT,
            "the activity ended right after the pause's last frame: {rotated:?}"
        );
        let last_audio = rotated
            .iter()
            .rev()
            .find_map(sent_audio)
            .expect("the last frame of the activity");
        assert_eq!(
            last_audio,
            level_frame(0)["data"].as_str().expect("base64"),
            "the activity was closed on the silent frame, not before it"
        );

        // The audio that follows the rotation is held: nothing more may reach
        // the upstream until it ends the activity the relay closed.
        let held: Vec<Value> = (0..10).map(|index| level_frame(8_100 + index)).collect();
        for frame in &held {
            client
                .send(WsMessage::text(frame.to_string()))
                .await
                .expect("a held frame");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let held_log = log_when(&log, 1, |_| true).await;
        assert_eq!(
            held_log.len(),
            rotated.len(),
            "the held audio did not cross the socket: {held_log:?}"
        );

        // The server ends the activity: the held audio is replayed into the new
        // one, in the order it arrived.
        inject
            .send(json!({
                "voiceActivity": { "type": "ACTIVITY_END", "audioOffset": "150.624s" }
            }))
            .expect("the injected activity end");
        let replayed = log_when(&log, 500, |rows| {
            rows.len() >= held_log.len() + held.len() + 1
        })
        .await;
        let tail = &replayed[held_log.len()..];
        assert!(
            tail[0].pointer("/realtimeInput/activityStart").is_some(),
            "the new activity starts before its audio: {tail:?}"
        );
        let replayed_audio: Vec<&str> = tail.iter().filter_map(sent_audio).collect();
        let expected: Vec<&str> = held
            .iter()
            .map(|frame| frame["data"].as_str().expect("base64"))
            .collect();
        assert_eq!(
            replayed_audio, expected,
            "the held frames are replayed in order: {tail:?}"
        );

        let records = journal_when(&root, 300, |rows| rotation(rows).is_some()).await;
        let rotation = rotation(&records).expect("the rotation is in the journal");
        assert_eq!(
            rotation["reason"], "pause",
            "a rotation at a pause, not at the limit: {rotation}"
        );
        assert_eq!(
            rotation["held_frames"], 10,
            "every held frame is counted: {rotation}"
        );
        assert_eq!(
            rotation["activity_audio_ms"], 150_624,
            "the activity's own length is recorded: {rotation}"
        );
        cleanup(diagnostics, &root);
    }

    /// The server ending an activity by itself is not the end of the take: the
    /// audio it never transcribed is replayed from the offset it named, so the
    /// next activity starts where the previous one was cut.
    #[tokio::test]
    async fn a_server_cut_replays_from_its_offset() {
        let root = scratch("server-cut");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, _inject, log) = scripted_upstream({
            let mut audio = 0_u64;
            move |frame| {
                if frame.get("setup").is_some() {
                    return vec![json!({ "setupComplete": {} })];
                }
                if frame.pointer("/realtimeInput/activityStart").is_some() {
                    return vec![json!({
                        "voiceActivity": { "type": "ACTIVITY_START", "audioOffset": "0s" }
                    })];
                }
                if frame.pointer("/realtimeInput/audio").is_some() {
                    audio += 1;
                    if audio == 100 {
                        // 1.6 s of the 3.2 s sent: the server ends the activity
                        // on its own and names the cut in the same breath.
                        return vec![
                            json!({
                                "serverContent": { "inputTranscription": { "text": "up to the cut" } }
                            }),
                            json!({
                                "voiceActivity": { "type": "ACTIVITY_END", "audioOffset": "1.600s" }
                            }),
                        ];
                    }
                }
                Vec::new()
            }
        })
        .await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        let frames: Vec<Value> = (0..100).map(|index| level_frame(8_000 + index)).collect();
        for frame in &frames {
            client
                .send(WsMessage::text(frame.to_string()))
                .await
                .expect("an audio frame");
        }

        // The relay's new activity: the audio after its `activityStart` is the
        // replay, and the first frame of it is the one the cut began in.
        let replayed = log_when(&log, 500, |rows| {
            let starts = rows
                .iter()
                .filter(|row| row.pointer("/realtimeInput/activityStart").is_some())
                .count();
            starts == 2 && rows.iter().filter_map(sent_audio).count() >= 101
        })
        .await;
        let last_start = replayed
            .iter()
            .rposition(|row| row.pointer("/realtimeInput/activityStart").is_some())
            .expect("the new activity");
        let replay: Vec<&str> = replayed[last_start..].iter().filter_map(sent_audio).collect();
        assert_eq!(
            replay.first().copied(),
            frames[50]["data"].as_str(),
            "the replay starts at the frame the cut fell in: {replay:?}"
        );
        assert_eq!(
            replay.len(),
            50,
            "everything from the cut to the end is replayed: {replay:?}"
        );

        let records = journal_when(&root, 300, |rows| rotation(rows).is_some()).await;
        let rotation = rotation(&records).expect("the rotation is in the journal");
        assert_eq!(
            rotation["reason"], "server_cut",
            "the server's own cut, not ours: {rotation}"
        );
        assert_eq!(
            rotation["replayed_frames"], 50,
            "every replayed frame is counted: {rotation}"
        );
        cleanup(diagnostics, &root);
    }

    /// An activity that ends while the upstream had speech it never finalized
    /// is a lost tail: the relay reports the dictation as incomplete instead of
    /// passing the interim on as if it were the whole text.
    #[tokio::test]
    async fn an_activity_end_with_pending_speech_is_incomplete() {
        let root = scratch("pending-speech");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, inject, _log) = scripted_upstream(|frame| {
            if frame.get("setup").is_some() {
                return vec![json!({ "setupComplete": {} })];
            }
            if frame.pointer("/realtimeInput/activityStart").is_some() {
                return vec![json!({
                    "voiceActivity": { "type": "ACTIVITY_START", "audioOffset": "0s" }
                })];
            }
            if frame.pointer("/realtimeInput/audio").is_some() {
                return vec![json!({
                    "serverContent": { "interimInputTranscription": { "text": "not final" } }
                })];
            }
            Vec::new()
        })
        .await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        client
            .send(WsMessage::text(audio_frame().to_string()))
            .await
            .expect("an audio frame");
        // The interim really reached the browser before the activity ends, so
        // the unfinished speech is not a race of the fixture.
        let interim = loop {
            let Some(Ok(WsMessage::Text(text))) = client.next().await else {
                panic!("the relay closed before the interim");
            };
            let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            if value["type"] == "interim" {
                break value;
            }
        };
        assert_eq!(interim["text"], "not final");
        inject
            .send(json!({
                "voiceActivity": { "type": "ACTIVITY_END", "audioOffset": "0.032s" }
            }))
            .expect("the injected activity end");

        let messages = browser_until_done(&mut client).await;
        let error = messages
            .iter()
            .find(|message| message["type"] == "error")
            .expect("the unfinished speech is reported");
        assert_eq!(
            error["ui"]["key"], "LiveIncomplete",
            "the dictation is incomplete, not finished: {error}"
        );
        assert_eq!(messages.last().expect("the one done")["type"], "done");

        let records = journal_when(&root, 300, |rows| {
            terminal(rows).is_some_and(|end| end["cause"] == "activity_incomplete")
        })
        .await;
        let end = terminal(&records).expect("the incomplete activity ends the request");
        assert_eq!(end["cause"], "activity_incomplete");
        assert_eq!(end["outcome"], "error");
        cleanup(diagnostics, &root);
    }

    /// The upstream error text is evidence: the frame as it arrived is archived
    /// in full, and so is the error the browser was given - while a key either
    /// of them echoed is redacted out of both.
    #[tokio::test]
    async fn an_upstream_error_message_is_archived_with_its_key_redacted() {
        let root = scratch("upstream-error");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, mut received) = fake_upstream(
            vec![json!({ "setupComplete": {} })],
            Some(json!({
                "error": { "message": format!("rejected: {SYNTHETIC_SECRET} key={SYNTHETIC_KEY}") }
            })),
            None,
        )
        .await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));
        let audio = audio_frame();
        let payload = audio["data"].as_str().expect("base64").to_string();
        client
            .send(WsMessage::text(audio.to_string()))
            .await
            .expect("an audio frame");
        upstream_until(&mut received, |value| {
            value["realtimeInput"]["audio"].is_object()
        })
        .await;

        // Wait for both records of the error: the frame as it arrived, and the
        // message the browser was given.
        let records = journal_when(&root, 300, |rows| {
            content_bodies(rows, "live_output")
                .iter()
                .any(|body| body.contains("\"type\":\"error\""))
                && content_bodies(rows, "live_message")
                    .iter()
                    .any(|body| body.contains(SYNTHETIC_SECRET))
        })
        .await;
        let frames = content_bodies(&records, "live_message");
        assert!(
            frames.iter().any(|body| body.contains(SYNTHETIC_SECRET)),
            "the upstream error frame is archived as it arrived: {frames:?}"
        );
        let outputs = content_bodies(&records, "live_output");
        assert!(
            outputs
                .iter()
                .any(|body| body.contains("\"type\":\"error\"")),
            "the browser-bound error is archived too: {outputs:?}"
        );
        for body in frames.iter().chain(outputs.iter()) {
            assert!(
                !body.contains(SYNTHETIC_KEY),
                "the key is redacted out of every record: {body}"
            );
        }

        let text = journal(&root);
        assert!(
            text.contains("\"cause\":\"upstream_error\""),
            "the upstream error is still the request's cause: {text}"
        );
        assert!(
            !text.contains(&payload),
            "the PCM payload is not duplicated into the archive: {text}"
        );
        cleanup(diagnostics, &root);
    }

    /// The upstream's close is archived whole - the code and the reason text
    /// that arrived with it - while the numeric code the counters use and the
    /// one close boundary stay exactly as they were. A key inside the reason is
    /// redacted out of the archived copy.
    #[tokio::test]
    async fn an_upstream_close_reason_is_archived_with_its_code() {
        let root = scratch("close-reason");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, _received) = fake_upstream(
            vec![json!({ "setupComplete": {} })],
            None,
            Some((
                1011,
                format!("closed because of {SYNTHETIC_SECRET} key={SYNTHETIC_KEY}"),
            )),
        )
        .await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        // The content record, the close boundary and the terminal record are
        // written separately; wait for all three before reading the journal.
        let records = journal_when(&root, 300, |rows| {
            !content_bodies(rows, "socket_close").is_empty()
                && !boundaries(rows, "close").is_empty()
                && terminal(rows).is_some()
        })
        .await;
        let closes = content_bodies(&records, "socket_close");
        assert_eq!(closes.len(), 1, "one close frame, one record: {closes:?}");
        assert!(
            closes[0].contains("\"code\":1011"),
            "the code the frame carried: {closes:?}"
        );
        assert!(
            closes[0].contains(SYNTHETIC_SECRET),
            "the reason text is archived in full: {closes:?}"
        );
        assert!(
            !closes[0].contains(SYNTHETIC_KEY),
            "the key is redacted out of the reason: {closes:?}"
        );

        let text = journal(&root);
        assert!(
            text.contains("\"close_code\":1011"),
            "the numeric close code is still recorded: {text}"
        );
        assert!(
            text.contains("\"cause\":\"upstream_close\""),
            "the close is still the request's cause: {text}"
        );
        assert_eq!(
            text.matches("\"kind\":\"close\"").count(),
            1,
            "the close frame is one close boundary, not two: {text}"
        );
        cleanup(diagnostics, &root);
    }

    /// A frame that is not JSON at all is archived before the relay drops it,
    /// and the take keeps running: the malformed bytes are evidence of
    /// themselves, exactly like every other message.
    #[tokio::test]
    async fn a_malformed_upstream_frame_is_archived_before_it_is_dropped() {
        let root = scratch("malformed");
        let (diagnostics, trace) = take(&root);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let upstream_port = listener.local_addr().expect("an address").port();
        let malformed = "{\"serverContent\": \"unterminated".to_string();
        let answer = malformed.clone();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let _ = ws.next().await;
            if ws
                .send(WsMessage::text(
                    json!({ "setupComplete": {} }).to_string(),
                ))
                .await
                .is_err()
            {
                return;
            }
            let _ = ws.send(WsMessage::text(answer)).await;
            // The socket stays open: the relay must survive the frame.
            while ws.next().await.is_some() {}
        });
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        let records = journal_when(&root, 300, |rows| {
            content_bodies(rows, "live_message")
                .iter()
                .any(|body| body.contains("unterminated"))
        })
        .await;
        let frames = content_bodies(&records, "live_message");
        assert!(
            frames.iter().any(|body| body.contains("unterminated")),
            "the malformed frame is archived as it arrived: {frames:?}"
        );
        assert!(
            terminal(&records).is_none(),
            "a malformed frame is not a terminal: {records:?}"
        );
        cleanup(diagnostics, &root);
    }

    /// A binary frame the relay cannot decode as UTF-8 is archived as the exact
    /// bytes it carried: the key inside it is masked where it is valid UTF-8,
    /// the octets that are not text survive, and the frame still counts as one
    /// off-shape receive instead of vanishing from the counters.
    #[tokio::test]
    async fn a_non_utf8_upstream_frame_is_archived_as_its_exact_bytes() {
        let root = scratch("binary-bytes");
        let (diagnostics, trace) = take(&root);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let upstream_port = listener.local_addr().expect("an address").port();
        let mut payload = format!("{{\"raw\":\"{SYNTHETIC_KEY}\"").into_bytes();
        payload.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x80]);
        let answer = payload.clone();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let _ = ws.next().await;
            if ws
                .send(WsMessage::text(json!({ "setupComplete": {} }).to_string()))
                .await
                .is_err()
            {
                return;
            }
            let _ = ws.send(WsMessage::binary(answer)).await;
            // The socket stays open: the relay must survive the frame.
            while ws.next().await.is_some() {}
        });
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        // The writer's snapshot is gated on the sink's own clock, and this
        // fixture's clock is frozen: the wait moves it on until the periodic
        // relay snapshot carrying the counter lands beside the byte record.
        let records = journal_when_advancing(&root, &diagnostics, 300, |rows| {
            content_bodies(rows, "live_message")
                .iter()
                .any(|body| body.contains("\"encoding\":\"base64\""))
                && counter(rows, "received_messages") == Some(2)
        })
        .await;
        let frames = content_bodies(&records, "live_message");
        let record = frames
            .iter()
            .find(|body| body.contains("\"encoding\":\"base64\""))
            .expect("the undecodable frame is archived as bytes");
        let envelope: Value =
            serde_json::from_str(record).expect("the record's own envelope");
        assert_eq!(envelope["frame"], json!("binary"));
        assert_eq!(envelope["bytes"], json!(payload.len()));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(envelope["data"].as_str().expect("the encoded bytes"))
            .expect("valid base64");
        let expected = {
            let mut expected = format!("{{\"raw\":\"[redacted]\"").into_bytes();
            expected.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x80]);
            expected
        };
        assert_eq!(
            decoded, expected,
            "the exact bytes arrive, with only the key masked"
        );
        assert!(
            !String::from_utf8_lossy(&decoded).contains(SYNTHETIC_KEY),
            "the key is not anywhere in the archived bytes"
        );
        // The packet crossed the socket: the setup and this frame, once each.
        assert_eq!(
            counter(&records, "received_messages"),
            Some(2),
            "the setup and the undecodable frame, counted once each: {records:?}"
        );
        assert!(
            terminal(&records).is_none(),
            "an undecodable frame is not a terminal: {records:?}"
        );
        cleanup(diagnostics, &root);
    }

    /// A control frame's payload is archived as its own kind, redacted and
    /// encoded the same way, and the ping is still counted as traffic - the
    /// observation never sends anything back.
    #[tokio::test]
    async fn a_control_frame_payload_is_archived_as_bytes() {
        let root = scratch("ping-bytes");
        let (diagnostics, trace) = take(&root);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let upstream_port = listener.local_addr().expect("an address").port();
        let mut payload = SYNTHETIC_KEY.as_bytes().to_vec();
        payload.extend_from_slice(&[0xFE, 0x01]);
        let answer = payload.clone();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let _ = ws.next().await;
            if ws
                .send(WsMessage::text(json!({ "setupComplete": {} }).to_string()))
                .await
                .is_err()
            {
                return;
            }
            let _ = ws
                .send(WsMessage::Ping(answer.into()))
                .await;
            while ws.next().await.is_some() {}
        });
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        // The same frozen-clock gate, moved on the same way, until the byte
        // record and the periodic snapshot that carries the counters are both
        // written.
        let records = journal_when_advancing(&root, &diagnostics, 300, |rows| {
            content_bodies(rows, "live_message")
                .iter()
                .any(|body| body.contains("\"frame\":\"ping\""))
                && counter(rows, "received_messages") == Some(2)
        })
        .await;
        let frames = content_bodies(&records, "live_message");
        let record = frames
            .iter()
            .find(|body| body.contains("\"frame\":\"ping\""))
            .expect("the ping payload is archived");
        let envelope: Value =
            serde_json::from_str(record).expect("the record's own envelope");
        assert_eq!(envelope["encoding"], json!("base64"));
        assert_eq!(envelope["bytes"], json!(payload.len()));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(envelope["data"].as_str().expect("the encoded bytes"))
            .expect("valid base64");
        let expected = {
            let mut expected = b"[redacted]".to_vec();
            expected.extend_from_slice(&[0xFE, 0x01]);
            expected
        };
        assert_eq!(
            decoded, expected,
            "the payload arrives with only the key masked"
        );
        assert!(
            !String::from_utf8_lossy(&decoded).contains(SYNTHETIC_KEY),
            "the key is not anywhere in the archived payload"
        );
        assert_eq!(
            counter(&records, "received_messages"),
            Some(2),
            "the setup and the ping are both counted: {records:?}"
        );
        assert!(
            terminal(&records).is_none(),
            "a control frame is not a terminal: {records:?}"
        );
        cleanup(diagnostics, &root);
    }

    /// The setup configuration and the browser-bound transcripts are archived in
    /// the order they happened: what was configured, then the interim, then the
    /// final - with both transcripts complete.
    #[tokio::test]
    async fn the_setup_and_the_browser_transcripts_are_archived_in_order() {
        let root = scratch("content-order");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, _received) = fake_upstream(
            vec![
                json!({ "setupComplete": {} }),
                json!({ "serverContent": { "interimInputTranscription": { "text": "first words" } } }),
                json!({ "serverContent": { "inputTranscription": { "text": "first words finally" } } }),
            ],
            None,
            None,
        )
        .await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        // `ready`, the interim and the final: three messages for the browser.
        let records = journal_when(&root, 300, |rows| {
            content_bodies(rows, "live_output").len() >= 3
        })
        .await;
        let outputs = content_bodies(&records, "live_output");
        let interim = outputs
            .iter()
            .position(|body| body.contains("\"type\":\"interim\""));
        let final_text = outputs
            .iter()
            .position(|body| body.contains("\"type\":\"final\""));
        assert!(
            interim.is_some() && final_text.is_some(),
            "both transcripts reach the archive: {outputs:?}"
        );
        assert!(
            interim < final_text,
            "the interim precedes the final: {outputs:?}"
        );
        assert!(
            interim.is_some_and(|index| index >= 1),
            "`ready` comes first: {outputs:?}"
        );
        assert!(
            outputs
                .iter()
                .any(|body| body.contains("first words finally")),
            "the final transcript is complete: {outputs:?}"
        );

        let setups = content_bodies(&records, "live_setup");
        assert_eq!(setups.len(), 1, "one setup frame, one record: {setups:?}");
        assert!(
            setups[0].contains("models/gemini-3.5-transcribe-live"),
            "the exact model the socket was opened with: {setups:?}"
        );
        assert!(
            setups[0].contains("\"mode\":\"SMART\""),
            "the mode the socket was opened with: {setups:?}"
        );
        // The rotation entry this socket bound, by ordinal and count: enough
        // to name the entry a 401/429 came from, never the key itself.
        assert!(
            setups[0].contains("\"key_ordinal\":1"),
            "the bound entry's 1-based ordinal: {setups:?}"
        );
        assert!(
            setups[0].contains("\"ring_keys\":1"),
            "how many entries the pinned ring had: {setups:?}"
        );
        assert!(
            !setups[0].contains(SYNTHETIC_KEY),
            "the key itself never enters the record: {setups:?}"
        );
        cleanup(diagnostics, &root);
    }

    /// The vocabulary the socket was bound to really reaches the setup frame,
    /// and the archive keeps only how many terms it had: a term is user text
    /// and never enters a record.
    #[tokio::test]
    async fn the_archived_setup_counts_the_vocabulary_without_its_terms() {
        let root = scratch("vocabulary-setup");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, _received) = fake_upstream(
            vec![json!({ "setupComplete": {} })],
            None,
            None,
        )
        .await;
        let (port, _key, _ring) = relay(
            upstream_port,
            Arc::from(vec!["Speechek".to_owned(), "Kubernetes".to_owned()]),
            Some(trace),
        )
        .await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        let records = journal_when(&root, 300, |rows| {
            !content_bodies(rows, "live_setup").is_empty()
        })
        .await;
        let setups = content_bodies(&records, "live_setup");
        assert_eq!(setups.len(), 1, "one setup frame, one record: {setups:?}");
        assert!(
            setups[0].contains("\"custom_vocabulary_terms\":2"),
            "how many terms the setup carried: {setups:?}"
        );
        assert!(
            setups[0].contains("\"mode\":\"SMART\""),
            "the mode the socket was opened with: {setups:?}"
        );
        assert!(
            !setups[0].contains("Kubernetes"),
            "no term of the vocabulary enters the record: {setups:?}"
        );
        cleanup(diagnostics, &root);
    }

    /// An upstream that drops the connection without a close frame is the 1006
    /// the browser API reports. The drop surfaces twice: once as the failed
    /// read it really is - a received message, and rightly counted - and then
    /// as the end of stream, which is not a message and adds no count. The one
    /// close boundary is the numeric 1006 the transport never framed.
    #[tokio::test]
    async fn an_abrupt_upstream_reset_records_the_1006_close_once() {
        let root = scratch("eof");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, mut received) = abrupt_upstream(vec![json!({ "setupComplete": {} })]).await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        client
            .send(WsMessage::text(audio_frame().to_string()))
            .await
            .expect("an audio frame");
        // The fixture drops the socket once it has that frame, so the relay runs
        // into the end of the stream while the test is still reading its close.
        upstream_until(&mut received, |value| {
            value["realtimeInput"]["audio"].is_object()
        })
        .await;
        loop {
            match client.next().await {
                Some(Ok(WsMessage::Text(text))) if text.contains("done") => break,
                Some(Ok(_)) => continue,
                other => panic!("the relay never finished: {other:?}"),
            }
        }

        let records = journal_when(&root, 300, |rows| {
            terminal(rows).is_some_and(|end| end["cause"] == "upstream_close")
        })
        .await;
        let end = terminal(&records).expect("the dropped socket ends the request");
        assert_eq!(end["cause"], "upstream_close");
        assert_eq!(
            end["close_code"], 1006,
            "the transport's own 1006 is the recorded code"
        );
        let close = boundaries(&records, "close");
        assert_eq!(
            close.len(),
            1,
            "the drop crosses exactly one close boundary: {close:?}"
        );
        assert_eq!(close[0]["code"], 1006);
        // Two received messages: the setup, and the failed read that ended the
        // stream. The end of stream itself adds nothing to the count.
        assert_eq!(
            counter(&records, "received_messages"),
            Some(2),
            "the failed read is counted, the end of stream is not"
        );
        cleanup(diagnostics, &root);
    }

    /// The dialed URL is the one place a Live key could reach the archive: a
    /// socket error quotes the address the relay opened, and that address
    /// carries the key in its query. The mask runs before the redaction, so the
    /// address itself - key included - is never part of the record.
    #[test]
    fn a_dialed_socket_url_is_masked_out_of_a_transport_error() {
        let masked = mask_urls("connect error: wss://host/ws?key=super-secret-9f3b refused");
        assert_eq!(masked, "connect error: [url] refused");
        assert!(
            !masked.contains("super-secret-9f3b"),
            "the key inside the URL goes with it: {masked}"
        );
        // A URL quoted inside a rendered error stops at the quote, so the
        // wording around it survives the masking.
        assert_eq!(
            mask_urls("\"ws://host/ws?key=abc\" refused"),
            "\"[url]\" refused"
        );
        // Nothing to mask: the detail is handed on unchanged.
        assert_eq!(mask_urls("timed out"), "timed out");
    }

    /// `goAway.timeLeft` is a duration or nothing at all: a value outside
    /// `Duration`'s range is no metadata, never a panic and never the raw text.
    #[test]
    fn a_go_away_duration_is_parsed_or_dropped() {
        assert_eq!(
            go_away_time_left(&json!({ "goAway": { "timeLeft": 12.5 } })),
            Some(Duration::from_millis(12_500))
        );
        assert_eq!(
            go_away_time_left(&json!({ "goAway": { "timeLeft": "5s" } })),
            Some(Duration::from_secs(5))
        );
        for value in [
            json!(1e30),
            json!(-3),
            json!("1e30s"),
            json!("soon"),
            json!(null),
            json!(true),
        ] {
            assert_eq!(
                go_away_time_left(&json!({ "goAway": { "timeLeft": value } })),
                None,
                "an unusable timeLeft is no duration: {value}"
            );
        }
        assert_eq!(go_away_time_left(&json!({ "setupComplete": {} })), None);
    }

    /// The absurd announcement is dropped where it belongs - in the metadata -
    /// and the take keeps recording afterwards.
    #[tokio::test]
    async fn an_overflowing_go_away_duration_leaves_the_relay_running() {
        let root = scratch("go-away-overflow");
        let (diagnostics, trace) = take(&root);
        let (upstream_port, mut received) = fake_upstream(
            vec![
                json!({ "setupComplete": {} }),
                json!({ "goAway": { "timeLeft": 1e30 } }),
            ],
            None,
            None,
        )
        .await;
        let (port, _key, _ring) = relay(upstream_port, Arc::default(), Some(trace)).await;

        let mut client = client(port).await;
        client
            .send(WsMessage::text(json!({ "type": "start", "mode": "smart" }).to_string()))
            .await
            .expect("the start frame");
        assert!(client.next().await.expect("a frame").expect("text")
            .to_text()
            .expect("text")
            .contains("ready"));

        client
            .send(WsMessage::text(audio_frame().to_string()))
            .await
            .expect("an audio frame");
        // The relay kept relaying: the frame it took after the announcement it
        // could not read a duration out of did reach the upstream.
        upstream_until(&mut received, |value| {
            value["realtimeInput"]["audio"].is_object()
        })
        .await;

        // A snapshot of the counters as they are now, and no terminal: an
        // observer that panicked or aborted would have left neither. The same
        // frozen-clock gate admits it once it is moved on.
        let records = journal_when_advancing(&root, &diagnostics, 30, |rows| {
            counter(rows, "upstream_sent_frames") == Some(1)
        })
        .await;
        let go_away = boundaries(&records, "go_away");
        assert_eq!(
            go_away.len(),
            1,
            "the go-away is a boundary of its own: {go_away:?}"
        );
        assert_eq!(
            go_away[0]["time_left_us"],
            Value::Null,
            "an unusable duration is none, never a number and never the text"
        );
        assert_eq!(go_away[0]["code"], Value::Null);
        assert_eq!(
            counter(&records, "client_audio_frames"),
            Some(1),
            "the take kept taking audio"
        );
        assert_eq!(
            counter(&records, "upstream_sent_frames"),
            Some(1),
            "the take kept sending it upstream"
        );
        assert!(
            terminal(&records).is_none(),
            "the relay is still running: {records:?}"
        );
        cleanup(diagnostics, &root);
    }
}

/// The rotation's pure decisions, pinned without a socket: the pause levels
/// that make a pause a pause, the thresholds a rotation is allowed at and the
/// audio offset the upstream announces.
#[cfg(test)]
mod rotation_tests {
    use super::*;

    #[test]
    fn a_rotation_waits_for_a_longer_pause_the_earlier_it_is() {
        assert!(!rotation_due(149_999, 10_000));
        assert!(rotation_due(150_000, 600));
        assert!(!rotation_due(175_000, 299));
        assert!(rotation_due(190_000, 150));
        assert!(rotation_due(200_000, 0));
    }

    #[test]
    fn an_audio_offset_is_a_seconds_string_or_nothing() {
        assert_eq!(parse_audio_offset(&json!("220.440s")), Some(220.44));
        assert_eq!(parse_audio_offset(&json!("0s")), Some(0.0));
        assert_eq!(parse_audio_offset(&json!("x")), None);
        assert_eq!(parse_audio_offset(&json!(220.44)), None);
        assert_eq!(parse_audio_offset(&json!("-1s")), None);
        assert_eq!(parse_audio_offset(&json!(null)), None);
    }

    #[test]
    fn a_frame_level_is_the_silence_of_zeros_and_the_level_of_a_constant() {
        assert_eq!(frame_level_db(&[]), -120.0);
        assert_eq!(frame_level_db(&[0; 1024]), -120.0);
        let loud = 8_000_i16.to_le_bytes();
        let mut pcm = Vec::new();
        for _ in 0..512 {
            pcm.extend_from_slice(&loud);
        }
        let level = frame_level_db(&pcm);
        assert!(
            (level - -12.25).abs() < 0.1,
            "a constant 8000 is about -12.25 dBFS: {level}"
        );
    }

    #[test]
    fn the_pause_detector_holds_speech_and_silence_apart() {
        let mut detector = PauseDetector::new();
        let loud = 8_000_i16.to_le_bytes();
        let mut pcm = Vec::new();
        for _ in 0..512 {
            pcm.extend_from_slice(&loud);
        }
        let level = frame_level_db(&pcm);
        assert!(
            !detector.observe(level, 32),
            "a loud frame is not a pause"
        );
        assert_eq!(detector.quiet_ms, 0, "speech resets the pause");
        assert!(detector.observe(-120.0, 32), "silence is a pause");
        assert_eq!(detector.quiet_ms, 32, "the pause accumulates");
    }
}

/// The setup frame's wire shape, pinned without the diagnostics the relay tests
/// above need: an empty vocabulary or language list leaves the frame exactly as
/// it was, set ones ride in `inputAudioTranscription` as `customVocabulary` and
/// `languageCodes`.
#[cfg(test)]
mod payload_tests {
    use super::*;

    #[test]
    fn the_live_setup_omits_an_empty_vocabulary_and_carries_a_set_one() {
        let empty = setup_payload(Mode::Smart, &[], &[]);
        assert_eq!(
            empty["setup"]["inputAudioTranscription"],
            json!({ "mode": "SMART" })
        );
        let set = setup_payload(Mode::Smart, &["Speechek".to_owned()], &[]);
        assert_eq!(
            set["setup"]["inputAudioTranscription"],
            json!({ "mode": "SMART", "customVocabulary": ["Speechek"] })
        );
    }

    #[test]
    fn the_live_setup_carries_the_dictation_languages_only_when_chosen() {
        let languages = ["ru-RU".to_owned(), "en-US".to_owned()];
        let hinted = setup_payload(Mode::Smart, &[], &languages);
        assert_eq!(
            hinted["setup"]["inputAudioTranscription"],
            json!({ "mode": "SMART", "languageCodes": ["ru-RU", "en-US"] })
        );
        let both = setup_payload(Mode::Smart, &["Speechek".to_owned()], &languages);
        assert_eq!(
            both["setup"]["inputAudioTranscription"],
            json!({
                "mode": "SMART",
                "customVocabulary": ["Speechek"],
                "languageCodes": ["ru-RU", "en-US"],
            })
        );
    }
}
