//! The loopback carrier's externally visible boundaries.
//!
//! Everything here goes through the real router the shell builds - the same
//! handler, limits, origin list and admission transaction - over a real
//! loopback socket and with the raw HTTP bytes this test writes by hand, so an
//! assertion is about the wire and never about a private helper. The sink is
//! the producer-only test sink: it registers takes and queues reports, and it
//! keeps no writer, so no file exists on any of these paths.
//!
//! What is deliberately not repeated here: the feature-off route, which the
//! backend's own disabled-sink test covers, and the producer's sealing and
//! queue-overflow arithmetic, which the producer fixtures own.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::http::StatusCode;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::super::*;
use crate::backend::diagnostic_test_router;
use crate::i18n::Language;
use crate::provider::Provider;
use crate::secrets::{KeyRing, SecretKey};
use crate::settings::{RuntimeSnapshot, Settings, SharedRuntime};

/* -------------------------------------------------------------------------- */
/* Fixtures                                                                   */
/* -------------------------------------------------------------------------- */

/// A directory of this test's own, removed when the test ends. The producer
/// sink never touches it; it only has to be a path nothing else uses.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "speechek-carrier-{}-{tag}-{}",
            std::process::id(),
            unique()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a scratch root");
        Self { path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::process::id() as u64 * 1_000_000 + COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// One configuration revision with a synthetic key: the router only reads it
/// for the audio routes, and nothing here ever calls one.
fn snapshot() -> Arc<RuntimeSnapshot> {
    Arc::new(RuntimeSnapshot {
        settings: Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            port: 4175,
            input_device: None,
            language: Language::En,
        },
        keys: Arc::new(KeyRing::new(vec![Arc::new(SecretKey::new(
            "synthetic-carrier-key".to_owned(),
        ))])),
        revision: 1,
    })
}

/// The real router on an ephemeral loopback port, plus the sink behind it.
struct App {
    port: u16,
    diagnostics: Arc<Diagnostics>,
    serving: tokio::task::JoinHandle<()>,
}

impl Drop for App {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

async fn app(scratch: &Scratch) -> App {
    let diagnostics = Diagnostics::for_test(&scratch.path, Policy::DEVELOPMENT, now());
    let runtime = Arc::new(SharedRuntime::new(snapshot()));
    // Explicit loopback bases: no environment variable is read and no real
    // endpoint can be reached even by accident.
    let provider = Arc::new(Provider::for_test(
        "http://127.0.0.1:1",
        "ws://127.0.0.1:1",
    ));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("a loopback port");
    let port = listener
        .local_addr()
        .expect("the bound address")
        .port();
    let router = diagnostic_test_router(runtime, provider, Arc::clone(&diagnostics), port);
    let serving = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    App {
        port,
        diagnostics,
        serving,
    }
}

impl App {
    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// One hand-written HTTP request.
    ///
    /// The write half stays open while the answer is read: a hyper server
    /// treats a client that closes its write half mid-request as a cancelled
    /// connection and then never answers at all, which is exactly the shape a
    /// premature shutdown produces. `Connection: close` is what ends the read
    /// instead - the server closes once its response is out, so the answer is
    /// read to the connection's own EOF rather than to a length this test
    /// would have to trust.
    async fn request(&self, head: &str, body: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port))
            .await
            .expect("the loopback connection");
        let mut bytes = head.as_bytes().to_vec();
        bytes.extend_from_slice(body);
        stream.write_all(&bytes).await.expect("the request");
        stream.flush().await.expect("the flush");
        let mut response = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
            .await;
        let text = String::from_utf8_lossy(&response).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        assert!(
            status != 0,
            "the server answered nothing at all for {head:?}"
        );
        (status, text)
    }

    /// The report body with the origin this router allows, as the wrapper
    /// sends it.
    async fn post(&self, body: &Value) -> (u16, String) {
        let bytes = serde_json::to_vec(body).expect("the report encoding");
        let head = format!(
            "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.port,
            self.origin(),
            bytes.len(),
        );
        self.request(&head, &bytes).await
    }
}

/// One report of `owner`/`generation`, carrying the numbers the wrapper would
/// actually post: every scalar present, both clocks explicit `null`, no event
/// list and no terminal. A case that needs a terminal sets it itself, so a
/// report never carries one by accident.
fn report(owner: &str, generation: u64, index: u64) -> Value {
    json!({
        "owner": owner,
        "generation": generation,
        "reportIndex": index,
        "clientElapsedMs": 1_000.0,
        "state": "recording",
        "handedFrames": 7_501,
        "handedSamples": 3_840_037,
        "ignoredFrames": 0,
        "sentFrames": 7_501,
        "sentPcmBytes": 7_680_074,
        "unsentFrames": 0,
        "bufferedBytes": 0,
        "readyCount": 1,
        "interimCount": 2,
        "finalCount": 1,
        "errorCount": 0,
        "doneCount": 1,
        "eventsDropped": 0,
        "lostReports": 0,
        "lastSendMs": null,
        "lastReceiveMs": null,
        "terminalOutcome": null,
        "events": [],
    })
}

fn accepted(text: &str) -> bool {
    text.contains("\"accepted\":true")
}

fn refused(text: &str, code: &str) -> bool {
    text.contains("\"accepted\":false") && text.contains(&format!("\"code\":\"{code}\""))
}

/// The same head with `Content-Type` replaced, for the type table.
fn head_with_type(app: &App, kind: &str, length: usize) -> String {
    format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nContent-Type: {kind}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
        app.port,
        app.origin(),
    )
}

fn chunked_head(app: &App) -> String {
    format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        app.port,
        app.origin(),
    )
}

/// One chunked body: the pieces as their own chunks, terminated properly.
fn chunked(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for piece in chunks {
        body.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
        body.extend_from_slice(piece);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"0\r\n\r\n");
    body
}

fn begin(diagnostics: &Diagnostics, owner: Owner, generation: u64) -> SessionHandle {
    diagnostics
        .begin(
            SessionKey { owner, generation },
            if owner == Owner::Lab { Mode::Comparison } else { Mode::Live },
        )
        .expect("a registered take")
}

/* -------------------------------------------------------------------------- */
/* Origin                                                                     */
/* -------------------------------------------------------------------------- */

#[tokio::test]
async fn origin_is_required_and_must_be_the_loopbacks_own() {
    let scratch = Scratch::new("origin");
    let app = app(&scratch).await;
    let body = serde_json::to_vec(&report("lab", 1, 1)).expect("the report encoding");
    let length = body.len();

    // No Origin at all: a browser always sends one, so its absence here is the
    // one shape this endpoint refuses instead of serving.
    let head = format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
        app.port,
    );
    let (status, text) = app.request(&head, &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN.as_u16());
    assert!(refused(&text, "cross_origin"), "{text}");

    // An empty Origin names no page either.
    let head = format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: \r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
        app.port,
    );
    let (status, text) = app.request(&head, &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN.as_u16());
    assert!(refused(&text, "cross_origin"), "{text}");

    // A foreign origin, even one that looks like a neighbouring port.
    let head = format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:9\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
        app.port,
    );
    let (status, text) = app.request(&head, &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN.as_u16());
    assert!(refused(&text, "cross_origin"), "{text}");

    // Two Origin headers: the browser cannot send that, so it is refused too.
    let head = format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
        app.port,
        app.origin(),
        app.origin(),
    );
    let (status, text) = app.request(&head, &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN.as_u16());
    assert!(refused(&text, "cross_origin"), "{text}");
}

/* -------------------------------------------------------------------------- */
/* Method and content type                                                    */
/* -------------------------------------------------------------------------- */

#[tokio::test]
async fn only_post_is_answered_and_the_refusal_names_it() {
    let scratch = Scratch::new("method");
    let app = app(&scratch).await;
    for method in ["GET", "PUT", "DELETE", "PATCH"] {
        let head = format!(
            "{} /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nConnection: close\r\n\r\n",
            method,
            app.port,
            app.origin(),
        );
        let (status, text) = app.request(&head, &[]).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED.as_u16(), "{method}");
        assert!(refused(&text, "method_not_allowed"), "{method}: {text}");
        // The `Allow` header is part of the answer, not decoration: it is what
        // tells a caller the one method this endpoint has.
        let head_part = text.split("\r\n\r\n").next().unwrap_or_default().to_lowercase();
        assert!(head_part.contains("\r\nallow: post"), "{method}: {text}");
    }

    // HEAD reaches the same branch; its answer may carry no body at all, so
    // only the status and the advertised method are asserted.
    let head = format!(
        "HEAD /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nConnection: close\r\n\r\n",
        app.port,
        app.origin(),
    );
    let (status, text) = app.request(&head, &[]).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED.as_u16());
    let head_part = text.split("\r\n\r\n").next().unwrap_or_default().to_lowercase();
    assert!(head_part.contains("\r\nallow: post"), "{text}");
}

#[tokio::test]
async fn the_body_must_announce_itself_as_json() {
    let scratch = Scratch::new("content-type");
    let app = app(&scratch).await;

    // The take the positive case below reports on: the endpoint only ever
    // looks up an already registered key and never creates one.
    let _take = begin(&app.diagnostics, Owner::Lab, 2);
    let body = serde_json::to_vec(&report("lab", 2, 1)).expect("the report encoding");
    let length = body.len();

    for kind in ["text/plain", "application/xml", "audio/wav", ""] {
        let (status, text) = app.request(&head_with_type(&app, kind, length), &body).await;
        assert_eq!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE.as_u16(),
            "{kind}: {text}"
        );
        assert!(refused(&text, "unsupported_type"), "{kind}: {text}");
    }

    // The parameters a browser adds are not part of the type.
    let (status, text) = app
        .request(
            &head_with_type(&app, "application/json; charset=utf-8", length),
            &body,
        )
        .await;
    assert!(accepted(&text), "{status}: {text}");
}

/* -------------------------------------------------------------------------- */
/* Body size                                                                  */
/* -------------------------------------------------------------------------- */

#[tokio::test]
async fn a_declared_body_over_the_cap_is_refused_before_it_is_read() {
    let scratch = Scratch::new("declared-size");
    let app = app(&scratch).await;
    // The declared length alone refuses it: no body byte is ever sent, so a
    // server that had to read the body first could not answer at all.
    let head = format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: 4097\r\nConnection: close\r\n\r\n",
        app.port,
        app.origin(),
    );
    let (status, text) = app.request(&head, &[]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE.as_u16());
    assert!(refused(&text, "report_too_large"), "{text}");

    // Exactly the cap is admitted to parsing and refused for what it is, which
    // is what makes the limit a `>` and not a `>=`.
    let body = vec![b' '; 4_096];
    let head = format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n",
        app.port,
        app.origin(),
    );
    let (status, text) = app.request(&head, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{text}");
    assert!(refused(&text, "invalid_report"), "{text}");
}

#[tokio::test]
async fn a_chunked_body_is_counted_as_it_arrives() {
    let scratch = Scratch::new("chunked-size");
    let app = app(&scratch).await;

    // The take this batch belongs to: one registered key for the whole test,
    // which the rejected oversize body never reaches.
    let _take = begin(&app.diagnostics, Owner::Lab, 3);

    // A chunked body past the cap: there is no length to trust, so the running
    // total is what refuses it.
    let oversize = chunked(&[vec![b' '; 1_500], vec![b' '; 1_500], vec![b' '; 1_500]]);
    let (status, text) = app.request(&chunked_head(&app), &oversize).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE.as_u16(), "{text}");
    assert!(refused(&text, "report_too_large"), "{text}");

    // The same shape under the cap is served like any other report.
    let body = serde_json::to_vec(&report("lab", 3, 1)).expect("the report encoding");
    let middle = body.len() / 2;
    let pieces = vec![body[..middle].to_vec(), body[middle..].to_vec()];
    let (status, text) = app.request(&chunked_head(&app), &chunked(&pieces)).await;
    assert!(accepted(&text), "{status}: {text}");
}

/* -------------------------------------------------------------------------- */
/* Shape                                                                      */
/* -------------------------------------------------------------------------- */

#[tokio::test]
async fn unknown_fields_are_refused_rather_than_ignored() {
    let scratch = Scratch::new("unknown");
    let app = app(&scratch).await;
    let mut body = report("lab", 4, 1);
    body["transcript"] = json!("synthetic-transcript");
    body["args"] = json!({ "device": "synthetic-device" });
    let (status, text) = app.post(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16());
    assert!(refused(&text, "invalid_report"), "{text}");
    // The refusal never reflects what it refused.
    assert!(!text.contains("synthetic-transcript"), "{text}");
    assert!(!text.contains("synthetic-device"), "{text}");

    // An unknown field inside an event is refused the same way.
    let mut body = report("lab", 4, 2);
    body["events"] = json!([{ "event": "socket_close", "atMs": 1.0, "message": "synthetic" }]);
    let (status, text) = app.post(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16());
    assert!(refused(&text, "invalid_report"), "{text}");
}

#[tokio::test]
async fn numbers_outside_the_wrappers_own_range_are_refused() {
    let scratch = Scratch::new("numbers");
    let app = app(&scratch).await;

    // A generation no JavaScript number can hold exactly.
    let mut body = report("lab", 5, 1);
    body["generation"] = json!(9_007_199_254_740_992_u64);
    let (status, text) = app.post(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{text}");
    assert!(refused(&text, "invalid_report"), "{text}");

    // Index zero: the wrapper's indices are 1-based.
    let mut body = report("lab", 5, 1);
    body["reportIndex"] = json!(0);
    let (status, text) = app.post(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{text}");
    assert!(refused(&text, "invalid_report"), "{text}");

    // A counter past the safe range.
    let mut body = report("lab", 5, 1);
    body["handedFrames"] = json!(9_007_199_254_740_992_u64);
    let (status, text) = app.post(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{text}");
    assert!(refused(&text, "invalid_report"), "{text}");

    // Counts are non-negative.
    let mut body = report("lab", 5, 1);
    body["bufferedBytes"] = json!(-1);
    let (status, text) = app.post(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{text}");
    assert!(refused(&text, "invalid_report"), "{text}");

    // Clocks are finite and non-negative.
    let mut body = report("lab", 5, 1);
    body["clientElapsedMs"] = json!(-0.5);
    let (status, text) = app.post(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{text}");
    assert!(refused(&text, "invalid_report"), "{text}");

    // A clock that cannot be a clock is refused the same way, whichever side
    // refuses it: a number the parser will not carry, or one the typed check
    // will not accept.
    let mut bytes = serde_json::to_vec(&report("lab", 5, 1)).expect("the report encoding");
    let needle: &[u8] = b"\"lastSendMs\":null";
    let at = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("the nullable send clock");
    let _ = bytes.splice(at..at + needle.len(), b"\"lastSendMs\":1e999".iter().copied());
    let head = format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        app.port,
        app.origin(),
        bytes.len(),
    );
    let (status, text) = app.request(&head, &bytes).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{text}");
    assert!(refused(&text, "invalid_report"), "{text}");

    // A number sent as a string is not a number.
    let mut body = report("lab", 5, 1);
    body["clientElapsedMs"] = json!("1000");
    let (status, text) = app.post(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{text}");
    assert!(refused(&text, "invalid_report"), "{text}");

    // Too many events for one report.
    let mut body = report("lab", 5, 1);
    body["events"] = Value::Array(
        (0..(RENDERER_EVENTS_MAX + 1))
            .map(|_| json!({ "event": "recorder_start", "atMs": 1.0 }))
            .collect(),
    );
    let (status, text) = app.post(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{text}");
    assert!(refused(&text, "invalid_report"), "{text}");
}

#[tokio::test]
async fn the_nullable_clocks_and_the_event_list_are_still_required() {
    let scratch = Scratch::new("required");
    let app = app(&scratch).await;

    // The take the accepted indices below name: a report rejected for a
    // missing field never reaches admission, so the take only has to exist
    // for the positive cases.
    let _take = begin(&app.diagnostics, Owner::Lab, 6);

    for missing in ["lastSendMs", "lastReceiveMs", "events"] {
        let mut body = report("lab", 6, 1);
        body.as_object_mut()
            .expect("a report object")
            .remove(missing);
        let (status, text) = app.post(&body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{missing}: {text}");
        assert!(refused(&text, "invalid_report"), "{missing}: {text}");
    }

    // The terminal is the one field a report may omit; null and a present
    // value are both reports.
    let mut body = report("lab", 6, 2);
    body.as_object_mut()
        .expect("a report object")
        .remove("terminalOutcome");
    let (status, text) = app.post(&body).await;
    assert!(accepted(&text), "{status}: {text}");

    let mut body = report("lab", 6, 3);
    body["terminalOutcome"] = Value::Null;
    let (status, text) = app.post(&body).await;
    assert!(accepted(&text), "{status}: {text}");

    let mut body = report("lab", 6, 4);
    body["terminalOutcome"] = json!("success");
    let (status, text) = app.post(&body).await;
    assert!(accepted(&text), "{status}: {text}");
}

#[tokio::test]
async fn a_body_that_is_not_utf8_is_refused() {
    let scratch = Scratch::new("utf8");
    let app = app(&scratch).await;
    let body = [0xff_u8, 0xfe, 0xfd, 0x00];
    let head = format!(
        "POST /api/dev-diagnostics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        app.port,
        app.origin(),
        body.len(),
    );
    let (status, text) = app.request(&head, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST.as_u16());
    assert!(refused(&text, "invalid_report"), "{text}");
}

/* -------------------------------------------------------------------------- */
/* Admission                                                                  */
/* -------------------------------------------------------------------------- */

#[tokio::test]
async fn a_full_queue_keeps_the_index_and_the_terminal_for_the_retry() {
    let scratch = Scratch::new("queue-full");
    let app = app(&scratch).await;
    let key = SessionKey {
        owner: Owner::Lab,
        generation: 7,
    };
    let handle = begin(&app.diagnostics, Owner::Lab, 7);

    // Fill the shared metadata queue: every accepted report holds one record
    // until the writer takes it.
    let mut index = 0_u64;
    let mut full = None;
    while index < 400 {
        index += 1;
        let (status, text) = app.post(&report("lab", 7, index)).await;
        if status == StatusCode::TOO_MANY_REQUESTS.as_u16() {
            assert!(refused(&text, "queue_full"), "{text}");
            full = Some(index);
            break;
        }
        assert!(accepted(&text), "index {index}: {status}: {text}");
    }
    let refused_index = full.expect("a full metadata queue");

    // The refused report changed nothing: neither the committed index nor the
    // terminal the retry will apply.
    {
        let admission = handle.entry.admission.lock();
        assert_eq!(admission.accepted_report_index, refused_index - 1);
        assert_eq!(admission.terminal, None);
    }

    // The writer takes one record, and the very same report - the one whose
    // terminal was refused - is accepted on the retry.
    let taken = app.diagnostics.take_meta(1);
    assert_eq!(taken.len(), 1);
    let mut retried = report("lab", 7, refused_index);
    retried["terminalOutcome"] = json!("success");
    let (status, text) = app.post(&retried).await;
    assert!(accepted(&text), "{status}: {text}");
    {
        let admission = handle.entry.admission.lock();
        assert_eq!(admission.accepted_report_index, refused_index);
        assert_eq!(admission.terminal, Some(Outcome::Success));
    }

    // The key is the one the report named, so a report for another take of the
    // same sink is a different session.
    assert_eq!(handle.key(), key);
}

#[tokio::test]
async fn a_report_at_or_below_the_committed_index_is_only_acknowledged() {
    let scratch = Scratch::new("stale-report");
    let app = app(&scratch).await;
    let handle = begin(&app.diagnostics, Owner::Lab, 8);

    let (status, text) = app.post(&report("lab", 8, 2)).await;
    assert!(accepted(&text), "{status}: {text}");

    // A duplicate of the committed index, and anything below it: acknowledged,
    // never written twice and never applied again.
    for index in [2, 1] {
        let (status, text) = app.post(&report("lab", 8, index)).await;
        assert_eq!(status, StatusCode::OK.as_u16(), "index {index}: {text}");
        assert!(refused(&text, "stale_report"), "index {index}: {text}");
    }

    // The next index is still free.
    let (status, text) = app.post(&report("lab", 8, 3)).await;
    assert!(accepted(&text), "{status}: {text}");
    assert_eq!(handle.entry.admission.lock().accepted_report_index, 3);
}

#[tokio::test]
async fn a_session_that_is_not_open_is_gone() {
    let scratch = Scratch::new("stale-session");
    let app = app(&scratch).await;

    // A generation this sink never registered.
    let (status, text) = app.post(&report("lab", 99, 1)).await;
    assert_eq!(status, StatusCode::GONE.as_u16());
    assert!(refused(&text, "stale_session"), "{text}");

    // An owner that has no such take either.
    let (status, text) = app.post(&report("dictation", 99, 1)).await;
    assert_eq!(status, StatusCode::GONE.as_u16());
    assert!(refused(&text, "stale_session"), "{text}");

    // A take that already ended: its reports are history.
    let sealed = SessionKey {
        owner: Owner::Dictation,
        generation: 10,
    };
    let handle = begin(&app.diagnostics, Owner::Dictation, 10);
    handle.end_capture(Outcome::Success);
    app.diagnostics.end(sealed, Outcome::Success);
    app.diagnostics.advance_clock(Duration::from_secs(2));
    app.diagnostics.advance_all();
    assert!(handle.entry.is_sealed(), "the take was sealed");

    let (status, text) = app.post(&report("dictation", 10, 1)).await;
    assert_eq!(status, StatusCode::GONE.as_u16());
    assert!(refused(&text, "stale_session"), "{text}");
}

#[tokio::test]
async fn an_unusable_sink_answers_that_it_is_unusable() {
    let scratch = Scratch::new("unavailable");
    let app = app(&scratch).await;

    // The enable gate itself is the backend's own test; what this pins is that
    // a sink that exists but archives nothing at all refuses a report outright
    // instead of serving it as if it had been recorded.
    let disabled = Diagnostics::disabled();
    assert!(!disabled.is_enabled());
    let admission = disabled.admit_renderer_report(
        serde_json::from_value::<RendererReport>(report("lab", 11, 1))
            .expect("an admissible report"),
    );
    assert_eq!(admission, ReportAdmission::Unavailable);

    // A sink that has stopped is a different answer on the wire: it admits no
    // new take, and a report of a take it was holding is stale rather than
    // queued behind a writer that never comes back.
    let handle = begin(&app.diagnostics, Owner::Lab, 11);
    app.diagnostics.shutdown(Duration::from_secs(1));
    assert!(
        app.diagnostics
            .begin(
                SessionKey {
                    owner: Owner::Lab,
                    generation: 12,
                },
                Mode::Comparison,
            )
            .is_none(),
        "a closed sink opens no take"
    );
    let (status, text) = app.post(&report("lab", 11, 1)).await;
    assert_eq!(status, StatusCode::GONE.as_u16(), "{text}");
    assert!(refused(&text, "stale_session"), "{text}");
    assert_eq!(handle.key().generation, 11);
}
