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
//! transcript (6 s once a final was already seen), 900 ms/600 ms settle windows, an
//! 11-minute session cap, a 10-minute audio cap and a 700 s idle timeout. Deadlines
//! are absolute here, so a stalled environment cannot move them.
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

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as ClientMessage, WebSocket};
use futures_util::future::OptionFuture;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::Message as UpstreamMessage;

use crate::backend::{redact, redact_ui};
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use crate::backend::{diagnostic_bytes, diagnostic_json, diagnostic_text};
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use base64::Engine as _;
use crate::i18n::{render, Language, MessageId, UiMessage};
use crate::provider::Provider;
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use crate::provider::key_ordinal;
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use crate::diagnostics::{
    ContentFormat, ContentKind, DeadlineTimer, Outcome, ReceiveFlags, RequestCause, RequestTrace,
    SendKind, UpstreamKind,
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
/// Shorter post-end deadline once a final was already seen: only the tail is missing.
const TAIL_GRACE: Duration = Duration::from_secs(6);
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
    /// The upstream went quiet after its last transcript or turn boundary.
    Settle,
    /// The session ran into the 11-minute cap.
    Hard,
    /// The browser stopped sending anything for the idle timeout.
    Idle,
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
    final_seen: bool,
    done: bool,
    /// Bytes of audio accepted so far, against the 10-minute cap.
    bytes: u64,
    /// Audio that arrived before the upstream confirmed its setup, in order.
    queue: Vec<String>,
    ready_at: Option<Instant>,
    final_at: Option<Instant>,
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
    /// The final deadline actually armed, so its expiry reports the timer that
    /// fired rather than one inferred from transcript state a final changed
    /// afterwards.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    armed_final: Option<RequestCause>,
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
            armed_final: None,
            ring,
            vocabulary,
            languages,
            mode: None,
            started: false,
            end_requested: false,
            upstream_ready: false,
            final_seen: false,
            done: false,
            bytes: 0,
            queue: Vec::new(),
            ready_at: None,
            final_at: None,
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
            self.settle_at,
            self.hard_at,
            self.idle_at,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Clears every timer and the undelivered audio.
    fn clear_timers(&mut self) {
        self.ready_at = None;
        self.final_at = None;
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        {
            self.armed_final = None;
        }
        self.settle_at = None;
        self.hard_at = None;
        self.idle_at = None;
        self.queue.clear();
    }

    /// One client frame arrived: the idle deadline starts over.
    fn touch(&mut self) {
        self.idle_at = Some(Instant::now() + IDLE_TIMEOUT);
    }
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
    let voice_activity = content
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

    if state.upstream_ready {
        write_upstream(upstream, state, &audio_payload(data)).await;
    } else {
        // Setup is still in flight: hold the frames and replay them in order.
        state.queue.push(data.to_owned());
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        if let Some(trace) = state.trace.as_ref() {
            trace.pre_ready_queue_depth(state.queue.len() as u64);
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
        write_upstream(
            upstream,
            state,
            &json!({ "realtimeInput": { "activityStart": {} } }),
        )
        .await;
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

    let Some(content) = message.get("serverContent") else {
        return;
    };

    if let Some(interim) = content.get("interimInputTranscription").and_then(text_of) {
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
        // Any final transcript shortens the post-end deadline.
        state.final_seen = true;
        if state.end_requested {
            state.settle_at = Some(Instant::now() + FINAL_SETTLE);
        }
    }

    // `input_transcription` is not guaranteed to arrive before the turn boundary, so
    // turnComplete only shortens the wait; the settle timer still guarantees `done`.
    if json_truthy(content.get("turnComplete")) && state.end_requested {
        state.settle_at = Some(Instant::now() + TURN_SETTLE);
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
    if state.end_requested && state.final_seen {
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
            // Whichever deadline this was, labelled where it was armed: a final
            // that arrived after the arm must not relabel the running timer.
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            {
                note_cause(state, armed_final_kind(state));
                state.armed_final = None;
            }
            if state.final_seen {
                // A final was seen, so only the tail was outstanding and the
                // request completes.
                finish(socket, upstream, state).await;
            } else {
                fail(
                    socket,
                    upstream,
                    state,
                    UiMessage::new(MessageId::LiveFinalTimeout),
                )
                .await;
            }
        }
        Some(Timer::Settle) => {
            state.settle_at = None;
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_deadline(state, DeadlineTimer::Settle);
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_cause(state, RequestCause::SettleTimeout);
            finish(socket, upstream, state).await;
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

/// After `activityEnd` only the spoken tail is outstanding, and a session that already
/// produced a final transcript must not keep the UI hanging for the full timeout.
fn arm_final_timeout(state: &mut Relay) {
    let final_seen = state.final_seen;
    let deadline = if final_seen {
        TAIL_GRACE
    } else {
        FINAL_TIMEOUT
    };
    // Which deadline this is, decided where it is armed: a final that arrives
    // later must not relabel the timer that is already running.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    {
        state.armed_final = Some(if final_seen {
            RequestCause::TailGrace
        } else {
            RequestCause::FinalTimeout
        });
    }
    state.final_at = Some(Instant::now() + deadline);
}

/// The final deadline the expiry reports: the one that was armed, never one
/// inferred from transcript state a later final changed. A relay with a final
/// deadline armed always has this recorded; nothing recorded means the long one.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn armed_final_kind(state: &Relay) -> RequestCause {
    state.armed_final.unwrap_or(RequestCause::FinalTimeout)
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
        write_upstream(upstream, state, &audio_payload(&frame)).await;
    }
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(trace) = state.trace.as_ref() {
        // The queue is empty now, so the last depth a reader sees is zero.
        trace.pre_ready_queue_depth(0);
    }
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

    /// One boolean counter of the newest relay snapshot.
    fn counter_flag(records: &[Value], field: &str) -> Option<bool> {
        counters(records)?.get(field)?.as_bool()
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

    /// A controlled silent upstream: the tail grace expires, the journal names
    /// it as the tail grace (not as a new final), and the stall is visible in
    /// the counters - the relay sent everything and heard nothing back.
    #[tokio::test]
    async fn a_silent_upstream_leaves_the_tail_grace_and_the_stall_in_the_journal() {
        let root = scratch("tail-grace");
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

        // While the upstream stays silent the writer keeps writing: a snapshot
        // taken before any terminal - the request has not ended yet - already
        // carries all three client frames with nothing new from the upstream.
        // That snapshot is gated on the sink's own clock, and this fixture's
        // clock is frozen, so advance it past the one-second gate; the wait is
        // three ticks, 300 ms, far inside the six-second grace still running.
        diagnostics.advance_clock(Duration::from_secs(2));
        let stall = journal_when(&root, 30, |rows| {
            terminal(rows).is_none() && counter(rows, "client_audio_frames") == Some(3)
        })
        .await;
        assert!(
            terminal(&stall).is_none(),
            "the tail grace had not expired when the stall was observed"
        );
        let stalled_upstream =
            counter(&stall, "last_upstream_us").expect("a snapshot written in the stall");
        assert_eq!(
            counter(&stall, "received_messages"),
            Some(2),
            "nothing new arrived from the silent upstream"
        );

        // The upstream is silent from here on: the six-second tail grace is the
        // only thing that can end this request, and it must.
        loop {
            match client.next().await {
                Some(Ok(WsMessage::Text(text))) if text.contains("done") => break,
                Some(Ok(_)) => continue,
                other => panic!("the relay never finished: {other:?}"),
            }
        }

        let records = journal_when(&root, 300, |rows| {
            terminal(rows).is_some_and(|end| end["cause"] == "tail_grace")
        })
        .await;
        let end = terminal(&records).expect("the tail grace ends the request");
        assert_eq!(
            end["cause"], "tail_grace",
            "the short deadline is the tail grace, not a missing final"
        );
        assert_eq!(
            end["outcome"], "success",
            "a final was already seen, so the request succeeded"
        );
        assert!(
            has_deadline(&records, "final"),
            "the final timer is the deadline that fired"
        );
        // The stall itself: three frames out, only the two upstream messages
        // that opened the session and carried the final back.
        assert_eq!(
            counter(&records, "client_audio_frames"),
            Some(3),
            "every client frame is counted"
        );
        assert_eq!(
            counter(&records, "upstream_sent_frames"),
            Some(3),
            "every frame was sent"
        );
        assert_eq!(
            counter(&records, "received_messages"),
            Some(2),
            "only the setup and the final came back"
        );
        assert_eq!(
            counter_flag(&records, "in_flight_send"),
            Some(false),
            "no send is left in flight"
        );
        assert_eq!(
            counter(&records, "upstream_pcm_bytes"),
            Some(3_072),
            "the PCM the relay sent is counted"
        );
        assert_eq!(
            counter(&records, "pending_send_us"),
            Some(0),
            "no send is waiting"
        );
        assert_eq!(
            counter(&records, "last_upstream_us"),
            Some(stalled_upstream),
            "the last upstream message never moved during the stall"
        );
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

        let records = journal_when(&root, 300, |rows| {
            !content_bodies(rows, "socket_close").is_empty()
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
        // fixture's clock is frozen: advance it past the one-second gate so the
        // periodic relay snapshot carrying the counter is written too. The wait
        // then needs both the byte record and that snapshot.
        diagnostics.advance_clock(Duration::from_secs(2));
        let records = journal_when(&root, 300, |rows| {
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

        // Advance the frozen clock past the snapshot gate, then wait for the
        // byte record and the periodic snapshot that carries the counters.
        diagnostics.advance_clock(Duration::from_secs(2));
        let records = journal_when(&root, 300, |rows| {
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

    /// The final deadline's expiry reports the timer that was armed, not one
    /// inferred from transcript state a later final changed: `end` before any
    /// final arms the long deadline, and a final arriving afterwards must not
    /// relabel it as the six-second tail grace.
    #[tokio::test]
    async fn the_final_deadline_reports_the_timer_that_was_armed() {
        fn new_relay() -> Relay {
            let key = Arc::new(SecretKey::new(SYNTHETIC_KEY.to_string()));
            let ring = Arc::new(KeyRing::new(vec![Arc::clone(&key)]));
            Relay::new(key, ring, Arc::default(), Arc::default(), None)
        }

        let mut state = new_relay();
        arm_final_timeout(&mut state);
        assert_eq!(armed_final_kind(&state), RequestCause::FinalTimeout);

        // The late final: it sets `final_seen` and a settle deadline, but the
        // fifteen-second deadline it did not re-arm is still the one running.
        state.final_seen = true;
        state.settle_at = Some(Instant::now() + FINAL_SETTLE);
        assert_eq!(armed_final_kind(&state), RequestCause::FinalTimeout);

        // A final already seen at arm time is the short deadline.
        let mut state = new_relay();
        state.final_seen = true;
        arm_final_timeout(&mut state);
        assert_eq!(armed_final_kind(&state), RequestCause::TailGrace);
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
        // frozen-clock gate, advanced the same way, admits it.
        diagnostics.advance_clock(Duration::from_secs(2));
        let records = journal_when(&root, 30, |rows| {
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
