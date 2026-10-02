//! Gemini provider transport: the Files API resumable upload and the
//! Interactions transcription call behind `POST /api/transcribe`, plus the
//! Live WebSocket URL the relay dials.
//!
//! Wire behavior is pinned:
//!
//! * Files API resumable upload: `Provider::upload_audio_file`.
//! * Interactions transcription call: `Provider::transcribe_uploaded_file`,
//!   with `extract_interaction_text` reading the transcript.
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
//!   [`crate::backend::redact`], so a configured key cannot escape through a
//!   response body or a log line. The key itself only travels in the
//!   `x-goog-api-key` header and, for Live, as a percent-encoded query
//!   parameter of the URL built by `Provider::live_url`.
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

/// Appended verbatim to mapped 401/403 failures.
const AUTH_HINT: &str = " The Gemini API key used for this request was rejected by Google.";

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
    /// The fixed line the page shows for this reason.
    pub fn message(self) -> String {
        match self {
            CheckFailure::Network => "Не удалось завершить проверку: сеть недоступна".to_owned(),
            CheckFailure::RateLimited => {
                "Не удалось завершить проверку: превышен лимит запросов".to_owned()
            }
            CheckFailure::Malformed => {
                "Не удалось завершить проверку: ответ сервиса не распознан".to_owned()
            }
            CheckFailure::Status(status) => {
                format!("Не удалось завершить проверку: сервис ответил кодом {status}")
            }
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
                    "mode must be 'smart' or 'verbatim'.",
                ));
            }
        };

        // The recording is bound to one key, so redaction only ever needs that
        // one: a borrowed one-element slice, never a copy of the secret.
        let keys = std::slice::from_ref(key);

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
            "Could not start the file upload",
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
                "Gemini did not return an upload URL.",
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
            "Could not upload the recording",
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
                "Gemini upload response is missing the file name or URI.",
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
                "Gemini rejected the uploaded recording.",
            ));
        }
        Ok(UploadedFile {
            name: name.to_owned(),
            uri: uri.to_owned(),
        })
    }

    /// Interactions API transcription: uploaded file URI, the mode object
    /// (`"smart"` or `{"type":"verbatim"}`), `language_codes: []` for automatic
    /// detection and `store: false`. The transcript is `output_text`; when it is
    /// blank, the text parts of `outputs` and then of `steps` content are used.
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
            .body(
                json!({
                    "model": BATCH_MODEL,
                    "input": [{ "type": "audio", "uri": file_uri, "mime_type": WAV_MIME_TYPE }],
                    "generation_config": {
                        "transcription_config": { "mode": mode_payload, "language_codes": [] },
                    },
                    "store": false,
                })
                .to_string(),
            );
        let response =
            send_checked(request, "Transcription failed", keys, INTERACTION_TIMEOUT).await?;

        let payload: Option<Value> = response.json::<Value>().await.ok().filter(Value::is_object);
        let Some(payload) = payload else {
            return Err(ApiError::new(
                502,
                "UPSTREAM_ERROR",
                "Gemini returned an unreadable transcription response.",
            ));
        };
        if let Some(failure) = upstream_error_message(&payload) {
            return Err(ApiError::new(502, "UPSTREAM_ERROR", redact(&failure, keys)));
        }

        let text = extract_interaction_text(&payload);
        if text.trim().is_empty() {
            let status = payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(ApiError::new(
                502,
                "EMPTY_TRANSCRIPT",
                redact(
                    &format!("Gemini returned an empty transcript (interaction status: {status})."),
                    keys,
                ),
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

/// The uploaded file that `finally` has to delete.
struct UploadedFile {
    name: String,
    uri: String,
}

/// Sends one request with its own deadline and maps a transport failure
/// (including an aborted timeout) to `UPSTREAM_UNREACHABLE`.
async fn send_checked(
    request: reqwest::RequestBuilder,
    detail: &str,
    keys: &[Arc<SecretKey>],
    timeout: Duration,
) -> Result<reqwest::Response, ApiError> {
    let response = request.timeout(timeout).send().await.map_err(|error| {
        ApiError::new(
            502,
            "UPSTREAM_UNREACHABLE",
            redact(&format!("Could not reach the Gemini API: {error}"), keys),
        )
    })?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(google_failure(response, detail, keys).await)
    }
}

/// Non-2xx from Google -> the mapped error, carrying Google's own message when
/// the body has one (redacted, 300 characters at most).
async fn google_failure(
    response: reqwest::Response,
    detail: &str,
    keys: &[Arc<SecretKey>],
) -> ApiError {
    let google_status = response.status().as_u16();
    let (status, code, hint) = status_mapping(google_status);
    let body = response.text().await.unwrap_or_default();
    let body_detail = if body.trim().is_empty() {
        "Gemini API request failed".to_string()
    } else {
        match serde_json::from_str::<Value>(&body)
            .ok()
            .as_ref()
            .and_then(upstream_error_message)
        {
            Some(message) => message,
            None => body.trim().chars().take(300).collect(),
        }
    };
    ApiError::new(
        status,
        code,
        redact(
            &format!("{detail}: {body_detail} (Google HTTP {google_status}).{hint}"),
            keys,
        ),
    )
}

/// Google HTTP status -> the status/code/hint triple the browser sees:
/// everything unmapped is a 502 `UPSTREAM_ERROR`.
fn status_mapping(google_status: u16) -> (u16, &'static str, &'static str) {
    match google_status {
        400 => (400, "BAD_REQUEST", ""),
        401 | 403 => (502, "AUTH_FAILED", AUTH_HINT),
        404 => (404, "NOT_FOUND", ""),
        429 => (429, "RATE_LIMITED", ""),
        _ => (502, "UPSTREAM_ERROR", ""),
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
fn upstream_error_message(payload: &Value) -> Option<String> {
    let message = payload
        .get("error")
        .and_then(Value::as_object)?
        .get("message")
        .and_then(Value::as_str)?;
    let trimmed = message.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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
