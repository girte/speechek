//! The recording history: the last few dictations, kept for the current
//! Windows user alone.
//!
//! A dictation is captured twice for this feature. The same PCM16 frames the
//! overlay receives are appended to a [`Take`], and the text the shell finally
//! hands to `paste.rs` is noted on that take. When the dictation ends -
//! successfully or not - the take becomes one entry: the whole take as a FLAC
//! stream and the text as a small JSON document, each sealed with DPAPI for the
//! current user and written through the same atomic replacement the settings
//! document uses. Nothing leaves the machine, and another account cannot read
//! either file.
//!
//! Two files make one entry, and the entry file is the commit point: the audio
//! is written first and the metadata last, so an entry a reader can see always
//! has its audio. An audio file without its entry is not an entry at all, and
//! the next listing removes it.
//!
//! [`HistoryStore`] is pure: it knows a directory, the two file formats and
//! nothing about the shell, so it can be tested on a temporary directory.
//! [`History`] is the managed state the shell talks to. It holds the takes of
//! the dictations that are still running, serializes every store operation
//! behind one lock, and remembers why the last write failed so the settings
//! window can say so instead of showing a history that quietly lost a take.
//!
//! The settings window owns the surface: it lists the entries, plays an entry's
//! audio, re-runs recognition over it in the configured mode - the shared Live
//! relay for `live`, the batch API for `smart` and `verbatim` - exports the
//! decrypted FLAC through the native save dialog, deletes one entry or clears
//! the history, and lowers the limit the store prunes to.
//!
//! A take is never stored when the dictation was cancelled (Escape), when it
//! was a laboratory take, when it is shorter than [`MIN_SAMPLES`], or when the
//! only thing that went wrong is that it was too short to recognize. A
//! dictation that produced no text at all is stored as not recognized, so it
//! can be retried once the network is back.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter as _, Manager as _, WebviewWindow};
use zeroize::Zeroizing;

use crate::flac::{self, FlacError};
use crate::i18n::{self, MessageId, UiMessage};
use crate::preferences::{self, PreferencesState, SettingsUiError};
use crate::provider::Provider;
use crate::secrets::{self, SecretKey};
use crate::settings::RuntimeSnapshot;

/* -------------------------------------------------------------------------- */
/* Names, sizes and formats                                                   */
/* -------------------------------------------------------------------------- */

/// Sent to the settings window whenever the stored entries change: after a
/// dictation was saved, after a retry rewrote one text, after a delete, a
/// clear, or a prune. Carries no data - the page asks for the list itself.
const EVENT_HISTORY_CHANGED: &str = "speechek:history-changed";

/// The first four bytes of `<id>.audio`, before the sealed FLAC stream.
const AUDIO_MAGIC: &[u8; 4] = b"SPA1";
/// The first four bytes of `<id>.entry`, before the sealed JSON document.
const ENTRY_MAGIC: &[u8; 4] = b"SPH1";

/// The version of the entry document this build writes and accepts.
const ENTRY_VERSION: u32 = 1;

/// Bytes of one PCM16 sample.
const BYTES_PER_SAMPLE: usize = 2;
/// Bytes of one millisecond of 16 kHz mono PCM16.
const BYTES_PER_MS: usize = 32;

/// Takes shorter than this - 100 ms - are not worth keeping: they hold no word
/// and only clutter the list.
const MIN_SAMPLES: usize = 1_600;

/// Bytes of one second of 16 kHz mono PCM16.
const BYTES_PER_SECOND: usize = 32_000;

/// The ceiling one take keeps in memory: ten minutes of 16 kHz mono PCM16, plus
/// a second of slack. A dictation that runs longer is still recognized, but the
/// history keeps its first ten minutes.
const MAX_PCM_BYTES: usize = 600 * BYTES_PER_SECOND + BYTES_PER_SECOND;

/* -------------------------------------------------------------------------- */
/* Failures                                                                   */
/* -------------------------------------------------------------------------- */

/// Why a store operation did not happen.
///
/// The variants are the three things that can really go wrong - the disk, the
/// current user's DPAPI, the codec - plus the two "the files are not what this
/// build wrote" cases a reader has to tell apart: a missing entry and a
/// damaged one.
#[derive(Debug)]
pub(crate) enum HistoryError {
    /// The file could not be read, written or removed.
    Io(io::Error),
    /// DPAPI refused to seal or open the bytes for this user.
    Crypto(windows::core::Error),
    /// The FLAC stream could not be encoded or decoded.
    Codec(FlacError),
    /// The files are not the layout this build writes.
    Format,
    /// The entry does not exist.
    Missing,
}

impl From<io::Error> for HistoryError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::Missing
        } else {
            Self::Io(error)
        }
    }
}

impl From<windows::core::Error> for HistoryError {
    fn from(error: windows::core::Error) -> Self {
        Self::Crypto(error)
    }
}

impl From<FlacError> for HistoryError {
    fn from(error: FlacError) -> Self {
        Self::Codec(error)
    }
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Crypto(error) => error.fmt(formatter),
            Self::Codec(error) => write!(formatter, "FLAC {error:?}"),
            Self::Format => formatter.write_str("not a history file this build wrote"),
            Self::Missing => formatter.write_str("the entry does not exist"),
        }
    }
}

impl HistoryError {
    /// What the settings window shows for this failure.
    ///
    /// A missing entry and a damaged file are told apart, because only one of
    /// them means "reload the list"; everything the current user's own profile
    /// cannot read is reported as a damaged recording rather than as a lost
    /// one, and a disk failure names the history as a whole.
    fn ui(&self) -> UiMessage {
        UiMessage::new(match self {
            Self::Missing => MessageId::HistoryEntryMissing,
            Self::Format | Self::Crypto(_) | Self::Codec(_) => MessageId::HistoryAudioDamaged,
            Self::Io(_) => MessageId::HistoryStorageFailed,
        })
    }

    /// The machine-readable code that goes with [`Self::ui`].
    fn code(&self) -> &'static str {
        match self {
            Self::Missing => "HISTORY_ENTRY_MISSING",
            Self::Format | Self::Crypto(_) | Self::Codec(_) => "HISTORY_AUDIO_DAMAGED",
            Self::Io(_) => "HISTORY_STORAGE_FAILED",
        }
    }

    /// The same failure as a settings-command error.
    fn command(self) -> SettingsUiError {
        SettingsUiError::new(self.code(), self.ui())
    }
}

/// The catalog message for a failure that carries the shell's own text: the
/// detail is a diagnostic, never a path of another account's data.
fn detail_message(key: MessageId, detail: impl std::fmt::Display) -> UiMessage {
    UiMessage::new(key).with_arg("detail", serde_json::json!(detail.to_string()))
}

/* -------------------------------------------------------------------------- */
/* The entry documents                                                        */
/* -------------------------------------------------------------------------- */

/// One entry's metadata, exactly as it is sealed in `<id>.entry`.
///
/// The names are the wire names the settings page reads, so the document is
/// camelCase and unknown fields are refused: a file this build did not write is
/// not silently half-read.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Metadata {
    /// The format version; only [`ENTRY_VERSION`] is accepted.
    version: u32,
    /// When the dictation started, in milliseconds since the Unix epoch. It is
    /// also the first half of the entry's id.
    started_utc_ms: u64,
    /// How much audio the entry holds.
    duration_ms: u64,
    /// The recognized text, empty when nothing was recognized.
    text: String,
    /// Why nothing was recognized, when nothing was.
    error: Option<UiMessage>,
}

/// One entry as the store knows it: the id, and the metadata behind it.
#[derive(Clone, Debug)]
pub(crate) struct StoredEntry {
    id: String,
    metadata: Metadata,
}

impl StoredEntry {
    /// The entry as the settings page draws it. `retrying` names the entry the
    /// one running retry belongs to, if any.
    fn view(self, retrying: Option<&str>) -> HistoryEntryView {
        let status = if retrying == Some(self.id.as_str()) {
            "transcribing"
        } else if self.metadata.text.is_empty() {
            "failed"
        } else {
            "recognized"
        };
        HistoryEntryView {
            id: self.id,
            started_utc_ms: self.metadata.started_utc_ms,
            duration_ms: self.metadata.duration_ms,
            status,
            text: self.metadata.text,
            error: self.metadata.error,
        }
    }
}

/// An id is the only thing that becomes a file name, so it is checked before
/// any path is built: two decimal numbers joined by one hyphen, each at most 20
/// digits. Nothing else - no separator, no dot, no letter - can reach the
/// directory.
fn validate_id(id: &str) -> Result<(), HistoryError> {
    let Some((started, generation)) = id.split_once('-') else {
        return Err(HistoryError::Format);
    };
    let decimal = |part: &str| {
        !part.is_empty() && part.len() <= 20 && part.bytes().all(|byte| byte.is_ascii_digit())
    };
    if decimal(started) && decimal(generation) {
        Ok(())
    } else {
        Err(HistoryError::Format)
    }
}

/* -------------------------------------------------------------------------- */
/* The store                                                                  */
/* -------------------------------------------------------------------------- */

/// The directory of sealed entries. Every method is independent of the shell,
/// so a test can drive the whole store on a temporary directory.
#[derive(Clone)]
pub(crate) struct HistoryStore {
    dir: PathBuf,
}

impl HistoryStore {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The path of one of an entry's two files. The id is validated first, so
    /// no caller can name a file outside [`Self::dir`].
    fn path(&self, id: &str, extension: &str) -> Result<PathBuf, HistoryError> {
        validate_id(id)?;
        Ok(self.dir.join(format!("{id}.{extension}")))
    }

    /// Seals `bytes` for the current user and writes them as `<id>.<extension>`
    /// in one atomic step. The directory is created on the first write by the
    /// replacement itself.
    fn write(
        &self,
        id: &str,
        extension: &str,
        magic: &[u8; 4],
        bytes: &[u8],
    ) -> Result<(), HistoryError> {
        let path = self.path(id, extension)?;
        let sealed = secrets::protect_bytes(bytes)?;
        let mut document = Vec::with_capacity(magic.len() + sealed.len());
        document.extend_from_slice(magic);
        document.extend_from_slice(&sealed);
        crate::settings::atomic_replace(&path, &document, true).map_err(|error| {
            HistoryError::Io(io::Error::other(error))
        })
    }

    /// Opens one of an entry's two files and returns the bytes behind the
    /// seal.
    fn read(&self, id: &str, extension: &str, magic: &[u8; 4]) -> Result<Zeroizing<Vec<u8>>, HistoryError> {
        let document = fs::read(self.path(id, extension)?)?;
        if !document.starts_with(magic) {
            return Err(HistoryError::Format);
        }
        Ok(secrets::unprotect_bytes(&document[magic.len()..])?)
    }

    /// Reads and parses one entry's metadata.
    fn entry(&self, id: &str) -> Result<StoredEntry, HistoryError> {
        let document = self.read(id, "entry", ENTRY_MAGIC)?;
        let metadata: Metadata =
            serde_json::from_slice(&document).map_err(|_| HistoryError::Format)?;
        if metadata.version != ENTRY_VERSION {
            return Err(HistoryError::Format);
        }
        Ok(StoredEntry {
            id: id.to_owned(),
            metadata,
        })
    }

    /// Rewrites one entry's metadata. The audio is never touched.
    fn write_entry(&self, entry: &StoredEntry) -> Result<(), HistoryError> {
        let document =
            Zeroizing::new(serde_json::to_vec(&entry.metadata).map_err(|_| HistoryError::Format)?);
        self.write(&entry.id, "entry", ENTRY_MAGIC, &document)
    }

    /// Stores one dictation: its audio as FLAC first, its metadata last.
    ///
    /// `text` is the recognized text, empty or absent when nothing was
    /// recognized; `error` says why, and is dropped when there is text.
    pub(crate) fn save(
        &self,
        id: &str,
        started_utc_ms: u64,
        duration_ms: u64,
        pcm: &[u8],
        text: Option<&str>,
        error: Option<UiMessage>,
    ) -> Result<(), HistoryError> {
        validate_id(id)?;
        let audio = Zeroizing::new(flac::encode_pcm(pcm)?);
        self.write(id, "audio", AUDIO_MAGIC, &audio)?;
        self.write_entry(&StoredEntry {
            id: id.to_owned(),
            metadata: Metadata {
                version: ENTRY_VERSION,
                started_utc_ms,
                duration_ms,
                text: text.unwrap_or_default().to_owned(),
                error: if text.is_some() { None } else { error },
            },
        })
    }

    /// Every entry, newest first, and whether anything in the directory could
    /// not be read.
    ///
    /// An audio file without its entry is a leftover of an interrupted write
    /// and is removed here; an entry that cannot be read is not listed, and
    /// says so through the flag instead of being silently forgotten.
    pub(crate) fn list(&self) -> (Vec<StoredEntry>, bool) {
        let mut entries = Vec::new();
        let mut unreadable = false;
        let directory = match fs::read_dir(&self.dir) {
            Ok(directory) => directory,
            Err(error) => return (entries, error.kind() != io::ErrorKind::NotFound),
        };
        for file in directory {
            let Ok(file) = file else {
                unreadable = true;
                continue;
            };
            let path = file.path();
            let Some(id) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("entry") => match self.entry(id) {
                    Ok(entry) => entries.push(entry),
                    Err(_) => unreadable = true,
                },
                Some("audio") => match self.audio_has_entry(id) {
                    Ok(true) => {}
                    Ok(false) => {
                        if fs::remove_file(&path).is_err() {
                            unreadable = true;
                        }
                    }
                    Err(_) => unreadable = true,
                },
                _ => {}
            }
        }
        entries.sort_by(|left, right| {
            right
                .metadata
                .started_utc_ms
                .cmp(&left.metadata.started_utc_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        (entries, unreadable)
    }

    /// Whether the entry file of an audio file's id exists.
    fn audio_has_entry(&self, id: &str) -> Result<bool, HistoryError> {
        self.path(id, "entry")?.try_exists().map_err(Into::into)
    }

    /// The decrypted FLAC stream of one entry, which is what the page plays and
    /// what a retry decodes. An entry without its audio is missing rather than
    /// empty.
    pub(crate) fn read_audio(&self, id: &str) -> Result<Zeroizing<Vec<u8>>, HistoryError> {
        self.entry(id)?;
        let audio = self.read(id, "audio", AUDIO_MAGIC)?;
        if !audio.starts_with(b"fLaC") {
            return Err(HistoryError::Format);
        }
        Ok(audio)
    }

    /// Replaces one entry's text with the text of a successful retry. The
    /// failure reason goes away with the failure, and the audio stays.
    pub(crate) fn replace_text(&self, id: &str, text: &str) -> Result<(), HistoryError> {
        let mut entry = self.entry(id)?;
        entry.metadata.text = text.to_owned();
        entry.metadata.error = None;
        self.write_entry(&entry)
    }

    /// Removes one entry. An entry that is already gone is not an error: the
    /// page's delete has an undo bar, and the entry may have been pruned by the
    /// limit in the meantime.
    pub(crate) fn delete(&self, id: &str) -> Result<(), HistoryError> {
        validate_id(id)?;
        for extension in ["audio", "entry"] {
            match fs::remove_file(self.path(id, extension)?) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Removes every entry. A directory that was never written is empty, not an
    /// error.
    pub(crate) fn clear(&self) -> Result<(), HistoryError> {
        let directory = match fs::read_dir(&self.dir) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for file in directory {
            let path = file?.path();
            if matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("audio" | "entry")
            ) {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    /// Keeps the `limit` newest entries and deletes the rest.
    pub(crate) fn prune(&self, limit: u32) -> Result<(), HistoryError> {
        let (entries, _) = self.list();
        for entry in entries.into_iter().skip(limit as usize) {
            self.delete(&entry.id)?;
        }
        Ok(())
    }
}

/* -------------------------------------------------------------------------- */
/* The take of one running dictation                                          */
/* -------------------------------------------------------------------------- */

/// The audio and the text of one dictation that has not ended yet.
///
/// The audio is appended by the capture thread and read once by the history
/// thread, so it is behind its own lock; the text is set by the shell when it
/// has a transcript to insert.
pub(crate) struct Take {
    started_utc_ms: u64,
    pcm: Mutex<Vec<u8>>,
    text: Mutex<Option<String>>,
}

impl Take {
    /// Appends one frame. A take longer than [`MAX_PCM_BYTES`] keeps what it
    /// already holds and ignores the rest: the history is a convenience, not a
    /// second recording of the whole session.
    pub(crate) fn push_pcm(&self, pcm: &[u8]) {
        let mut buffer = self.pcm.lock();
        let room = MAX_PCM_BYTES.saturating_sub(buffer.len());
        buffer.extend_from_slice(&pcm[..pcm.len().min(room)]);
    }
}

/* -------------------------------------------------------------------------- */
/* The managed state                                                          */
/* -------------------------------------------------------------------------- */

/// The history as the shell holds it: the store, the takes of the dictations
/// that are running, and the two pieces of state the settings page asks about.
pub(crate) struct History {
    /// The store, or `None` when the profile directory could not be resolved -
    /// the shell then runs without a history instead of refusing to start.
    store: Option<HistoryStore>,
    /// The take of every dictation between its start and its end, keyed by the
    /// generation the shell assigned.
    takes: Mutex<HashMap<u64, Arc<Take>>>,
    /// Held for the whole of every store operation and every listing, so a
    /// prune cannot delete an entry a retry is reading. Cloned into the retry
    /// helper so its commit takes the same lock.
    io: Arc<Mutex<()>>,
    /// The entry one retry is running for, if any.
    retrying: Mutex<Option<String>>,
    /// Why the last write failed, until a write succeeds. Cleared by a
    /// successful write and by clearing the history.
    last_error: Mutex<Option<UiMessage>>,
}

impl History {
    pub(crate) fn new(store: Option<HistoryStore>) -> Self {
        Self {
            store,
            takes: Mutex::new(HashMap::new()),
            io: Arc::new(Mutex::new(())),
            retrying: Mutex::new(None),
            last_error: Mutex::new(None),
        }
    }

    /// Starts the take of a new dictation.
    pub(crate) fn begin(&self, generation: u64) {
        let started_utc_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.takes.lock().insert(
            generation,
            Arc::new(Take {
                started_utc_ms,
                pcm: Mutex::new(Vec::new()),
                text: Mutex::new(None),
            }),
        );
    }

    /// The take the capture of this generation appends its frames to. `None`
    /// for a generation that is not a dictation - the laboratory's takes never
    /// reach the history.
    pub(crate) fn take(&self, generation: u64) -> Option<Arc<Take>> {
        self.takes.lock().get(&generation).cloned()
    }

    /// Notes the text the shell is about to insert, so the entry can be stored
    /// as recognized even if the paste itself fails.
    pub(crate) fn note_text(&self, generation: u64, text: &str) {
        if let Some(take) = self.take(generation) {
            *take.text.lock() = Some(text.to_owned());
        }
    }

    /// Drops the take of a dictation that was cancelled: nothing of it is
    /// stored.
    pub(crate) fn discard(&self, generation: u64) {
        self.takes.lock().remove(&generation);
    }

    /// Ends a dictation and stores its take.
    ///
    /// `error` is why the dictation produced no text, when it did not. The
    /// take is dropped - stored nowhere - when it is too short to hold a word,
    /// or when the only thing that went wrong is that it was too short. The
    /// write runs on a thread of its own: the caller is the command that ends
    /// the dictation, and sealing and encoding a take must not keep it.
    pub(crate) fn complete(&self, app: &AppHandle, generation: u64, error: Option<UiMessage>) {
        let Some(take) = self.takes.lock().remove(&generation) else {
            return;
        };
        let pcm = Zeroizing::new(std::mem::take(&mut *take.pcm.lock()));
        let text = take.text.lock().take();
        if pcm.len() / BYTES_PER_SAMPLE < MIN_SAMPLES {
            return;
        }
        let recognized = text.as_deref().is_some_and(|text| !text.is_empty());
        if !recognized && error.as_ref().is_some_and(|error| error.key == MessageId::RecorderTooShort) {
            return;
        }
        if self.store.is_none() {
            return;
        }
        let started_utc_ms = take.started_utc_ms;
        let duration_ms = pcm.len() as u64 / BYTES_PER_MS as u64;
        let id = format!("{started_utc_ms}-{generation}");
        let stored_error = if recognized {
            None
        } else {
            Some(error.unwrap_or_else(|| UiMessage::new(MessageId::HistoryNotRecognized)))
        };
        let app = app.clone();
        let failure_app = app.clone();
        let spawned = std::thread::Builder::new()
            .name("speechek-history".to_owned())
            .spawn(move || {
                let history = app.state::<History>();
                let result = {
                    let _io = history.io.lock();
                    match history.store.as_ref() {
                        Some(store) => store
                            .save(&id, started_utc_ms, duration_ms, &pcm, text.as_deref(), stored_error)
                            .and_then(|()| store.prune(limit(&app))),
                        None => Ok(()),
                    }
                };
                history.record_result(result);
                changed(&app);
            });
        if let Err(error) = spawned {
            failure_app
                .state::<History>()
                .record_result(Err(HistoryError::Io(error)));
            changed(&failure_app);
        }
    }

    /// Remembers why a write failed, or forgets the previous failure after a
    /// successful one.
    fn record_result(&self, result: Result<(), HistoryError>) {
        *self.last_error.lock() = result
            .err()
            .map(|error| detail_message(MessageId::HistoryWriteFailed, error));
    }
}

/// Keeps the history to the limit of the configuration the shell runs on.
///
/// Called after a general apply lowered `history_limit`, on a thread of its
/// own: the caller is a settings-command worker, and the prune touches the disk
/// once per removed entry.
pub(crate) fn prune_to_limit(app: &AppHandle) {
    let app = app.clone();
    let failure_app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("speechek-history-prune".to_owned())
        .spawn(move || {
            let history = app.state::<History>();
            let result = {
                let _io = history.io.lock();
                match history.store.as_ref() {
                    Some(store) => store.prune(limit(&app)),
                    None => Ok(()),
                }
            };
            history.record_result(result);
            changed(&app);
        });
    if let Err(error) = spawned {
        failure_app
            .state::<History>()
            .record_result(Err(HistoryError::Io(error)));
        changed(&failure_app);
    }
}

/// Tells the settings window that the stored entries changed.
fn changed(app: &AppHandle) {
    let _ = app.emit_to(preferences::SETTINGS_LABEL, EVENT_HISTORY_CHANGED, ());
}

/// The limit the running configuration keeps.
fn limit(app: &AppHandle) -> u32 {
    app.state::<PreferencesState>()
        .runtime
        .snapshot()
        .settings
        .history_limit
}

/// The store of the running shell, or the failure the page shows when the
/// profile directory could not be resolved.
fn store_of(history: &History) -> Result<HistoryStore, SettingsUiError> {
    history.store.clone().ok_or_else(|| {
        SettingsUiError::new(
            "HISTORY_UNAVAILABLE",
            UiMessage::new(MessageId::HistoryUnavailable),
        )
    })
}

/// Runs one command body off the UI thread. The store reads and writes files
/// and seals them with DPAPI; none of that belongs on the thread that drives
/// the WebView.
async fn run_blocking<T, F>(body: F) -> Result<T, SettingsUiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, SettingsUiError> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(body).await {
        Ok(result) => result,
        Err(_) => Err(SettingsUiError::new(
            "INTERNAL",
            UiMessage::new(MessageId::SettingsWorkerUnavailable),
        )),
    }
}

/* -------------------------------------------------------------------------- */
/* DTOs                                                                       */
/* -------------------------------------------------------------------------- */

/// What the history tab draws: the limit the running configuration keeps, the
/// entries newest first, and why some of them could not be read.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryView {
    pub limit: u32,
    pub entries: Vec<HistoryEntryView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_error: Option<UiMessage>,
}

/// One entry as the page draws it. `status` is `recognized`, `failed` or
/// `transcribing`; `text` is empty for the two latter, and `error` says why
/// nothing was recognized.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntryView {
    pub id: String,
    pub started_utc_ms: u64,
    pub duration_ms: u64,
    pub status: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<UiMessage>,
}

/* -------------------------------------------------------------------------- */
/* Command table: the history                                                 */
/* -------------------------------------------------------------------------- */

/// Every stored entry, newest first.
#[tauri::command]
pub async fn history_list(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<HistoryView, SettingsUiError> {
    preferences::check_window(&window)?;
    run_blocking(move || {
        let history = app.state::<History>();
        let _io = history.io.lock();
        let (entries, unreadable) = store_of(&history)?.list();
        let retrying = history.retrying.lock();
        let storage_error = history
            .last_error
            .lock()
            .clone()
            .or_else(|| unreadable.then(|| UiMessage::new(MessageId::HistoryStorageFailed)));
        Ok(HistoryView {
            limit: limit(&app),
            entries: entries
                .into_iter()
                .map(|entry| entry.view(retrying.as_deref()))
                .collect(),
            storage_error,
        })
    })
    .await
}

/// The decrypted FLAC stream of one entry, as raw bytes: the page plays it from
/// a blob and never sees a file path.
#[tauri::command]
pub async fn history_audio(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
) -> Result<tauri::ipc::Response, SettingsUiError> {
    preferences::check_window(&window)?;
    run_blocking(move || {
        let history = app.state::<History>();
        let _io = history.io.lock();
        let mut audio = store_of(&history)?
            .read_audio(&id)
            .map_err(HistoryError::command)?;
        Ok(tauri::ipc::Response::new(std::mem::take(&mut *audio)))
    })
    .await
}

/// Re-runs recognition over one entry's own audio with the mode the running
/// configuration had when the retry started: `live` replays the take through
/// the same Live relay, `smart` and `verbatim` stay on the batch API. The
/// stored text is replaced only by a complete, non-empty result; a failure
/// keeps the previous text.
#[tauri::command]
pub async fn history_retry(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
) -> Result<HistoryEntryView, SettingsUiError> {
    preferences::check_window(&window)?;
    let history = app.state::<History>();
    let store = store_of(&history)?;
    validate_id(&id).map_err(HistoryError::command)?;
    {
        let mut retrying = history.retrying.lock();
        if retrying.is_some() {
            return Err(SettingsUiError::new(
                "HISTORY_RETRY_BUSY",
                UiMessage::new(MessageId::HistoryRetryBusy),
            ));
        }
        *retrying = Some(id.clone());
    }
    // The ticket clears the marker and tells the page on every path out,
    // including an early return and a failed request.
    let _ticket = RetryTicket(app.clone());
    changed(&app);

    // The snapshot is captured before the first await: the whole retry - the
    // mode, the ring, the vocabulary and the languages - is the revision Save
    // had published when Retry was pressed. Its key is chosen only after the
    // audio was successfully decoded, so a rejected take consumes no turn.
    let state = app.state::<PreferencesState>();
    let snapshot = state.runtime.snapshot();
    let provider = Arc::clone(&state.provider);
    let io = Arc::clone(&history.io);

    retry_stored_entry(store, io, id, provider, snapshot).await
}

/// Decodes one stored take, recognizes it with the snapshot's selected mode and
/// commits a non-empty result. The caller has already pinned the snapshot; its
/// one key is selected here only after the audio decoded. A failure never
/// touches the entry.
pub(crate) async fn retry_stored_entry(
    store: HistoryStore,
    io: Arc<Mutex<()>>,
    id: String,
    provider: Arc<Provider>,
    snapshot: Arc<RuntimeSnapshot>,
) -> Result<HistoryEntryView, SettingsUiError> {
    let decoded = store.clone();
    let entry_id = id.clone();
    let pcm = run_blocking(move || {
        let audio = decoded
            .read_audio(&entry_id)
            .map_err(HistoryError::command)?;
        flac::decode_pcm(&audio).map_err(|error| HistoryError::Codec(error).command())
    })
    .await?;

    let key = snapshot.keys.next_key().ok_or_else(|| {
        SettingsUiError::new("MISSING_API_KEY", UiMessage::new(MessageId::RecorderMissingApiKey))
    })?;
    let text = recognize_retry(&provider, &snapshot, &key, pcm).await?;
    if text.trim().is_empty() {
        return Err(empty_retry_text());
    }

    run_blocking(move || {
        let _io = io.lock();
        store
            .replace_text(&id, &text)
            .map_err(HistoryError::command)?;
        Ok(store.entry(&id).map_err(HistoryError::command)?.view(None))
    })
    .await
}

/// The one selected mode decides the transport: `live` replays the saved PCM
/// through the shared Live relay in Smart mode; every other mode is the batch
/// request it always was. There is no fallback between them.
async fn recognize_retry(
    provider: &Provider,
    snapshot: &RuntimeSnapshot,
    key: &Arc<SecretKey>,
    pcm: Vec<u8>,
) -> Result<String, SettingsUiError> {
    let settings = &snapshot.settings;
    if settings.mode == "live" {
        return crate::live::transcribe_saved(
            provider,
            Arc::clone(key),
            Arc::clone(&snapshot.keys),
            Arc::clone(&settings.custom_vocabulary),
            Arc::clone(&settings.dictation_languages),
            pcm,
        )
        .await
        .map_err(|error| SettingsUiError::new(error.code, error.ui));
    }
    provider
        .transcribe(
            key,
            snapshot.keys.keys(),
            settings.mode.as_str(),
            &settings.custom_vocabulary,
            &settings.dictation_languages,
            wav_from_pcm(&pcm),
            #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
            None,
        )
        .await
        .map_err(|error| SettingsUiError::new(error.code, error.ui))
}

/// One stored entry's empty result: the failure the page already shows for a
/// batch take whose model returned nothing.
fn empty_retry_text() -> SettingsUiError {
    SettingsUiError::new("EMPTY_TEXT", UiMessage::new(MessageId::RecorderEmptyText))
}

/// Holds the one-retry marker for as long as the retry runs and releases it on
/// every path out, including an early return and a failed request.
struct RetryTicket(AppHandle);

impl Drop for RetryTicket {
    fn drop(&mut self) {
        *self.0.state::<History>().retrying.lock() = None;
        changed(&self.0);
    }
}

/// Removes one entry.
#[tauri::command]
pub async fn history_delete(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
) -> Result<(), SettingsUiError> {
    preferences::check_window(&window)?;
    run_blocking(move || {
        let history = app.state::<History>();
        let _io = history.io.lock();
        store_of(&history)?
            .delete(&id)
            .map_err(HistoryError::command)?;
        changed(&app);
        Ok(())
    })
    .await
}

/// Removes every entry.
#[tauri::command]
pub async fn history_clear(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<(), SettingsUiError> {
    preferences::check_window(&window)?;
    run_blocking(move || {
        let history = app.state::<History>();
        let _io = history.io.lock();
        store_of(&history)?.clear().map_err(HistoryError::command)?;
        *history.last_error.lock() = None;
        changed(&app);
        Ok(())
    })
    .await
}

/// Exports one entry's decrypted audio as a FLAC file the user picks.
///
/// Answers `false` when the dialog was cancelled - which is not a failure - and
/// `true` after the file was written.
#[tauri::command]
pub async fn history_save_audio(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
    file_name: String,
) -> Result<bool, SettingsUiError> {
    preferences::check_window(&window)?;
    // The handle is carried as a number: `HWND` is not `Send`, and the dialog
    // runs on a thread of its own.
    let owner = window.hwnd().map_err(save_failed)?.0 as isize;
    run_blocking(move || {
        let audio = {
            let history = app.state::<History>();
            let _io = history.io.lock();
            store_of(&history)?.read_audio(&id).map_err(HistoryError::command)?
        };
        let language = app.state::<PreferencesState>().runtime.snapshot().settings.language;
        let title = i18n::render(language, &UiMessage::new(MessageId::HistorySaveTitle)).into_owned();
        let filter =
            i18n::render(language, &UiMessage::new(MessageId::HistorySaveFilter)).into_owned();
        let default_name = save_name(&file_name);
        // The dialog is a COM object and runs its own message loop, so it gets
        // a thread of its own: the shell's other COM user lives in a
        // multithreaded apartment, and the dialog wants a single-threaded one.
        let saved = std::thread::Builder::new()
            .name("speechek-history-save".to_owned())
            .spawn(move || {
                let owner = windows::Win32::Foundation::HWND(owner as *mut core::ffi::c_void);
                let Some(path) =
                    crate::save_dialog::pick_save_path(owner, &default_name, &title, &filter)
                        .map_err(save_failed)?
                else {
                    return Ok(false);
                };
                fs::write(path, &*audio).map_err(save_failed)?;
                Ok(true)
            })
            .map_err(save_failed)?
            .join()
            .map_err(|_| {
                SettingsUiError::new(
                    "INTERNAL",
                    UiMessage::new(MessageId::SettingsWorkerUnavailable),
                )
            })??;
        Ok(saved)
    })
    .await
}

/// The failure the page shows when the export could not be written.
fn save_failed(error: impl std::fmt::Display) -> SettingsUiError {
    SettingsUiError::new(
        "HISTORY_SAVE_FAILED",
        detail_message(MessageId::HistorySaveFailed, error),
    )
}

/// The file name the dialog starts with: the page's suggestion without the
/// characters Windows does not accept in a name, and always with the `.flac`
/// extension the dialog filters for.
fn save_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|character| !character.is_control() && !"<>:\"/\\|?*".contains(*character))
        .collect();
    let cleaned: String = cleaned.trim().chars().take(100).collect();
    if cleaned.len() > ".flac".len() && cleaned.ends_with(".flac") {
        cleaned
    } else {
        "Speechek.flac".to_owned()
    }
}

/// A 16 kHz mono PCM16 WAV around `pcm`, which is what the batch endpoint
/// takes: the 44-byte canonical RIFF header followed by the samples.
fn wav_from_pcm(pcm: &[u8]) -> Vec<u8> {
    let size = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(size + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&size.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A store on a directory of its own, removed when the test ends.
    struct Fixture(HistoryStore);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let unique = format!(
                "speechek-history-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            Self(HistoryStore::new(std::env::temp_dir().join(unique)))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0.dir);
        }
    }

    /// 2000 samples of a rising ramp, which is long enough to be an entry.
    fn pcm() -> Vec<u8> {
        (0..2_000i16).flat_map(i16::to_le_bytes).collect()
    }

    #[test]
    fn a_saved_take_round_trips_through_its_sealed_files() {
        let fixture = Fixture::new();
        let pcm = pcm();
        fixture
            .0
            .save(
                "1-1",
                1,
                125,
                &pcm,
                None,
                Some(UiMessage::new(MessageId::HistoryNotRecognized)),
            )
            .unwrap();

        let (entries, unreadable) = fixture.0.list();
        assert!(!unreadable);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].clone().view(None).status, "failed");

        // The page plays exactly the FLAC stream the codec produced, and the
        // audio the retry decodes is the PCM that went in.
        let audio = fixture.0.read_audio("1-1").unwrap();
        assert_eq!(&*audio, &flac::encode_pcm(&pcm).unwrap());
        assert_eq!(flac::decode_pcm(&audio).unwrap(), pcm);
        assert!(fs::read(fixture.0.path("1-1", "audio").unwrap())
            .unwrap()
            .starts_with(AUDIO_MAGIC));
    }

    #[test]
    fn a_retried_text_replaces_the_failure() {
        let fixture = Fixture::new();
        fixture
            .0
            .save("1-1", 1, 125, &pcm(), None, Some(UiMessage::new(MessageId::HistoryNotRecognized)))
            .unwrap();
        fixture.0.replace_text("1-1", "recognized").unwrap();

        let view = fixture.0.entry("1-1").unwrap().view(None);
        assert_eq!(view.status, "recognized");
        assert_eq!(view.text, "recognized");
        assert!(view.error.is_none());
    }

    #[test]
    fn deleting_an_entry_removes_both_of_its_files() {
        let fixture = Fixture::new();
        fixture.0.save("1-1", 1, 125, &pcm(), Some("text"), None).unwrap();
        fixture.0.delete("1-1").unwrap();

        assert!(!fixture.0.path("1-1", "audio").unwrap().exists());
        assert!(!fixture.0.path("1-1", "entry").unwrap().exists());
        // Deleting what is already gone is not a failure; rewriting it is.
        fixture.0.delete("1-1").unwrap();
        assert!(matches!(
            fixture.0.replace_text("1-1", "text"),
            Err(HistoryError::Missing)
        ));
    }

    #[test]
    fn pruning_keeps_the_newest_entries() {
        let fixture = Fixture::new();
        for started in 1..=3 {
            fixture
                .0
                .save(&format!("{started}-1"), started, 125, &pcm(), Some("text"), None)
                .unwrap();
        }
        fixture.0.prune(2).unwrap();

        let (entries, unreadable) = fixture.0.list();
        assert!(!unreadable);
        assert_eq!(
            entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(),
            ["3-1", "2-1"]
        );
    }

    #[test]
    fn listing_removes_audio_without_its_entry() {
        let fixture = Fixture::new();
        fixture.0.save("1-1", 1, 125, &pcm(), Some("text"), None).unwrap();
        fs::write(fixture.0.path("4-1", "audio").unwrap(), b"orphan").unwrap();

        let (entries, unreadable) = fixture.0.list();
        assert!(!unreadable);
        assert_eq!(entries.len(), 1);
        assert!(!fixture.0.path("4-1", "audio").unwrap().exists());

        fixture.0.clear().unwrap();
        assert!(fixture.0.list().0.is_empty());
    }

    #[test]
    fn an_id_that_is_not_two_numbers_is_refused_before_any_path() {
        let fixture = Fixture::new();
        for id in [
            "..\\x",
            "../1-1",
            "1-1/2",
            "1--1",
            "-1",
            "1-",
            "123456789012345678901-1",
            "1-1.",
        ] {
            assert!(fixture.0.path(id, "entry").is_err());
            assert!(fixture.0.save(id, 1, 1, &pcm(), None, None).is_err());
            assert!(fixture.0.read_audio(id).is_err());
            assert!(fixture.0.replace_text(id, "text").is_err());
            assert!(fixture.0.delete(id).is_err());
        }
        // Nothing above created the directory.
        assert!(!fixture.0.dir.exists());
    }

    #[test]
    fn an_entry_of_another_version_or_layout_is_not_read() {
        let fixture = Fixture::new();
        for document in [
            br#"{"version":2,"startedUtcMs":1,"durationMs":1,"text":"","error":null}"#.as_slice(),
            br#"{"version":1,"started_utc_ms":1,"durationMs":1,"text":"","error":null}"#,
        ] {
            fixture.0.write("1-1", "entry", ENTRY_MAGIC, document).unwrap();
            assert!(fixture.0.list().1);
        }
    }

    #[test]
    fn a_retry_audio_is_pcm16_mono_at_16_khz() {
        let pcm = pcm();
        let wav = wav_from_pcm(&pcm);
        assert_eq!(wav.len(), pcm.len() + 44);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), pcm.len() as u32 + 36);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 32_000);
        assert_eq!(&wav[20..24], &[1, 0, 1, 0]);
        assert_eq!(&wav[32..36], &[2, 0, 16, 0]);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), pcm.len() as u32);
        assert_eq!(&wav[44..], &pcm);
    }

    /// The retry path itself - a real sealed store, a real FLAC decode and the
    /// shared Live relay - against a fake provider. The batch endpoint is a
    /// counting listener, so "the Live mode never touches batch" is evidence,
    /// not an assumption.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    mod retry_tests {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use base64::Engine as _;
        use futures_util::{SinkExt as _, StreamExt as _};
        use serde_json::{json, Value};
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        use super::{pcm, Fixture};
        use super::super::{recognize_retry, retry_stored_entry, AUDIO_MAGIC};
        use crate::i18n::{Language, MessageId, UiMessage};
        use crate::provider::Provider;
        use crate::secrets::{KeyRing, SecretKey};
        use crate::settings::{RuntimeSnapshot, Settings};

        const SYNTHETIC_KEY: &str = "speechek-retry-synthetic-key-1d4a";

        fn settings(mode: &str) -> Settings {
            Settings {
                hotkey: "Ctrl+Shift+Space".to_owned(),
                mode: mode.to_owned(),
                mute_during_recording: false,
                input_device: None,
                language: Language::En,
                custom_vocabulary: Arc::default(),
                dictation_languages: Arc::default(),
                history_limit: 10,
            }
        }

        fn test_snapshot(mode: &str, ring: Arc<KeyRing>) -> Arc<RuntimeSnapshot> {
            Arc::new(RuntimeSnapshot {
                settings: settings(mode),
                keys: ring,
                revision: 1,
            })
        }

        fn ring_and_key() -> (Arc<SecretKey>, Arc<KeyRing>) {
            let key = Arc::new(SecretKey::new(SYNTHETIC_KEY.to_owned()));
            let ring = Arc::new(KeyRing::new(vec![Arc::clone(&key)]));
            (key, ring)
        }

        fn fake_provider(http_port: u16, live_port: u16) -> Provider {
            Provider::for_test(
                &format!("http://127.0.0.1:{http_port}"),
                &format!("ws://127.0.0.1:{live_port}"),
            )
        }

        /// A fake Live endpoint: every frame the relay sends is logged, and
        /// `answer` decides what comes back. It exits when the relay closes.
        async fn fake_live(
            mut answer: impl FnMut(&Value) -> Vec<Value> + Send + 'static,
        ) -> (u16, Arc<Mutex<Vec<Value>>>) {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback port");
            let port = listener.local_addr().expect("an address").port();
            let log = Arc::new(Mutex::new(Vec::new()));
            let upstream_log = Arc::clone(&log);
            tokio::spawn(async move {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                loop {
                    let Some(message) = ws.next().await else {
                        return;
                    };
                    let Ok(message) = message else {
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
            });
            (port, log)
        }

        /// A listener that counts connections and drops each one, so a request
        /// to the batch endpoint fails quickly and is still observable.
        async fn connection_counter() -> (u16, Arc<AtomicUsize>) {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback port");
            let port = listener.local_addr().expect("an address").port();
            let count = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&count);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    counter.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
            });
            (port, count)
        }

        /// The saved take crosses the Live socket as the same PCM, the end
        /// follows the last byte, and only the non-empty finals become the
        /// committed text. The batch endpoint never sees a connection.
        async fn live_retry_round_trip() {
            let fixture = Fixture::new();
            let pcm = pcm();
            fixture
                .0
                .save("1-1", 1, 125, &pcm, Some("stale text"), None)
                .unwrap();
            let audio_before = fixture.0.read_audio("1-1").unwrap();

            let (live_port, log) = fake_live(move |frame| {
                if frame.get("setup").is_some() {
                    return vec![json!({ "setupComplete": {} })];
                }
                if frame.pointer("/realtimeInput/activityEnd").is_some() {
                    return vec![
                        json!({ "voiceActivity": { "type": "ACTIVITY_END" } }),
                        json!({ "serverContent": { "inputTranscription": { "text": "fresh" } } }),
                        json!({ "serverContent": { "inputTranscription": { "text": "text" } } }),
                    ];
                }
                if frame.pointer("/realtimeInput/audio").is_some() {
                    return vec![json!({
                        "serverContent": { "interimInputTranscription": { "text": "never used" } }
                    })];
                }
                Vec::new()
            })
            .await;
            let (http_port, http_hits) = connection_counter().await;
            let provider = Arc::new(fake_provider(http_port, live_port));
            let (_, ring) = ring_and_key();
            let snapshot = test_snapshot("live", Arc::clone(&ring));

            let view = retry_stored_entry(
                fixture.0.clone(),
                Arc::new(parking_lot::Mutex::new(())),
                "1-1".to_owned(),
                provider,
                snapshot,
            )
            .await
            .expect("the live retry succeeds");

            assert_eq!(view.text, "fresh text");
            assert_eq!(view.status, "recognized");
            assert!(view.error.is_none());
            assert_eq!(
                (view.started_utc_ms, view.duration_ms),
                (1, 125),
                "the retry rewrites only the text"
            );
            assert_eq!(
                fixture.0.entry("1-1").unwrap().view(None).text,
                "fresh text"
            );
            let audio_after = fixture.0.read_audio("1-1").unwrap();
            assert_eq!(
                &*audio_after, &*audio_before,
                "a retry never rewrites the saved audio"
            );
            assert_eq!(
                http_hits.load(Ordering::SeqCst),
                0,
                "the Live mode never dials the batch endpoint"
            );

            let frames = log.lock().expect("the upstream log lock").clone();
            let mut forwarded = Vec::new();
            let mut ends = Vec::new();
            for frame in &frames {
                if let Some(data) = frame
                    .pointer("/realtimeInput/audio/data")
                    .and_then(Value::as_str)
                {
                    forwarded.extend_from_slice(
                        &base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .expect("the fixture's base64"),
                    );
                }
                if frame.pointer("/realtimeInput/activityEnd").is_some() {
                    ends.push(forwarded.len());
                }
            }
            assert_eq!(forwarded, pcm, "the relay carried the exact saved PCM");
            assert_eq!(
                ends,
                vec![pcm.len()],
                "the end follows every saved byte and precedes the final"
            );
            assert_eq!(
                frames
                    .iter()
                    .filter(|frame| frame.pointer("/realtimeInput/activityStart").is_some())
                    .count(),
                1
            );
        }

        #[tokio::test]
        async fn a_live_retry_replays_the_saved_audio_and_commits_the_complete_text() {
            live_retry_round_trip().await;
        }

        /// A terminal error after an interim and a final is still a failure:
        /// the stored entry - including its previous failure reason - and its
        /// audio stay exactly as they were.
        #[tokio::test]
        async fn a_live_retry_failure_after_partial_text_keeps_the_entry() {
            let fixture = Fixture::new();
            let pcm = pcm();
            fixture
                .0
                .save(
                    "1-1",
                    1,
                    125,
                    &pcm,
                    None,
                    Some(UiMessage::new(MessageId::HistoryNotRecognized)),
                )
                .unwrap();
            let audio_before = fixture.0.read_audio("1-1").unwrap();

            let (live_port, _log) = fake_live(move |frame| {
                if frame.get("setup").is_some() {
                    return vec![json!({ "setupComplete": {} })];
                }
                if frame.pointer("/realtimeInput/activityEnd").is_some() {
                    return vec![
                        json!({ "voiceActivity": { "type": "ACTIVITY_END" } }),
                        json!({ "serverContent": { "inputTranscription": { "text": "partial" } } }),
                        json!({ "error": { "message": "synthetic retry failure" } }),
                    ];
                }
                if frame.pointer("/realtimeInput/audio").is_some() {
                    return vec![json!({
                        "serverContent": { "interimInputTranscription": { "text": "unfinished" } }
                    })];
                }
                Vec::new()
            })
            .await;
            let (http_port, _http_hits) = connection_counter().await;
            let provider = Arc::new(fake_provider(http_port, live_port));
            let (_, ring) = ring_and_key();
            let snapshot = test_snapshot("live", Arc::clone(&ring));

            let error = retry_stored_entry(
                fixture.0.clone(),
                Arc::new(parking_lot::Mutex::new(())),
                "1-1".to_owned(),
                provider,
                snapshot,
            )
            .await
            .expect_err("a terminal error is not a retry result");
            assert_eq!(error.ui.key, MessageId::LiveApiError);

            let view = fixture.0.entry("1-1").unwrap().view(None);
            assert!(view.text.is_empty());
            assert_eq!(
                view.error.as_ref().map(|error| error.key),
                Some(MessageId::HistoryNotRecognized)
            );
            let audio_after = fixture.0.read_audio("1-1").unwrap();
            assert_eq!(&*audio_after, &*audio_before);
        }

        /// A session that ends without any final transcript reports the same
        /// empty-text failure a batch retry reports, and commits nothing.
        #[tokio::test]
        async fn a_live_retry_without_a_final_reports_empty_text_and_keeps_the_entry() {
            let fixture = Fixture::new();
            fixture
                .0
                .save("1-1", 1, 125, &pcm(), Some("keep this"), None)
                .unwrap();

            let (live_port, _log) = fake_live(move |frame| {
                if frame.get("setup").is_some() {
                    return vec![json!({ "setupComplete": {} })];
                }
                if frame.pointer("/realtimeInput/activityEnd").is_some() {
                    return vec![json!({ "voiceActivity": { "type": "ACTIVITY_END" } })];
                }
                Vec::new()
            })
            .await;
            let (http_port, _http_hits) = connection_counter().await;
            let provider = Arc::new(fake_provider(http_port, live_port));
            let (_, ring) = ring_and_key();
            let snapshot = test_snapshot("live", Arc::clone(&ring));

            let error = retry_stored_entry(
                fixture.0.clone(),
                Arc::new(parking_lot::Mutex::new(())),
                "1-1".to_owned(),
                provider,
                snapshot,
            )
            .await
            .expect_err("an empty transcript is not committed");
            assert_eq!(error.ui.key, MessageId::RecorderEmptyText);
            assert_eq!(fixture.0.entry("1-1").unwrap().view(None).text, "keep this");
        }

        /// A take that cannot be decoded is rejected before the pinned key is
        /// drawn from the ring: a corrupt recording must not consume a turn.
        #[tokio::test]
        async fn unreadable_audio_does_not_consume_a_key_turn() {
            let fixture = Fixture::new();
            fixture
                .0
                .save("1-1", 1, 125, &pcm(), Some("keep this"), None)
                .unwrap();
            fixture
                .0
                .write("1-1", "audio", AUDIO_MAGIC, b"not a FLAC stream")
                .unwrap();
            let first = Arc::new(SecretKey::new("speechek-retry-first-key".to_owned()));
            let second = Arc::new(SecretKey::new("speechek-retry-second-key".to_owned()));
            let ring = Arc::new(KeyRing::new(vec![Arc::clone(&first), Arc::clone(&second)]));
            let snapshot = test_snapshot("live", Arc::clone(&ring));
            let provider = Arc::new(fake_provider(1, 1));

            let error = retry_stored_entry(
                fixture.0.clone(),
                Arc::new(parking_lot::Mutex::new(())),
                "1-1".to_owned(),
                provider,
                snapshot,
            )
            .await
            .expect_err("a corrupt take is rejected");
            assert_eq!(error.ui.key, MessageId::HistoryAudioDamaged);

            let next = ring.next_key().expect("the ring still has keys");
            assert!(
                Arc::ptr_eq(&next, &first),
                "a rejected take consumed no key turn"
            );
            assert_eq!(fixture.0.entry("1-1").unwrap().view(None).text, "keep this");
        }

        /// Smart and Verbatim keep the batch transport they always had; only
        /// the Live selection reaches the Live socket.
        #[tokio::test]
        async fn smart_and_verbatim_retries_stay_on_the_batch_transport() {
            for mode in ["smart", "verbatim"] {
                let (http_port, http_hits) = connection_counter().await;
                let (live_port, live_hits) = connection_counter().await;
                let provider = fake_provider(http_port, live_port);
                let (key, ring) = ring_and_key();
                let snapshot = test_snapshot(mode, ring);

                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    recognize_retry(&provider, &snapshot, &key, pcm()),
                )
                .await
                .expect("the batch attempt completes");
                assert!(
                    result.is_err(),
                    "{mode} fails against a batch endpoint that drops the request"
                );
                assert!(
                    http_hits.load(Ordering::SeqCst) >= 1,
                    "{mode} still dials the batch endpoint"
                );
                assert_eq!(
                    live_hits.load(Ordering::SeqCst),
                    0,
                    "{mode} never opens the Live socket"
                );
            }
        }
    }
}
