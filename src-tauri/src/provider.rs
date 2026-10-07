//! Gemini provider transport: the Files API resumable upload and the
//! Interactions transcription call behind `POST /api/transcribe`, plus the
//! Live WebSocket URL the relay dials.
//!
//! Wire behavior is pinned:
//!
//! * Files API resumable upload: `Provider::upload_audio_file`.
//! * Interactions transcription body: `interaction_payload`, sent by
//!   `Provider::transcribe_uploaded_file`, with `extract_interaction_text`
//!   reading the transcript.
//! * Cleanup DELETE: `Provider::delete_uploaded_file`.
//! * Request deadlines and Google error mapping: `send_checked` and
//!   `google_failure`.
//! * Live WebSocket URL with the percent-encoded key: `Provider::live_url`.
//!
//! `Provider::check_key`, the settings window's access probe, is the documented
//! `models.list` call
//! (<https://ai.google.dev/api/models#method:-models.list>).
//!
//! Upstream documentation: <https://ai.google.dev/gemini-api/docs/files>,
//! <https://ai.google.dev/gemini-api/docs/transcribe>,
//! <https://ai.google.dev/gemini-api/docs/interactions-overview>.
//!
//! Invariants:
//! * One recording is bound to one API key: the upload start, the interaction
//!   and the `finally` DELETE all receive the caller's key and nothing else.
//! * An upload never outlives its request; the DELETE is best effort and its
//!   failure never changes the transcription result.
//! * Every message that can leave this module passes through
//!   [`crate::backend::redact`] with the request-selected ring, so a configured
//!   key cannot escape through a response body or a log line. The chosen key
//!   itself only travels in the `x-goog-api-key` header and, for Live, as a
//!   percent-encoded query parameter of the URL built by `Provider::live_url`.
//! * A key check reports one fixed category and builds no message at all: the
//!   page it reads is only classified, so no Google body, URL or key can reach a
//!   response or a log line, and a check never rotates the ring.
//! * The transport bases are Google's constants in every production build.
//!   Only a debug build compiled with the `test-provider` feature reads
//!   `SPEECHEK_TEST_PROVIDER_HTTP`/`SPEECHEK_TEST_PROVIDER_WSS`; release
//!   binaries contain no override code at all.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::backend::{redact, ApiError};
use crate::i18n::{MessageId, UiMessage};
use crate::secrets::SecretKey;

/// Google's Generative Language API over HTTPS. Production builds never talk
/// to anything else.
const GOOGLE_HTTP_BASE: &str = "https://generativelanguage.googleapis.com";
/// Google's Live `BidiGenerateContent` endpoint base over WSS.
const GOOGLE_WSS_BASE: &str = "wss://generativelanguage.googleapis.com";
/// Live endpoint path, appended to the WSS base.
const LIVE_WS_PATH: &str =
    "/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

/// Files API resumable upload endpoint (the upload URL itself comes back in
/// the `x-goog-upload-url` header of the start request).
const FILES_UPLOAD_PATH: &str = "/upload/v1beta/files";
/// Interactions API endpoint that performs the actual transcription.
const INTERACTIONS_PATH: &str = "/v1beta/interactions";

/// Batch transcription model backing both `smart` and `verbatim` modes.
const BATCH_MODEL: &str = "gemini-3.5-transcribe";

/// The browser records exactly this container/encoding; both the upload and
/// the interaction announce it.
const WAV_MIME_TYPE: &str = "audio/wav";
const DISPLAY_NAME: &str = "speechek-recording.wav";

/// Per-request deadlines: upload metadata 60 s, upload+finalize 300 s,
/// interaction 600 s, cleanup DELETE 30 s.
const UPLOAD_START_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_FINALIZE_TIMEOUT: Duration = Duration::from_secs(300);
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(600);
const DELETE_TIMEOUT: Duration = Duration::from_secs(30);
/// The key check's deadline: one `models.list` probe, one attempt, no retry.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// The key check's endpoint. The smallest documented page proves API access
/// without reading a model or touching a recording.
const MODELS_CHECK_PATH: &str = "/v1beta/models?pageSize=1";


/* -------------------------------------------------------------------------- */
/* Key check outcome                                                          */
/* -------------------------------------------------------------------------- */

/// What one key check found for one key. The settings window reports exactly
/// this; the type lives beside the transport so a check cannot drift from the
/// classification it names, and nothing here depends on the settings page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCheckOutcome {
    /// The service accepted the key.
    Ok,
    /// The service refused the key.
    Denied,
    /// The check could not decide; the payload names the fixed reason.
    Indeterminate(CheckFailure),
}

/// The reasons a key check cannot decide. Named rather than free text so the
/// page only ever shows wording that was compiled in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckFailure {
    /// The request never reached the service.
    Network,
    /// The service asked the caller to slow down.
    RateLimited,
    /// The answer was not the shape the endpoint documents.
    Malformed,
    /// Any other status, named by the number for diagnosis.
    Status(u16),
}

impl CheckFailure {
    pub fn ui(self) -> UiMessage {
        match self {
            CheckFailure::Network => UiMessage::new(MessageId::KeyCheckNetwork),
            CheckFailure::RateLimited => UiMessage::new(MessageId::KeyCheckRateLimited),
            CheckFailure::Malformed => UiMessage::new(MessageId::KeyCheckMalformed),
            CheckFailure::Status(status) => UiMessage::new(MessageId::KeyCheckStatus)
                .with_arg("status", json!(status)),
        }
    }
}

/// The Google-facing transport. Built once by the launcher and shared by the
/// router and the Live relay.
pub struct Provider {
    /// One pooled client for every request: upload, interaction and delete
    /// reuse its connections.
    http: reqwest::Client,
    http_base: String,
    wss_base: String,
}

impl Provider {
    /// Builds the transport with Google's bases, or - in a debug build compiled
    /// with the `test-provider` feature only - the local override that lets the
    /// integration harness run without touching the real API.
    pub fn new() -> Self {
        let (http_base, wss_base) = configure_bases();
        Provider {
            http: reqwest::Client::new(),
            http_base,
            wss_base,
        }
    }

    /// Full Live WebSocket URL for one socket. The key is percent-encoded as
    /// ECMAScript's `encodeURIComponent` does, so the URL is wire-identical.
    /// The key arrives borrowed, straight from the
    /// [`SecretKey`](crate::secrets::SecretKey) this socket selected.
    pub fn live_url(&self, key: &str) -> String {
        format!(
            "{}{}?key={}",
            self.wss_base,
            LIVE_WS_PATH,
            encode_uri_component(key)
        )
    }

    /// Probes whether one key may use the API: `GET /v1beta/models?pageSize=1`
    /// on the same client and base as every other request, one attempt with a
    /// 10 s deadline, no rotation and no retry. The settings window calls this
    /// per draft key; an unsaved draft is never rotated and Save never checks.
    ///
    /// The result is one fixed category: the response is classified, never
    /// quoted, so no Google body, URL or key can reach a message or a log line.
    /// Google's own status mapping is deliberately not reused: a check is not a
    /// transcription and its failures are not the browser's `UPSTREAM_*` codes.
    pub async fn check_key(&self, key: &SecretKey) -> KeyCheckOutcome {
        let request = self
            .http
            .get(format!("{}{}", self.http_base, MODELS_CHECK_PATH))
            .header("x-goog-api-key", key.as_str())
            .timeout(CHECK_TIMEOUT);
        let response = match request.send().await {
            Ok(response) => response,
            // A rejected request, refused connection or missed deadline never
            // reached the service, so the check cannot decide.
            Err(_) => return KeyCheckOutcome::Indeterminate(CheckFailure::Network),
        };
        match response.status().as_u16() {
            // `ListModelsResponse`: `models` is an array, `nextPageToken` is a
            // string, and either may be absent. What the models are is not read:
            // a well-formed page is all that proves access.
            200..=299 => match response.json::<Value>().await {
                Ok(payload) if is_models_page(&payload) => KeyCheckOutcome::Ok,
                _ => KeyCheckOutcome::Indeterminate(CheckFailure::Malformed),
            },
            // Google rejects a bad key with either the auth statuses or a 400
            // whose `error.details[].reason` names the key.
            401 | 403 => KeyCheckOutcome::Denied,
            400 => {
                let body = response.text().await.unwrap_or_default();
                match serde_json::from_str::<Value>(&body) {
                    Ok(payload) if names_invalid_api_key(&payload) => KeyCheckOutcome::Denied,
                    _ => KeyCheckOutcome::Indeterminate(CheckFailure::Status(400)),
                }
            }
            429 => KeyCheckOutcome::Indeterminate(CheckFailure::RateLimited),
            other => KeyCheckOutcome::Indeterminate(CheckFailure::Status(other)),
        }
    }

    /// Transcribes one 16 kHz mono PCM16 WAV recording: the same key performs
    /// the upload, the interaction and the final delete.
    ///
    /// `mode` is `"smart"` or `"verbatim"`; anything else is the route's 400,
    /// checked here so an invalid mode never costs an upload.
    pub async fn transcribe(
        &self,
        key: &Arc<SecretKey>,
        keys: &[Arc<SecretKey>],
        mode: &str,
        wav: Vec<u8>,
    ) -> Result<String, ApiError> {
        let mode_payload: Value = match mode {
            "smart" => Value::String("smart".to_string()),
            "verbatim" => json!({ "type": "verbatim" }),
            _ => {
                return Err(ApiError::new(
                    400,
                    "INVALID_MODE",
                    UiMessage::new(MessageId::InvalidMode),
                ));
            }
        };

        // Transport still uses exactly one chosen key. Diagnostic redaction
        // borrows the entire request-selected ring before any truncation, so a
        // Google body cannot expose a prefix of another key in that revision.

        let uploaded = self.upload_audio_file(key, keys, wav).await?;
        let result = self
            .transcribe_uploaded_file(key, keys, &uploaded.uri, mode_payload)
            .await;
        // Finally: the upload must not outlive the request that created it,
        // even when the transcription failed.
        self.delete_uploaded_file(key, keys, &uploaded.name).await;
        result
    }

    /// Documented Files API resumable upload: start (metadata, 60 s) then
    /// `upload, finalize` (bytes, 300 s). Returns the file `finally` deletes.
    async fn upload_audio_file(
        &self,
        key: &Arc<SecretKey>,
        keys: &[Arc<SecretKey>],
        bytes: Vec<u8>,
    ) -> Result<UploadedFile, ApiError> {
        let byte_length = bytes.len();
        let start = self
            .http
            .post(format!("{}{}", self.http_base, FILES_UPLOAD_PATH))
            .header("x-goog-api-key", key.as_str())
            .header("x-goog-upload-protocol", "resumable")
            .header("x-goog-upload-command", "start")
            .header(
                "x-goog-upload-header-content-length",
                byte_length.to_string(),
            )
            .header("x-goog-upload-header-content-type", WAV_MIME_TYPE)
            .header("content-type", "application/json")
            .body(json!({ "file": { "display_name": DISPLAY_NAME } }).to_string());
        let start_response = send_checked(
            start,
            MessageId::GoogleUploadStartFailed,
            keys,
            UPLOAD_START_TIMEOUT,
        )
        .await?;

        let upload_url = start_response
            .headers()
            .get("x-goog-upload-url")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        // Dropping the response cancels the metadata body, and the upload URL
        // above is all we read from it.
        drop(start_response);
        let Some(upload_url) = upload_url else {
            return Err(ApiError::new(
                502,
                "UPSTREAM_ERROR",
                UiMessage::new(MessageId::UploadUrlMissing),
            ));
        };

        let finalize = self
            .http
            .post(upload_url)
            .header("content-length", byte_length.to_string())
            .header("x-goog-upload-offset", "0")
            .header("x-goog-upload-command", "upload, finalize")
            .header("content-type", WAV_MIME_TYPE)
            .body(bytes);
        let uploaded = send_checked(
            finalize,
            MessageId::GoogleUploadFailed,
            keys,
            UPLOAD_FINALIZE_TIMEOUT,
        )
        .await?;

        let payload: Option<Value> = uploaded.json::<Value>().await.ok().filter(Value::is_object);
        let file = payload
            .as_ref()
            .and_then(|payload| payload.get("file"))
            .and_then(Value::as_object);
        let name = file
            .and_then(|file| file.get("name"))
            .and_then(Value::as_str);
        let uri = file
            .and_then(|file| file.get("uri"))
            .and_then(Value::as_str);
        let (Some(name), Some(uri)) = (name, uri) else {
            return Err(ApiError::new(
                502,
                "UPSTREAM_ERROR",
                UiMessage::new(MessageId::UploadFileMetadataMissing),
            ));
        };
        if file
            .and_then(|file| file.get("state"))
            .and_then(Value::as_str)
            == Some("FAILED")
        {
            return Err(ApiError::new(
                502,
                "UPSTREAM_ERROR",
                UiMessage::new(MessageId::UploadRejected),
            ));
        }
        Ok(UploadedFile {
            name: name.to_owned(),
            uri: uri.to_owned(),
        })
    }

    /// Interactions API transcription: the body built by [`interaction_payload`],
    /// pinned to `gemini-3.5-transcribe`. The transcript is `output_text`; when
    /// it is blank, the text parts of `outputs` and then of `steps` content are
    /// used.
    async fn transcribe_uploaded_file(
        &self,
        key: &Arc<SecretKey>,
        keys: &[Arc<SecretKey>],
        file_uri: &str,
        mode_payload: Value,
    ) -> Result<String, ApiError> {
        let request = self
            .http
            .post(format!("{}{}", self.http_base, INTERACTIONS_PATH))
            .header("x-goog-api-key", key.as_str())
            .header("content-type", "application/json")
            .body(interaction_payload(file_uri, mode_payload).to_string());
        let response =
            send_checked(request, MessageId::GoogleTranscriptionFailed, keys, INTERACTION_TIMEOUT).await?;

        let payload: Option<Value> = response.json::<Value>().await.ok().filter(Value::is_object);
        let Some(payload) = payload else {
            return Err(ApiError::new(
                502,
                "UPSTREAM_ERROR",
                UiMessage::new(MessageId::TranscriptionUnreadable),
            ));
        };
        if let Some(failure) = upstream_error_message(&payload) {
            return Err(ApiError::new(502, "UPSTREAM_ERROR",
                UiMessage::new(MessageId::GoogleRequestFailed)
                    .with_arg("detail", json!(redact(&failure, keys)))));
        }

        let text = extract_interaction_text(&payload);
        if text.trim().is_empty() {
            let status = payload
                .get("status")
                .and_then(Value::as_str)
                .map(|status| json!(redact(status, keys)))
                .unwrap_or_else(|| json!(UiMessage::new(MessageId::InteractionStatusUnknown)));
            return Err(ApiError::new(
                502,
                "EMPTY_TRANSCRIPT",
                UiMessage::new(MessageId::EmptyTranscript)
                    .with_arg("status", status),
            ));
        }
        Ok(text)
    }

    /// Best effort cleanup, 30 s: an upload must never outlive its request.
    /// Failures are logged redacted and never change the transcription result.
    async fn delete_uploaded_file(
        &self,
        key: &Arc<SecretKey>,
        keys: &[Arc<SecretKey>],
        name: &str,
    ) {
        let request = self
            .http
            .delete(format!("{}/v1beta/{}", self.http_base, name))
            .header("x-goog-api-key", key.as_str());
        match request.timeout(DELETE_TIMEOUT).send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => eprintln!(
                "speechek: provider: Gemini upload deletion failed (HTTP {}); Google may retain the file until expiry.",
                response.status().as_u16()
            ),
            Err(error) => eprintln!(
                "speechek: provider: {}",
                redact(
                    &format!("Could not delete the temporary Gemini upload: {error}"),
                    keys
                )
            ),
        }
    }
}

/// The exact `POST /v1beta/interactions` body for one transcription, field for
/// field: the uploaded file URI, the mode (`"smart"` or `{"type":"verbatim"}`),
/// `language_codes: []` for automatic detection and `store: false`. Kept as one
/// function so the wire shape is pinned by the tests below instead of drifting
/// here.
fn interaction_payload(file_uri: &str, mode_payload: Value) -> Value {
    json!({
        "model": BATCH_MODEL,
        "input": [{ "type": "audio", "uri": file_uri, "mime_type": WAV_MIME_TYPE }],
        "generation_config": {
            "transcription_config": { "mode": mode_payload, "language_codes": [] },
        },
        "store": false,
    })
}

/// The uploaded file that `finally` has to delete.
struct UploadedFile {
    name: String,
    uri: String,
}

/// Sends one request with its own deadline and maps a transport failure
/// (including an aborted timeout) to `UPSTREAM_UNREACHABLE`.
async fn send_checked(
    request: reqwest::RequestBuilder,
    action: MessageId,
    keys: &[Arc<SecretKey>],
    timeout: Duration,
) -> Result<reqwest::Response, ApiError> {
    let response = request.timeout(timeout).send().await.map_err(|error| {
        ApiError::new(
            502,
            "UPSTREAM_UNREACHABLE",
            UiMessage::new(MessageId::UpstreamUnreachable)
                .with_arg("detail", json!(redact(&error.to_string(), keys))),
        )
    })?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(google_failure(response, action, keys).await)
    }
}

/// Non-2xx from Google, carrying redacted diagnostics inside an authored reason.
async fn google_failure(
    response: reqwest::Response,
    action: MessageId,
    keys: &[Arc<SecretKey>],
) -> ApiError {
    let google_status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    google_failure_body(google_status, action, &body, keys)
}

/// Classifies one non-2xx body. The thinking gate the unary
/// `gemini-3.5-transcribe` route currently rejects every transcription with is
/// reported as the standalone hint instead of the upstream text; every other
/// failure keeps the redacted detail. Split from [`google_failure`] so the
/// classification is testable without an HTTP response.
fn google_failure_body(
    google_status: u16,
    action: MessageId,
    body: &str,
    keys: &[Arc<SecretKey>],
) -> ApiError {
    let (status, code) = status_mapping(google_status);
    let parsed = serde_json::from_str::<Value>(body).ok();
    let raw = parsed.as_ref().and_then(upstream_error_message)
        .unwrap_or_else(|| body.trim());
    if action == MessageId::GoogleTranscriptionFailed && is_thinking_gate_error(google_status, raw) {
        return ApiError::new(
            status,
            code,
            UiMessage::new(MessageId::GoogleThinkingUnsupported),
        );
    }
    let action = if matches!(google_status, 401 | 403) {
        match action {
            MessageId::GoogleUploadStartFailed => MessageId::GoogleUploadStartAuthRejected,
            MessageId::GoogleUploadFailed => MessageId::GoogleUploadAuthRejected,
            MessageId::GoogleTranscriptionFailed => MessageId::GoogleTranscriptionAuthRejected,
            _ => action,
        }
    } else {
        action
    };
    let detail = if body.trim().is_empty() {
        json!(UiMessage::new(MessageId::GoogleEmptyErrorBody))
    } else {
        // Redact before truncating: truncation must not expose a key prefix.
        json!(redact(raw, keys).chars().take(300).collect::<String>())
    };
    ApiError::new(status, code, UiMessage::new(action)
        .with_arg("detail", detail)
        .with_arg("status", json!(google_status)))
}

/// The thinking gate: an HTTP 400 whose upstream message names the thinking
/// setting the unary `gemini-3.5-transcribe` route currently rejects every
/// request over (googleapis/js-genai#2011). Only this status and these two
/// wordings qualify, so an unrelated 400 keeps its redacted detail; Live Smart
/// never reaches this mapping at all.
fn is_thinking_gate_error(google_status: u16, message: &str) -> bool {
    google_status == 400
        && (contains_ignore_ascii_case(message, "thinking is not enabled")
            || contains_ignore_ascii_case(message, "thinking level"))
}

/// `haystack.contains(needle)` ignoring ASCII case, without allocating.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Google HTTP status maps to the existing browser status/code pair.
fn status_mapping(google_status: u16) -> (u16, &'static str) {
    match google_status {
        400 => (400, "BAD_REQUEST"),
        401 | 403 => (502, "AUTH_FAILED"),
        404 => (404, "NOT_FOUND"),
        429 => (429, "RATE_LIMITED"),
        _ => (502, "UPSTREAM_ERROR"),
    }
}

/// Extracts the interaction transcript: a non-blank `output_text` wins,
/// otherwise the text parts of `outputs` are joined, and only when those are
/// empty are the text parts of each `steps[].content` joined instead.
fn extract_interaction_text(payload: &Value) -> String {
    if let Some(text) = payload.get("output_text").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return text.to_string();
        }
    }

    let mut parts = Vec::new();
    collect_text_parts(payload.get("outputs"), &mut parts);
    if parts.is_empty() {
        if let Some(Value::Array(steps)) = payload.get("steps") {
            for step in steps {
                collect_text_parts(
                    step.as_object().and_then(|step| step.get("content")),
                    &mut parts,
                );
            }
        }
    }
    parts.concat()
}

/// The text parts of one content list: an entry counts when its `type` is
/// absent or `"text"` and it carries a string `text`.
fn collect_text_parts(contents: Option<&Value>, parts: &mut Vec<String>) {
    let Some(Value::Array(entries)) = contents else {
        return;
    };
    for entry in entries {
        let Some(part) = entry.as_object() else {
            continue;
        };
        let is_text = match part.get("type") {
            None => true,
            Some(Value::String(kind)) => kind == "text",
            Some(_) => false,
        };
        if !is_text {
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            parts.push(text.to_string());
        }
    }
}

/// Whether a 2xx body is the documented `ListModelsResponse` page the check
/// accepts: a JSON object whose `models` field is an array or absent and whose
/// `nextPageToken` field is a string or absent. The model entries themselves are
/// never inspected, so Google can add fields without breaking a check.
fn is_models_page(payload: &Value) -> bool {
    let Some(object) = payload.as_object() else {
        return false;
    };
    let models_shaped = match object.get("models") {
        None | Some(Value::Array(_)) => true,
        Some(_) => false,
    };
    let token_shaped = match object.get("nextPageToken") {
        None | Some(Value::String(_)) => true,
        Some(_) => false,
    };
    models_shaped && token_shaped
}

/// Whether a Google error body says the key itself is the problem: some
/// `error.details[]` entry carries `reason == "API_KEY_INVALID"`. Only the
/// reason is read; the message and the rest of the body stay out of every
/// message the caller can see.
fn names_invalid_api_key(payload: &Value) -> bool {
    let details = payload
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("details"))
        .and_then(Value::as_array);
    let Some(details) = details else {
        return false;
    };
    details.iter().any(|detail| {
        detail
            .as_object()
            .and_then(|detail| detail.get("reason"))
            .and_then(Value::as_str)
            == Some("API_KEY_INVALID")
    })
}

/// The OpenAI-shaped `{ "error": { "message": string } }` node Google returns,
/// trimmed; `None` when it is absent, of another type or blank.
fn upstream_error_message(payload: &Value) -> Option<&str> {
    let message = payload
        .get("error")
        .and_then(Value::as_object)?
        .get("message")
        .and_then(Value::as_str)?;
    let trimmed = message.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// `encodeURIComponent`: unreserved characters plus the ECMAScript extras
/// (`- _ . ! ~ * ' ( )`) pass through; every other byte of the UTF-8 encoding
/// becomes an uppercase percent triplet.
fn encode_uri_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => encoded.push(char::from(byte)),
            _ => {
                encoded.push('%');
                encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX[usize::from(byte & 0x0F)]));
            }
        }
    }
    encoded
}

/// Transport bases for `Provider::new`. Production builds use Google's
/// constants; a debug build compiled with the `test-provider` feature may
/// override both through the environment, and blank or whitespace-only values
/// still fall back to Google. The override names do not exist in release code.
fn configure_bases() -> (String, String) {
    #[cfg(all(feature = "test-provider", debug_assertions))]
    let (http_base, wss_base) = {
        /// One override: trimmed, without a trailing slash, empty -> fallback.
        fn env_base(name: &str, fallback: &str) -> String {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| fallback.to_string())
        }
        (
            env_base("SPEECHEK_TEST_PROVIDER_HTTP", GOOGLE_HTTP_BASE),
            env_base("SPEECHEK_TEST_PROVIDER_WSS", GOOGLE_WSS_BASE),
        )
    };

    #[cfg(not(all(feature = "test-provider", debug_assertions)))]
    let (http_base, wss_base) = (GOOGLE_HTTP_BASE.to_string(), GOOGLE_WSS_BASE.to_string());

    (http_base, wss_base)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smart request: the mode is the bare `"smart"` string the parameter
    /// reference documents.
    #[test]
    fn smart_interaction_payload_matches_the_documented_shape() {
        let payload = interaction_payload("files/abc", Value::String("smart".to_owned()));
        assert_eq!(payload["model"], BATCH_MODEL);
        assert_eq!(payload["input"][0]["type"], "audio");
        assert_eq!(payload["input"][0]["uri"], "files/abc");
        assert_eq!(payload["input"][0]["mime_type"], WAV_MIME_TYPE);
        assert_eq!(
            payload["generation_config"]["transcription_config"]["mode"],
            "smart"
        );
        assert_eq!(
            payload["generation_config"]["transcription_config"]["language_codes"],
            json!([])
        );
        assert_eq!(payload["store"], false);
    }

    /// Verbatim keeps the object form of the mode, which is what the reference
    /// requires (`{"type":"verbatim"}`).
    #[test]
    fn verbatim_interaction_payload_carries_the_mode_object() {
        let payload = interaction_payload("files/abc", json!({ "type": "verbatim" }));
        assert_eq!(
            payload["generation_config"]["transcription_config"]["mode"],
            json!({ "type": "verbatim" })
        );
    }

    /// The regression guard: the batch body stays exactly the shape that worked
    /// through 2026-10-06, so neither mode carries a thinking field
    /// (`thinking_level`, `thinking_config`, `thinking_budget` or
    /// `thinking_summaries`). Pinning a level is not a fix: since 2026-10-07
    /// Google's unary `gemini-3.5-transcribe` route rejects every body,
    /// including this one, with HTTP 400 `Thinking is not enabled for this
    /// model`, and adding a level only changes that message to `Thinking level
    /// is not supported for this model`.
    #[test]
    fn transcription_requests_never_ask_for_thinking() {
        for mode in [Value::String("smart".to_owned()), json!({ "type": "verbatim" })] {
            let payload = interaction_payload("files/abc", mode.clone());
            assert_eq!(
                payload,
                json!({
                    "model": BATCH_MODEL,
                    "input": [{ "type": "audio", "uri": "files/abc", "mime_type": WAV_MIME_TYPE }],
                    "generation_config": {
                        "transcription_config": { "mode": mode, "language_codes": [] },
                    },
                    "store": false,
                })
            );
        }
    }

    /// Every wording the thinking gate returns since 2026-10-07 maps to the
    /// standalone hint: no upstream text, no arguments, nothing to redact.
    #[test]
    fn thinking_gate_400_maps_to_the_hint() {
        use crate::i18n::Language;

        let bodies = [
            "Thinking is not enabled for this model",
            "Thinking level is not supported for this model.",
            "'medium' is not a supported thinking level for this model. Allowed values are: high, low.",
        ];
        for body in bodies {
            let failure = google_failure_body(
                400,
                MessageId::GoogleTranscriptionFailed,
                &json!({ "error": { "message": body } }).to_string(),
                &[],
            );
            assert_eq!(failure.status, 400);
            assert_eq!(failure.code, "BAD_REQUEST");
            assert_eq!(failure.ui.key, MessageId::GoogleThinkingUnsupported);
            assert_eq!(failure.ui.args, None);
            assert_eq!(
                failure.message,
                MessageId::GoogleThinkingUnsupported.template(Language::En)
            );
        }
    }

    /// The gate is narrow: the same 400 status without a thinking wording, a
    /// thinking wording on another status, and a thinking wording on another
    /// request all keep their own failure and its detail.
    #[test]
    fn failures_outside_the_thinking_gate_keep_their_detail() {
        let unrelated = json!({ "error": { "message": "Invalid JSON payload received." } }).to_string();
        let failure = google_failure_body(400, MessageId::GoogleTranscriptionFailed, &unrelated, &[]);
        assert_eq!(failure.code, "BAD_REQUEST");
        assert_eq!(failure.ui.key, MessageId::GoogleTranscriptionFailed);
        assert_eq!(
            failure.ui.args.as_ref().and_then(|args| args.get("status")),
            Some(&json!(400u16))
        );
        assert!(failure.message.contains("Invalid JSON payload received."));

        let thinking = json!({ "error": { "message": "Thinking is not enabled for this model" } }).to_string();
        let other_status = google_failure_body(502, MessageId::GoogleTranscriptionFailed, &thinking, &[]);
        assert_eq!(other_status.ui.key, MessageId::GoogleTranscriptionFailed);
        assert!(other_status.message.contains("Thinking is not enabled for this model"));

        let other_action = google_failure_body(400, MessageId::GoogleUploadFailed, &thinking, &[]);
        assert_eq!(other_action.ui.key, MessageId::GoogleUploadFailed);
    }

    /// The gate matches ASCII case-insensitively and only on its two wordings.
    #[test]
    fn thinking_gate_wording_is_matched_case_insensitively() {
        assert!(is_thinking_gate_error(400, "THINKING LEVEL IS NOT SUPPORTED FOR THIS MODEL."));
        assert!(is_thinking_gate_error(400, "thinking is not enabled for this model"));
        assert!(is_thinking_gate_error(400, "not a supported thinking level"));
        assert!(!is_thinking_gate_error(400, "Thinking budget is out of range."));
        assert!(!is_thinking_gate_error(400, ""));
        assert!(!is_thinking_gate_error(429, "Thinking level is not supported for this model."));
    }
}
