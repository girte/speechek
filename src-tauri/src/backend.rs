//! Speechek's in-process loopback backend.
//!
//! One Axum service owns the loopback socket this process reserved: the
//! configured port, or a temporary one when it was already taken. It hands the
//! fifteen `public/` files to the overlay, lab and settings windows from memory
//! and proxies transcription to Gemini through [`crate::provider`] and
//! [`crate::live`].
//!
//! Four properties outrank the rest:
//!
//! * The port is claimed exclusively, after a connect probe, with
//!   `SO_EXCLUSIVEADDRUSE`. Windows otherwise lets a second process bind (and
//!   silently share) a port that somebody is already serving.
//! * A key rotates only for an *accepted* request. Every rejection - origin,
//!   mode, content type, WAV shape, busy server - happens before a key is
//!   chosen, so malformed traffic never consumes a turn of the rotation.
//! * No configured key can reach a response body, an error message or a log
//!   line: every outgoing message passes through [`redact`].
//! * Every request is served by one revision. A tagged audio request
//!   (`?session=<generation>`) is answered from the settings and the key ring
//!   its dictation pinned; a tag that no longer resolves is a 409 and never
//!   falls forward to newer settings. Untagged requests observe the current
//!   revision, which is the comparison lab's contract.
//!
//! The HTTP and WebSocket wire contracts preserve the original local backend's
//! status codes, headers, rejection order and JSON shapes consumed by the
//! shared recorder and comparison lab.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::sync::{oneshot, Semaphore};

use crate::i18n::{render, Language, MessageId, UiError, UiMessage};
use crate::live;
use crate::provider::Provider;
use crate::secrets::SecretKey;
use crate::settings::{RuntimeSnapshot, SharedRuntime};

/* -------------------------------------------------------------------------- */
/* Constants                                                                  */
/* -------------------------------------------------------------------------- */

const HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// The settings `mode` that drives `/api/live`. A tagged socket is only served
/// from a revision whose settings say exactly this: the Live relay and a batch
/// recording are two different dictations.
const LIVE_MODE: &str = "live";

/// `Number.MAX_SAFE_INTEGER`: the largest generation a browser can name
/// without losing it to float rounding. A larger tag could not have come out of
/// the shell's generation counter.
const MAX_SESSION_ID: u64 = 9_007_199_254_740_991;

/// Audio contract shared by both paths: raw 16-bit PCM, 16 kHz, mono.
const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;
const BYTES_PER_SAMPLE: u32 = (BITS_PER_SAMPLE / 8) as u32;

/// 10 minutes, the documented Live session limit.
const MAX_RECORDING_SECONDS: u32 = 600;
const MAX_AUDIO_BYTES: u32 =
    SAMPLE_RATE * (CHANNELS as u32) * BYTES_PER_SAMPLE * MAX_RECORDING_SECONDS;
/// Header/container room; the duration check speaks before this cap does.
const MAX_UPLOAD_BYTES: u64 = MAX_AUDIO_BYTES as u64 + 64 * 1024;
const MAX_CONCURRENT_BATCH: usize = 2;

/// Message size cap for `/api/live`: one message may not exceed 2 MiB.
const MAX_WS_PAYLOAD: usize = 1 << 21;

/// The connect probe answers within a second or the port counts as free.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1_000);
/// Bound on draining spawned work while the backend shuts down.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(1_500);

const MIME_HTML: &str = "text/html;charset=utf-8";
const MIME_JS: &str = "text/javascript;charset=utf-8";
const MIME_CSS: &str = "text/css;charset=utf-8";
const MIME_TEXT: &str = "text/plain;charset=utf-8";

/* -------------------------------------------------------------------------- */
/* Errors and redaction                                                       */
/* -------------------------------------------------------------------------- */

/// An HTTP-visible failure. `status` and `code` are the stable machine-readable
/// fields the browser switches on; `message` is the English diagnostic rendered
/// from `ui`, and `ui` is the descriptor the interface language renders.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub ui: UiMessage,
}

impl ApiError {
    /// Build a failure from its semantic descriptor; the English diagnostic is
    /// rendered from that descriptor, not kept as a second source of copy.
    pub fn new(status: u16, code: &'static str, ui: UiMessage) -> Self {
        let message = render(Language::En, &ui).into_owned();
        Self {
            status,
            code,
            message,
            ui,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ApiError {}

/// Never let any configured key escape through an error string, a log line or a
/// response body. The keys are borrowed `Arc<SecretKey>` handles, so redaction
/// never copies a secret. Empty keys are skipped: replacing an empty needle
/// would insert `[redacted]` between every single character.
pub fn redact(message: &str, keys: &[Arc<SecretKey>]) -> String {
    let mut safe = message.to_string();
    redact_string(&mut safe, keys);
    safe
}

/// Replaces every configured key inside one string, in place.
fn redact_string(text: &mut String, keys: &[Arc<SecretKey>]) {
    for key in keys {
        let key = key.as_str();
        if key.is_empty() || !text.contains(key) {
            continue;
        }
        *text = text.replace(key, "[redacted]");
    }
}

/// Redacts one descriptor in place: every string argument, however deeply nested
/// behind a nested `UiMessage`, is cleaned against the same ring the message's
/// own HTTP or WebSocket boundary selected. Numbers and identifiers are left
/// alone, and the keys stay borrowed handles rather than copies of a secret.
pub fn redact_ui(ui: &mut UiMessage, keys: &[Arc<SecretKey>]) {
    let Some(args) = ui.args.as_mut() else {
        return;
    };
    for value in args.values_mut() {
        redact_value(value, keys);
    }
}

fn redact_value(value: &mut Value, keys: &[Arc<SecretKey>]) {
    match value {
        Value::String(text) => redact_string(text, keys),
        Value::Array(items) => {
            for item in items {
                redact_value(item, keys);
            }
        }
        Value::Object(map) => {
            for nested in map.values_mut() {
                redact_value(nested, keys);
            }
        }
        _ => {}
    }
}

/// Redacts both halves of one outgoing failure against `keys`, the ring the
/// request selected: the English diagnostic and the descriptor, argument by
/// argument. A newer ring is never consulted here.
fn redact_api_error(error: &mut ApiError, keys: &[Arc<SecretKey>]) {
    redact_string(&mut error.message, keys);
    redact_ui(&mut error.ui, keys);
}

/// The response boundary retains the selected request revision even if Save or
/// dictation retirement changes the runtime while the request is in flight.
fn request_error_response(
    mut error: ApiError,
    selected: Option<&RequestSnapshot>,
    runtime: &SharedRuntime,
) -> Response {
    let current;
    let keys = match selected {
        Some(request) => request.keys(),
        None => {
            current = runtime.snapshot();
            current.keys.keys()
        }
    };
    redact_api_error(&mut error, keys);
    api_error_response(error)
}

fn missing_key_error() -> ApiError {
    ApiError::new(503, "MISSING_API_KEY", UiMessage::new(MessageId::MissingApiKey))
}

fn cross_origin_error() -> ApiError {
    ApiError::new(403, "CROSS_ORIGIN", UiMessage::new(MessageId::CrossOrigin))
}

/// A `session` tag that is not the exact decimal spelling of one positive
/// integer a browser can hold. An unreadable tag is refused rather than treated
/// as an absent one: silently untagging a request would let it run on settings
/// it does not belong to.
fn invalid_session_error() -> ApiError {
    ApiError::new(400, "INVALID_SESSION", UiMessage::new(MessageId::InvalidSession))
}

/// The dictation's revision is gone: it was retired when the take ended. A late
/// request of that take must not run on newer settings instead.
fn stale_session_error() -> ApiError {
    ApiError::new(409, "STALE_SESSION", UiMessage::new(MessageId::StaleSession))
}

/// The request is not the dictation it claims to be part of: a batch request
/// asks for a mode the pinned settings do not have, or the Live socket asks to
/// be served by a dictation that is not Live.
fn mode_mismatch_error() -> ApiError {
    ApiError::new(409, "MODE_MISMATCH", UiMessage::new(MessageId::ModeMismatch))
}

/* -------------------------------------------------------------------------- */
/* State                                                                      */
/* -------------------------------------------------------------------------- */

/// Everything a request needs: the runtime revisions with their key rings, the
/// one provider client and the batch admission counter. The rotation state lives
/// in the snapshot's [`KeyRing`](crate::secrets::KeyRing), so a request can never
/// observe half of a Save.
struct AppState {
    runtime: Arc<SharedRuntime>,
    provider: Arc<Provider>,
    /// Two concurrent batch transcriptions, mirroring `MAX_CONCURRENT_BATCH`.
    batches: Arc<Semaphore>,
    /// The loopback origins allowed to drive the audio endpoints, derived once
    /// from the port this backend really serves: the configured one, or the
    /// temporary one the reservation fell back to.
    allowed_origins: [String; 3],
}

/* -------------------------------------------------------------------------- */
/* Backend lifecycle                                                          */
/* -------------------------------------------------------------------------- */

/// The running loopback backend: one bound socket, one Tokio runtime thread and
/// the shutdown signal that retires both.
pub struct Backend {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    /// Serve the socket [`reserve`] already bound.
    ///
    /// Returns only once the listener is owned by the runtime thread and
    /// accepting, so the caller may create windows and register hotkeys
    /// immediately afterwards. The port is neither probed nor bound again here:
    /// the reservation is the single place that decides it, so nothing can take
    /// the port between that decision and the accept loop.
    pub fn start(
        reservation: ReservedListener,
        runtime: Arc<SharedRuntime>,
        provider: Arc<Provider>,
    ) -> Result<Self, UiError> {
        let ReservedListener {
            listener,
            actual_port,
            ..
        } = reservation;

        let state = Arc::new(AppState {
            runtime,
            provider,
            batches: Arc::new(Semaphore::new(MAX_CONCURRENT_BATCH)),
            allowed_origins: loopback_origins(actual_port),
        });

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("speechek-backend")
            .build()
            .map_err(|error| UiError::new("BACKEND_RUNTIME", UiMessage::new(MessageId::BackendRuntimeFailed)
                .with_arg("detail", json!(error.to_string()))))?;

        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), UiError>>(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let thread = std::thread::Builder::new()
            .name("speechek-backend".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(listener) => listener,
                        Err(error) => {
                            let _ = ready_tx.send(Err(UiError::new("BACKEND_SERVE",
                                UiMessage::new(MessageId::BackendServeFailed)
                                    .with_arg("port", json!(actual_port))
                                    .with_arg("detail", json!(error.to_string())))));
                            return;
                        }
                    };

                    let mut server = tokio::spawn(async move {
                        if let Err(error) = axum::serve(listener, router(state)).await {
                            eprintln!("speechek backend stopped serving: {error}");
                        }
                    });
                    // The socket has been listening since the bind; accept is
                    // running now, so anything already queued is about to be served.
                    let _ = ready_tx.send(Ok(()));

                    tokio::select! {
                        _ = shutdown_rx => {}
                        _ = &mut server => {}
                    }
                    // Aborting drops the accept loop and its listener, so the port
                    // is free immediately instead of waiting out an open Live socket.
                    server.abort();
                });

                // Spawned work (Live sockets, in-flight uploads) is bounded rather
                // than awaited forever: this backend is on its way out either way.
                runtime.shutdown_timeout(DRAIN_TIMEOUT);
            })
            .map_err(|error| UiError::new("BACKEND_THREAD", UiMessage::new(MessageId::BackendThreadFailed)
                .with_arg("detail", json!(error.to_string()))))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shutdown: Some(shutdown_tx),
                thread: Some(thread),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(UiError::new("BACKEND_NOT_READY", UiMessage::new(MessageId::BackendStoppedBeforeReady))),
        }
    }

    /// Release the port, stop accepting and retire the runtime thread.
    pub fn shutdown(&mut self) {
        if let Some(signal) = self.shutdown.take() {
            let _ = signal.send(());
        }
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                eprintln!("speechek backend thread ended abnormally");
            }
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The loopback socket a start serves from: bound once by [`reserve`], after the
/// port was decided, and held until [`Backend::start`] hands it to the runtime
/// thread. Nothing between the decision and the accept loop binds the port
/// again, so no other process can take it in between and the page origin is the
/// port the backend really serves.
pub struct ReservedListener {
    listener: TcpListener,
    /// The port the socket actually answers on; it differs from
    /// `configured_port` only when that one was taken.
    actual_port: u16,
    /// The port the settings document named for this start.
    configured_port: u16,
    /// Set when `configured_port` was already in use and a temporary loopback
    /// port was claimed instead. The saved value is not changed: the next start
    /// tries the configured port again.
    fallback: bool,
}

impl ReservedListener {
    /// The port the socket answers on.
    pub fn actual_port(&self) -> u16 {
        self.actual_port
    }

    /// The port the settings document named for this start.
    pub fn configured_port(&self) -> u16 {
        self.configured_port
    }

    /// Whether the configured port was taken and a temporary one is in use.
    pub fn fallback(&self) -> bool {
        self.fallback
    }
}

/// Claim the loopback socket the shell will serve from.
///
/// `configured_port` is the port the settings document names; it is claimed
/// exclusively, after the same connect probe Windows needs, and *only* a port
/// that is genuinely in use makes this fall back to a temporary loopback port,
/// whose number is read from the socket itself. A bind that fails for any other
/// reason, and a configured port of 0, are returned as errors: an unusable
/// stored value must never turn into a random port, and a missing socket exists
/// nowhere else to fall back to.
pub fn reserve(configured_port: u16) -> Result<ReservedListener, UiError> {
    if configured_port == 0 {
        return Err(UiError::new("BACKEND_INVALID_PORT", UiMessage::new(MessageId::BackendPortZero)));
    }

    // Windows lets a second process bind a port that is already being served, so
    // an occupied port has to be detected by connecting to it; without this
    // probe two backends would share the port and requests would land on either
    // one. The probe is only the cheap test: `bind_exclusive` refuses an
    // occupied port on its own through `SO_EXCLUSIVEADDRUSE`.
    if port_in_use(configured_port) {
        return reserve_fallback(configured_port);
    }

    match bind_exclusive(configured_port) {
        Ok(listener) => finish_reservation(listener, configured_port, false),
        Err(BindError::InUse) => reserve_fallback(configured_port),
        Err(BindError::Fatal(message)) => Err(message),
    }
}

/// Claim a temporary loopback port for a start whose configured port is taken.
fn reserve_fallback(configured_port: u16) -> Result<ReservedListener, UiError> {
    match bind_exclusive(0) {
        Ok(listener) => finish_reservation(listener, configured_port, true),
        // Port zero is never in use; a second `InUse` is the system saying it
        // has no temporary port to give, which is as fatal as a taken one.
        Err(BindError::InUse) => Err(UiError::new("BACKEND_NO_FALLBACK",
            UiMessage::new(MessageId::BackendNoFallbackPort)
                .with_arg("port", json!(configured_port)))),
        Err(BindError::Fatal(message)) => Err(message),
    }
}

/// The reservation for a socket that is bound but not yet served: the actual
/// port is read from the socket, so `fallback` reports the port the operating
/// system really gave rather than one this code assumed.
fn finish_reservation(
    listener: TcpListener,
    configured_port: u16,
    fallback: bool,
) -> Result<ReservedListener, UiError> {
    let actual_port = listener
        .local_addr()
        .map_err(|error| UiError::new("BACKEND_PORT_READ", UiMessage::new(MessageId::BackendPortReadFailed)
            .with_arg("detail", json!(error.to_string()))))?
        .port();
    Ok(ReservedListener {
        listener,
        actual_port,
        configured_port,
        fallback,
    })
}

/// Windows lets a second process bind a port that is already being served, so an
/// occupied port has to be detected by connecting to it: without this probe two
/// backends would share the port and requests would land on either one.
fn port_in_use(port: u16) -> bool {
    let address = SocketAddr::V4(SocketAddrV4::new(HOST, port));
    match TcpStream::connect_timeout(&address, PROBE_TIMEOUT) {
        Ok(stream) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            true
        }
        Err(_) => false,
    }
}

/// How a bind attempt failed: the address is taken (a fallback may follow) or
/// something else went wrong (a fallback must not follow).
enum BindError {
    /// Another socket holds the address.
    InUse,
    /// A message to show; the failure is not a taken port.
    Fatal(UiError),
}

/// Bind and listen on the loopback port, refusing to share it.
fn bind_exclusive(port: u16) -> Result<TcpListener, BindError> {
    let address = SocketAddrV4::new(HOST, port);
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).map_err(|error| {
        BindError::Fatal(UiError::new("BACKEND_SOCKET", UiMessage::new(MessageId::BackendSocketCreateFailed)
            .with_arg("detail", json!(error.to_string()))))
    })?;

    // `SO_REUSEADDR` is deliberately never enabled: on Windows it is exactly the
    // option that lets a later process steal a bound port, and on every other
    // platform the default already refuses a second bind.
    #[cfg(windows)]
    set_exclusive_address_use(&socket).map_err(BindError::Fatal)?;
    #[cfg(not(windows))]
    socket.set_reuse_address(false).map_err(|error| {
        BindError::Fatal(UiError::new("BACKEND_SOCKET_CONFIG", UiMessage::new(MessageId::BackendSocketConfigureFailed)
            .with_arg("detail", json!(error.to_string()))))
    })?;

    socket.bind(&address.into()).map_err(|error| {
        if is_address_in_use(&error) {
            BindError::InUse
        } else {
            BindError::Fatal(UiError::new("BACKEND_BIND", UiMessage::new(MessageId::BackendBindFailed)
                .with_arg("port", json!(port)).with_arg("detail", json!(error.to_string()))))
        }
    })?;
    socket.listen(128).map_err(|error| {
        BindError::Fatal(UiError::new("BACKEND_LISTEN", UiMessage::new(MessageId::BackendListenFailed)
            .with_arg("port", json!(port)).with_arg("detail", json!(error.to_string()))))
    })?;
    socket.set_nonblocking(true).map_err(|error| {
        BindError::Fatal(UiError::new("BACKEND_PREPARE", UiMessage::new(MessageId::BackendPrepareFailed)
            .with_arg("port", json!(port)).with_arg("detail", json!(error.to_string()))))
    })?;
    Ok(socket.into())
}

/// Whether a bind failed because somebody else holds the address.
///
/// `std` maps the Win32/Winsock code to `AddrInUse`, and the raw Winsock code
/// is checked as well: `socket2` reports the bind's own error, and the fallback
/// must not be skipped over a code `std` happens to leave as `Uncategorized`.
fn is_address_in_use(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::AddrInUse {
        return true;
    }
    #[cfg(windows)]
    if error.raw_os_error() == Some(WSAEADDRINUSE) {
        return true;
    }
    false
}

/// `WSAEADDRINUSE`: the Winsock code for "this address is already bound", which
/// is what `SO_EXCLUSIVEADDRUSE` produces when the probe could not see the
/// owner.
#[cfg(windows)]
const WSAEADDRINUSE: i32 = 10048;

/// `SO_EXCLUSIVEADDRUSE` makes every later bind of this port fail, including one
/// that asks for `SO_REUSEADDR`. That is the Windows-only half of "one backend
/// per port"; the connect probe covers the other half.
#[cfg(windows)]
fn set_exclusive_address_use(socket: &Socket) -> Result<(), UiError> {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{
        setsockopt, SOCKET, SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
    };

    let enabled = 1i32.to_ne_bytes();
    // SAFETY: `socket` owns a live socket handle for the duration of this call.
    let result = unsafe {
        setsockopt(
            SOCKET(socket.as_raw_socket() as usize),
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            Some(&enabled[..]),
        )
    };
    if result != 0 {
        return Err(UiError::new("BACKEND_EXCLUSIVE", UiMessage::new(MessageId::BackendExclusiveFailed)
            .with_arg("detail", json!(io::Error::last_os_error().to_string()))));
    }
    Ok(())
}

/* -------------------------------------------------------------------------- */
/* Router                                                                     */
/* -------------------------------------------------------------------------- */

fn router(state: Arc<AppState>) -> Router {
    // `DefaultBodyLimit::disable()` hands body sizing to `read_body_capped`,
    // which enforces the same limit with the JSON error the browser expects: the
    // default 2 MiB cap would reject every long recording before it is parsed.
    Router::new()
        .fallback(any(dispatch))
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

/// Which audio route is asking; decides what a session tag has to match.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioRoute {
    /// `POST /api/transcribe?mode=smart|verbatim`.
    Batch,
    /// `GET /api/live`, the WebSocket upgrade.
    Live,
}

/// The one revision a request is served with, plus the tag that chose it.
#[derive(Clone)]
struct RequestSnapshot {
    /// Pinned for a tagged audio request, current for everything else: the
    /// settings a request reads and the ring it is redacted against.
    snapshot: Arc<RuntimeSnapshot>,
    /// The generation the request tagged itself with, when it carried a tag.
    session: Option<u64>,
}

impl RequestSnapshot {
    /// The key ring behind this revision, for [`redact`].
    fn keys(&self) -> &[Arc<SecretKey>] {
        self.snapshot.keys.keys()
    }
}

/// Chooses the revision one audio request is served with.
///
/// Called only after the route's own gates (method, upgrade request, Origin) have
/// passed: a foreign Origin keeps answering `CROSS_ORIGIN` and never looks like a
/// session problem. A request without a tag observes the current revision, which
/// is the comparison lab's contract. A tag binds the request to the dictation
/// that pinned that generation; a generation that is no longer pinned - a
/// finished, cancelled or never-started take - is a 409, never a silent fallback
/// to newer settings.
fn select_request_snapshot(
    state: &AppState,
    route: AudioRoute,
    uri: &Uri,
) -> Result<RequestSnapshot, ApiError> {
    let Some(generation) = query_session(uri.query())? else {
        return Ok(RequestSnapshot {
            snapshot: state.runtime.snapshot(),
            session: None,
        });
    };
    let Some(snapshot) = state.runtime.resolve(Some(generation)) else {
        return Err(stale_session_error());
    };
    // The Live socket and a batch recording are two different dictations, so a
    // tag may only ask for the one that was actually pinned.
    if route == AudioRoute::Live && snapshot.settings.mode != LIVE_MODE {
        return Err(mode_mismatch_error());
    }
    Ok(RequestSnapshot {
        snapshot,
        session: Some(generation),
    })
}

async fn dispatch(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    // The upgrade is taken out of the request by hand rather than declared as a
    // handler extractor: `WebSocketUpgrade` claims the `OnUpgrade` extension, so
    // it has to see this request, and only `/api/live` has any use for it.
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &state)
        .await
        .ok();
    let method = parts.method;
    let uri = parts.uri;
    let headers = parts.headers;
    let path = normalize_path(uri.path());

    // The revision this request is served with, recorded by an audio route once
    // its own gates have passed. An error raised before that point is redacted
    // against the current revision, because the request never got a ring of its
    // own.
    let mut selected: Option<RequestSnapshot> = None;

    // Bound before the match, so the handler future - and with it the mutable
    // borrow of `selected` - is done with before the error path reads it back.
    let outcome = handle(
        &state,
        &mut selected,
        &method,
        &path,
        &uri,
        &headers,
        upgrade,
        body,
    )
    .await;

    match outcome {
        Ok(response) => response,
        Err(error) => {
            // Method, status and code only: untrusted paths can contain secrets.
            eprintln!("{method} -> {} {}", error.status, error.code);
            request_error_response(error, selected.as_ref(), &state.runtime)
        }
    }
}

async fn handle(
    state: &Arc<AppState>,
    selected: &mut Option<RequestSnapshot>,
    method: &Method,
    path: &str,
    uri: &Uri,
    headers: &HeaderMap,
    upgrade: Option<WebSocketUpgrade>,
    body: Body,
) -> Result<Response, ApiError> {
    if path == "/api/health" {
        if *method != Method::GET {
            return Err(ApiError::new(
                405,
                "METHOD_NOT_ALLOWED",
                UiMessage::new(MessageId::HealthGetOnly),
            ));
        }
        return Ok(json_response(StatusCode::OK, json!({ "status": "ok" })));
    }

    if path == "/api/settings" {
        if *method != Method::GET {
            return Err(ApiError::new(
                405,
                "METHOD_NOT_ALLOWED",
                UiMessage::new(MessageId::SettingsGetOnly),
            ));
        }
        // Read by the launcher before it starts a recording. One revision answers
        // the whole request, so the hotkey and the mode always describe the same
        // configuration; ordinary dictation uses exactly the selected mode. The
        // ring, its keys and count, the configuration paths and any unsaved draft
        // stay on this side of the loopback.
        let snapshot = state.runtime.snapshot();
        return Ok(json_response(
            StatusCode::OK,
            json!({
                "hotkey": snapshot.settings.hotkey.as_str(),
                "mode": snapshot.settings.mode.as_str(),
                "language": snapshot.settings.language,
                "revision": snapshot.revision,
            }),
        ));
    }

    if path == "/api/live" {
        if !upgrade_requested(headers) {
            return Err(ApiError::new(
                426,
                "UPGRADE_REQUIRED",
                UiMessage::new(MessageId::LiveUpgradeRequired),
            ));
        }
        if !origin_allowed(headers, &state.allowed_origins) {
            return Err(cross_origin_error());
        }

        // The upgrade request and the Origin are settled, so a session tag may now
        // pick the revision this socket is bound to.
        let request = select_request_snapshot(state, AudioRoute::Live, uri)?;
        // Recorded before the remaining answers: every error from here on is
        // redacted against the ring this socket would have used.
        *selected = Some(request.clone());

        if request.snapshot.keys.is_empty() {
            return Err(missing_key_error());
        }

        let Some(upgrade) = upgrade else {
            return Err(ApiError::new(
                400,
                "UPGRADE_FAILED",
                UiMessage::new(MessageId::LiveUpgradeFailed),
            ));
        };

        // One key per socket, taken only once the upgrade is actually there and
        // reused for the whole Live session. The pinned ring is re-read under the
        // runtime lock, so a socket whose dictation ended in the meantime is stale
        // instead of silently running on newer keys.
        let key = match request.session {
            Some(generation) => state
                .runtime
                .pinned_key(generation)
                .ok_or_else(stale_session_error)?
                .ok_or_else(missing_key_error)?,
            None => request
                .snapshot
                .keys
                .next_key()
                .ok_or_else(missing_key_error)?,
        };
        let provider = Arc::clone(&state.provider);
        let ring = Arc::clone(&request.snapshot.keys);
        return Ok(upgrade
            .max_message_size(MAX_WS_PAYLOAD)
            .on_upgrade(move |socket| async move {
                live::serve(socket, provider, key, ring).await;
            }));
    }

    if path == "/api/transcribe" {
        if *method != Method::POST {
            return Err(ApiError::new(
                405,
                "METHOD_NOT_ALLOWED",
                UiMessage::new(MessageId::TranscribePostOnly),
            ));
        }
        return handle_transcribe(state, selected, uri, headers, body).await;
    }

    if path.starts_with("/api/") {
        return Err(ApiError::new(
            404,
            "NOT_FOUND",
            UiMessage::new(MessageId::UnknownApiRoute).with_arg("path", json!(path)),
        ));
    }

    handle_static(method, uri.path(), path)
}

/* -------------------------------------------------------------------------- */
/* POST /api/transcribe                                                       */
/* -------------------------------------------------------------------------- */

async fn handle_transcribe(
    state: &Arc<AppState>,
    selected: &mut Option<RequestSnapshot>,
    uri: &Uri,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    if !origin_allowed(headers, &state.allowed_origins) {
        return Err(cross_origin_error());
    }

    // The Origin is settled, so a session tag may now pick the revision this
    // recording is served from. The choice is recorded before anything else can
    // fail, so every later error is redacted against the ring this request would
    // have used.
    let request = select_request_snapshot(state, AudioRoute::Batch, uri)?;
    *selected = Some(request.clone());

    if request.snapshot.keys.is_empty() {
        return Err(missing_key_error());
    }

    let mode = query_mode(uri.query()).unwrap_or_else(|| "smart".to_string());
    if mode != "smart" && mode != "verbatim" {
        return Err(ApiError::new(
            400,
            "INVALID_MODE",
            UiMessage::new(MessageId::InvalidBatchMode),
        ));
    }
    // A tagged recording belongs to the dictation that pinned it: asking for a
    // mode those settings do not have is a 409, never a licence to transcribe
    // with whichever settings are current.
    if request.session.is_some() && mode != request.snapshot.settings.mode {
        return Err(mode_mismatch_error());
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if content_type != "audio/wav" && content_type != "audio/wave" && content_type != "audio/x-wav"
    {
        return Err(ApiError::new(
            415,
            "UNSUPPORTED_TYPE",
            UiMessage::new(MessageId::TranscribeUnsupportedType),
        ));
    }

    let bytes = read_body_capped(body, headers).await?;
    let duration = parse_wav(&bytes)?;
    if duration > MAX_RECORDING_SECONDS as f64 + 0.5 {
        return Err(ApiError::new(
            413,
            "AUDIO_TOO_LONG",
            UiMessage::new(MessageId::AudioTooLong)
                .with_arg("seconds", json!((duration + 0.5).floor() as i64))
                .with_arg("minutes", json!(MAX_RECORDING_SECONDS / 60)),
        ));
    }

    // Admission before key selection, exactly like the rejected-request rule: a
    // busy server must not consume a turn of the rotation.
    let _permit = match Arc::clone(&state.batches).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return Err(ApiError::new(
                429,
                "SERVER_BUSY",
                UiMessage::new(MessageId::ServerBusy),
            ))
        }
    };

    // One key per recording: upload, transcription interaction and delete all run
    // on this same key. A tagged recording takes it from the ring its dictation
    // pinned - re-read under the runtime lock, so a take that ended during the
    // upload of its WAV is a 409 instead of a turn of the newer rotation. An
    // untagged recording takes it from the revision it was already reading.
    let key = match request.session {
        Some(generation) => state
            .runtime
            .pinned_key(generation)
            .ok_or_else(stale_session_error)?
            .ok_or_else(missing_key_error)?,
        None => request
            .snapshot
            .keys
            .next_key()
            .ok_or_else(missing_key_error)?,
    };
    let text = state.provider.transcribe(&key, request.keys(), &mode, bytes).await?;
    Ok(json_response(StatusCode::OK, json!({ "text": text })))
}

/// Reads the request body, refusing anything above the upload cap.
///
/// The declared length is checked first so an oversize recording is rejected
/// before a single byte is pulled off the socket; the running total catches a
/// chunked body that lied about (or omitted) its length. A body that fails while
/// being read is a server-side failure, not a size problem.
async fn read_body_capped(body: Body, headers: &HeaderMap) -> Result<Vec<u8>, ApiError> {
    fn too_large() -> ApiError {
        ApiError::new(
            413,
            "AUDIO_TOO_LARGE",
            UiMessage::new(MessageId::AudioTooLarge)
                .with_arg("minutes", json!(MAX_RECORDING_SECONDS / 60)),
        )
    }

    if let Some(declared) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(length) = declared.trim().parse::<f64>() {
            if length > MAX_UPLOAD_BYTES as f64 {
                return Err(too_large());
            }
        }
    }

    let mut chunks: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| ApiError::new(500, "INTERNAL", UiMessage::new(MessageId::InternalError)))?;
        if chunk.is_empty() {
            continue;
        }
        total += chunk.len() as u64;
        if total > MAX_UPLOAD_BYTES {
            return Err(too_large());
        }
        chunks.extend_from_slice(&chunk);
    }
    Ok(chunks)
}

/// Accepts only the format the browser promised to send: RIFF/WAVE, PCM16,
/// 16 kHz, mono. Returns the recording length in seconds.
fn parse_wav(bytes: &[u8]) -> Result<f64, ApiError> {
    if bytes.len() < 44 || read_ascii(bytes, 0, 4) != "RIFF" || read_ascii(bytes, 8, 4) != "WAVE" {
        return Err(ApiError::new(
            400,
            "INVALID_WAV",
            UiMessage::new(MessageId::WavNotRiff),
        ));
    }

    let mut offset = 12usize;
    let mut audio_format = 0u16;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut pcm_bytes: Option<usize> = None;

    while offset + 8 <= bytes.len() {
        let id = read_ascii(bytes, offset, 4);
        let size = read_u32_le(bytes, offset + 4) as usize;
        let data_start = offset + 8;
        let available = size.min(bytes.len().saturating_sub(data_start));

        if id == "fmt " && available >= 16 {
            audio_format = read_u16_le(bytes, data_start);
            channels = read_u16_le(bytes, data_start + 2);
            sample_rate = read_u32_le(bytes, data_start + 4);
            bits_per_sample = read_u16_le(bytes, data_start + 14);
        } else if id == "data" && pcm_bytes.is_none() {
            pcm_bytes = Some(available);
        }

        offset = data_start + available + (size % 2); // chunks are word aligned
    }

    if audio_format != 1 {
        return Err(ApiError::new(
            400,
            "INVALID_WAV",
            UiMessage::new(MessageId::WavNotPcm),
        ));
    }
    if channels != CHANNELS || sample_rate != SAMPLE_RATE || bits_per_sample != BITS_PER_SAMPLE {
        return Err(ApiError::new(
            400,
            "INVALID_WAV",
            UiMessage::new(MessageId::WavFormatMismatch)
                .with_arg("rate", json!(SAMPLE_RATE))
                .with_arg("channels", json!(CHANNELS))
                .with_arg("bits", json!(BITS_PER_SAMPLE))
                .with_arg("gotRate", json!(sample_rate))
                .with_arg("gotChannels", json!(channels))
                .with_arg("gotBits", json!(bits_per_sample)),
        ));
    }
    match pcm_bytes {
        None | Some(0) => Err(ApiError::new(
            400,
            "INVALID_WAV",
            UiMessage::new(MessageId::WavNoSamples),
        )),
        Some(pcm_bytes) => {
            Ok(pcm_bytes as f64 / (SAMPLE_RATE * (CHANNELS as u32) * BYTES_PER_SAMPLE) as f64)
        }
    }
}

fn read_ascii(bytes: &[u8], offset: usize, length: usize) -> String {
    (0..length)
        .map(|index| char::from(bytes.get(offset + index).copied().unwrap_or(0)))
        .collect()
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes.get(offset).copied().unwrap_or(0),
        bytes.get(offset + 1).copied().unwrap_or(0),
        bytes.get(offset + 2).copied().unwrap_or(0),
        bytes.get(offset + 3).copied().unwrap_or(0),
    ])
}

fn read_u16_le(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([
        bytes.get(offset).copied().unwrap_or(0),
        bytes.get(offset + 1).copied().unwrap_or(0),
    ])
}

/* -------------------------------------------------------------------------- */
/* Embedded static files                                                      */
/* -------------------------------------------------------------------------- */

/// One file the loopback server hands out. Everything is compiled into the EXE:
/// the distributed application never reads `public/` from disk, so a lone
/// `speechek.exe` in an empty directory serves every window unchanged.
struct StaticAsset {
    path: &'static str,
    body: &'static [u8],
    content_type: &'static str,
}

const ASSETS: [StaticAsset; 15] = [
    StaticAsset {
        path: "/index.html",
        body: include_bytes!("../../public/index.html"),
        content_type: MIME_HTML,
    },
    StaticAsset {
        path: "/app.js",
        body: include_bytes!("../../public/app.js"),
        content_type: MIME_JS,
    },
    StaticAsset {
        path: "/recorder.js",
        body: include_bytes!("../../public/recorder.js"),
        content_type: MIME_JS,
    },
    StaticAsset {
        path: "/pcm-worklet.js",
        body: include_bytes!("../../public/pcm-worklet.js"),
        content_type: MIME_JS,
    },
    StaticAsset {
        path: "/styles.css",
        body: include_bytes!("../../public/styles.css"),
        content_type: MIME_CSS,
    },
    StaticAsset {
        path: "/overlay.html",
        body: include_bytes!("../../public/overlay.html"),
        content_type: MIME_HTML,
    },
    StaticAsset {
        path: "/overlay.js",
        body: include_bytes!("../../public/overlay.js"),
        content_type: MIME_JS,
    },
    StaticAsset {
        path: "/overlay.css",
        body: include_bytes!("../../public/overlay.css"),
        content_type: MIME_CSS,
    },
    // The settings window: the second window the shell opens, served from the
    // same in-memory whitelist as the overlay and the lab.
    StaticAsset {
        path: "/settings.html",
        body: include_bytes!("../../public/settings.html"),
        content_type: MIME_HTML,
    },
    StaticAsset {
        path: "/settings.js",
        body: include_bytes!("../../public/settings.js"),
        content_type: MIME_JS,
    },
    StaticAsset {
        path: "/settings.css",
        body: include_bytes!("../../public/settings.css"),
        content_type: MIME_CSS,
    },
    // The interface message catalog and its WebView loader: the same dictionary
    // the native shell compiles in, served to every window over the loopback.
    StaticAsset {
        path: "/messages.json",
        body: include_bytes!("../../public/messages.json"),
        content_type: "application/json;charset=utf-8",
    },
    StaticAsset {
        path: "/i18n.js",
        body: include_bytes!("../../public/i18n.js"),
        content_type: MIME_JS,
    },
    // The paste transaction is adapted from Handy; its license text ships inside
    // the binary so the distributed EXE stands alone.
    StaticAsset {
        path: "/licenses/Handy.LICENSE",
        body: include_str!("../../third_party/Handy.LICENSE").as_bytes(),
        content_type: MIME_TEXT,
    },
    // The generated third-party notices for everything linked into this EXE.
    // scripts/collect-notices.ps1 derives it from the locked release graph;
    // embedding it keeps the single-file distribution self-contained.
    StaticAsset {
        path: "/licenses/THIRD-PARTY-NOTICES.txt",
        body: include_str!("../../third_party/THIRD-PARTY-NOTICES.txt").as_bytes(),
        content_type: MIME_TEXT,
    },
];

fn find_asset(path: &str) -> Option<&'static StaticAsset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

fn handle_static(method: &Method, raw_path: &str, normalized: &str) -> Result<Response, ApiError> {
    if *method != Method::GET && *method != Method::HEAD {
        return Err(ApiError::new(
            405,
            "METHOD_NOT_ALLOWED",
            UiMessage::new(MessageId::StaticGetHeadOnly),
        ));
    }

    // `decodeURIComponent` is strict: a malformed escape or invalid UTF-8 makes the
    // path unusable instead of silently producing a different file name.
    let Some(decoded) = percent_decode(raw_path, true) else {
        return Err(ApiError::new(400, "BAD_PATH", UiMessage::new(MessageId::MalformedRequestPath)));
    };
    if decoded.contains('\0') || decoded.contains('\\') {
        return Err(ApiError::new(400, "BAD_PATH", UiMessage::new(MessageId::InvalidRequestPath)));
    }

    let mut lookup = normalize_path(&decoded);
    if lookup.ends_with('/') {
        lookup.push_str("index.html");
    }

    let Some(asset) = find_asset(&lookup) else {
        // Every path outside the embedded whitelist is simply absent, so
        // nothing else can be served to the browser.
        return Err(ApiError::new(
            404,
            "NOT_FOUND",
            UiMessage::new(MessageId::NoSuchFile).with_arg("path", json!(normalized)),
        ));
    };

    let mut response = Response::new(if *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(asset.body)
    });
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(asset.body.len() as u64),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/* -------------------------------------------------------------------------- */
/* Request helpers                                                            */
/* -------------------------------------------------------------------------- */

fn json_response(status: StatusCode, body: Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn api_error_response(error: ApiError) -> Response {
    json_response(
        status_code(error.status),
        json!({ "error": { "code": error.code, "message": error.message, "ui": error.ui } }),
    )
}

fn status_code(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// The only origins allowed to drive the audio endpoints: the three spellings
/// of the loopback host, on the port this backend really serves. Derived from
/// the reservation's actual port rather than fixed, because a start whose
/// configured port was taken serves a temporary one instead.
fn loopback_origins(actual_port: u16) -> [String; 3] {
    [
        format!("http://127.0.0.1:{actual_port}"),
        format!("http://localhost:{actual_port}"),
        format!("http://[::1]:{actual_port}"),
    ]
}

/// Browser same-origin guard. Browsers always attach Origin to POST and WebSocket
/// handshakes, so a missing Origin means a non-browser client (curl, health
/// probe) and carries no CSRF risk. A present Origin must be one of the loopback
/// origins of the port this backend serves; the old fixed port, a neighbouring
/// port, a scheme the shell never serves and a lookalike host are all foreign.
fn origin_allowed(headers: &HeaderMap, allowed_origins: &[String]) -> bool {
    match headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        None | Some("") => true,
        Some(origin) => allowed_origins
            .iter()
            .any(|allowed| allowed.as_str() == origin),
    }
}

fn upgrade_requested(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// `mode` from the query string, first occurrence wins, mirroring
/// `URLSearchParams.get`, which never fails on a malformed escape.
fn query_mode(query: Option<&str>) -> Option<String> {
    for pair in query?.split('&') {
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None => (pair, ""),
        };
        if percent_decode(name, false).as_deref() == Some("mode") {
            return Some(percent_decode(value, false).unwrap_or_default());
        }
    }
    None
}

/// `session` from the query string: exactly one occurrence, holding the decimal
/// spelling of one positive integer a browser can hold exactly.
///
/// `None` means "no tag" and keeps the laboratory contract. Anything the grammar
/// rejects is an error rather than an absent tag: a request that named a session
/// must never be answered from whichever settings happen to be current. Malformed
/// escapes keep their literal text, exactly like `URLSearchParams`, and simply
/// fail the grammar.
fn query_session(query: Option<&str>) -> Result<Option<u64>, ApiError> {
    let Some(query) = query else {
        return Ok(None);
    };
    let mut found: Option<u64> = None;
    for pair in query.split('&') {
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None => (pair, ""),
        };
        if percent_decode(name, false).as_deref() != Some("session") {
            continue;
        }
        // A second `session` makes the tag ambiguous, and guessing which one was
        // meant is the one thing a tagged request must not do.
        if found.is_some() {
            return Err(invalid_session_error());
        }
        found = Some(parse_session_id(
            &percent_decode(value, false).unwrap_or_default(),
        )?);
    }
    Ok(found)
}

/// One `session` value: a positive integer a browser can hold exactly.
fn parse_session_id(text: &str) -> Result<u64, ApiError> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_session_error());
    }
    match text.parse::<u64>() {
        Ok(generation) if generation > 0 && generation <= MAX_SESSION_ID => Ok(generation),
        // Zero, a value past `Number.MAX_SAFE_INTEGER`, or more digits than a
        // `u64` holds: no browser could have produced this generation.
        _ => Err(invalid_session_error()),
    }
}

/// Percent-decodes one URI component.
///
/// `strict` mirrors `decodeURIComponent`, which refuses malformed escapes and
/// invalid UTF-8; the lenient form mirrors `URLSearchParams`, which keeps such
/// text verbatim. Both keep `+` as written: only form-encoded bodies turn it into
/// a space, and no route reads one.
fn percent_decode(text: &str, strict: bool) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let escape = bytes
                .get(index + 1..index + 3)
                .and_then(|pair| std::str::from_utf8(pair).ok())
                .and_then(|pair| u8::from_str_radix(pair, 16).ok());
            match escape {
                Some(byte) => {
                    decoded.push(byte);
                    index += 3;
                    continue;
                }
                None if strict => return None,
                None => {}
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }

    if strict {
        String::from_utf8(decoded).ok()
    } else {
        Some(String::from_utf8_lossy(&decoded).into_owned())
    }
}

/// Collapses `.` and `..` segments the way a URL parser does, so `/a/../index.html`
/// names the same file the browser would ask for after normalization.
fn normalize_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for (index, segment) in path.split('/').enumerate() {
        if index == 0 && absolute {
            continue; // the empty prefix before the root slash
        }
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }

    let mut normalized = String::with_capacity(path.len() + 1);
    if absolute {
        normalized.push('/');
    }
    normalized.push_str(&segments.join("/"));
    // A trailing dot segment resolves to a directory, which the URL form spells
    // with a trailing slash; `/` must not end up as the empty string.
    if (path.ends_with("/.") || path.ends_with("/..")) && !normalized.ends_with('/') {
        normalized.push('/');
    }
    if normalized.is_empty() {
        normalized.push('/');
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    fn test_snapshot(language: Language, revision: u64, keys: crate::secrets::KeyRing) -> Arc<RuntimeSnapshot> {
        Arc::new(RuntimeSnapshot {
            settings: crate::settings::Settings {
                hotkey: "F2".to_owned(),
                mode: "smart".to_owned(),
                mute_during_recording: false,
                port: 4175,
                input_device: None,
                language,
            },
            keys: Arc::new(keys),
            revision,
        })
    }

    #[test]
    fn nested_ui_arguments_cannot_leak_synthetic_keys() {
        let key = Arc::new(SecretKey::new("synthetic-nested-key".to_owned()));
        let nested = UiMessage::new(MessageId::UpstreamUnreachable)
            .with_arg("detail", json!("synthetic-nested-key / synthetic-nested-key"));
        let mut ui = UiMessage::new(MessageId::GoogleRequestFailed)
            .with_arg("detail", json!(nested))
            .with_arg("diagnostics", json!([
                {"values": ["prefix synthetic-nested-key suffix", {"raw": "synthetic-nested-key"}]},
                429, null, true
            ]));
        let unchanged = ui.clone();
        redact_ui(&mut ui, &[]);
        assert_eq!(ui, unchanged);
        redact_ui(&mut ui, std::slice::from_ref(&key));
        let wire = serde_json::to_value(&ui).expect("descriptor serialization");
        assert!(!wire.to_string().contains(key.as_str()));
        assert_eq!(wire["args"]["detail"]["key"], "UpstreamUnreachable");
        assert_eq!(wire["args"]["detail"]["args"]["detail"], "[redacted] / [redacted]");
        assert_eq!(wire["args"]["diagnostics"][1], 429);
        assert_eq!(wire["args"]["diagnostics"][2], Value::Null);
        assert_eq!(wire["args"]["diagnostics"][3], true);
    }

    #[test]
    fn response_redaction_retains_the_selected_request_ring_after_publish_and_retire() {
        let pinned_key = Arc::new(SecretKey::new("synthetic-pinned-key".to_owned()));
        let newer_key = Arc::new(SecretKey::new("synthetic-current-key".to_owned()));
        let initial = test_snapshot(Language::En, 1,
            crate::secrets::KeyRing::new(vec![Arc::clone(&pinned_key)]));
        let runtime = Arc::new(SharedRuntime::new(Arc::clone(&initial)));
        runtime.pin(77, initial);
        let state = AppState {
            runtime: Arc::clone(&runtime),
            provider: Arc::new(Provider::new()),
            batches: Arc::new(Semaphore::new(MAX_CONCURRENT_BATCH)),
            allowed_origins: loopback_origins(4175),
        };
        let selected = select_request_snapshot(&state, AudioRoute::Batch,
            &"/api/transcribe?mode=smart&session=77".parse().expect("request URI"))
            .expect("pinned request revision");
        runtime.publish(test_snapshot(Language::Ru, 2,
            crate::secrets::KeyRing::new(vec![Arc::clone(&newer_key)])));
        runtime.retire(77);
        assert!(runtime.resolve(Some(77)).is_none());
        let cause = UiMessage::new(MessageId::UpstreamUnreachable)
            .with_arg("detail", json!("synthetic-pinned-key / synthetic-current-key"));
        let error = ApiError::new(502, "UPSTREAM_ERROR",
            UiMessage::new(MessageId::GoogleRequestFailed).with_arg("detail", json!(cause)));
        let response = request_error_response(error, Some(&selected), &runtime);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = tokio::runtime::Runtime::new().expect("test runtime").block_on(
            axum::body::to_bytes(response.into_body(), 16 * 1024))
            .expect("error response body");
        let body: Value = serde_json::from_slice(&bytes).expect("error JSON");
        assert_eq!(body["error"]["code"], "UPSTREAM_ERROR");
        let diagnostic = body["error"]["message"].as_str().expect("diagnostic");
        let ui = &body["error"]["ui"];
        assert!(!diagnostic.contains(pinned_key.as_str()));
        assert!(!ui.to_string().contains(pinned_key.as_str()));
        // A current-ring-only sentinel is deliberately retained: consulting the
        // newer ring instead would redact it while leaking the pinned key.
        assert!(diagnostic.contains(newer_key.as_str()));
        assert!(ui.to_string().contains(newer_key.as_str()));
        assert_eq!(ui["args"]["detail"]["args"]["detail"],
            "[redacted] / synthetic-current-key");
    }

    /// A listener holding an ephemeral loopback port, and the port it holds:
    /// the "already in use" side of every reservation test.
    fn held_port() -> (TcpListener, u16) {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
        let port = held.local_addr().expect("the held address").port();
        (held, port)
    }

    /// Whether a connection to `port` on the loopback succeeds, which a bound
    /// listening socket answers even before the backend's accept loop runs.
    fn accepts(port: u16) -> bool {
        TcpStream::connect_timeout(
            &SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            Duration::from_millis(500),
        )
        .is_ok()
    }

    /// A free configured port is claimed as named: the reservation reports it as
    /// both configured and actual, is not a fallback, and the socket really
    /// listens - the backend never binds it a second time, so this socket is the
    /// one requests will reach.
    #[test]
    fn a_free_configured_port_is_reserved_as_it_was_named() {
        let (held, port) = held_port();
        drop(held);

        let reserved = reserve(port).expect("a free port is reserved");

        assert_eq!(reserved.configured_port(), port);
        assert_eq!(reserved.actual_port(), port, "the actual port is the named one");
        assert!(!reserved.fallback(), "a free port is not a fallback");
        assert!(accepts(port), "the reserved socket accepts connections");
    }

    /// A configured port somebody else is serving falls back to a temporary
    /// loopback port: the configured value stays what the document names, the
    /// actual port is a different, live port, and the fallback is reported.
    #[test]
    fn a_port_in_use_falls_back_to_a_temporary_port() {
        let (held, port) = held_port();

        let reserved = reserve(port).expect("a taken port falls back");

        assert_eq!(reserved.configured_port(), port, "the saved value is kept");
        assert!(reserved.fallback(), "the reservation had to fall back");
        assert_ne!(reserved.actual_port(), port, "a temporary port was claimed");
        assert!(accepts(reserved.actual_port()), "the temporary socket listens");
        drop(reserved);
        drop(held);
    }

    /// A configured port of 0 names no port at all: it is refused before any
    /// socket exists rather than turned into a random one, because an unusable
    /// stored value must never move the server somewhere nobody asked for.
    #[test]
    fn a_configured_port_of_zero_is_refused() {
        let error = match reserve(0) {
            Err(error) => error,
            Ok(_) => panic!("port zero names no loopback port"),
        };
        assert_eq!(error.code, "BACKEND_INVALID_PORT");
        assert_eq!(error.ui.key, MessageId::BackendPortZero);
    }

    /// A bind on a port somebody serves reads as `InUse` - the only failure that
    /// may fall back - even though Windows would otherwise let a second bind
    /// succeed. A free port binds at the requested number instead.
    #[test]
    fn a_bind_on_a_held_port_is_reported_as_in_use() {
        let (held, port) = held_port();
        match bind_exclusive(port) {
            Err(BindError::InUse) => {}
            Err(BindError::Fatal(message)) => {
                panic!("a held port has to read as in use, not as: {message}")
            }
            Ok(_) => panic!("a held exclusive port must not be bindable"),
        }
        drop(held);

        let (free, port) = held_port();
        drop(free);
        let listener = match bind_exclusive(port) {
            Ok(listener) => listener,
            Err(_) => panic!("a free port binds"),
        };
        assert_eq!(listener.local_addr().expect("the bound address").port(), port);
    }

    /// Both spellings of "address already in use" are recognised, so a fallback
    /// is never skipped because the Winsock code arrived raw; any other failure
    /// is not mistaken for a taken port.
    #[test]
    fn the_in_use_classifier_covers_both_the_kind_and_the_winsock_code() {
        assert!(is_address_in_use(&io::Error::from(io::ErrorKind::AddrInUse)));
        #[cfg(windows)]
        assert!(is_address_in_use(&io::Error::from_raw_os_error(WSAEADDRINUSE)));
        assert!(!is_address_in_use(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not a taken port",
        )));
    }

    /// Dropping a reservation returns its port: the exit path releases the
    /// socket, and the next start can claim the same configured port again
    /// instead of being pushed onto a fallback by a leak.
    #[test]
    fn dropping_a_reservation_releases_the_configured_port() {
        let (held, port) = held_port();
        drop(held);

        let reserved = reserve(port).expect("a free port is reserved");
        assert_eq!(reserved.actual_port(), port);
        drop(reserved);

        let again = reserve(port).expect("the port is claimable again");
        assert_eq!(again.actual_port(), port);
        assert!(!again.fallback(), "the port was released, not still held");
    }

    fn origin_header(origin: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
        headers
    }

    /// The audio endpoints answer loopback origins on the port the reservation
    /// really claimed and nothing else: the three loopback host spellings are
    /// accepted for that port, while the old fixed port, a neighbouring port, a
    /// scheme the shell never serves, a lookalike host and a path are foreign.
    #[test]
    fn origins_are_allowed_only_on_the_actual_port() {
        let allowed = loopback_origins(43117);

        for origin in [
            "http://127.0.0.1:43117",
            "http://localhost:43117",
            "http://[::1]:43117",
        ] {
            assert!(
                origin_allowed(&origin_header(origin), &allowed),
                "{origin} is a loopback origin of the served port"
            );
        }

        for origin in [
            "http://127.0.0.1:4173",
            "http://localhost:4173",
            "http://[::1]:4173",
            "http://127.0.0.1:43118",
            "https://127.0.0.1:43117",
            "http://localhost.evil.test:43117",
            "http://127.0.0.1:43117/",
        ] {
            assert!(
                !origin_allowed(&origin_header(origin), &allowed),
                "{origin} is not an origin this backend serves"
            );
        }

        assert!(
            origin_allowed(&HeaderMap::new(), &allowed),
            "a client that sends no Origin (curl, probe) is not cross-origin"
        );
    }
}
