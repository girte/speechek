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
//!
//! Development diagnostics may additionally archive this module's content: the
//! pinned request configuration together with the rotation entry the request
//! used - its 1-based ordinal and the ring's key count, never the key - and
//! every response body that was read - the
//! upload finalize and interaction bodies, a non-2xx refusal, and the bodies the
//! transport itself ignores, which a diagnostics-only observer reads without
//! ever making the request wait. Each record carries the status and the stage it
//! belongs to, is redacted against the request-selected ring, and is built only
//! when `all(debug_assertions, feature = "dev-diagnostics")` holds and the
//! request is traced; the transport, the payloads and the deadlines never depend
//! on it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::backend::{redact, ApiError};
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use crate::backend::{diagnostic_bytes, diagnostic_json, diagnostic_text};
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use base64::Engine as _;
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use crate::diagnostics::{
    ContentFormat, ContentKind, ErrorClass, RequestCause, RequestTrace, Stage, StagePhase,
};
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

    /// The tests' own transport: the same `reqwest` client and the same wire
    /// behavior as [`Provider::new`], with both loopback bases given
    /// explicitly. It reads no process environment, so a fixture never has to
    /// mutate one and two fixtures can never race on it.
    #[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
    pub fn for_test(http_base: &str, wss_base: &str) -> Self {
        Provider {
            http: reqwest::Client::new(),
            http_base: http_base.trim_end_matches('/').to_string(),
            wss_base: wss_base.trim_end_matches('/').to_string(),
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
    /// `vocabulary` biases recognition toward the user's terms; empty means the
    /// request carries no vocabulary field at all. `languages` are the
    /// dictation-language hints; empty is Auto-detect and keeps `language_codes: []`.
    /// The development diagnostics may observe this request; the transport
    /// behavior, the payloads and the deadlines never depend on it.
    pub async fn transcribe(
        &self,
        key: &Arc<SecretKey>,
        keys: &[Arc<SecretKey>],
        mode: &str,
        vocabulary: &[String],
        languages: &[String],
        wav: Vec<u8>,
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        traced: Option<RequestTrace>,
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

        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        let traced = traced.as_ref();
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        if let Some(traced) = traced {
            // The exact batch request about to be sent: the pinned model, the
            // mode and the configuration the interaction body carries. The
            // redaction borrows the same request-selected ring the transport
            // uses, so a key echoed by any of its members never reaches the
            // archive.
            traced.content(
                ContentKind::ProviderRequest,
                ContentFormat::Json,
                diagnostic_json(
                    &json!({
                        "route": "batch",
                        "model": BATCH_MODEL,
                        "mode": mode,
                        "mode_payload": &mode_payload,
                        // How many terms the request biases recognition toward:
                        // the count only, never a term.
                        "custom_vocabulary_terms": vocabulary.len(),
                        // The dictation-language hints: configuration, not
                        // user text.
                        "language_codes": languages,
                        "wav_bytes": wav.len(),
                        // Which pinned rotation entry this request used, and
                        // how many the ring had: the ordinal and the count
                        // only, never the key.
                        "key_ordinal": key_ordinal(key, keys),
                        "ring_keys": keys.len(),
                    }),
                    keys,
                ),
            );
        }
        let uploaded = self
            .upload_audio_file(
                key,
                keys,
                wav,
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                traced,
            )
            .await?;
        let result = self
            .transcribe_uploaded_file(
                key,
                keys,
                &uploaded.uri,
                vocabulary,
                languages,
                mode_payload,
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                traced,
            )
            .await;
        // Finally: the upload must not outlive the request that created it,
        // even when the transcription failed.
        self.delete_uploaded_file(
            key,
            keys,
            &uploaded.name,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            traced,
        )
        .await;
        result
    }

    /// Documented Files API resumable upload: start (metadata, 60 s) then
    /// `upload, finalize` (bytes, 300 s). Returns the file `finally` deletes.
    async fn upload_audio_file(
        &self,
        key: &Arc<SecretKey>,
        keys: &[Arc<SecretKey>],
        bytes: Vec<u8>,
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] traced: Option<&RequestTrace>,
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
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            traced,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            Stage::UploadStart,
        )
        .await?;

        let upload_url = start_response
            .headers()
            .get("x-goog-upload-url")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        // Dropping the response cancels the metadata body, and the upload URL
        // above is all the transport reads from it. A traced request hands that
        // ignored body to a diagnostics-only observer instead: the request path
        // never awaits it, and an untraced one drops it exactly as before.
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        match traced {
            Some(traced) => observe_body(start_response, Stage::UploadStart, traced, keys),
            None => drop(start_response),
        }
        #[cfg(not(all(debug_assertions, feature = "dev-diagnostics")))]
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
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            traced,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            Stage::UploadFinalize,
        )
        .await?;

        let payload: Option<Value> = read_stage_body(
            uploaded,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            traced,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            Stage::UploadFinalize,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            keys,
        )
        .await
        .filter(Value::is_object);
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
    /// used. `vocabulary` is the custom vocabulary the body biases recognition
    /// toward; an empty slice adds no vocabulary field at all. `languages` are
    /// the dictation-language hints the body's `language_codes` carries.
    async fn transcribe_uploaded_file(
        &self,
        key: &Arc<SecretKey>,
        keys: &[Arc<SecretKey>],
        file_uri: &str,
        vocabulary: &[String],
        languages: &[String],
        mode_payload: Value,
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] traced: Option<&RequestTrace>,
    ) -> Result<String, ApiError> {
        let request = self
            .http
            .post(format!("{}{}", self.http_base, INTERACTIONS_PATH))
            .header("x-goog-api-key", key.as_str())
            .header("content-type", "application/json")
            .body(interaction_payload(file_uri, mode_payload, vocabulary, languages).to_string());
        let response = send_checked(
            request,
            MessageId::GoogleTranscriptionFailed,
            keys,
            INTERACTION_TIMEOUT,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            traced,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            Stage::Interaction,
        )
        .await?;

        let payload: Option<Value> = read_stage_body(
            response,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            traced,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            Stage::Interaction,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            keys,
        )
        .await
        .filter(Value::is_object);
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
        // The interaction returned a transcript: this request really did end
        // with its final result, whatever the caller does with it.
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        if let Some(traced) = traced {
            traced.cause(RequestCause::FinalReceived);
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
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] traced: Option<&RequestTrace>,
    ) {
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        note_stage(traced, Stage::Delete, StagePhase::Begin, None, false);
        let request = self
            .http
            .delete(format!("{}/v1beta/{}", self.http_base, name))
            .header("x-goog-api-key", key.as_str());
        match request.timeout(DELETE_TIMEOUT).send().await {
            Ok(response) if response.status().is_success() => {
                // The status the cleanup's own completion carried, recorded
                // before the body is handed on.
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                let delete_status = response.status().as_u16();
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                note_stage(traced, Stage::Delete, StagePhase::End, Some(delete_status), false);
                // A cleanup that succeeded has its own body, and the transport
                // never reads it: the same diagnostics-only observer takes it,
                // so this path still does not wait for it.
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                if let Some(traced) = traced {
                    observe_body(response, Stage::Delete, traced, keys);
                }
            }
            Ok(response) => {
                let status = response.status().as_u16();
                eprintln!(
                    "speechek: provider: Gemini upload deletion failed (HTTP {}); Google may retain the file until expiry.",
                    status
                );
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                note_stage(traced, Stage::Delete, StagePhase::Error, Some(status), false);
                // The refusal's own body is evidence, and the cleanup must not
                // wait for it: an ignored body is read by a diagnostics-only
                // observer that can outlive the request without changing the
                // result the caller already has.
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                if let Some(traced) = traced {
                    observe_body(response, Stage::Delete, traced, keys);
                }
            }
            Err(error) => {
                eprintln!(
                    "speechek: provider: {}",
                    redact(
                        &format!("Could not delete the temporary Gemini upload: {error}"),
                        keys
                    )
                );
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                if let Some(traced) = traced {
                    traced.content(
                        ContentKind::TransportError,
                        ContentFormat::Text,
                        transport_detail(&error, keys),
                    );
                }
                // A delete failure is best effort: it reports its own stage and
                // never overwrites the cause, the class or the status of the
                // request it followed.
                #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
                note_stage(
                    traced,
                    Stage::Delete,
                    StagePhase::Error,
                    None,
                    error.is_timeout(),
                );
            }
        }
    }
}

/// The exact `POST /v1beta/interactions` body for one transcription, field for
/// field: the uploaded file URI, the mode (`"smart"` or `{"type":"verbatim"}`),
/// `language_codes` with the dictation-language hints - `[]`, automatic
/// detection, when there are none - and `store: false`. A non-empty
/// `vocabulary` rides in the same `transcription_config`; an empty one leaves
/// the body exactly as it was before the vocabulary existed. Kept as one
/// function so the wire shape is pinned by the tests below instead of drifting
/// here.
fn interaction_payload(
    file_uri: &str,
    mode_payload: Value,
    vocabulary: &[String],
    languages: &[String],
) -> Value {
    let mut transcription_config = json!({ "mode": mode_payload, "language_codes": languages });
    if !vocabulary.is_empty() {
        transcription_config["custom_vocabulary"] = json!(vocabulary);
    }
    json!({
        "model": BATCH_MODEL,
        "input": [{ "type": "audio", "uri": file_uri, "mime_type": WAV_MIME_TYPE }],
        "generation_config": {
            "transcription_config": transcription_config,
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
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] traced: Option<&RequestTrace>,
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] stage: Stage,
) -> Result<reqwest::Response, ApiError> {
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    note_stage(traced, stage, StagePhase::Begin, None, false);
    let response = match request.timeout(timeout).send().await {
        Ok(response) => response,
        // The transport never produced an HTTP status, so the failure is
        // classified from the `reqwest::Error` alone - timeout, connect, body
        // or decode - before its rendered text is redacted into the authored
        // detail the browser sees. The archive keeps that failure in full with
        // the URL removed: neither the URL nor an auth header ever reaches it.
        Err(error) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            {
                note_stage(traced, stage, StagePhase::Error, None, error.is_timeout());
                if let Some(traced) = traced {
                    traced.error_class(classify_transport_error(&error));
                    // The whole transport failure, not just the mapped 502: the
                    // request URL is stripped because it carries the key in its
                    // query, and every key of the pinned ring is redacted.
                    traced.content(
                        ContentKind::TransportError,
                        ContentFormat::Text,
                        transport_detail(&error, keys),
                    );
                }
            }
            return Err(ApiError::new(
                502,
                "UPSTREAM_UNREACHABLE",
                UiMessage::new(MessageId::UpstreamUnreachable)
                    .with_arg("detail", json!(redact(&error.to_string(), keys))),
            ));
        }
    };
    let status = response.status();
    if status.is_success() {
        // `UploadStart` really is complete once its headers arrive: the upload
        // URL header is all it is read for. The body-bearing stages end where
        // their body has been consumed, so a stall or a truncated body is not
        // reported as a fast success.
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        if matches!(stage, Stage::UploadStart) {
            note_stage(traced, stage, StagePhase::End, Some(status.as_u16()), false);
        }
        Ok(response)
    } else {
        let failure = google_failure(
            response,
            action,
            keys,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            traced,
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            stage,
        )
        .await;
        #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
        note_stage(
            traced,
            stage,
            StagePhase::Error,
            // The upstream's own status, captured before the response was
            // consumed for the browser-facing mapping: that mapping is a 502,
            // and this is what actually came back.
            Some(status.as_u16()),
            false,
        );
        Err(failure)
    }
}

/// Consumes one body-bearing stage's JSON response and records where that stage
/// really ended: at the body, not at its headers. A body that times out and a
/// payload that will not decode are classified from the actual `reqwest::Error`
/// and still yield `None`, exactly like the previous `.ok()`.
///
/// The archive needs the exact bytes this stage consumed and a body can only be
/// read once, so a traced request reads them once, archives them in full with
/// the status and the stage, and decodes them from memory with the same
/// `serde_json` call `.json()` makes - which is `bytes` followed by
/// `from_slice`, its failure mapped to the decode class. An untraced request
/// keeps `.json()` exactly as it was, and neither path reads a body twice.
async fn read_stage_body(
    response: reqwest::Response,
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] traced: Option<&RequestTrace>,
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] stage: Stage,
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] keys: &[Arc<SecretKey>],
) -> Option<Value> {
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    let status = response.status().as_u16();
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    if let Some(traced) = traced {
        let (class, timeout) = match response.bytes().await {
            Ok(bytes) => {
                note_body(Some(traced), stage, status, &bytes, keys);
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(payload) => {
                        note_stage(Some(traced), stage, StagePhase::End, Some(status), false);
                        return Some(payload);
                    }
                    // The headers arrived and the body was not JSON: this is a
                    // *complete* body the archive already has in full, so it is
                    // not a lost observation - only a decode failure, with the
                    // class the old `.json()` reported, at exactly this moment.
                    Err(_) => (ErrorClass::Decode, false),
                }
            }
            // The body itself failed: the actual `reqwest::Error` classifies
            // it, no complete copy exists to archive, and the observation is
            // recorded as lost with the stage, the status and the full redacted
            // detail rather than being silently reduced to a numeric stage.
            Err(error) => {
                note_body_unreadable(Some(traced), stage, status, &error, keys);
                (classify_transport_error(&error), error.is_timeout())
            }
        };
        note_stage(Some(traced), stage, StagePhase::Error, Some(status), timeout);
        traced.error_class(class);
        return None;
    }
    match response.json::<Value>().await {
        Ok(payload) => {
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            note_stage(traced, stage, StagePhase::End, Some(status), false);
            Some(payload)
        }
        Err(_error) => {
            // The headers did arrive, so the status they carried is still the
            // one to record; what failed is the body, and the class names it.
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            {
                note_stage(traced, stage, StagePhase::Error, Some(status), _error.is_timeout());
                if let Some(traced) = traced {
                    traced.error_class(classify_transport_error(&_error));
                }
            }
            None
        }
    }
}

/// Which entry of the request-selected ring the transport actually used, as a
/// 1-based ordinal, and how many entries that ring had. Identity comes from the
/// pinned `Arc`s themselves - never the key text, a fingerprint or a header -
/// so the archive can say which rotation entry a 401/429 belonged to without
/// holding or exposing any key material. `None` when the chosen handle is not
/// an element of the ring it was handed with.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
pub(crate) fn key_ordinal(key: &Arc<SecretKey>, keys: &[Arc<SecretKey>]) -> Option<usize> {
    keys.iter()
        .position(|candidate| Arc::ptr_eq(candidate, key))
        .map(|index| index + 1)
}

/// One request stage observation: a request without a trace instruments
/// nothing, and a stage call is the only thing that goes to the sink.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_stage(
    traced: Option<&RequestTrace>,
    stage: Stage,
    phase: StagePhase,
    status: Option<u16>,
    timeout: bool,
) {
    if let Some(traced) = traced {
        traced.stage(stage, phase, status, timeout);
    }
}

/// One response body offered to the archive, in full: the status, the stage and
/// the exact body that came back. The stage is spelled by its own `Serialize`
/// implementation, so the record can never drift from the stage record and the
/// manifest. Nothing here truncates: the sink owns the queued-content budget,
/// and a record it cannot hold is a counted, diagnosable drop rather than a
/// shortened body. A body that is not text at all is not thrown away as a
/// length either: its valid UTF-8 spans are redacted with the request's ring,
/// the bytes are base64-encoded, and the record says which transport encoding
/// they carry and how many there were.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_body(
    traced: Option<&RequestTrace>,
    stage: Stage,
    status: u16,
    body: &[u8],
    keys: &[Arc<SecretKey>],
) {
    let Some(traced) = traced else {
        return;
    };
    let stage = json!(stage);
    match std::str::from_utf8(body) {
        Ok(text) => {
            traced.content(
            ContentKind::ProviderResponse,
            ContentFormat::Json,
            diagnostic_json(
                &json!({
                    "status": status,
                    "stage": stage,
                    "body": text,
                }),
                keys,
            ),
            );
        }
        Err(_) => {
            // Byte evidence is redacted before it is encoded and serialized
            // directly: the JSON redactor must not be handed an already-encoded
            // span, where a known key could match the base64 by coincidence.
            let safe = diagnostic_bytes(body, keys);
            traced.content(
                ContentKind::ProviderResponse,
                ContentFormat::Json,
                json!({
                    "status": status,
                    "stage": stage,
                    "encoding": "base64",
                    "data": base64::engine::general_purpose::STANDARD.encode(safe.as_ref()),
                    "bytes": body.len(),
                })
                .to_string(),
            );
        }
    }
}

/// One body the archive does not have at all: the read failed before any
/// complete copy existed. The record says exactly that - the stage and the
/// status it belonged to, and the actual `reqwest::Error` classed and rendered
/// with its URL stripped and every pinned key redacted - and the observation is
/// counted as lost. An empty body is never passed off as the upstream's answer.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn note_body_unreadable(
    traced: Option<&RequestTrace>,
    stage: Stage,
    status: u16,
    error: &reqwest::Error,
    keys: &[Arc<SecretKey>],
) {
    let Some(traced) = traced else {
        return;
    };
    traced.note_content_lost();
    traced.content(
        ContentKind::TransportError,
        ContentFormat::Json,
        diagnostic_json(
            &json!({
                "stage": json!(stage),
                "status": status,
                "body_incomplete": true,
                "reason": "body_unreadable",
                "error_class": classify_transport_error(error),
                "message": transport_detail(error, keys),
                "causes": error_causes(error, error.url().map(|url| url.as_str()), keys),
            }),
            keys,
        ),
    );
}
/// The cap one observed body is read up to: the sink's own content budget, so a
/// large Provider JSON body is still eligible for the archive rather than being
/// cut at some smaller diagnostic limit.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
const OBSERVED_BODY_MAX_BYTES: usize = crate::diagnostics::CONTENT_MAX_BYTES;

/// Reads at most [`OBSERVED_BODY_MAX_BYTES`] of one body, on a bounded loop of
/// its own. `true` means the body was longer than the cap and the read stopped
/// there; what came back is then incomplete and is never presented as the body.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
async fn read_body_capped(
    mut response: reqwest::Response,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > OBSERVED_BODY_MAX_BYTES {
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// Archives a body the transport path does not read, without ever making that
/// path wait for it: a diagnostics-only task owns the response, the trace and a
/// clone of the pinned ring, reads the body once, and records either the body
/// itself, an explicit incompleteness when it exceeded the cap, or the read
/// failure. The request has already ended semantically; an observation that
/// lands after it is timestamped as what it is.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn observe_body(
    response: reqwest::Response,
    stage: Stage,
    traced: &RequestTrace,
    keys: &[Arc<SecretKey>],
) {
    // The status comes off the response itself, so the record can never carry
    // one its caller guessed: the upstream's own is the only status archived.
    let status = response.status().as_u16();
    let traced = traced.clone();
    let keys = keys.to_vec();
    tokio::spawn(async move {
        match read_body_capped(response).await {
            Ok((body, false)) => note_body(Some(&traced), stage, status, &body, &keys),
            Ok((body, true)) => {
                // A partial body is not the body: the observation is counted as
                // lost and the record says what arrived, why there is no more,
                // and that the cap - not the upstream - is the reason. Passing
                // a fragment off as the whole answer is never an option.
                traced.note_content_lost();
                traced.content(
                    ContentKind::TransportError,
                    ContentFormat::Json,
                    diagnostic_json(
                        &json!({
                            "stage": json!(stage),
                            "status": status,
                            "body_incomplete": true,
                            "observed_bytes": body.len(),
                            "cap_bytes": OBSERVED_BODY_MAX_BYTES,
                            "reason": "body_exceeds_cap",
                        }),
                        &keys,
                    ),
                );
            }
            Err(error) => {
                // A body that could not be read at all is one more observation
                // the archive does not have.
                traced.note_content_lost();
                traced.content(
                    ContentKind::TransportError,
                    ContentFormat::Text,
                    transport_detail(&error, &keys),
                );
            }
        }
    });
}

/// The whole failure as text: the top message and every `std::error::Error::
/// source()` behind it, in order. A top-level rendering such as `error decoding
/// response body` hides the EOF or the timeout that actually happened; the
/// chain does not. The text is not sanitized here - the caller redacts it, and
/// the mask it applies covers every line the chain contributed.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
pub(crate) fn transport_chain_text(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str("\n  caused by: ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Every `source()` of one failure, each rendered and sanitized on its own, for
/// the structured record: the pairs a reader needs when the top message is the
/// only one that was rendered into `message`.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn error_causes(
    error: &dyn std::error::Error,
    url: Option<&str>,
    keys: &[Arc<SecretKey>],
) -> Vec<String> {
    let mut causes = Vec::new();
    let mut source = error.source();
    while let Some(cause) = source {
        let mut text = cause.to_string();
        if let Some(url) = url {
            text = text.replace(url, "[url]");
        }
        causes.push(diagnostic_text(&text, keys));
        source = cause.source();
    }
    causes
}

/// One transport failure in full, for the archive: the whole source chain, with
/// the request URL removed - that URL carries the key - and with every spelling
/// of every key of the pinned ring redacted. No URL, no header and no key is
/// ever part of it.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn transport_detail(error: &reqwest::Error, keys: &[Arc<SecretKey>]) -> String {
    let mut text = transport_chain_text(error);
    if let Some(url) = error.url() {
        text = text.replace(url.as_str(), "[url]");
    }
    diagnostic_text(&text, keys)
}

/// Which way a transport that never reached a status failed. The rendered
/// `reqwest::Error` is never read for this.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn classify_transport_error(error: &reqwest::Error) -> ErrorClass {
    if error.is_timeout() {
        ErrorClass::Timeout
    } else if error.is_connect() {
        ErrorClass::Connect
    } else if error.is_body() {
        ErrorClass::Body
    } else if error.is_decode() {
        ErrorClass::Decode
    } else {
        ErrorClass::Other
    }
}

/// Non-2xx from Google, carrying redacted diagnostics inside an authored reason.
///
/// The body is read exactly once, as it always was. A traced request also
/// archives that whole body - not the 300-character slice the browser shows -
/// together with the status and the stage it belongs to, and distinguishes a
/// body that was read from one whose read failed: only a complete body becomes
/// a `ProviderResponse`, while a failed read is counted as lost and recorded as
/// an incomplete observation. The browser mapping is unchanged either way.
async fn google_failure(
    response: reqwest::Response,
    action: MessageId,
    keys: &[Arc<SecretKey>],
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] traced: Option<&RequestTrace>,
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))] stage: Stage,
) -> ApiError {
    let google_status = response.status().as_u16();
    // The body is read exactly once. A read that failed produced nothing, and
    // the archive is told that explicitly instead of being handed an empty
    // string as if the upstream had answered with no body at all.
    let read = response.text().await;
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    match read.as_ref() {
        Ok(body) => note_body(traced, stage, google_status, body.as_bytes(), keys),
        Err(error) => note_body_unreadable(traced, stage, google_status, error, keys),
    }
    // The browser mapping is unchanged: an unreadable body is mapped exactly
    // like the empty one it always was, and never becomes a `ProviderResponse`
    // that claims the upstream answered with nothing.
    let body = read.as_deref().unwrap_or_default();
    google_failure_body(google_status, action, body, keys)
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
pub(crate) fn encode_uri_component(value: &str) -> String {
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
        let payload =
            interaction_payload("files/abc", Value::String("smart".to_owned()), &[], &[]);
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
        let payload = interaction_payload("files/abc", json!({ "type": "verbatim" }), &[], &[]);
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
            let payload = interaction_payload("files/abc", mode.clone(), &[], &[]);
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

    /// A non-empty vocabulary rides in the same `transcription_config` the mode
    /// uses, as the plain array of terms the reference documents; the terms keep
    /// the order the user listed them in.
    #[test]
    fn a_vocabulary_rides_in_the_transcription_config() {
        let payload = interaction_payload(
            "files/abc",
            Value::String("smart".to_owned()),
            &["Speechek".to_owned(), "BigQuery".to_owned()],
            &[],
        );
        assert_eq!(
            payload["generation_config"]["transcription_config"],
            json!({
                "mode": "smart",
                "language_codes": [],
                "custom_vocabulary": ["Speechek", "BigQuery"],
            })
        );
    }

    /// The dictation-language hints ride in `language_codes`, in the order the
    /// user chose them.
    #[test]
    fn the_dictation_languages_ride_in_language_codes() {
        let payload = interaction_payload(
            "files/abc",
            Value::String("smart".to_owned()),
            &[],
            &["ru-RU".to_owned(), "en-US".to_owned()],
        );
        assert_eq!(
            payload["generation_config"]["transcription_config"],
            json!({ "mode": "smart", "language_codes": ["ru-RU", "en-US"] })
        );
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

    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    use crate::diagnostics::{
        BatchStage, Diagnostics, Event, Meta, Mode as DiagMode, Outcome, Owner, Policy, Route,
        SessionHandle, SessionKey,
    };

    /// One open take of this test's own, with the trace its request records
    /// into. No writer, no directory, no environment: the events stay in the
    /// sink's queue for the test to read.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn sink() -> (Arc<Diagnostics>, RequestTrace) {
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "speechek-provider-stages-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let diagnostics = Diagnostics::for_test(
            &root,
            Policy {
                max_age: Duration::from_secs(600),
                max_bytes: 1_000_000,
            },
            std::time::SystemTime::UNIX_EPOCH,
        );
        let key = SessionKey {
            owner: Owner::Dictation,
            generation: 1,
        };
        let handle = diagnostics.begin(key, DiagMode::Live).expect("an open take");
        let trace = handle.request(Route::Live).expect("a request slot");
        (diagnostics, trace)
    }

    /// A one-shot loopback HTTP server: it reads one request, answers with the
    /// fixed `head`, then writes `body` after `body_delay`. Splitting the head
    /// from the body is what makes a stage's header/body boundary observable.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn head_then_body_server(head: String, body: String, body_delay: Duration) -> u16 {
        head_then_bytes_server(head, body.into_bytes(), body_delay)
    }

    /// The same one-shot server for a body that is not text at all: a body can
    /// carry octets no string can, so the fixture takes the bytes themselves.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn head_then_bytes_server(head: String, body: Vec<u8>, body_delay: Duration) -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Read the request head and its declared body before answering, so
            // the answer is not discarded by an abortive close.
            let mut received = Vec::new();
            let mut chunk = [0u8; 1024];
            let mut wanted = None;
            while received.len() < wanted.unwrap_or(usize::MAX) {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => received.extend_from_slice(&chunk[..n]),
                }
                if wanted.is_none() {
                    if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                        wanted = Some(end + 4 + declared_body_length(&received[..end]));
                    }
                }
            }
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
            std::thread::sleep(body_delay);
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        });
        port
    }

    /// The `content-length` of one request head: 0 when it declares none.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn declared_body_length(head: &[u8]) -> usize {
        String::from_utf8_lossy(head)
            .to_ascii_lowercase()
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Every stage record the sink has queued, drained.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn stages(diagnostics: &Diagnostics) -> Vec<BatchStage> {
        diagnostics
            .take_meta(64)
            .into_iter()
            .filter_map(|pending| match pending.meta {
                Meta::Event(Event::BatchStage(stage)) => Some(stage),
                _ => None,
            })
            .collect()
    }

    /// The request's one terminal record, drained.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn request_end(diagnostics: &Diagnostics) -> Option<crate::diagnostics::RequestEnd> {
        diagnostics
            .take_meta(64)
            .into_iter()
            .find_map(|pending| match pending.meta {
                Meta::Event(Event::RequestEnd(end)) => Some(end),
                _ => None,
            })
    }

    /// A 200 whose body then fails is not a fast, header-only success: the
    /// stage stays open through the body, and the actual `reqwest::Error` is
    /// classified instead of being discarded.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn a_body_failure_ends_the_stage_at_the_body_not_at_its_headers() {
        let port = head_then_body_server(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4\r\n\r\n"
                .to_string(),
            "{\"a\"".to_string(),
            Duration::from_millis(200),
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, trace) = sink();
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/v1beta/interactions"));
        let response = send_checked(
            request,
            MessageId::GoogleTranscriptionFailed,
            &[],
            Duration::from_secs(5),
            Some(&trace),
            Stage::Interaction,
        )
        .await
        .expect("the headers arrive");

        // The headers are in and the stage is still open.
        let open = stages(&diagnostics);
        assert_eq!(open.len(), 1, "the stage is open at its headers: {open:?}");
        assert!(matches!(open[0].phase, StagePhase::Begin));

        // The body lands 200 ms later: consuming it is what ends the stage.
        let began = std::time::Instant::now();
        let payload = read_stage_body(response, Some(&trace), Stage::Interaction, &[]).await;
        assert!(payload.is_none(), "a broken body is no payload");
        assert!(
            began.elapsed() >= Duration::from_millis(150),
            "the body really was awaited"
        );

        // The request really ended first, so its terminal is in the same drain
        // as everything else it produced: one `take_meta`, one set of records.
        trace.finish(Outcome::Error);
        let drained = diagnostics.take_meta(64);
        let ended: Vec<_> = drained
            .iter()
            .filter_map(|pending| match &pending.meta {
                Meta::Event(Event::BatchStage(stage)) => Some(stage),
                _ => None,
            })
            .collect();
        let content: Vec<_> = drained
            .iter()
            .filter_map(|pending| match &pending.meta {
                Meta::Content(queued) => Some(&queued.record),
                _ => None,
            })
            .collect();
        let end = drained
            .iter()
            .find_map(|pending| match &pending.meta {
                Meta::Event(Event::RequestEnd(end)) => Some(end),
                _ => None,
            })
            .expect("the request end, in the same drain");
        let interaction = ended
            .iter()
            .find(|stage| {
                matches!(stage.stage, Stage::Interaction)
                    && !matches!(stage.phase, StagePhase::Begin)
            })
            .expect("the consumed stage ends");
        assert!(matches!(interaction.phase, StagePhase::Error));
        assert_eq!(interaction.status, Some(200));
        assert!(!interaction.timeout);
        assert!(interaction.elapsed_us.is_some());

        // The body arrived *complete* and simply was not JSON: the archive
        // keeps it whole, and none of it is a lost observation. `note_body`
        // wraps the raw bytes in the record's own envelope, so the archived
        // body is read back out of that envelope and compared exactly.
        let answers: Vec<String> = content
            .iter()
            .filter(|record| matches!(record.kind, ContentKind::ProviderResponse))
            .map(|record| {
                let envelope: Value =
                    serde_json::from_str(&record.body).expect("the record's own envelope");
                assert_eq!(envelope["stage"], json!("interaction"));
                assert_eq!(envelope["status"], json!(200));
                envelope["body"]
                    .as_str()
                    .expect("the raw body inside the envelope")
                    .to_owned()
            })
            .collect();
        assert_eq!(
            answers,
            vec!["{\"a\"".to_owned()],
            "the complete body is archived exactly as it arrived"
        );
        assert!(
            !content
                .iter()
                .any(|record| matches!(record.kind, ContentKind::TransportError)),
            "a decode failure is not a lost observation: {content:?}"
        );

        assert_eq!(end.error_class, Some(ErrorClass::Decode));
        assert_eq!(end.status, Some(200), "the headers' status still reaches the request end");
        assert_eq!(end.outcome, Outcome::Error);
    }

    /// A sink with its real writer, and one open Smart take in it: the journal
    /// is the evidence here, so the records have to come from the writer rather
    /// from the producer's queue. The handle and the key come back with the trace,
    /// because a take is only written out once it has been ended explicitly - the
    /// writer keeps servicing a take that has no terminal, and a take nobody ended
    /// never reaches its `session_end`.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn storage_sink(
        root: &std::path::Path,
    ) -> (
        Arc<Diagnostics>,
        SessionHandle,
        SessionKey,
        RequestTrace,
    ) {
        let diagnostics = Diagnostics::for_test_with_storage(
            root,
            Policy {
                max_age: Duration::from_secs(600),
                max_bytes: 100_000_000,
            },
            std::time::SystemTime::now(),
        );
        let key = SessionKey {
            owner: Owner::Dictation,
            generation: 1,
        };
        let handle = diagnostics.begin(key, DiagMode::Smart).expect("an open take");
        let trace = handle.request(Route::Smart).expect("a request slot");
        (diagnostics, handle, key, trace)
    }

    /// Ends one take the way the shell ends it - the capture terminal, then the
    /// take's own - and lets the writer seal and write it out. The fake clock is
    /// advanced past the seal grace with the writer running, so the take is not
    /// still waiting for a request the test has already dropped.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn end_take(
        sink: &Arc<Diagnostics>,
        handle: &SessionHandle,
        key: SessionKey,
        outcome: Outcome,
    ) {
        handle.end_capture(outcome);
        sink.end(key, outcome);
        // The longest a sealed candidate waits for a request to end is the sink's
        // own seal grace; the extra millisecond is what makes it expired, not equal.
        // Several writer passes, so the take is sealed, written and closed out.
        sink.advance_clock(Duration::from_secs(2) + Duration::from_millis(1));
        sink.worker_resume();
        sink.worker_passes(6);
    }

    /// Every journal record written under `root`, the take's own directory
    /// included, oldest segment first.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn journal_records(root: &std::path::Path) -> Vec<Value> {
        let mut segments = Vec::new();
        let Ok(takes) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        for take in takes.flatten() {
            if !take.path().is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(take.path()) else {
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

    /// One journal record of the given event name.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn journal_event<'a>(records: &'a [Value], event: &str) -> Option<&'a Value> {
        records.iter().find(|record| record["event"] == event)
    }

    /// A truncated non-2xx body: the upstream declared more bytes than it
    /// delivered and then closed. The read fails, so no complete copy exists -
    /// the browser keeps the mapping an unreadable body always had, the archive
    /// records the failure with its stage, status, class and source chain, no
    /// `ProviderResponse` claims Google answered with nothing, and the lost
    /// observation is counted.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn a_truncated_non_2xx_body_is_never_archived_as_a_complete_one() {
        let root = std::env::temp_dir().join(format!(
            "speechek-provider-truncated-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0)
        ));
        let delivered = "{\"error\":{\"message\":\"gone\"}}";
        let port = head_then_body_server(
            format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                delivered.len() + 64
            ),
            delivered.to_string(),
            Duration::ZERO,
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, handle, key, trace) = storage_sink(&root);
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/v1beta/interactions"));
        let failure = send_checked(
            request,
            MessageId::GoogleTranscriptionFailed,
            &[],
            Duration::from_secs(5),
            Some(&trace),
            Stage::Interaction,
        )
        .await
        .expect_err("a 401 is a failure");
        // The browser contract is exactly the one an unreadable body always had:
        // a 401 still maps to the auth failure, its code and its own wording.
        assert_eq!(failure.status, 502, "401 still maps to the auth failure");
        assert_eq!(failure.code, "AUTH_FAILED");
        assert_eq!(
            failure.ui.key,
            MessageId::GoogleTranscriptionAuthRejected,
            "the auth wording, unchanged by the failed read"
        );

        trace.finish(Outcome::Error);
        // The request's last clone goes with the trace, and the take is ended
        // the way the shell ends it: only then does the writer seal it and
        // write the `session_end` this test reads its lost count from. Reading
        // the journal before that would see a take that is still open.
        drop(trace);
        end_take(&diagnostics, &handle, key, Outcome::Error);
        let records = journal_records(&root);
        let content: Vec<&Value> = records
            .iter()
            .filter(|record| record["event"] == "content")
            .collect();
        assert!(
            content
                .iter()
                .all(|record| record["data"]["kind"] != "provider_response"),
            "no response record claims a body that was never read: {content:?}"
        );
        let incomplete: Vec<&Value> = content
            .iter()
            .filter(|record| record["data"]["kind"] == "transport_error")
            .copied()
            .collect();
        assert_eq!(
            incomplete.len(),
            1,
            "one incomplete observation, one record: {incomplete:?}"
        );
        let body = incomplete[0]["data"]["body"]
            .as_str()
            .expect("the observation's body");
        assert!(body.contains("\"body_incomplete\":true"), "{body}");
        assert!(body.contains("\"reason\":\"body_unreadable\""), "{body}");
        assert!(body.contains("\"stage\":\"interaction\""), "{body}");
        assert!(
            body.contains("\"status\":401"),
            "the upstream's own status, not the mapped 502: {body}"
        );
        assert!(
            body.contains("\"causes\":"),
            "the source chain is recorded beside the message: {body}"
        );
        // The request's own terminal, read from the same one set of records:
        // the failed read is a failed request, not a silent success.
        let terminal = journal_event(&records, "request_end").expect("the request's terminal");
        assert_eq!(terminal["data"]["outcome"], json!("error"), "{terminal}");
        // And the lost observation really was counted, at the take's own end.
        let end = journal_event(&records, "session_end").expect("the take's own end");
        assert!(
            end["data"]["dropped_events"].as_u64().unwrap_or(0) >= 1,
            "the lost observation is counted: {end}"
        );
        // The archive was proven first; only now is the sink stopped, and only
        // then is the root this test owns removed.
        diagnostics.shutdown(Duration::from_millis(500));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A rejected upload records the status Google really sent; the browser's
    /// own mapping stays the 502 it always was.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn a_failed_google_status_is_recorded_before_the_browser_mapping() {
        let port = head_then_body_server(
            "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: 0\r\n\r\n"
                .to_string(),
            String::new(),
            Duration::ZERO,
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, trace) = sink();
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/upload/v1beta/files"));
        let failure = send_checked(
            request,
            MessageId::GoogleUploadFailed,
            &[],
            Duration::from_secs(5),
            Some(&trace),
            Stage::UploadFinalize,
        )
        .await
        .expect_err("a 503 is a failure");

        // The browser mapping is exactly what it was.
        assert_eq!(failure.status, 502);
        assert_eq!(failure.code, "UPSTREAM_ERROR");

        let ended = stages(&diagnostics);
        let finalize = ended
            .iter()
            .find(|stage| {
                matches!(stage.stage, Stage::UploadFinalize)
                    && !matches!(stage.phase, StagePhase::Begin)
            })
            .expect("the failed stage");
        assert!(matches!(finalize.phase, StagePhase::Error));
        assert_eq!(
            finalize.status,
            Some(503),
            "the upstream's own status, not the mapped 502"
        );
        assert!(!finalize.timeout);

        trace.finish(Outcome::Error);
        let end = request_end(&diagnostics).expect("the request end");
        assert_eq!(end.status, Some(503));
        assert_eq!(end.outcome, Outcome::Error);
    }

    /// A successful body-bearing stage ends once its body has been read, and
    /// the payload is the one the previous `.ok()` returned.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn a_successful_finalize_stage_ends_when_its_body_is_read() {
        let body = "{\"file\":{\"name\":\"files/abc\",\"uri\":\"files/abc\",\"state\":\"ACTIVE\"}}";
        let port = head_then_body_server(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            ),
            body.to_string(),
            Duration::ZERO,
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, trace) = sink();
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/upload/v1beta/files"));
        let response = send_checked(
            request,
            MessageId::GoogleUploadFailed,
            &[],
            Duration::from_secs(5),
            Some(&trace),
            Stage::UploadFinalize,
        )
        .await
        .expect("a 200");

        let open = stages(&diagnostics);
        assert_eq!(open.len(), 1, "the stage is still open at the headers: {open:?}");
        assert!(matches!(open[0].phase, StagePhase::Begin));

        let payload = read_stage_body(response, Some(&trace), Stage::UploadFinalize, &[])
            .await
            .expect("the payload");
        assert_eq!(payload["file"]["name"], "files/abc");

        let ended = stages(&diagnostics);
        let end = ended
            .iter()
            .find(|stage| {
                matches!(stage.stage, Stage::UploadFinalize)
                    && matches!(stage.phase, StagePhase::End)
            })
            .expect("the stage ends at its body");
        assert_eq!(end.status, Some(200));
        assert!(end.elapsed_us.is_some());
    }

    /// The synthetic keys these tests echo into a body: constants of the test,
    /// not secrets of any account, and exactly what the redaction assertions
    /// look for. `SYNTHETIC_RING_KEY` also has a percent-encoded spelling of
    /// its own, so a body cannot hide it behind `encodeURIComponent`.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    const SYNTHETIC_KEY: &str = "speechek-synthetic-key-9f3b7c1d";
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    const SYNTHETIC_RING_KEY: &str = "speechek/ring+key=2nd";

    /// Every content record the sink has queued, drained in order.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn contents(diagnostics: &Diagnostics) -> Vec<crate::diagnostics::ContentRecord> {
        diagnostics
            .take_meta(64)
            .into_iter()
            .filter_map(|pending| match pending.meta {
                Meta::Content(content) => Some(content.record),
                _ => None,
            })
            .collect()
    }

    /// The body of the archived provider response.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn archived_body(diagnostics: &Diagnostics) -> String {
        contents(diagnostics)
            .into_iter()
            .find(|record| matches!(record.kind, ContentKind::ProviderResponse))
            .expect("the response body is archived")
            .body
    }

    /// A rejected stage keeps the whole body it answered with - past the
    /// 300-character slice the browser is shown - with the status Google really
    /// sent, and a key echoed inside that body is redacted rather than quoted.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn a_rejected_body_is_archived_in_full_with_its_actual_status() {
        let key = Arc::new(SecretKey::new(SYNTHETIC_KEY.to_string()));
        let ringed = Arc::new(SecretKey::new(SYNTHETIC_RING_KEY.to_string()));
        let encoded = encode_uri_component(SYNTHETIC_RING_KEY);
        assert_ne!(
            encoded, SYNTHETIC_RING_KEY,
            "the second key really has a distinct encoded spelling"
        );
        // Well past the 300 characters the browser-facing detail is cut to.
        let tail = "x".repeat(400);
        let body = json!({
            "error": {
                "message": format!(
                    "rejected {SYNTHETIC_KEY} {SYNTHETIC_RING_KEY} {encoded} {tail}"
                )
            }
        })
        .to_string();
        let port = head_then_body_server(
            format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            ),
            body,
            Duration::ZERO,
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, trace) = sink();
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/v1beta/interactions"));
        let failure = send_checked(
            request,
            MessageId::GoogleTranscriptionFailed,
            &[Arc::clone(&key), Arc::clone(&ringed)],
            Duration::from_secs(5),
            Some(&trace),
            Stage::Interaction,
        )
        .await
        .expect_err("a 400 is a failure");

        // The browser keeps exactly the redacted slice it always did.
        assert_eq!(failure.status, 400);
        assert!(!failure.message.contains(SYNTHETIC_KEY));
        assert!(!failure.message.contains(SYNTHETIC_RING_KEY));
        assert!(!failure.message.contains(&tail));

        let archived = archived_body(&diagnostics);
        assert!(
            archived.contains("\"status\":400"),
            "the status Google really sent: {archived}"
        );
        assert!(
            archived.contains("\"stage\":\"interaction\""),
            "the stage the body belongs to: {archived}"
        );
        assert!(
            archived.contains(&tail),
            "the whole body, not the 300-character detail: {archived}"
        );
        assert!(
            !archived.contains(SYNTHETIC_KEY),
            "a key echoed by the body is redacted: {archived}"
        );
        assert!(
            !archived.contains(SYNTHETIC_RING_KEY),
            "and so is a second key of the pinned ring: {archived}"
        );
        assert!(
            !archived.contains(&encoded),
            "the percent-encoded spelling of a key is redacted too: {archived}"
        );
    }

    /// A long body that only holds unicode text is archived without a single
    /// character being lost.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn a_long_unicode_body_is_archived_whole() {
        let text = "привет-🌍-транскрипт-√".repeat(20);
        let body = json!({ "output_text": text.clone() }).to_string();
        let port = head_then_body_server(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            ),
            body,
            Duration::ZERO,
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, trace) = sink();
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/v1beta/interactions"));
        let response = send_checked(
            request,
            MessageId::GoogleTranscriptionFailed,
            &[],
            Duration::from_secs(5),
            Some(&trace),
            Stage::Interaction,
        )
        .await
        .expect("a 200");
        let payload = read_stage_body(response, Some(&trace), Stage::Interaction, &[])
            .await
            .expect("the payload");
        assert_eq!(extract_interaction_text(&payload), text);

        let archived = archived_body(&diagnostics);
        assert!(
            archived.contains(&text),
            "every character of the body is archived: {archived}"
        );
        assert!(
            archived.contains("\"status\":200"),
            "the stage's own status: {archived}"
        );
    }

    /// A rejected stage whose body is larger than the renderer record limit is
    /// still archived whole, with the status that came with it: the error path
    /// has no cut-off of its own.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn a_large_ignored_body_is_archived_with_its_actual_status() {
        let filler = "y".repeat(crate::diagnostics::RENDERER_CONTENT_MAX_BYTES + 4096);
        let body = json!({ "error": { "message": filler.clone() } }).to_string();
        let port = head_then_body_server(
            format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            ),
            body,
            Duration::ZERO,
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, trace) = sink();
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/upload/v1beta/files"));
        let failure = send_checked(
            request,
            MessageId::GoogleUploadStartFailed,
            &[],
            Duration::from_secs(5),
            Some(&trace),
            Stage::UploadStart,
        )
        .await
        .expect_err("a 400 is a failure");
        assert_eq!(failure.status, 400, "the browser keeps its own mapping");
        // The whole error body is archived as it arrived, with the status
        // Google really sent - not the browser's mapping and not a slice.
        let archived = contents(&diagnostics)
            .into_iter()
            .find(|record| matches!(record.kind, ContentKind::ProviderResponse))
            .expect("the error body is archived");
        assert!(
            archived.body.contains("\"status\":400"),
            "the status that came with the body"
        );
        assert!(
            archived.body.contains("\"stage\":\"upload_start\""),
            "the stage the body belongs to"
        );
        assert!(
            archived.body.contains(&filler),
            "a body past the renderer record limit is archived whole: {} bytes",
            archived.body.len()
        );
    }

    /// Handing an ignored body to the archive never waits for it: the caller
    /// returns while the body is still in flight, and the body - or the read
    /// failure - is recorded afterwards.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn an_ignored_body_still_in_flight_never_blocks_its_caller() {
        // Larger than the renderer record limit, so this also pins the
        // observer's own cap at the sink's content budget.
        let filler = "z".repeat(crate::diagnostics::RENDERER_CONTENT_MAX_BYTES + 4096);
        let body = json!({ "file": { "name": "files/abc", "pad": filler } }).to_string();
        let port = head_then_body_server(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            ),
            body,
            Duration::from_millis(1500),
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, trace) = sink();
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/upload/v1beta/files"));
        let response = send_checked(
            request,
            MessageId::GoogleUploadStartFailed,
            &[],
            Duration::from_secs(5),
            Some(&trace),
            Stage::UploadStart,
        )
        .await
        .expect("the headers arrive");

        let began = std::time::Instant::now();
        observe_body(response, Stage::UploadStart, &trace, &[]);
        assert!(
            began.elapsed() < Duration::from_millis(500),
            "the caller does not wait for a body that is still in flight"
        );

        // The body lands long after the call returned; the observer is what
        // picks it up, and the wait here is only the test's.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let archived = contents(&diagnostics)
            .into_iter()
            .find(|record| record.body.contains("\"stage\":\"upload_start\""))
            .expect("the late body is archived");
        assert!(matches!(archived.kind, ContentKind::ProviderResponse));
        assert!(archived.body.contains("files/abc"));
        assert!(archived.body.contains("\"status\":200"));
        assert!(
            archived.body.len() > crate::diagnostics::RENDERER_CONTENT_MAX_BYTES,
            "the observer's cap is the sink's content budget, not the renderer limit"
        );
    }

    /// A body past the observer's cap cannot be the archive's copy of the
    /// answer: the read stops, the observation is counted as lost, and the
    /// record says what arrived, why there is no more and that the cap - not
    /// the upstream - is the reason. A fragment is never passed off as the body.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn a_body_past_the_observer_cap_is_never_passed_off_as_the_body() {
        let body = "z".repeat(crate::diagnostics::CONTENT_MAX_BYTES + 4096);
        let port = head_then_body_server(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            ),
            body,
            Duration::ZERO,
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, trace) = sink();
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/upload/v1beta/files"));
        let response = send_checked(
            request,
            MessageId::GoogleUploadStartFailed,
            &[],
            Duration::from_secs(5),
            Some(&trace),
            Stage::UploadStart,
        )
        .await
        .expect("the headers arrive");
        observe_body(response, Stage::UploadStart, &trace, &[]);

        // The observer's record lands after the caller returned; the wait here
        // is only the test's.
        let mut archived = None;
        for _ in 0..400 {
            if let Some(record) = contents(&diagnostics)
                .into_iter()
                .find(|record| matches!(record.kind, ContentKind::TransportError))
            {
                archived = Some(record);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let archived = archived.expect("the incomplete observation is archived");
        assert!(archived.body.contains("\"body_incomplete\":true"));
        assert!(archived.body.contains("\"reason\":\"body_exceeds_cap\""));
        assert!(archived.body.contains("\"stage\":\"upload_start\""));
        assert!(archived.body.contains("\"status\":200"));
        assert!(archived.body.contains("\"observed_bytes\":"));
        assert!(
            !archived.body.contains("zzzz"),
            "no fragment of the body is presented as the body: {archived:?}"
        );
    }

    /// A body that is not UTF-8 at all is still archived as itself: its valid
    /// UTF-8 spans are redacted, the bytes are carried base64 with their own
    /// length, the octets that are not text survive exactly, and the
    /// recognition path is untouched - the payload still comes back `None`
    /// with the decode class, exactly as before.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    #[tokio::test]
    async fn a_non_utf8_body_is_archived_as_its_exact_bytes() {
        // The request's own pinned ring: with a key in it, the redaction of the
        // body's valid UTF-8 span is really exercised, exactly as it is in a
        // real request.
        let keys = vec![Arc::new(SecretKey::new(SYNTHETIC_KEY.to_owned()))];
        let mut raw = format!("{{\"error\":\"{SYNTHETIC_KEY}\"").into_bytes();
        raw.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x9F]);
        let port = head_then_bytes_server(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                raw.len()
            ),
            raw.clone(),
            Duration::ZERO,
        );
        let provider = Provider::for_test("http://127.0.0.1:1", "ws://127.0.0.1:1");
        let (diagnostics, trace) = sink();
        let request = provider
            .http
            .post(format!("http://127.0.0.1:{port}/v1beta/interactions"));
        let response = send_checked(
            request,
            MessageId::GoogleTranscriptionFailed,
            &keys,
            Duration::from_secs(5),
            Some(&trace),
            Stage::Interaction,
        )
        .await
        .expect("the headers arrive");
        // The recognition path is exactly what it was: no payload, decode class.
        let payload = read_stage_body(response, Some(&trace), Stage::Interaction, &keys)
            .await;
        assert!(payload.is_none(), "non-UTF-8 is not a payload");

        // The request really ended before the one drain this test makes: the
        // records, the byte evidence and the terminal all come from it.
        trace.finish(Outcome::Error);
        let drained = diagnostics.take_meta(64);
        let content: Vec<_> = drained
            .iter()
            .filter_map(|pending| match &pending.meta {
                Meta::Content(queued) => Some(&queued.record),
                _ => None,
            })
            .collect();
        let end = drained
            .iter()
            .find_map(|pending| match &pending.meta {
                Meta::Event(Event::RequestEnd(end)) => Some(end),
                _ => None,
            })
            .expect("the request end, in the same drain");
        let archived = content
            .iter()
            .find(|record| matches!(record.kind, ContentKind::ProviderResponse))
            .expect("the byte evidence is archived");
        let envelope: Value =
            serde_json::from_str(&archived.body).expect("the record's own envelope");
        assert_eq!(envelope["encoding"], json!("base64"));
        assert_eq!(envelope["status"], json!(200));
        assert_eq!(envelope["stage"], json!("interaction"));
        assert_eq!(envelope["bytes"], json!(raw.len()));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(envelope["data"].as_str().expect("the encoded bytes"))
            .expect("valid base64");
        // Byte for byte: the redacted prefix and the exact non-text tail. A
        // lossy comparison could hide a changed octet, so the whole buffer is
        // compared as bytes.
        let mut expected = format!("{{\"error\":\"[redacted]\"").into_bytes();
        expected.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x9F]);
        assert_eq!(
            decoded, expected,
            "the exact bytes arrive, with only the key masked"
        );
        assert_eq!(
            &decoded[decoded.len() - 4..],
            &[0xFF, 0xFE, 0x00, 0x9F],
            "the octets that are not text survive exactly"
        );
        assert!(
            !String::from_utf8_lossy(&decoded).contains(SYNTHETIC_KEY),
            "the key is not anywhere in the archived bytes"
        );
        // A complete body that is not text is not a lost observation, and the
        // recognition path is exactly what it was.
        assert!(
            !content
                .iter()
                .any(|record| matches!(record.kind, ContentKind::TransportError)),
            "a complete body is not content loss: {content:?}"
        );
        assert_eq!(end.error_class, Some(ErrorClass::Decode));
        assert_eq!(end.status, Some(200));
    }
}
