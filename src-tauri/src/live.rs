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

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as ClientMessage, WebSocket};
use futures_util::future::OptionFuture;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::Message as UpstreamMessage;

use crate::backend::redact;
use crate::provider::Provider;
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
}

impl Relay {
    fn new(key: Arc<SecretKey>, ring: Arc<KeyRing>) -> Self {
        Self {
            key,
            ring,
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
) {
    // Redaction is only ever as complete as the ring: the socket's own key has to
    // be part of it, and it is, because it was selected from it.
    debug_assert!(ring
        .keys()
        .iter()
        .any(|candidate| Arc::ptr_eq(candidate, &key)));

    let mut socket = socket;
    let mut upstream: Option<Upstream> = None;
    let mut state = Relay::new(key, ring);

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
        fail(socket, upstream, state, "Malformed JSON message.").await;
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
                &format!("Unknown message type: {described}"),
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
            "Live start must include mode 'smart' or 'verbatim'.",
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
            let message = format!("Could not open the Live API connection: {error}");
            fail(socket, upstream, state, &message).await;
            return;
        }
        Err(_) => {
            fail(
                socket,
                upstream,
                state,
                "Gemini did not confirm the Live session setup in time.",
            )
            .await;
            return;
        }
    };

    match tokio::time::timeout_at(
        ready_at,
        live.send(UpstreamMessage::text(setup_payload(mode).to_string())),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            // A close event or the ready deadline reports a setup that never confirms.
            warn(
                state.ring.keys(),
                &format!("Could not write to the Live API socket: {error}"),
            );
        }
        Err(_) => {
            fail(
                socket,
                upstream,
                state,
                "Gemini did not confirm the Live session setup in time.",
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
            "Audio arrived before the start message.",
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
            "Audio frames must carry base64 PCM16 in a 'data' string.",
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
            "Audio frames must be base64-encoded 16-bit PCM.",
        )
        .await;
        return;
    }
    if state.bytes + byte_length as u64 > MAX_AUDIO_BYTES {
        fail(
            socket,
            upstream,
            state,
            &format!(
                "Recording exceeded the {} minute limit.",
                MAX_RECORDING_SECONDS / 60
            ),
        )
        .await;
        return;
    }
    state.bytes += byte_length as u64;

    if state.upstream_ready {
        write_upstream(upstream, state, &audio_payload(data)).await;
    } else {
        // Setup is still in flight: hold the frames and replay them in order.
        state.queue.push(data.to_owned());
    }
}

/// `end`: stop the recording and wait for the final transcript.
async fn end(socket: &mut WebSocket, upstream: &mut Option<Upstream>, state: &mut Relay) {
    if state.done {
        return;
    }
    if !state.started {
        fail(socket, upstream, state, "Received end before start.").await;
        return;
    }
    if state.end_requested {
        return;
    }
    state.end_requested = true;

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
            handle_upstream_message(socket, upstream, state, &raw).await;
        }
        Some(Ok(UpstreamMessage::Close(frame))) => {
            let (code, reason) = match frame {
                Some(frame) => (u16::from(frame.code), frame.reason.as_str().to_owned()),
                None => (1005, String::new()),
            };
            upstream_closed(socket, upstream, state, code, &reason).await;
        }
        Some(Ok(UpstreamMessage::Binary(data))) => {
            // Binary frames are decoded as UTF-8 and parsed the same way;
            // undecodable bytes are not JSON either, so they are dropped.
            if let Ok(raw) = std::str::from_utf8(&data) {
                let raw = raw.to_owned();
                handle_upstream_message(socket, upstream, state, &raw).await;
            }
        }
        // Ping/Pong are answered by the socket layer; a raw frame never surfaces here.
        Some(Ok(
            UpstreamMessage::Ping(_) | UpstreamMessage::Pong(_) | UpstreamMessage::Frame(_),
        )) => {}
        Some(Err(error)) => {
            // This only warns; the close event (or the ready deadline) reports the
            // failure, and a terminated stream yields `None` next.
            warn(
                state.ring.keys(),
                &format!("Live API socket reported an error: {error}"),
            );
        }
        // End of stream without a close frame: the browser API reports that as 1006.
        None => upstream_closed(socket, upstream, state, 1006, "").await,
    }
}

/// Handles one JSON message from the Live socket.
async fn handle_upstream_message(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    raw: &str,
) {
    if state.done || raw.is_empty() {
        return;
    }
    let Some(message) = parse_json_object(raw) else {
        return;
    };

    if message.get("setupComplete").is_some() {
        state.ready_at = None;
        state.upstream_ready = true;
        write_client(socket, state.ring.keys(), &json!({ "type": "ready" })).await;
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
        fail(
            socket,
            upstream,
            state,
            &format!("Live API error: {failure}"),
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
            state.ring.keys(),
            &json!({ "type": "interim", "text": interim }),
        )
        .await;
    }

    if let Some(final_text) = content.get("inputTranscription").and_then(text_of) {
        write_client(
            socket,
            state.ring.keys(),
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
    if !state.upstream_ready {
        let suffix = if detail.is_empty() {
            " (closed before setup completed)".to_owned()
        } else {
            format!(": {detail}")
        };
        fail(
            socket,
            upstream,
            state,
            &format!("Live session setup failed{suffix}."),
        )
        .await;
        return;
    }
    if state.end_requested && state.final_seen {
        finish(socket, upstream, state).await;
        return;
    }
    let suffix = if detail.is_empty() {
        " unexpectedly".to_owned()
    } else {
        format!(": {detail}")
    };
    fail(
        socket,
        upstream,
        state,
        &format!("Live connection closed{suffix}."),
    )
    .await;
}

/// Human-readable close reason: Google carries an `{"error":{"message":...}}`
/// payload in the close reason, anything else is text.
fn describe_close(code: u16, reason: &str) -> String {
    let reason = reason.trim();
    if !reason.is_empty() {
        if let Some(message) = parse_json_object(reason)
            .as_ref()
            .and_then(upstream_error_message)
        {
            return message;
        }
        return reason.to_owned();
    }
    if code != 1000 {
        return format!("close code {code}");
    }
    String::new()
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
            if !state.upstream_ready {
                fail(
                    socket,
                    upstream,
                    state,
                    "Gemini did not confirm the Live session setup in time.",
                )
                .await;
            }
        }
        Some(Timer::Final) => {
            state.final_at = None;
            if state.final_seen {
                finish(socket, upstream, state).await;
            } else {
                fail(
                    socket,
                    upstream,
                    state,
                    "Timed out waiting for the final transcript from the Live API.",
                )
                .await;
            }
        }
        Some(Timer::Settle) => {
            state.settle_at = None;
            finish(socket, upstream, state).await;
        }
        Some(Timer::Hard) => {
            state.hard_at = None;
            fail(
                socket,
                upstream,
                state,
                &format!(
                    "Live session exceeded the {} minute limit.",
                    MAX_RECORDING_SECONDS / 60
                ),
            )
            .await;
        }
        Some(Timer::Idle) => {
            // No verdict to report: the socket itself timed out, so the browser sees
            // the close and nothing else.
            state.idle_at = None;
            cancel(socket, upstream, state).await;
        }
        // Nothing was actually due: a wake that arrived after the timer was cleared.
        None => {}
    }
}

/// After `activityEnd` only the spoken tail is outstanding, and a session that already
/// produced a final transcript must not keep the UI hanging for the full timeout.
fn arm_final_timeout(state: &mut Relay) {
    let deadline = if state.final_seen {
        TAIL_GRACE
    } else {
        FINAL_TIMEOUT
    };
    state.final_at = Some(Instant::now() + deadline);
}

/* -------------------------------------------------------------------------- */
/* Terminal states                                                            */
/* -------------------------------------------------------------------------- */

/// Terminal failure: report it, then always finish with `done` so the UI cannot hang.
async fn fail(
    socket: &mut WebSocket,
    upstream: &mut Option<Upstream>,
    state: &mut Relay,
    message: &str,
) {
    if state.done {
        return;
    }
    let safe = redact(message, state.ring.keys());
    write_client(
        socket,
        state.ring.keys(),
        &json!({ "type": "error", "message": safe }),
    )
    .await;
    finish(socket, upstream, state).await;
}

/// Terminal success: exactly one `done`, then both sockets close.
async fn finish(socket: &mut WebSocket, upstream: &mut Option<Upstream>, state: &mut Relay) {
    if state.done {
        return;
    }
    state.done = true;
    state.clear_timers();
    write_client(socket, state.ring.keys(), &json!({ "type": "done" })).await;
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
async fn write_client(socket: &mut WebSocket, keys: &[Arc<SecretKey>], payload: &Value) {
    if let Err(error) = socket.send(ClientMessage::text(payload.to_string())).await {
        warn(
            keys,
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
    if let Err(error) = live.send(UpstreamMessage::text(payload.to_string())).await {
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

/// The exact setup frame the Live API expects, field for field.
fn setup_payload(mode: Mode) -> Value {
    json!({
        "setup": {
            "model": format!("models/{LIVE_MODEL}"),
            "generationConfig": { "responseModalities": ["TEXT"] },
            "realtimeInputConfig": { "automaticActivityDetection": { "disabled": true } },
            "inputAudioTranscription": { "mode": mode.live_value() },
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
