//! Full Development evidence, separate from the small Copy counter vocabulary.
//! Payloads are already redacted at their provider/renderer/native boundary.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use super::Route;

/// One shared byte budget supplements the existing bounded record count.
const QUEUED_CONTENT_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const CONTENT_MAX_BYTES: usize = QUEUED_CONTENT_BYTES as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    RuntimeContext,
    CaptureContext,
    LiveSetup,
    LiveMessage,
    LiveOutput,
    ProviderRequest,
    ProviderResponse,
    TransportError,
    SocketClose,
    RecorderResult,
    RecorderError,
    PasteRequested,
    PasteResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentFormat {
    Json,
    Text,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentRecord {
    pub kind: ContentKind,
    pub route: Option<Route>,
    pub format: ContentFormat,
    pub body: String,
    pub observed_utc_ms: u64,
    /// Relative to this take, unlike the envelope's process-relative clock.
    pub observed_elapsed_us: u64,
}

#[derive(Debug, Default)]
pub(super) struct ContentBudget {
    used: AtomicU64,
}

impl ContentBudget {
    pub(super) fn claim(self: &Arc<Self>, bytes: u64) -> Option<ContentLease> {
        if bytes == 0 {
            return Some(ContentLease { _charge: None });
        }
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= QUEUED_CONTENT_BYTES)
            })
            .ok()?;
        Some(ContentLease { _charge: Some(Arc::new(Charge {
            budget: Arc::clone(self),
            bytes,
        })) })
    }
}

#[derive(Debug)]
struct Charge {
    budget: Arc<ContentBudget>,
    bytes: u64,
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Cloned test observations share the charge; the last owner releases it.
#[derive(Clone, Debug)]
pub(super) struct ContentLease {
    _charge: Option<Arc<Charge>>,
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedContent {
    pub(crate) record: ContentRecord,
    pub(super) _lease: ContentLease,
}

/// Conservative owned-payload accounting without serializing a second copy.
/// Strings include worst-case JSON escaping; Value nodes include their storage.
pub(super) fn value_bytes(value: &Value) -> u64 {
    let node = std::mem::size_of::<Value>() as u64;
    match value {
        Value::String(text) => node.saturating_add((text.len() as u64).saturating_mul(6)),
        Value::Array(values) => values.iter().fold(node, |sum, value| sum.saturating_add(value_bytes(value))),
        Value::Object(values) => values.iter().fold(node, |sum, (key, value)| {
            sum.saturating_add((key.len() as u64).saturating_mul(6))
                .saturating_add(value_bytes(value))
        }),
        _ => node,
    }
}

pub(super) fn valid_result(result: &Value) -> bool {
    let Some(object) = result.as_object() else { return false; };
    if object.keys().any(|key| !matches!(key.as_str(), "results" | "errors" | "errorDetails")) {
        return false;
    }
    for name in ["results", "errors", "errorDetails"] {
        let Some(map) = object.get(name).and_then(Value::as_object) else { return false; };
        for (mode, value) in map {
            if !matches!(mode.as_str(), "live" | "smart" | "verbatim" | "") {
                return false;
            }
            if name != "errorDetails" && !value.is_string() {
                return false;
            }
            if name == "errorDetails" && !value.is_object() {
                return false;
            }
        }
    }
    true
}

pub(super) fn valid_error(error: &Value) -> bool {
    let Some(object) = error.as_object() else { return false; };
    if object.keys().any(|key| !matches!(key.as_str(), "mode" | "message" | "ui" | "diagnostic" | "atMs")) {
        return false;
    }
    object.get("mode").and_then(Value::as_str)
        .is_some_and(|mode| matches!(mode, "live" | "smart" | "verbatim" | ""))
        && object.get("message").is_some_and(Value::is_string)
        && object.get("ui").is_none_or(|ui| ui.is_null() || ui.is_object())
        && object.get("diagnostic").is_none_or(|value| value.is_null() || value.is_string())
        && object.get("atMs").is_none_or(|value| value.as_f64().is_some_and(|clock| clock.is_finite() && clock >= 0.0))
}
