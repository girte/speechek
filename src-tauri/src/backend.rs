//! Speechek's in-process loopback backend.
//!
//! One Axum service owns the loopback socket this process reserved: the
//! configured port, or a temporary one when it was already taken. It hands the
//! eleven `public/` files to the overlay, lab and settings windows from memory
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

/// An HTTP-visible failure. `code` is a stable machine-readable string the
/// browser switches on; `message` is already safe to show a user.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
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
    for key in keys {
        let key = key.as_str();
        if key.is_empty() || !safe.contains(key) {
            continue;
        }
        safe = safe.replace(key, "[redacted]");
    }
    safe
}

fn missing_key_error() -> ApiError {
    ApiError::new(
        503,
        "MISSING_API_KEY",
        "No Gemini API key is available. Add at least one key in the Speechek settings window under \"API keys\".",
    )
}

fn cross_origin_error() -> ApiError {
    ApiError::new(
        403,
        "CROSS_ORIGIN",
        "This endpoint only accepts same-origin requests.",
    )
}

/// A `session` tag that is not the exact decimal spelling of one positive
/// integer a browser can hold. An unreadable tag is refused rather than treated
/// as an absent one: silently untagging a request would let it run on settings
/// it does not belong to.
fn invalid_session_error() -> ApiError {
    ApiError::new(
        400,
        "INVALID_SESSION",
        "session must be the numeric generation of a running dictation.",
    )
}

/// The dictation's revision is gone: it was retired when the take ended. A late
/// request of that take must not run on newer settings instead.
fn stale_session_error() -> ApiError {
    ApiError::new(
        409,
        "STALE_SESSION",
        "This dictation is no longer active; start a new recording.",
    )
}

/// The request is not the dictation it claims to be part of: a batch request
/// asks for a mode the pinned settings do not have, or the Live socket asks to
/// be served by a dictation that is not Live.
fn mode_mismatch_error() -> ApiError {
    ApiError::new(
        409,
        "MODE_MISMATCH",
        "The requested mode does not match the dictation this session belongs to.",
    )
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
    ) -> Result<Self, String> {
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
            .map_err(|error| format!("Could not start the speechek backend runtime: {error}"))?;

        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let thread = std::thread::Builder::new()
            .name("speechek-backend".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(listener) => listener,
                        Err(error) => {
                            let _ = ready_tx.send(Err(format!(
                                "Could not serve http://127.0.0.1:{actual_port}: {error}"
                            )));
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
            .map_err(|error| format!("Could not start the speechek backend thread: {error}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shutdown: Some(shutdown_tx),
                thread: Some(thread),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("The speechek backend stopped before it was ready.".to_string()),
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
pub fn reserve(configured_port: u16) -> Result<ReservedListener, String> {
    if configured_port == 0 {
        return Err("Could not start speechek: a port of 0 names no loopback port.".to_string());
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
fn reserve_fallback(configured_port: u16) -> Result<ReservedListener, String> {
    match bind_exclusive(0) {
        Ok(listener) => finish_reservation(listener, configured_port, true),
        // Port zero is never in use; a second `InUse` is the system saying it
        // has no temporary port to give, which is as fatal as a taken one.
        Err(BindError::InUse) => Err(format!(
            "Could not start speechek: {configured_port} is already in use by another program and no temporary loopback port could be claimed; stop that program and try again."
        )),
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
) -> Result<ReservedListener, String> {
    let actual_port = listener
        .local_addr()
        .map_err(|error| format!("Could not read the backend port: {error}"))?
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
    Fatal(String),
}

/// Bind and listen on the loopback port, refusing to share it.
fn bind_exclusive(port: u16) -> Result<TcpListener, BindError> {
    let address = SocketAddrV4::new(HOST, port);
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).map_err(|error| {
        BindError::Fatal(format!("Could not create the backend socket: {error}"))
    })?;

    // `SO_REUSEADDR` is deliberately never enabled: on Windows it is exactly the
    // option that lets a later process steal a bound port, and on every other
    // platform the default already refuses a second bind.
    #[cfg(windows)]
    set_exclusive_address_use(&socket).map_err(BindError::Fatal)?;
    #[cfg(not(windows))]
    socket.set_reuse_address(false).map_err(|error| {
        BindError::Fatal(format!("Could not configure the backend socket: {error}"))
    })?;

    socket.bind(&address.into()).map_err(|error| {
        if is_address_in_use(&error) {
            BindError::InUse
        } else {
            BindError::Fatal(format!("Could not bind 127.0.0.1:{port}: {error}"))
        }
    })?;
    socket.listen(128).map_err(|error| {
        BindError::Fatal(format!("Could not listen on 127.0.0.1:{port}: {error}"))
    })?;
    socket.set_nonblocking(true).map_err(|error| {
        BindError::Fatal(format!("Could not prepare 127.0.0.1:{port}: {error}"))
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
fn set_exclusive_address_use(socket: &Socket) -> Result<(), String> {
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
        return Err(format!(
            "Could not make the backend port exclusive: {}",
            io::Error::last_os_error()
        ));
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
            // Method, path, status and code only: never a key, never a body.
            eprintln!("{method} {} -> {} {}", uri.path(), error.status, error.code);
            let current;
            let keys = match &selected {
                Some(request) => request.keys(),
                None => {
                    current = state.runtime.snapshot();
                    current.keys.keys()
                }
            };
            api_error_response(
                status_code(error.status),
                error.code,
                redact(&error.message, keys),
            )
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
                "/api/health accepts GET only.",
            ));
        }
        return Ok(json_response(StatusCode::OK, json!({ "status": "ok" })));
    }

    if path == "/api/settings" {
        if *method != Method::GET {
            return Err(ApiError::new(
                405,
                "METHOD_NOT_ALLOWED",
                "/api/settings accepts GET only.",
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
            }),
        ));
    }

    if path == "/api/live" {
        if !upgrade_requested(headers) {
            return Err(ApiError::new(
                426,
                "UPGRADE_REQUIRED",
                "/api/live requires a WebSocket upgrade.",
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
                "Could not upgrade the connection to WebSocket.",
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
                "/api/transcribe accepts POST only.",
            ));
        }
        return handle_transcribe(state, selected, uri, headers, body).await;
    }

    if path.starts_with("/api/") {
        return Err(ApiError::new(
            404,
            "NOT_FOUND",
            format!("Unknown API route: {path}"),
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
            "mode must be 'smart' or 'verbatim'.",
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
            "Send the recording as Content-Type: audio/wav.",
        ));
    }

    let bytes = read_body_capped(body, headers).await?;
    let duration = parse_wav(&bytes)?;
    if duration > MAX_RECORDING_SECONDS as f64 + 0.5 {
        return Err(ApiError::new(
            413,
            "AUDIO_TOO_LONG",
            format!(
                "Recording is {}s; the limit is {} minutes.",
                (duration + 0.5).floor() as i64,
                MAX_RECORDING_SECONDS / 60
            ),
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
                "Too many transcriptions in flight; try again in a moment.",
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
    let text = state.provider.transcribe(&key, &mode, bytes).await?;
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
            format!(
                "Recording is larger than the {} minute limit.",
                MAX_RECORDING_SECONDS / 60
            ),
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
            chunk.map_err(|_| ApiError::new(500, "INTERNAL", "Unexpected server error."))?;
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
            "Body must be a RIFF/WAVE file (audio/wav).",
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
            "WAV must contain uncompressed PCM audio (format 1).",
        ));
    }
    if channels != CHANNELS || sample_rate != SAMPLE_RATE || bits_per_sample != BITS_PER_SAMPLE {
        return Err(ApiError::new(
            400,
            "INVALID_WAV",
            format!(
                "WAV must be {SAMPLE_RATE} Hz, {CHANNELS} channel, {BITS_PER_SAMPLE}-bit PCM (got {sample_rate} Hz, {channels} ch, {bits_per_sample}-bit)."
            ),
        ));
    }
    match pcm_bytes {
        None | Some(0) => Err(ApiError::new(
            400,
            "INVALID_WAV",
            "WAV contains no audio samples.",
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

const ASSETS: [StaticAsset; 13] = [
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
            "Static files are served with GET or HEAD only.",
        ));
    }

    // `decodeURIComponent` is strict: a malformed escape or invalid UTF-8 makes the
    // path unusable instead of silently producing a different file name.
    let Some(decoded) = percent_decode(raw_path, true) else {
        return Err(ApiError::new(400, "BAD_PATH", "Malformed request path."));
    };
    if decoded.contains('\0') || decoded.contains('\\') {
        return Err(ApiError::new(400, "BAD_PATH", "Invalid request path."));
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
            format!("No such file: {normalized}"),
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

fn api_error_response(status: StatusCode, code: &'static str, message: String) -> Response {
    json_response(
        status,
        json!({ "error": { "code": code, "message": message } }),
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
        assert!(error.contains('0'), "the refusal names the port: {error}");
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
