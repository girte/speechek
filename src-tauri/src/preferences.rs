//! The native side of the settings window: the draft the user edits, the
//! commands the page calls, and the transactions that apply a new
//! configuration without a restart.
//!
//! The page never sees a key unless the user asks for one. The draft lives here,
//! in the shell's own memory: the WebView receives what it needs to draw the
//! window - counts, statuses, fixed error text - and a full key list only from
//! [`settings_reveal_keys`], which the page calls for exactly that purpose.
//! Nothing in this module writes a key into a log, an event or an HTTP route.
//!
//! A draft is native state, not page state: hiding the tab or the window keeps
//! it, closing the window or the process drops it, and a failure while
//! publishing (`partial_persistence`) outlives both, because it is a fact about
//! the files on disk rather than about the editing session.
//!
//! One internal writer applies configuration to the disk and the runtime. It
//! serializes on the same session lock the dictation state machine uses,
//! validates everything before it touches the disk, keeps an undo record of the
//! bytes it is about to replace, and publishes exactly one runtime revision once
//! both files have been read back. A failure that could not be rolled back
//! completely is reported as `SAVE_PARTIAL` and makes every following apply
//! rewrite and read back the roles it names before it may succeed again.
//!
//! What an apply owns is its scope. The general form - `mode` and
//! `mute_during_recording` - is applied at once on every accepted change;
//! the chord the page typed is validated, registered and read back before it is
//! reported as accepted; and the key list changes only through the page's
//! explicit apply, never as a side effect of a mode or a chord change.
//!
//! Every command runs its body on a blocking worker. A body takes the draft
//! lock, reads the settings document behind the window's statuses and may touch
//! the key container; none of that belongs on the thread that drives the
//! WebView, and none of it may run while a UI callback waits.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait as _, HostTrait as _};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter as _, Manager as _, WebviewWindow};
use zeroize::Zeroizing;

use crate::provider::{KeyCheckOutcome, Provider};
use crate::secrets::{self, KeyRing, SecretKey, SecretStoreError};
use crate::settings::{self, RuntimeSnapshot, Settings, SharedRuntime};

/* -------------------------------------------------------------------------- */
/* Names, codes and fixed wording                                             */
/* -------------------------------------------------------------------------- */

/// The settings webview's label. Exported so the shell's window work and this
/// module cannot disagree about it.
pub(crate) const SETTINGS_LABEL: &str = "settings";

/// The sections the page can be opened on, as the native request spells them.
const SECTION_GENERAL: &str = "general";
const SECTION_KEYS: &str = "keys";
const SECTION_LAST: &str = "last";

/// Sent to the overlay once an apply has published a new revision, so a caption
/// that shows the launcher chord stops showing the old one. Carries no key.
const EVENT_SETTINGS_CHANGED: &str = "speechek:settings-changed";

/// Modes the launcher understands, in the order the page shows them.
const MODES: &[&str] = &["live", "smart", "verbatim"];

/// Wording for the one failure the user has to fix on disk.
const SAVE_PARTIAL_MESSAGE: &str = "Файлы могли измениться частично; приложение продолжает использовать прежние настройки. Устраните ошибку доступа и повторите применение.";
/// Wording for an apply that would change the launcher chord mid-dictation.
const DICTATION_ACTIVE_MESSAGE: &str =
    "Чтобы изменить горячую клавишу, завершите или отмените диктовку.";
/// Wording for a key store that exists but cannot be opened.
const SECRETS_UNAVAILABLE_MESSAGE: &str =
    "Хранилище ключей недоступно: сохранённый список невозможно прочитать.";
/// Wording for a launcher chord the grammar cannot express.
const HOTKEY_INVALID_MESSAGE: &str =
    "Сочетание клавиш не поддерживается. Примеры: F2, Ctrl+Shift+Space.";
/// Wording for a general-form port outside `1..=65535`.
const PORT_INVALID_MESSAGE: &str = "Укажите целое число от 1 до 65535.";
/// Wording for a chosen input device id the document's own reader would refuse.
const INPUT_DEVICE_INVALID_MESSAGE: &str =
    "Идентификатор выбранного микрофона не поддерживается. Выберите устройство заново.";
/// Wording for an input-device enumeration the host refused to perform. The
/// page keeps the running choice and its own options; the operating system's
/// text, which may name endpoints, never reaches it.
const INPUT_DEVICES_UNAVAILABLE_MESSAGE: &str =
    "Не удалось получить список микрофонов. Проверьте доступ к устройствам записи.";

/* -------------------------------------------------------------------------- */
/* Command failure                                                            */
/* -------------------------------------------------------------------------- */

/// A settings-command failure: a stable machine-readable code, fixed text that
/// never quotes a key, a Google response or a file's contents, and - where the
/// page can point at something - the field or the physical line it belongs to.
#[derive(Clone, Debug, Serialize)]
pub struct SettingsUiError {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
}

impl SettingsUiError {
    /// A failure with fixed text. Every call site passes compile-time constants
    /// or text this module composed from a path, never a value read from a
    /// configuration file.
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            field: None,
            line: None,
        }
    }

    /// The same failure, naming the form field it belongs to.
    fn field(code: &str, message: &str, field: &str) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            field: Some(field.to_owned()),
            line: None,
        }
    }

    /// The same failure, naming the physical line of the list it belongs to.
    fn line(code: &str, message: &str, line: u64) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            field: None,
            line: Some(line),
        }
    }

    /// Only the settings page may call the settings commands.
    fn forbidden() -> Self {
        Self::new(
            "FORBIDDEN",
            "Команда доступна только окну настроек Speechek.",
        )
    }

    /// The draft is gone, or the caller names a revision that is not the one
    /// the draft is on.
    fn stale_draft() -> Self {
        Self::new(
            "STALE_DRAFT",
            "Черновик настроек изменился; перечитайте окно и повторите действие.",
        )
    }

    /// A destructive step needs its own confirmation.
    fn confirm_required(message: &str) -> Self {
        Self::new("CONFIRM_REQUIRED", message)
    }
}

impl std::fmt::Display for SettingsUiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SettingsUiError {}

/* -------------------------------------------------------------------------- */
/* DTOs                                                                       */
/* -------------------------------------------------------------------------- */

/// What the settings page draws, for one draft revision. The nested `settings`
/// keeps the document's own names: `hotkey`, `mode`, `mute_during_recording`,
/// `port` and `input_device`. Its `port` is the value the next start will use -
/// editable, saved immediately, applied by a restart - while `startup_port`,
/// `running_port` and `port_fallback` describe the socket this run already
/// owns and never follow an edit.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    /// Identifies the draft every later command has to name.
    pub draft_id: u64,
    /// Monotonic revision of this draft; a command that changes the draft
    /// carries the revision it expects, so a late answer cannot overwrite a
    /// newer one.
    pub revision: u64,
    /// The values the page shows for the two form sections.
    pub settings: Settings,
    /// The port this run claimed from the settings document before the shell
    /// existed. Fixed for the whole run: the editable `settings.port` may move
    /// on while the socket cannot, and a conflict has to name the port the
    /// start really asked for.
    pub startup_port: u16,
    /// The port the backend really serves for this run. Equal to
    /// [`Self::startup_port`] unless that one was taken.
    pub running_port: u16,
    /// Whether the configured port was taken at start and a temporary port is
    /// served until the next start.
    pub port_fallback: bool,
    /// Whether the key section holds a list that still needs a decision before
    /// the window may be dropped: viewing the saved list, or retyping it, is
    /// not a change and never sets this.
    pub dirty: bool,
    /// Number of keys the displayed list holds.
    pub key_count: usize,
    /// Whether the key section differs from the saved key list: a normalized
    /// list equal to the saved one does not count, while an unusable list - and
    /// any list typed while the container cannot be read - does.
    pub keys_changed: bool,
    /// The line and fixed text of an unusable key list, when the draft has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_error: Option<KeyError>,
    /// `ready`, `missing` or `unavailable`: what the page may say about the key
    /// store without claiming that a list it cannot read is empty.
    pub secrets_status: String,
    /// Why the launcher chord is not registered, when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotkey_error: Option<String>,
    /// A dictation is in progress, so the chord cannot be changed right now.
    pub dictation_active: bool,
    /// A previous apply could not be rolled back completely.
    pub partial_persistence: bool,
    /// Which section the window should show first.
    pub selected_section: String,
}

/// One input device the settings window may choose between. `id` is the
/// `cpal::DeviceId` in its `Display` form - the exact string the settings
/// document stores - and `is_default` marks the device a `null` choice opens
/// as the system default at capture time. Names are what the user sees; two
/// devices may share one, so every entry carries its own id and the page keeps
/// all of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputDeviceView {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// An unusable key list, by physical line; the message never holds a key.
#[derive(Clone, Debug, Serialize)]
pub struct KeyError {
    pub line: u64,
    pub message: String,
}

/// The outcome of an apply: the new view, whether the new configuration is in
/// effect, and a warning when something outlived the apply that could not be
/// finished (a launcher chord that could not be released, for instance).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub view: SettingsView,
    pub applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// What one key check found. A report is either about the draft revision it
/// names, with one result per key, or stale: the draft moved on while the
/// checks were running, so the results describe values the page no longer
/// shows, and there are none.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyCheckReport {
    pub draft_id: u64,
    pub revision: u64,
    pub stale: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<KeyCheckEntry>,
}

/// One checked key, named by the physical line it sits on.
#[derive(Clone, Debug, Serialize)]
pub struct KeyCheckEntry {
    pub line: u64,
    pub status: String,
    pub message: String,
}

/// The payload of the one event an apply sends: the launcher chord the running
/// revision now uses. It never carries a key.
#[derive(Clone, Debug, Serialize)]
struct SettingsChanged {
    hotkey: String,
}

/* -------------------------------------------------------------------------- */
/* Draft and file state                                                       */
/* -------------------------------------------------------------------------- */

/// What the page may say about the key container: `ready` when a container is
/// there and this process can read it, `missing` when none exists yet (a normal
/// first run, not a failure), `unavailable` when one exists but cannot be used.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SecretsState {
    Ready,
    Missing,
    Unavailable,
}

impl SecretsState {
    /// The state as the page sees it.
    fn name(self) -> &'static str {
        match self {
            SecretsState::Ready => "ready",
            SecretsState::Missing => "missing",
            SecretsState::Unavailable => "unavailable",
        }
    }

    /// The state the shell started in: an error that says the container is not
    /// there is a first run, any other error is a container this process cannot
    /// use.
    fn from_error(error: Option<SecretStoreError>) -> Self {
        match error {
            None => SecretsState::Ready,
            Some(error) if error.is_missing() => SecretsState::Missing,
            Some(_) => SecretsState::Unavailable,
        }
    }
}

/// What the key container looked like before the apply that failed. Only the
/// bytes matter: guessing a list for a container this process could not decrypt
/// is not a repair.
#[derive(Clone, Debug)]
enum VaultBefore {
    /// There was no container.
    Absent,
    /// There was a readable container, whose list the running ring holds.
    Usable,
    /// There was a container this process could not decrypt.
    Opaque(Vec<u8>),
}

/// A failure that left the files potentially different from the running
/// configuration. It survives a Close and a hidden window, because it is a fact
/// about the disk; only an apply that rewrites and reads back every role named
/// here clears it.
#[derive(Debug)]
struct PartialPersistence {
    /// The settings document may not hold what the running configuration says.
    settings: bool,
    /// The key container may hold a different list.
    secrets: bool,
    /// What the container looked like before the first unresolved failure that
    /// owed the container role. `Some` exactly while `secrets` is set: a failure
    /// that never touched the container has no before-image to record, and none
    /// to lend to a later one that does.
    vault_before: Option<VaultBefore>,
}

/// The bytes one apply is about to replace, so exactly the roles that were
/// really written can be put back in the opposite order. Both halves are
/// ciphertext or document bytes, never a key decoded from them.
struct PersistUndo {
    settings_before: Vec<u8>,
    vault_before: Option<Vec<u8>>,
    settings_replaced: bool,
    secrets_replaced: bool,
}

/// What an apply will do with the key container.
enum KeyPlan {
    /// The saved list stands: nothing to write for this role.
    Keep,
    /// Write this list.
    Replace(Vec<Arc<SecretKey>>),
    /// Put back the ciphertext the container had while it was still unreadable.
    RestoreOpaque(Vec<u8>),
    /// Make sure no container exists, because there was none.
    RestoreAbsent,
}

impl KeyPlan {
    /// Whether this plan touches the container at all.
    fn writes_container(&self) -> bool {
        !matches!(self, KeyPlan::Keep)
    }
}

/// Which slice of the configuration one apply owns. Internal: a command's
/// contract fixes it, and the page never passes it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ApplyScope {
    /// The general form: the recognition mode and the mute switch.
    General,
    /// The chord the page typed in the hotkey field.
    Hotkey,
    /// The key list the page typed, through its explicit apply only.
    Keys,
}

/// The page's editing session. Owned by [`PreferencesState`]; every command
/// works on it under that state's draft lock, and a command that changes it
/// holds that lock for the whole transition.
struct SettingsDraft {
    /// Identity of this editing session; a new one gets a new id, so a late
    /// answer of a closed draft can never touch its successor.
    id: u64,
    /// Monotonic revision, raised by every change the commands accept.
    revision: u64,
    /// Replacement key list from the page, exactly as typed and never logged.
    /// `None` means the saved list is untouched.
    raw_keys: Option<Zeroizing<String>>,
    /// The user confirmed that an empty list is meant to clear the saved keys.
    clear_confirmed: bool,
}

impl SettingsDraft {
    fn new(id: u64) -> Self {
        Self {
            id,
            revision: 1,
            raw_keys: None,
            clear_confirmed: false,
        }
    }
}

/// How a typed key list reads against the list that is saved: the count to
/// show, whether it still needs a decision, and the fixed reason it cannot be
/// used when it cannot. The rule lives here in one place, so the view and both
/// close decisions cannot disagree about what counts as an unapplied change.
struct KeyDraftReading {
    /// The typed list's own count when it can be used, the saved one otherwise.
    count: usize,
    /// Whether the typed list has to be applied or discarded before the window
    /// may be dropped.
    changed: bool,
    /// Why the typed list cannot be used, when it cannot.
    error: Option<KeyError>,
}

/// Reads a typed key list against the saved one. A usable list that normalizes
/// to exactly the saved keys is not a change: a revealed list that was only
/// viewed, hidden again, or retyped as it is leaves nothing to decide. An
/// unusable list always needs a decision, and so does any list typed while the
/// container cannot be read, because the saved list is unknown then and the
/// typed one would replace it.
fn read_key_draft(
    raw: Option<&str>,
    saved: &[Arc<SecretKey>],
    store_unreadable: bool,
) -> KeyDraftReading {
    let Some(raw) = raw else {
        return KeyDraftReading {
            count: saved.len(),
            changed: false,
            error: None,
        };
    };
    match secrets::normalize_key_text(raw) {
        Ok(normalized) => KeyDraftReading {
            count: normalized.len(),
            changed: store_unreadable || !secrets::list_equal(normalized.keys(), saved),
            error: None,
        },
        Err(error) => KeyDraftReading {
            // An unusable list keeps the saved count visible and says why the
            // typed list cannot be used; it is never dropped.
            count: saved.len(),
            changed: true,
            error: Some(KeyError {
                line: error.line() as u64,
                message: error.message().to_owned(),
            }),
        },
    }
}

/* -------------------------------------------------------------------------- */
/* State                                                                      */
/* -------------------------------------------------------------------------- */

/// The port facts of one run, fixed when the socket was reserved before any
/// window existed. The settings window reports them and they never follow the
/// editable document: the socket cannot move without a restart, while the saved
/// port may be edited by the same window that shows these.
#[derive(Clone, Copy)]
pub(crate) struct StartupPorts {
    /// The port the start claimed from the settings document.
    pub configured: u16,
    /// The port the backend really serves; differs only in a fallback.
    pub actual: u16,
    /// Whether the configured port was taken at start.
    pub fallback: bool,
}

/// Everything the settings commands share. Handed to Tauri as managed state
/// beside the shell's own.
pub struct PreferencesState {
    /// The configuration the shell runs on; an apply publishes into it.
    pub(crate) runtime: Arc<SharedRuntime>,
    /// The one provider instance, used by the key check only.
    pub(crate) provider: Arc<Provider>,
    /// The port facts of this run, for the window's fallback and restart
    /// notices.
    ports: StartupPorts,
    /// The resolved `settings.json` this draft edits.
    pub(crate) config_path: PathBuf,
    /// The container beside it.
    pub(crate) secrets_path: PathBuf,
    /// Why the launcher chord is not registered, for the banner in the window.
    pub(crate) hotkey_error: Mutex<Option<String>>,
    /// The section a pending open (tray activation, startup) asked for.
    pub(crate) pending_section: Mutex<String>,
    /// Set while the shell is leaving. The quit path latches it before its
    /// checks and every command that would change the draft or the files
    /// refuses while it is set, so nothing can slip in between the decision to
    /// leave and the exit.
    closing: AtomicBool,
    /// Set when the installer asked this shell to leave. Monotone, unlike
    /// [`Self::closing`]: no refusal on another path may ever clear it, so a
    /// late interactive quit cannot re-open the shell to a writer behind the
    /// installer's decision.
    installer_closing: AtomicBool,
    /// The editing session, or `None` while the window has no draft.
    draft: Mutex<Option<SettingsDraft>>,
    /// Whether the key container can be used, as the window shows it.
    secrets: Mutex<SecretsState>,
    /// A previous apply that could not be rolled back completely, if there was
    /// one.
    partial: Mutex<Option<PartialPersistence>>,
    /// One key check at a time. An atomic rather than a lock, because the check
    /// awaits the network and a `parking_lot` guard may not cross an await.
    check_busy: AtomicBool,
    /// Monotonic draft identifiers.
    next_id: AtomicU64,
}

impl PreferencesState {
    pub fn new(
        config_path: PathBuf,
        runtime: Arc<SharedRuntime>,
        provider: Arc<Provider>,
        secret_error: Option<SecretStoreError>,
        ports: StartupPorts,
    ) -> Self {
        Self {
            secrets_path: secrets::secrets_path(&config_path),
            config_path,
            runtime,
            provider,
            ports,
            hotkey_error: Mutex::new(None),
            pending_section: Mutex::new(SECTION_LAST.to_owned()),
            closing: AtomicBool::new(false),
            installer_closing: AtomicBool::new(false),
            draft: Mutex::new(None),
            secrets: Mutex::new(SecretsState::from_error(secret_error)),
            partial: Mutex::new(None),
            check_busy: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        }
    }

    /// Drops the editing session. Called when the window is destroyed or the
    /// process leaves: the draft is not carried into the next open, while the
    /// key store's real state and a partial persistence flag are, because
    /// neither is about the window.
    pub fn invalidate(&self) {
        let mut slot = self.draft.lock();
        *slot = None;
    }

    /// Whether a previous apply left the files in an unknown state.
    pub fn partial_persistence(&self) -> bool {
        self.partial.lock().is_some()
    }

    /// Whether an apply still owes the key container a reconciliation: the file
    /// may hold something other than what the runtime is using.
    fn secrets_reconciliation_owed(&self) -> bool {
        self.partial
            .lock()
            .as_ref()
            .is_some_and(|flag| flag.secrets)
    }

    /// What the page may say about the key store, without claiming that a list
    /// it cannot read is empty.
    fn secrets_status(&self) -> &'static str {
        self.secrets.lock().name()
    }

    /// Whether the container exists but this process cannot read it, so the
    /// list that is saved is unknown.
    fn secrets_unreadable(&self) -> bool {
        self.secrets_status() == "unavailable"
    }

    /// How the draft's key list reads against the ring in `snapshot`: the saved
    /// list is the ring that is running, and the store's readability decides
    /// whether a typed list has to stay a decision of its own.
    fn key_reading(&self, draft: &SettingsDraft, snapshot: &RuntimeSnapshot) -> KeyDraftReading {
        read_key_draft(
            draft.raw_keys.as_deref().map(String::as_str),
            snapshot.keys.keys(),
            self.secrets_unreadable(),
        )
    }

    /// Whether the draft holds a key list that needs a decision before the
    /// window may be dropped. This is the same rule the view reports, so a
    /// close cannot refuse a draft the page draws as clean.
    fn keys_need_decision(&self, draft: &SettingsDraft) -> bool {
        let snapshot = self.runtime.snapshot();
        self.key_reading(draft, &snapshot).changed
    }

    /// Which section a fresh draft should open on: the section a pending open
    /// asked for, otherwise the key section on a first run without usable keys.
    fn section_for_new_draft(&self) -> String {
        let mut pending = self.pending_section.lock();
        let requested = pending.clone();
        *pending = SECTION_LAST.to_owned();
        if requested == SECTION_GENERAL || requested == SECTION_KEYS {
            return requested;
        }
        if self.runtime.snapshot().keys.is_empty() && self.secrets_status() != "ready" {
            return SECTION_KEYS.to_owned();
        }
        SECTION_LAST.to_owned()
    }

    /// What the page draws for one draft. The general values are always the
    /// running document's - the page never owns a copy the shell could fall
    /// behind - while the port facts of this run are fixed at reservation and
    /// are reported as they were.
    fn view(
        &self,
        app: &AppHandle,
        draft: &SettingsDraft,
        selected_section: String,
    ) -> SettingsView {
        let snapshot = self.runtime.snapshot();
        // The typed list is read once, under the same rule the close decisions
        // use: the same answer decides what the page shows and whether a close
        // still has to ask about it.
        let reading = self.key_reading(draft, &snapshot);
        SettingsView {
            draft_id: draft.id,
            revision: draft.revision,
            settings: snapshot.settings.clone(),
            startup_port: self.ports.configured,
            running_port: self.ports.actual,
            port_fallback: self.ports.fallback,
            dirty: reading.changed,
            key_count: reading.count,
            keys_changed: reading.changed,
            key_error: reading.error,
            secrets_status: self.secrets_status().to_owned(),
            hotkey_error: self.hotkey_error.lock().clone(),
            dictation_active: crate::phase_of(&crate::inner(app)).is_active(),
            partial_persistence: self.partial_persistence(),
            selected_section,
        }
    }
}

/* -------------------------------------------------------------------------- */
/* Guards and the blocking worker                                             */
/* -------------------------------------------------------------------------- */

/// The page a settings command may be called from. Both halves are checked: the
/// window label, and the exact document URL — derived from the running dev URL,
/// without a query or a fragment — the settings window was really opened on.
fn check_window(window: &WebviewWindow) -> Result<(), SettingsUiError> {
    if crate::is_app_page(window, SETTINGS_LABEL, crate::SETTINGS_PAGE_PATH) {
        Ok(())
    } else {
        Err(SettingsUiError::forbidden())
    }
}

/// Runs one command body off the UI thread. The body takes locks, reads the
/// document and may touch the key container; the answer is the only thing that
/// comes back.
async fn run_blocking<T, F>(body: F) -> Result<T, SettingsUiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, SettingsUiError> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(body).await {
        Ok(result) => result,
        Err(_) => Err(SettingsUiError::new(
            "INTERNAL",
            "Не удалось выполнить действие: рабочий поток недоступен.",
        )),
    }
}

/// Refuses a command that would change the draft or the files once the shell
/// has begun to leave: an edit accepted after the quit decision could neither
/// be saved nor reported, and the exit path must not race a writer. Both
/// latches count — the ordinary closing one and the installer's monotone one,
/// which no refusal may ever clear.
fn ensure_running(state: &PreferencesState) -> Result<(), SettingsUiError> {
    if state.closing.load(Ordering::SeqCst) || state.installer_closing.load(Ordering::SeqCst) {
        return Err(SettingsUiError::new(
            "CLOSING",
            "Приложение завершает работу, изменение настроек недоступно.",
        ));
    }
    Ok(())
}

/* -------------------------------------------------------------------------- */
/* Command table: the draft                                                   */
/* -------------------------------------------------------------------------- */

/// Opens the window's draft, or answers with the live one. The window is
/// idempotent about it: a second open of a visible window never replaces a
/// draft the user is still editing, and a new draft is created only after the
/// previous one was closed.
#[tauri::command]
pub async fn settings_open(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<SettingsView, SettingsUiError> {
    check_window(&window)?;
    run_blocking(move || open_blocking(&app)).await
}

fn open_blocking(app: &AppHandle) -> Result<SettingsView, SettingsUiError> {
    let state = app.state::<PreferencesState>();
    let view = {
        let mut slot = state.draft.lock();
        match slot.as_ref() {
            Some(draft) => state.view(app, draft, SECTION_LAST.to_owned()),
            None => {
                let section = state.section_for_new_draft();
                let id = state.next_id.fetch_add(1, Ordering::SeqCst);
                let draft = SettingsDraft::new(id);
                let view = state.view(app, &draft, section);
                *slot = Some(draft);
                view
            }
        }
    };
    // The page registers its listeners before it calls this command, so this is
    // the moment the shell may deliver what was waiting for a listener: a
    // section a tray activation asked for, or a close the user asked for while
    // the page was still loading. The call is outside the draft lock and the
    // shell does the emitting itself.
    crate::settings_ui_ready(app);
    Ok(view)
}

/// The slice of the general form the page may change. The chord is absent on
/// purpose: it is taken from the running configuration, so a window left open
/// across a chord change cannot put the old one back. The retired `compare_all`
/// is absent too - a payload that still carries it is read as an unknown field
/// and ignored. Every managed value the general section owns is part of the
/// page's contract: the command refuses a payload that omits the mute switch,
/// the port or the chosen device instead of reading a value the page never sent
/// as `false`, as a port nobody chose or as "system default".
#[derive(Deserialize)]
pub struct GeneralSettings {
    pub mode: String,
    pub mute_during_recording: bool,
    /// The port the document should name for the next start, in `1..=65535`.
    pub port: u16,
    /// The chosen input device id, or `null` for the system default device.
    /// `null` is a value of the contract; an omitted field is refused.
    #[serde(deserialize_with = "input_device_choice")]
    pub input_device: Option<String>,
}

/// Reads the general form's `input_device`: `null` is the system default
/// device, a string is the chosen id. The field has to be there - with the
/// custom reader serde treats it as required - so a payload that drops it is
/// refused rather than read as the system device, which would silently undo a
/// choice the user made.
fn input_device_choice<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ChoiceVisitor;

    impl<'de> serde::de::Visitor<'de> for ChoiceVisitor {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("null or an input device id string")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            String::deserialize(deserializer).map(Some)
        }
    }

    deserializer.deserialize_option(ChoiceVisitor)
}

/// The general form's port, as the document will carry it: a whole number in
/// `1..=65535`. Deserialization has already refused a fraction, a string and
/// anything above the range, so zero is the one value left to refuse here.
fn usable_port(port: u16) -> Result<u16, SettingsUiError> {
    if port == 0 {
        return Err(SettingsUiError::field(
            "PORT_INVALID",
            PORT_INVALID_MESSAGE,
            "port",
        ));
    }
    Ok(port)
}

/// The general form's device, checked with the same rule the settings document
/// is read with: `None` is the system default device, and an id has to be one
/// `cpal::DeviceId` can parse back. A choice the next read would refuse can
/// never be written.
fn usable_input_device(device: Option<String>) -> Result<Option<String>, SettingsUiError> {
    match device {
        None => Ok(None),
        Some(id) => settings::usable_device_id(&id).map(Some).ok_or_else(|| {
            SettingsUiError::field(
                "INPUT_DEVICE_INVALID",
                INPUT_DEVICE_INVALID_MESSAGE,
                "input_device",
            )
        }),
    }
}

/// Applies the general form at once. The revision the page worked on has to be
/// the current one, so an answer that raced a clear or a chord change is
/// refused instead of overwriting what came after it.
#[tauri::command]
pub async fn settings_update_general(
    window: WebviewWindow,
    app: AppHandle,
    draft_id: u64,
    revision: u64,
    settings: GeneralSettings,
) -> Result<SaveResult, SettingsUiError> {
    check_window(&window)?;
    run_blocking(move || update_general_blocking(&app, draft_id, revision, settings)).await
}

fn update_general_blocking(
    app: &AppHandle,
    draft_id: u64,
    revision: u64,
    request: GeneralSettings,
) -> Result<SaveResult, SettingsUiError> {
    let state = app.state::<PreferencesState>();
    let inner = crate::inner(app);
    // The same lock order every apply uses: a mode change is a write, and it
    // must not run beside a dictation that is pinning the configuration.
    let _session = inner.session.lock();
    ensure_running(&state)?;
    // The page's values are checked before anything is planned for the draft:
    // a port or a device the document would refuse never reaches the draft,
    // the files or the runtime.
    let port = usable_port(request.port)?;
    let input_device = usable_input_device(request.input_device)?;
    let snapshot = inner.runtime.snapshot();
    let planned = Settings {
        hotkey: snapshot.settings.hotkey.clone(),
        mode: request.mode,
        mute_during_recording: request.mute_during_recording,
        port,
        input_device,
    };
    let mut slot = state.draft.lock();
    let draft = slot.as_mut().ok_or_else(SettingsUiError::stale_draft)?;
    if draft.id != draft_id || draft.revision != revision {
        return Err(SettingsUiError::stale_draft());
    }
    apply_locked(app, &state, draft, planned, ApplyScope::General, false)
}

/* -------------------------------------------------------------------------- */
/* Command table: input devices                                               */
/* -------------------------------------------------------------------------- */

/// The input devices the host offers, for the general form's picker. This is
/// the only command that reads hardware, it is reachable only from the settings
/// window, and it is never served over HTTP. The saved choice is not written
/// here: a device that is gone shows up as an entry the page marks unavailable,
/// and the document keeps the id until the user picks another one.
#[tauri::command]
pub async fn settings_list_input_devices(
    window: WebviewWindow,
    app: AppHandle,
) -> Result<Vec<InputDeviceView>, SettingsUiError> {
    check_window(&window)?;
    run_blocking(move || list_input_devices(&app)).await
}

/// Reads the host's input devices for the settings window. A quit that has
/// already closed over the shell refuses the read instead of handing a window a
/// list it can no longer act on.
fn list_input_devices(app: &AppHandle) -> Result<Vec<InputDeviceView>, SettingsUiError> {
    let state = app.state::<PreferencesState>();
    ensure_running(&state)?;
    let host = cpal::default_host();
    let default_id = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let devices = host.input_devices().map_err(|_| list_failure())?;
    let mut enumerated = Vec::new();
    for device in devices {
        let id = device.id().map_err(|_| list_failure())?.to_string();
        let name = device
            .description()
            .map_err(|_| list_failure())?
            .name()
            .to_owned();
        enumerated.push((id, name));
    }
    Ok(describe_input_devices(enumerated, default_id.as_deref()))
}

/// A fixed failure for an enumeration the host refused. The operating system's
/// own text may name endpoints, so it never reaches the page.
fn list_failure() -> SettingsUiError {
    SettingsUiError::new("INPUT_DEVICES_UNAVAILABLE", INPUT_DEVICES_UNAVAILABLE_MESSAGE)
}

/// Turns enumerated `(id, name)` pairs into the window's list: the default
/// device is flagged by id, and the list is sorted by display name with the id
/// as a tie-breaker, so two devices that share a name keep both entries in one
/// deterministic order instead of one overwriting the other. An empty
/// enumeration is an empty list, not a failure.
fn describe_input_devices(
    devices: Vec<(String, String)>,
    default_id: Option<&str>,
) -> Vec<InputDeviceView> {
    let mut listed: Vec<InputDeviceView> = devices
        .into_iter()
        .map(|(id, name)| InputDeviceView {
            is_default: default_id == Some(id.as_str()),
            id,
            name,
        })
        .collect();
    listed.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    listed
}

/// Applies the chord the page typed into its hotkey field: validates the text
/// against the document's own grammar, registers the chord with its gate still
/// closed, writes the document and reads it back, and only then publishes the
/// runtime revision and opens the new gate. The mode, the mute switch and the
/// key list are the running ones, never a page's copy; a typed key draft is
/// neither applied nor cleared, and the launcher slot is repaired even when the
/// chord is the one that is already saved.
#[tauri::command]
pub async fn settings_update_hotkey(
    window: WebviewWindow,
    app: AppHandle,
    draft_id: u64,
    revision: u64,
    hotkey: String,
) -> Result<SaveResult, SettingsUiError> {
    check_window(&window)?;
    run_blocking(move || update_hotkey_blocking(&app, draft_id, revision, hotkey)).await
}

fn update_hotkey_blocking(
    app: &AppHandle,
    draft_id: u64,
    revision: u64,
    hotkey: String,
) -> Result<SaveResult, SettingsUiError> {
    let state = app.state::<PreferencesState>();
    let inner = crate::inner(app);
    // The same lock order every apply uses: a chord change is a write, and it
    // must not run beside a dictation that is pinning the configuration.
    let _session = inner.session.lock();
    ensure_running(&state)?;
    let snapshot = inner.runtime.snapshot();
    let planned = Settings {
        hotkey,
        mode: snapshot.settings.mode.clone(),
        mute_during_recording: snapshot.settings.mute_during_recording,
        port: snapshot.settings.port,
        input_device: snapshot.settings.input_device.clone(),
    };
    let mut slot = state.draft.lock();
    let draft = slot.as_mut().ok_or_else(SettingsUiError::stale_draft)?;
    if draft.id != draft_id || draft.revision != revision {
        return Err(SettingsUiError::stale_draft());
    }
    apply_locked(app, &state, draft, planned, ApplyScope::Hotkey, false)
}

/// Records whether the settings window's chord field holds the keyboard.
///
/// The page reports when the field gains or loses the focus, when its window
/// comes back in front and when it goes away. Every report carries the identity
/// of the document that sent it — read from the shell with a handshake before
/// the first report — and a number of its own, so two fire-and-forget reports
/// that arrive in either order can only ever write the newer state, and a
/// signal that was still in flight when the page reloaded cannot move the field
/// of the document that replaced it. The command answers with that identity:
/// it is what the page tags its following reports with.
///
/// The flag only gates a new take, so a release is always taken. A signal that
/// arrives late — after the window was hidden or destroyed — has to be able to
/// clear it, and a report of a window that is not visible can only release:
/// nothing off screen holds a keyboard, and a stale `true` must not silence the
/// launcher it no longer owns.
#[tauri::command]
pub async fn settings_hotkey_capture(
    window: WebviewWindow,
    app: AppHandle,
    document: u64,
    sequence: u64,
    active: bool,
) -> Result<u64, SettingsUiError> {
    check_window(&window)?;
    // Read before any lock of the report stream: the window getter talks to the
    // main thread, and no lock may be held while it does. The stream itself is
    // guarded by its own leaf lock, not the session lock — the window's events
    // run on the main thread a save worker may be waiting for while it holds
    // the session lock.
    let visible = window.is_visible().unwrap_or(false);
    let inner = crate::inner(&app);
    Ok(crate::record_hotkey_field_report(
        &inner, document, sequence, active, visible,
    ))
}

/// Takes the replacement key list from the page, exactly as typed. The text is
/// moved into a zeroizing buffer at once; an unusable list stays in the draft
/// so the user can repair it, and the view reports the line it failed on.
#[tauri::command]
pub async fn settings_set_keys(
    window: WebviewWindow,
    app: AppHandle,
    draft_id: u64,
    revision: u64,
    text: String,
) -> Result<SettingsView, SettingsUiError> {
    // The text is a key list before anything else is decided about it, so even
    // a refused command clears it from memory.
    let text = Zeroizing::new(text);
    check_window(&window)?;
    run_blocking(move || set_keys_blocking(&app, draft_id, revision, text)).await
}

fn set_keys_blocking(
    app: &AppHandle,
    draft_id: u64,
    revision: u64,
    text: Zeroizing<String>,
) -> Result<SettingsView, SettingsUiError> {
    let state = app.state::<PreferencesState>();
    let mut slot = state.draft.lock();
    ensure_running(&state)?;
    let draft = slot.as_mut().ok_or_else(SettingsUiError::stale_draft)?;
    if draft.id != draft_id || draft.revision != revision {
        return Err(SettingsUiError::stale_draft());
    }
    // An empty edit is not a clear: clearing has its own confirmation.
    if !text.trim().is_empty() {
        draft.clear_confirmed = false;
    }
    draft.raw_keys = Some(text);
    draft.revision += 1;
    Ok(state.view(app, draft, SECTION_LAST.to_owned()))
}

/// Reveals the full key list - the draft's replacement if it has one, otherwise
/// the saved one. This is the only command that returns key values, and it
/// answers the key section alone. A store that cannot be decrypted is refused
/// rather than shown as an empty list.
#[tauri::command]
pub async fn settings_reveal_keys(
    window: WebviewWindow,
    app: AppHandle,
    draft_id: u64,
) -> Result<String, SettingsUiError> {
    check_window(&window)?;
    run_blocking(move || reveal_keys_blocking(&app, draft_id)).await
}

fn reveal_keys_blocking(app: &AppHandle, draft_id: u64) -> Result<String, SettingsUiError> {
    let state = app.state::<PreferencesState>();
    let draft_keys = {
        let slot = state.draft.lock();
        let draft = slot.as_ref().ok_or_else(SettingsUiError::stale_draft)?;
        if draft.id != draft_id {
            return Err(SettingsUiError::stale_draft());
        }
        draft
            .raw_keys
            .as_deref()
            .map(|text| Zeroizing::new(text.to_owned()))
    };
    if let Some(raw) = draft_keys {
        // The only copy that leaves this module is the answer itself, which the
        // page asked for by name.
        return Ok(raw.as_str().to_owned());
    }
    if state.secrets_status() == "unavailable" {
        return Err(SettingsUiError::new(
            "SECRETS_UNAVAILABLE",
            SECRETS_UNAVAILABLE_MESSAGE,
        ));
    }
    // An apply that could not be rolled back may have left the container
    // holding something the runtime is not using; until the next apply
    // reconciles them, the list in effect is the only true answer to "which
    // keys are saved".
    let keys: Vec<Arc<SecretKey>> = if state.secrets_reconciliation_owed() {
        state.runtime.snapshot().keys.keys().to_vec()
    } else {
        match secrets::load_secret_keys(&state.secrets_path) {
            Ok(ring) => ring.keys().to_vec(),
            Err(error) if error.is_missing() => Vec::new(),
            Err(_) => {
                return Err(SettingsUiError::new(
                    "SECRETS_UNAVAILABLE",
                    SECRETS_UNAVAILABLE_MESSAGE,
                ));
            }
        }
    };
    let capacity =
        keys.iter().map(|key| key.as_str().len()).sum::<usize>() + keys.len().saturating_sub(1);
    let mut text = Zeroizing::new(String::with_capacity(capacity));
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(key.as_str());
    }
    Ok(text.as_str().to_owned())
}

/// Clears the draft's key list, but only against the confirmation that turns
/// dictation off until new keys are applied. The saved list is untouched until
/// the draft is applied.
#[tauri::command]
pub async fn settings_clear_keys(
    window: WebviewWindow,
    app: AppHandle,
    draft_id: u64,
    confirmed: bool,
) -> Result<SettingsView, SettingsUiError> {
    check_window(&window)?;
    run_blocking(move || clear_keys_blocking(&app, draft_id, confirmed)).await
}

fn clear_keys_blocking(
    app: &AppHandle,
    draft_id: u64,
    confirmed: bool,
) -> Result<SettingsView, SettingsUiError> {
    if !confirmed {
        return Err(SettingsUiError::confirm_required(
            "Очистка списка отключит диктовку до сохранения новых ключей. Подтвердите очистку.",
        ));
    }
    let state = app.state::<PreferencesState>();
    let mut slot = state.draft.lock();
    ensure_running(&state)?;
    let draft = slot.as_mut().ok_or_else(SettingsUiError::stale_draft)?;
    if draft.id != draft_id {
        return Err(SettingsUiError::stale_draft());
    }
    draft.raw_keys = Some(Zeroizing::new(String::new()));
    draft.clear_confirmed = true;
    draft.revision += 1;
    Ok(state.view(app, draft, SECTION_LAST.to_owned()))
}

/// Closes the window. A key list that really differs from the saved one is not
/// thrown away by a close that did not say so: viewing the saved keys, or
/// retyping them, never blocks a close. The close is one transition: the draft
/// is taken out of the state before the decision is made, so no command can
/// slip in between.
#[tauri::command]
pub async fn settings_close(
    window: WebviewWindow,
    app: AppHandle,
    draft_id: u64,
    discard: bool,
    for_quit: bool,
) -> Result<(), SettingsUiError> {
    check_window(&window)?;
    run_blocking(move || close_blocking(&app, draft_id, discard, for_quit)).await
}

fn close_blocking(
    app: &AppHandle,
    draft_id: u64,
    discard: bool,
    for_quit: bool,
) -> Result<(), SettingsUiError> {
    let state = app.state::<PreferencesState>();
    {
        // One critical section: the draft is only taken out when the close is
        // really going through, so a concurrent open cannot create a second
        // draft behind a refusal.
        let mut slot = state.draft.lock();
        let draft = slot.as_ref().ok_or_else(SettingsUiError::stale_draft)?;
        if draft.id != draft_id {
            return Err(SettingsUiError::stale_draft());
        }
        if state.keys_need_decision(draft) && !discard {
            return Err(SettingsUiError::new(
                "UNSAVED_CHANGES",
                "Есть неприменённые ключи API.",
            ));
        }
        slot.take();
    }
    if !for_quit {
        // A close hides the window. A quit leaves it visible until the native
        // action really leaves, so a refusal can still put its warning in front
        // of the user instead of into a hidden WebView.
        crate::request_settings(app, "close");
    }
    Ok(())
}

/// The window's two actions: opening the comparison window, and leaving the
/// application. Neither bypasses the draft: leaving refuses an unapplied key
/// draft, and an apply that could not be rolled back needs its own
/// acknowledgement.
#[tauri::command]
pub async fn settings_action(
    window: WebviewWindow,
    app: AppHandle,
    action: String,
    acknowledged_partial: Option<bool>,
) -> Result<(), SettingsUiError> {
    check_window(&window)?;
    run_blocking(move || action_blocking(&app, action, acknowledged_partial)).await
}

fn action_blocking(
    app: &AppHandle,
    action: String,
    acknowledged_partial: Option<bool>,
) -> Result<(), SettingsUiError> {
    let acknowledged = acknowledged_partial.unwrap_or(false);
    match action.as_str() {
        "lab" => {
            crate::open_lab(app);
            Ok(())
        }
        "quit" => quit_guarded(app, acknowledged),
        _ => Err(SettingsUiError::field(
            "INVALID_ACTION",
            "Неизвестное действие.",
            "action",
        )),
    }
}

/// The one way the shell leaves, whichever side asked for it.
///
/// The decision is taken under the same locks an apply uses: the latch goes up
/// first, the session lock is the barrier that waits for an apply that is
/// already running, and the key draft and a pending partial persistence are read
/// while it is held. Nothing can start an apply after this point, so the answer
/// cannot go stale.
///
/// The latch has exactly one owner: the call that takes it is the only one that
/// may clear it again on a refusal. A second quit that arrives meanwhile is
/// refused instead, because two callers clearing each other's latch would open a
/// window between a validation and the exit it approved.
fn quit_guarded(app: &AppHandle, acknowledged: bool) -> Result<(), SettingsUiError> {
    let state = app.state::<PreferencesState>();
    if state
        .closing
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(SettingsUiError::new(
            "CLOSING",
            "Завершение уже выполняется.",
        ));
    }
    {
        let inner = crate::inner(app);
        let _session = inner.session.lock();
        let unsaved_keys = {
            let slot = state.draft.lock();
            slot.as_ref()
                .is_some_and(|draft| state.keys_need_decision(draft))
        };
        if unsaved_keys {
            state.closing.store(false, Ordering::SeqCst);
            // The window is asked back before the refusal is answered: a quit
            // that comes from outside the window must not leave its answer with
            // no visible window to show it in.
            crate::request_settings(app, "last");
            return Err(SettingsUiError::new(
                "UNSAVED_CHANGES",
                "Есть неприменённые ключи API: примените их или откажитесь от них.",
            ));
        }
        if state.partial_persistence() && !acknowledged {
            state.closing.store(false, Ordering::SeqCst);
            crate::request_settings(app, "last");
            return Err(SettingsUiError::new("SAVE_PARTIAL", SAVE_PARTIAL_MESSAGE));
        }
    }
    // The latch stays up across the gap, and the session lock is released before
    // the exit path takes it itself; the latch is what keeps an apply out of the
    // window between the two.
    crate::quit_from_settings(app);
    Ok(())
}

/// Leaves the application from outside the settings window: the tray's "Выход".
///
/// A window or a key draft that exists is always sent through the page, because
/// the page may hold text the native draft has not seen yet - a draft with no
/// unapplied change of its own still owes the page that chance. Only a shell
/// with no draft at all and nothing owed to the disk leaves immediately, through
/// the same barrier the command uses.
pub(crate) fn request_quit(app: &AppHandle) {
    let state = app.state::<PreferencesState>();
    {
        let inner = crate::inner(app);
        // The same lock order every apply uses: the session first, then the
        // draft. Nothing can create a draft or start an apply under this read.
        let _session = inner.session.lock();
        if state.draft.lock().is_some() || state.partial_persistence() {
            drop(_session);
            crate::request_settings_quit(app);
            return;
        }
    }
    if let Err(error) = quit_guarded(app, false) {
        eprintln!("speechek: quit refused: {error}");
    }
}

/// The two latch writes of an exit the installer asked for, in one step: the
/// ordinary closing latch and the installer's own monotone companion go up
/// before the exit does anything else, because every command that could write
/// has to see them from the first moment.
fn installer_quit_latch(state: &PreferencesState) {
    state.installer_closing.store(true, Ordering::SeqCst);
    state.closing.store(true, Ordering::SeqCst);
}

/// Latches the installer's exit and then waits for the session lock, in that
/// order: an apply that already runs finishes under the lock — the same barrier
/// every writer takes — while one that arrives after the latch is refused by
/// [`ensure_running`] instead of queueing behind this wait. The caller holds
/// the returned guard for the whole bounded cleanup.
fn installer_exit_guard<'a>(
    state: &PreferencesState,
    inner: &'a crate::Inner,
) -> parking_lot::MutexGuard<'a, ()> {
    installer_quit_latch(state);
    inner.session.lock()
}

/// Leaves the application on the installer's request.
///
/// This is not [`request_quit`]: the installer has already confirmed that an
/// unapplied draft and an unfinished hand-over may be lost, and it waits with a
/// deadline of its own. So this path shows no window, asks no page, keeps no
/// draft and never waits on a capture that may be winding down; it latches the
/// shell first (nothing can write after that), waits for an apply that is
/// already running — the session lock is the same barrier every writer takes —
/// cancels whatever dictation exists without waiting for its renderer or its
/// driver, drops the draft in the same session-then-draft order every writer
/// uses, and only then requests the exit.
///
/// Monotonic on purpose: a late interactive quit may refuse and clear its own
/// latch, but never the installer's, so no apply can slip in behind this
/// decision. A repeated request is idempotent beyond repeating the same
/// bounded cleanup; it still reaches the exit even when the tray already took
/// the other latch.
pub(crate) fn request_installer_quit(app: &AppHandle) {
    let state = app.state::<PreferencesState>();
    let inner = crate::inner(app);
    installer_quit_latch(&state);
    inner.installer_quit.store(true, Ordering::SeqCst);
    inner.shutdown.store(true, Ordering::SeqCst);
    {
        // The latch above is up before this lock is even attempted: an apply
        // that is already running finishes under the lock, while one that
        // arrives later is refused by the latch instead of queueing here.
        let _session = installer_exit_guard(&state, &inner);
        crate::installer_exit_cleanup(app, &inner);
        // The unapplied draft goes in the order every writer reads it: session
        // first, draft second. It is discarded, not offered back — the
        // installer's warning already named this loss, and no second modal is
        // shown for it. `partial` is deliberately not cleared: a failure that
        // could not be rolled back is a fact about the files, not about the
        // window, and this exit must not claim it was reconciled.
        *state.draft.lock() = None;
    }
    app.exit(0);
}

/* -------------------------------------------------------------------------- */
/* Command table: the key check                                               */
/* -------------------------------------------------------------------------- */

/// Checks the draft's keys against the service, one at a time. Nothing is
/// saved and the rotation is not touched: the snapshot is taken from the draft
/// and each key is asked about exactly once, with the draft's identity checked
/// again before every request, so an answer that arrives after the list moved
/// on is dropped instead of reported.
#[tauri::command]
pub async fn settings_check_keys(
    window: WebviewWindow,
    app: AppHandle,
    draft_id: u64,
    revision: u64,
) -> Result<KeyCheckReport, SettingsUiError> {
    check_window(&window)?;
    let state = app.state::<PreferencesState>();
    if state.check_busy.swap(true, Ordering::SeqCst) {
        return Err(SettingsUiError::new(
            "CHECK_IN_PROGRESS",
            "Проверка ключей уже выполняется.",
        ));
    }
    let _ticket = CheckTicket(&state.check_busy);

    let keys = check_keys(&state, draft_id, revision)?;
    let mut results = Vec::with_capacity(keys.len());
    for (key, line) in keys {
        // The list may have been edited, cleared, saved or closed while earlier
        // checks were in flight; those answers are no longer about this draft.
        if !draft_matches(&state, draft_id, revision) {
            return Ok(stale_report(draft_id, revision));
        }
        let (status, message) = match state.provider.check_key(key.as_ref()).await {
            KeyCheckOutcome::Ok => ("ok", "Доступ к API подтверждён".to_owned()),
            KeyCheckOutcome::Denied => ("denied", "Доступ запрещён".to_owned()),
            KeyCheckOutcome::Indeterminate(reason) => ("indeterminate", reason.message()),
        };
        results.push(KeyCheckEntry {
            line: line as u64,
            status: status.to_owned(),
            message,
        });
    }
    if !draft_matches(&state, draft_id, revision) {
        return Ok(stale_report(draft_id, revision));
    }
    Ok(KeyCheckReport {
        draft_id,
        revision,
        stale: false,
        results,
    })
}

/// Holds the one-check-at-a-time marker for as long as the check runs and
/// releases it on every path out, including an early return and a panic-free
/// drop.
struct CheckTicket<'a>(&'a AtomicBool);

impl Drop for CheckTicket<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn stale_report(draft_id: u64, revision: u64) -> KeyCheckReport {
    KeyCheckReport {
        draft_id,
        revision,
        stale: true,
        results: Vec::new(),
    }
}

/// The keys this draft would be checked against: the typed list when it has
/// one, otherwise the saved ring. The physical lines travel with the keys, so a
/// result points at the line the page shows. An unreadable container is an
/// error rather than an empty list.
fn check_keys(
    state: &PreferencesState,
    draft_id: u64,
    revision: u64,
) -> Result<Vec<(Arc<SecretKey>, usize)>, SettingsUiError> {
    let slot = state.draft.lock();
    let draft = slot.as_ref().ok_or_else(SettingsUiError::stale_draft)?;
    if draft.id != draft_id || draft.revision != revision {
        return Err(SettingsUiError::stale_draft());
    }
    match draft.raw_keys.as_deref() {
        Some(raw) => {
            let normalized = secrets::normalize_key_text(raw).map_err(|error| {
                SettingsUiError::line("KEY_LIST_INVALID", error.message(), error.line() as u64)
            })?;
            Ok(normalized
                .keys()
                .iter()
                .cloned()
                .zip(normalized.lines().iter().copied())
                .collect())
        }
        None => {
            if state.secrets_status() == "unavailable" {
                return Err(SettingsUiError::new(
                    "SECRETS_UNAVAILABLE",
                    SECRETS_UNAVAILABLE_MESSAGE,
                ));
            }
            // The saved list is compact by construction, so its lines are the
            // positions the container holds.
            let snapshot = state.runtime.snapshot();
            Ok(snapshot
                .keys
                .keys()
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, key)| (key, index + 1))
                .collect())
        }
    }
}

/// Whether the draft the caller names is still the one the state holds, on the
/// revision the check started with.
fn draft_matches(state: &PreferencesState, draft_id: u64, revision: u64) -> bool {
    let slot = state.draft.lock();
    slot.as_ref()
        .is_some_and(|draft| draft.id == draft_id && draft.revision == revision)
}

/* -------------------------------------------------------------------------- */
/* Command table: applying configuration                                        */
/* -------------------------------------------------------------------------- */

/// Applies the page's key list: validates it, writes the container, reads it
/// back, and publishes exactly one runtime revision. Only this explicit apply
/// can change the keys; the general form never applies a list.
#[tauri::command]
pub async fn settings_apply_keys(
    window: WebviewWindow,
    app: AppHandle,
    draft_id: u64,
    revision: u64,
    confirmed_replace: bool,
) -> Result<SaveResult, SettingsUiError> {
    check_window(&window)?;
    run_blocking(move || apply_keys_blocking(&app, draft_id, revision, confirmed_replace)).await
}

fn apply_keys_blocking(
    app: &AppHandle,
    draft_id: u64,
    revision: u64,
    confirmed_replace: bool,
) -> Result<SaveResult, SettingsUiError> {
    let state = app.state::<PreferencesState>();
    let inner = crate::inner(app);
    // The apply runs with the session lock held: a dictation that starts while
    // the key ring is being replaced would pin the very list this apply is
    // about to publish.
    let _session = inner.session.lock();
    // The quit path takes this same lock to read the draft, and latches the
    // state before it lets go again: an apply that was already waiting here
    // must not write after that decision.
    ensure_running(&state)?;
    let snapshot = inner.runtime.snapshot();
    let mut slot = state.draft.lock();
    let draft = slot.as_mut().ok_or_else(SettingsUiError::stale_draft)?;
    if draft.id != draft_id || draft.revision != revision {
        return Err(SettingsUiError::stale_draft());
    }
    apply_locked(
        app,
        &state,
        draft,
        snapshot.settings.clone(),
        ApplyScope::Keys,
        confirmed_replace,
    )
}

/// The configuration one scope is about to apply: each scope owns its slice and
/// takes every other value from the running revision, never from a page that
/// may have gone stale. The chord is validated here as well, because a chord
/// that only the hotkey scope may change still has to be one the grammar can
/// express.
///
/// The port and the chosen device are ordinary values of this rule: a general
/// change carries them from the page, while a chord or key change keeps the
/// ones that are running, so no other form can silently undo a port or a
/// microphone the user chose.
fn planned_for_scope(
    scope: ApplyScope,
    planned: Settings,
    snapshot: &Settings,
    config_path: &Path,
) -> Result<Settings, SettingsUiError> {
    Ok(match scope {
        ApplyScope::General => Settings {
            hotkey: snapshot.hotkey.clone(),
            mode: planned.mode,
            mute_during_recording: planned.mute_during_recording,
            port: planned.port,
            input_device: planned.input_device,
        },
        ApplyScope::Keys => snapshot.clone(),
        ApplyScope::Hotkey => Settings {
            hotkey: settings::validate_hotkey(&planned.hotkey, config_path).map_err(|_| {
                SettingsUiError::field("HOTKEY_INVALID", HOTKEY_INVALID_MESSAGE, "hotkey")
            })?,
            mode: snapshot.mode.clone(),
            mute_during_recording: snapshot.mute_during_recording,
            port: snapshot.port,
            input_device: snapshot.input_device.clone(),
        },
    })
}

/// The one writer: applies one scope of the configuration - validates what that
/// scope owns against the running revision, writes the key container and then
/// the settings document, reads both back, and publishes exactly one runtime
/// revision. A failure that could not be rolled back becomes
/// `partial_persistence` rather than a silent half-state.
///
/// The caller holds the session lock and the draft lock, so a dictation cannot
/// start, no other writer can interleave, and the chord a hotkey change is
/// replacing cannot be given away underneath it.
fn apply_locked(
    app: &AppHandle,
    state: &PreferencesState,
    draft: &mut SettingsDraft,
    planned: Settings,
    scope: ApplyScope,
    confirmed_replace: bool,
) -> Result<SaveResult, SettingsUiError> {
    ensure_running(state)?;
    let inner = crate::inner(app);
    let snapshot = inner.runtime.snapshot();

    // Everything that can be decided without touching anything is decided here:
    // the mode, the chord grammar, the key list, and the confirmations a
    // destructive step needs. Each scope owns one slice and takes the rest from
    // the running revision, never from a page that may have gone stale.
    let planned = planned_for_scope(scope, planned, &snapshot.settings, &state.config_path)?;
    if !MODES.contains(&planned.mode.as_str()) {
        return Err(SettingsUiError::field(
            "INVALID_MODE",
            "Выберите режим распознавания: live, smart или verbatim.",
            "mode",
        ));
    }

    // Whether the wanted chord still has to be registered. A chord is compared
    // by what it registers, not by how it is spelled, so choosing the running
    // chord again is not work - unless the launcher slot is empty, which a
    // hotkey change is allowed to repair even for the same key. A mode or key
    // change never touches the chord, so it never takes this path.
    let registration_needed =
        scope == ApplyScope::Hotkey && !crate::settings_hotkey_is_current(app, &planned.hotkey);
    // A chord change is refused whole while a dictation is running: nothing is
    // half-applied and the take keeps the configuration it started with. Every
    // other scope - a mode, a key list, a close - is served during a take.
    if registration_needed && crate::phase_of(&inner).is_active() {
        return Err(SettingsUiError::new(
            "DICTATION_ACTIVE",
            DICTATION_ACTIVE_MESSAGE,
        ));
    }

    let (previous_vault, reconcile_secrets) = {
        let partial = state.partial.lock();
        match partial.as_ref() {
            // The before-image belongs to the container role: an obligation that
            // does not owe the container carries none, so a settings-only
            // failure cannot hand this transaction a container state it never
            // observed.
            Some(flag) => (flag.vault_before.clone(), flag.secrets),
            None => (None, false),
        }
    };
    // The page's typed list, in the shape the container holds. Only the key
    // scope ever looks at it: a mode or chord change must not be able to apply
    // or reject a list the page happened to leave in the draft.
    let typed_keys: Option<Vec<Arc<SecretKey>>> = if scope == ApplyScope::Keys {
        match draft.raw_keys.as_deref() {
            Some(raw) => Some(
                secrets::normalize_key_text(raw)
                    .map_err(|error| {
                        SettingsUiError::line(
                            "KEY_LIST_INVALID",
                            error.message(),
                            error.line() as u64,
                        )
                    })?
                    .keys()
                    .to_vec(),
            ),
            None => None,
        }
    } else {
        None
    };

    let plan = match scope {
        ApplyScope::Keys => match typed_keys {
            Some(typed) => {
                if state.secrets_status() == "unavailable" {
                    // The container cannot be read, and the running ring is
                    // empty only because of that. Comparing the typed list with
                    // it would call a clear "nothing to do" and leave an
                    // unknown list in place, so any list - including an empty
                    // one - is an explicit replacement here, and it needs its
                    // own confirmation.
                    if !confirmed_replace {
                        return Err(SettingsUiError::confirm_required(
                            "Прежний список ключей прочитать не удалось, он будет заменён новым. Подтвердите замену.",
                        ));
                    }
                    KeyPlan::Replace(typed)
                } else if secrets::list_equal(&typed, snapshot.keys.keys()) {
                    if reconcile_secrets {
                        repair_plan(&previous_vault, typed)
                    } else {
                        KeyPlan::Keep
                    }
                } else if typed.is_empty() && !(draft.clear_confirmed || confirmed_replace) {
                    return Err(SettingsUiError::confirm_required(
                        "Очистка списка отключит диктовку до сохранения новых ключей. Подтвердите очистку.",
                    ));
                } else {
                    KeyPlan::Replace(typed)
                }
            }
            None => {
                if reconcile_secrets {
                    repair_plan(&previous_vault, snapshot.keys.keys().to_vec())
                } else {
                    KeyPlan::Keep
                }
            }
        },
        // A mode or chord applies the running ring, or the write a previous
        // failure owed the disk. The typed list is not a desired state here.
        ApplyScope::General | ApplyScope::Hotkey => {
            if reconcile_secrets {
                repair_plan(&previous_vault, snapshot.keys.keys().to_vec())
            } else {
                KeyPlan::Keep
            }
        }
    };
    let desired_keys: Vec<Arc<SecretKey>> = match &plan {
        KeyPlan::Replace(keys) => keys.clone(),
        KeyPlan::Keep | KeyPlan::RestoreOpaque(_) | KeyPlan::RestoreAbsent => {
            snapshot.keys.keys().to_vec()
        }
    };

    // Nothing to write, register or reconcile: the running configuration is
    // already what was asked for. An explicit apply of the list that is already
    // saved still consumes the draft and moves the revision, so a check that was
    // already answered cannot describe the draft it came from; a resend of the
    // chord that is already running - or of its equivalent spelling - is not a
    // change and moves nothing. No byte is rewritten and the ring keeps its
    // cursor either way.
    let same_config = planned == snapshot.settings
        || (scope == ApplyScope::Hotkey
            && !registration_needed
            && planned.mode == snapshot.settings.mode
            && planned.mute_during_recording == snapshot.settings.mute_during_recording);
    if !plan.writes_container()
        && !state.partial_persistence()
        && same_config
        && !registration_needed
    {
        let consumed_keys = scope == ApplyScope::Keys && draft.raw_keys.is_some();
        if consumed_keys {
            draft.raw_keys = None;
            draft.clear_confirmed = false;
            draft.revision += 1;
        }
        let view = state.view(app, draft, SECTION_LAST.to_owned());
        return Ok(SaveResult {
            view,
            applied: true,
            warning: None,
        });
    }

    // The document is read and patched, and the container encoded, before
    // anything is written. Each failure is one of this module's fixed lines:
    // the library's own text names paths and property names, and the window
    // shows neither.
    let before_settings = settings::read_settings_text(&state.config_path, "файл настроек")
        .map_err(|_| {
            SettingsUiError::new(
                "SAVE_FAILED",
                "Не удалось прочитать файл настроек; изменения не сохранены.",
            )
        })?;
    let patched = settings::patch_settings_document(&before_settings, &planned)
        .map_err(|_| {
            SettingsUiError::new(
                "SAVE_FAILED",
                "Файл настроек нельзя изменить: в нём нет одного из полей Speechek или есть лишнее. Исправьте файл вручную и повторите.",
            )
        })?;
    let new_container = match &plan {
        KeyPlan::Replace(keys) => Some(Some(secrets::encode_secret_keys(keys).map_err(|_| {
            SettingsUiError::new(
                "SAVE_FAILED",
                "Не удалось зашифровать список ключей для текущего пользователя Windows.",
            )
        })?)),
        KeyPlan::RestoreOpaque(bytes) => Some(Some(bytes.clone())),
        // The reconciliation owes the disk the absence of a container, which is
        // not the same thing as leaving the file alone.
        KeyPlan::RestoreAbsent => Some(None),
        KeyPlan::Keep => None,
    };
    let before_container = if plan.writes_container() {
        match secrets::read_secret_container(&state.secrets_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.is_missing() => None,
            Err(_) => {
                return Err(SettingsUiError::new(
                    "SAVE_FAILED",
                    "Не удалось прочитать файл ключей; прежний список не изменён.",
                ));
            }
        }
    } else {
        None
    };
    let undo = PersistUndo {
        settings_before: before_settings.into_bytes(),
        vault_before: before_container,
        settings_replaced: false,
        secrets_replaced: false,
    };

    // A free candidate is registered only now, after every decision has been
    // made and before the first write: a chord another program owns leaves the
    // current registration, the files and the draft exactly as they were. Only
    // a hotkey change registers anything; a mode or key change leaves the
    // launcher exactly as it is, including one that could not start.
    let mut candidate = match scope {
        ApplyScope::Hotkey => crate::prepare_settings_hotkey(app, &planned.hotkey)?,
        ApplyScope::General | ApplyScope::Keys => None,
    };

    // The file transaction itself: container first, then the document, both
    // read back, and only the roles that were really written put back on a
    // failure. It takes no shell, so the regression tests drive it directly.
    let writes = FileWrites {
        settings: patched,
        container: new_container,
    };
    let context = FileContext {
        config_path: &state.config_path,
        secrets_path: &state.secrets_path,
        previous_vault: previous_vault.as_ref(),
        secrets_unreadable: state.secrets_status() == "unavailable",
    };
    let outcome = match transact_files(&context, &writes, &plan, &planned, undo) {
        Ok(outcome) => outcome,
        Err(failure) => return Err(abandon_save(app, &state, failure, candidate.take())),
    };
    let container_readable = outcome.container_verified;

    // One publish, after both files are known good. An identical list keeps the
    // ring the dictations in flight are already using, cursor and all.
    let keys: Arc<KeyRing> =
        if container_readable && !secrets::list_equal(&desired_keys, snapshot.keys.keys()) {
            Arc::new(KeyRing::new(desired_keys))
        } else {
            Arc::clone(&snapshot.keys)
        };
    let revision = snapshot.revision + 1;
    inner.runtime.publish(Arc::new(RuntimeSnapshot {
        settings: planned.clone(),
        keys,
        revision,
    }));

    // The runtime is on the new revision; only now may the old chord go.
    let warning = crate::commit_settings_hotkey(app, candidate.take());

    // The store's state is now a fact about the file that was just verified.
    match &plan {
        KeyPlan::Replace(_) => *state.secrets.lock() = SecretsState::Ready,
        KeyPlan::RestoreOpaque(_) => *state.secrets.lock() = SecretsState::Unavailable,
        KeyPlan::RestoreAbsent => *state.secrets.lock() = SecretsState::Missing,
        KeyPlan::Keep => {}
    }
    // Only the key scope consumes the page's draft; a mode or chord change
    // leaves whatever the page typed exactly where it was.
    if scope == ApplyScope::Keys {
        draft.raw_keys = None;
        draft.clear_confirmed = false;
    }
    draft.revision += 1;
    // Whatever a previous apply owed the disk has been rewritten and read back.
    *state.partial.lock() = None;

    let view = state.view(app, draft, SECTION_LAST.to_owned());
    let event = SettingsChanged {
        hotkey: view.settings.hotkey.clone(),
    };
    // The caller's draft lock is still held here, and the event is queued to the
    // overlay's webview rather than dispatched from this thread: a listener
    // cannot re-enter a settings command under this lock.
    if let Err(error) = app.emit_to(crate::OVERLAY_LABEL, EVENT_SETTINGS_CHANGED, event) {
        eprintln!("speechek: cannot reach the overlay about new settings: {error}.");
    }
    Ok(SaveResult {
        view,
        applied: true,
        warning,
    })
}

/// The container write a reconciliation owes the running ring: the list has to
/// end up on disk as the ring says, but a container that could not be read may
/// only be put back exactly as it was.
fn repair_plan(before: &Option<VaultBefore>, ring: Vec<Arc<SecretKey>>) -> KeyPlan {
    match before {
        Some(VaultBefore::Absent) => KeyPlan::RestoreAbsent,
        Some(VaultBefore::Opaque(bytes)) => KeyPlan::RestoreOpaque(bytes.clone()),
        Some(VaultBefore::Usable) | None => KeyPlan::Replace(ring),
    }
}

/// Gives up an apply that could not finish: the transaction has already put
/// back what it could, and what it could not becomes the state's problem.
///
/// An obligation that was already recorded stays recorded until an apply reads
/// the role back, so a failure that happened before the reconciliation ran does
/// not look like a repair.
fn abandon_save(
    app: &AppHandle,
    state: &PreferencesState,
    failure: FileFailure,
    candidate: Option<crate::RegisteredHotkey>,
) -> SettingsUiError {
    let chord = crate::rollback_settings_hotkey(app, candidate);
    let mut message = match failure.obligation {
        Some(_) => SAVE_PARTIAL_MESSAGE.to_owned(),
        None => failure.kind.message().to_owned(),
    };
    if let Some(chord) = chord {
        message.push(' ');
        message.push_str(&chord);
    }
    let Some(obligation) = failure.obligation else {
        return SettingsUiError::new("SAVE_FAILED", &message);
    };
    // What an earlier failed apply owed is still owed: this transaction did not
    // read those roles back, whatever it managed to put back itself.
    let mut slot = state.partial.lock();
    let merged = fold_obligation(slot.as_ref(), obligation);
    *slot = Some(merged);
    SettingsUiError::new("SAVE_PARTIAL", &message)
}

/* -------------------------------------------------------------------------- */
/* The file transaction                                                       */
/* -------------------------------------------------------------------------- */

/// The two files an apply owns, and what it knew about them before it started.
struct FileContext<'a> {
    config_path: &'a Path,
    secrets_path: &'a Path,
    /// The container state a previous, unresolved failure left behind.
    previous_vault: Option<&'a VaultBefore>,
    /// Whether the container could not be read before this transaction.
    secrets_unreadable: bool,
}

/// Everything a transaction will write, already encoded.
struct FileWrites {
    /// The document to publish.
    settings: String,
    /// The container role: `Some(Some(bytes))` writes those bytes,
    /// `Some(None)` removes the container, `None` leaves it alone.
    container: Option<Option<Vec<u8>>>,
}

/// What the files hold once the transaction is over.
#[derive(Debug)]
struct FileOutcome {
    /// The container was written and read back as the list that was intended.
    container_verified: bool,
}

/// Why a transaction stopped, in the wording the window uses. The library's own
/// text names paths and the operating system's reason; the page gets neither, so
/// every failure is reported as one of these fixed categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileFailureKind {
    /// The key container could not be written.
    ContainerWrite,
    /// The container could not be removed.
    ContainerRemove,
    /// The settings document could not be written.
    DocumentWrite,
    /// The container is not the one that was written.
    ContainerVerify,
    /// The document is not the one that was written.
    DocumentVerify,
}

impl FileFailureKind {
    /// The fixed Russian line for this category. No path, no key and no system
    /// message: the window only needs to know which half of the apply failed
    /// and that the previous configuration is still in effect.
    fn message(self) -> &'static str {
        match self {
            FileFailureKind::ContainerWrite => {
                "Не удалось записать файл ключей; приложение продолжает использовать прежнюю конфигурацию."
            }
            FileFailureKind::ContainerRemove => {
                "Не удалось удалить файл ключей; приложение продолжает использовать прежнюю конфигурацию."
            }
            FileFailureKind::DocumentWrite => {
                "Не удалось записать файл настроек; приложение продолжает использовать прежние настройки."
            }
            FileFailureKind::ContainerVerify => {
                "Записанный файл ключей не совпал с ожидаемым; прежняя конфигурация восстановлена."
            }
            FileFailureKind::DocumentVerify => {
                "Записанный файл настроек не совпал с ожидаемым; прежние настройки восстановлены."
            }
        }
    }
}

/// Why a transaction stopped. `obligation` is `None` when both files hold
/// exactly what they held before; otherwise it is what the next apply has to
/// reconcile.
#[derive(Debug)]
struct FileFailure {
    kind: FileFailureKind,
    obligation: Option<PartialPersistence>,
}

/// The file half of an apply: the container first, then the document, both read
/// back, and only the roles that were really written put back when anything
/// fails.
///
/// This is the whole transaction, and it takes no shell: [`apply_locked`] is
/// only its caller, and the regression tests drive it with real files, real
/// DPAPI and a temporary directory of their own.
fn transact_files(
    context: &FileContext<'_>,
    writes: &FileWrites,
    plan: &KeyPlan,
    planned: &Settings,
    mut undo: PersistUndo,
) -> Result<FileOutcome, FileFailure> {
    // A crash between the two writes leaves a document that explains the list
    // it names, so the container goes first.
    match &writes.container {
        Some(Some(bytes)) => match settings::atomic_replace(context.secrets_path, bytes, true) {
            Ok(()) => undo.secrets_replaced = true,
            Err(error) => {
                undo.secrets_replaced = error.replaced();
                return Err(failed_files(
                    context,
                    &undo,
                    FileFailureKind::ContainerWrite,
                ));
            }
        },
        Some(None) => {
            // A reconciliation owes the disk the absence of a container.
            match std::fs::remove_file(context.secrets_path) {
                Ok(()) => undo.secrets_replaced = true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(failed_files(
                        context,
                        &undo,
                        FileFailureKind::ContainerRemove,
                    ))
                }
            }
        }
        None => {}
    }
    match settings::atomic_replace(context.config_path, writes.settings.as_bytes(), false) {
        Ok(()) => {
            undo.settings_replaced = true;
        }
        Err(error) => {
            undo.settings_replaced = error.replaced();
            return Err(failed_files(context, &undo, FileFailureKind::DocumentWrite));
        }
    }

    // Both files are read back before anything believes them, and only the
    // roles this transaction wrote are read at all.
    let container_verified = match plan {
        KeyPlan::Keep => false,
        KeyPlan::RestoreAbsent => {
            if context.secrets_path.exists() {
                return Err(failed_files(
                    context,
                    &undo,
                    FileFailureKind::ContainerVerify,
                ));
            }
            false
        }
        KeyPlan::RestoreOpaque(bytes) => {
            let current = std::fs::read(context.secrets_path).unwrap_or_default();
            if current != *bytes {
                return Err(failed_files(
                    context,
                    &undo,
                    FileFailureKind::ContainerVerify,
                ));
            }
            false
        }
        KeyPlan::Replace(keys) => match secrets::load_secret_keys(context.secrets_path) {
            Ok(ring) if secrets::list_equal(ring.keys(), keys) => true,
            _ => {
                return Err(failed_files(
                    context,
                    &undo,
                    FileFailureKind::ContainerVerify,
                ));
            }
        },
    };
    if undo.settings_replaced {
        match settings::read_settings_document(context.config_path, "файл настроек") {
            Ok(saved) if saved == *planned => {}
            _ => {
                return Err(failed_files(
                    context,
                    &undo,
                    FileFailureKind::DocumentVerify,
                ));
            }
        }
    }
    Ok(FileOutcome { container_verified })
}

/// Rolls a stopped transaction back and describes what is left of it: `None`
/// when both files hold exactly what they held before this transaction ran.
fn failed_files(
    context: &FileContext<'_>,
    undo: &PersistUndo,
    kind: FileFailureKind,
) -> FileFailure {
    let (settings_ok, secrets_ok) =
        rollback_writes(context.config_path, context.secrets_path, undo);
    if settings_ok && secrets_ok {
        return FileFailure {
            kind,
            obligation: None,
        };
    }
    // Only a role that is really owed gets a before-image, and it comes from
    // whoever really saw the container: the obligation this transaction
    // inherited, or - when this failure is the first one to owe the container -
    // this transaction's own undo. An apply that never wrote the container has
    // no before-image to lend.
    let vault_before = if secrets_ok {
        None
    } else {
        Some(match context.previous_vault {
            Some(previous) => previous.clone(),
            None => match undo.vault_before.as_deref() {
                Some(bytes) if context.secrets_unreadable => VaultBefore::Opaque(bytes.to_vec()),
                Some(_) => VaultBefore::Usable,
                None => VaultBefore::Absent,
            },
        })
    };
    FileFailure {
        kind,
        obligation: Some(PartialPersistence {
            settings: !settings_ok,
            secrets: !secrets_ok,
            vault_before,
        }),
    }
}

/// Folds a new obligation into the one already recorded.
///
/// The before-image of the container is the one the *first* unresolved failure
/// that owed the container recorded, and a later failure's own bytes only say
/// what that unfinished write left behind - so they are taken only when the
/// recorded obligation did not owe the container at all. Keeping the first one
/// is also what makes an unreadable container restorable to the bytes it had
/// before anything was written to it.
fn fold_obligation(
    prior: Option<&PartialPersistence>,
    obligation: PartialPersistence,
) -> PartialPersistence {
    let mut settings = obligation.settings;
    let mut secrets = obligation.secrets;
    let mut vault_before = obligation.vault_before;
    if let Some(prior) = prior {
        settings |= prior.settings;
        if prior.secrets {
            vault_before = prior.vault_before.clone();
        }
        secrets |= prior.secrets;
    }
    PartialPersistence {
        settings,
        secrets,
        vault_before,
    }
}

/// Puts back only the roles this transaction replaced, in the opposite order. A
/// role whose write reported no replacement is left alone - and not even read,
/// because a destination that cannot be read is still one this transaction never
/// wrote.
fn rollback_writes(config_path: &Path, secrets_path: &Path, undo: &PersistUndo) -> (bool, bool) {
    let settings_ok = restore_role(
        config_path,
        &undo.settings_before,
        false,
        undo.settings_replaced,
    );
    let secrets_ok = match undo.vault_before.as_deref() {
        Some(bytes) => restore_role(secrets_path, bytes, true, undo.secrets_replaced),
        None => remove_role(secrets_path, undo.secrets_replaced),
    };
    (settings_ok, secrets_ok)
}

/// Puts one file back to `before` when this transaction replaced it, and reports
/// whether the file now holds the previous bytes.
///
/// `replaced` is the writer's own answer and the only thing consulted:
/// `atomic_replace` reports true exactly when the destination was already
/// replaced - including the case where its access list failed its check after
/// the move - and false when nothing was written at all, whether the write never
/// started or the destination was locked or unreadable. A destination is
/// therefore never *read* to guess whether it was touched: a file that cannot be
/// read is a file this transaction did not write, and the previous bytes it
/// still holds are the previous bytes.
///
/// A restore that fails is a failure the next apply has to reconcile, so a role
/// this transaction could not put back - or could not prove it put back - is
/// never reported as clean.
fn restore_role(path: &Path, before: &[u8], secret: bool, replaced: bool) -> bool {
    if !replaced {
        return true;
    }
    settings::atomic_replace(path, before, secret).is_ok()
}

/// Removes a container this apply created where there was none before.
fn remove_role(path: &Path, replaced: bool) -> bool {
    if !replaced {
        return true;
    }
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/* -------------------------------------------------------------------------- */
/* Tests                                                                      */
/* -------------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::os::windows::fs::OpenOptionsExt as _;

    /// A directory of its own for one test, removed with everything in it. The
    /// user's `%APPDATA%` is never touched.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "speechek-preferences-{name}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&path).expect("a scratch directory");
            Self(path)
        }

        fn config(&self) -> PathBuf {
            self.0.join("settings.json")
        }

        fn secrets(&self) -> PathBuf {
            self.0.join("secrets.bin")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A document with the byte order mark, a comment and CRLF, the way a
    /// hand-edited file may look, still carrying the retired root `compare_all`
    /// that the next save has to migrate away, so the patch runs on a real
    /// document.
    fn document(mode: &str) -> String {
        format!(
            "\u{feff}{{\r\n  // режим\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"{mode}\",\r\n  \"compare_all\": false\r\n}}\r\n"
        )
    }

    fn settings_of(mode: &str) -> Settings {
        Settings {
            hotkey: "F2".to_owned(),
            mode: mode.to_owned(),
            mute_during_recording: false,
            port: settings::DEFAULT_PORT,
            input_device: None,
        }
    }

    fn fake_keys(list: &str) -> Vec<Arc<SecretKey>> {
        secrets::normalize_key_text(list)
            .expect("a valid fake list")
            .keys()
            .to_vec()
    }

    /// A real container for a list of fake credentials, sealed for this user.
    fn container_of(list: &str) -> Vec<u8> {
        secrets::encode_secret_keys(&fake_keys(list)).expect("a container")
    }

    /// The undo record a transaction gets from the bytes on disk.
    fn undo_for(config: &Path, secrets_path: &Path) -> PersistUndo {
        PersistUndo {
            settings_before: fs::read(config).expect("the document bytes"),
            vault_before: fs::read(secrets_path).ok(),
            settings_replaced: false,
            secrets_replaced: false,
        }
    }

    /// Holds the document so that no replacement can be moved onto it.
    fn hold_document(config: &Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(config)
            .expect("the document to hold")
    }

    /// The key draft is a difference, not the presence of text. A list that was
    /// only viewed and hidden, or retyped exactly as it is saved, is not an
    /// unapplied change; a different list - including the clear of a non-empty
    /// one - is, and so is a list that cannot be used at all. The rule answers
    /// with a count and fixed text, so no key is ever printed here.
    #[test]
    fn a_key_draft_is_only_a_change_against_the_saved_list() {
        let saved = fake_keys("FAKE_KEY_ONE\nFAKE_KEY_TWO");

        // No field was opened: there is nothing to decide, readable store or
        // not.
        assert!(!read_key_draft(None, &saved, false).changed);
        assert!(!read_key_draft(None, &saved, true).changed);

        // The saved list revealed and hidden again, and the same list as it may
        // be retyped: CRLF, blank lines, surrounding blanks and a duplicate the
        // normal form drops.
        assert!(!read_key_draft(Some("FAKE_KEY_ONE\nFAKE_KEY_TWO"), &saved, false).changed);
        assert!(
            !read_key_draft(
                Some("\r\nFAKE_KEY_ONE\r\n\r\n  FAKE_KEY_TWO  \r\nFAKE_KEY_ONE\r\n"),
                &saved,
                false
            )
            .changed
        );

        // A different list, a reordering, and the clear of a non-empty list.
        assert!(read_key_draft(Some("FAKE_KEY_THREE"), &saved, false).changed);
        assert!(read_key_draft(Some("FAKE_KEY_TWO\nFAKE_KEY_ONE"), &saved, false).changed);
        assert!(read_key_draft(Some(""), &saved, false).changed);

        // Clearing a list that is already empty is not a change; the first key
        // of a first run is.
        assert!(!read_key_draft(Some(" \n"), &[], false).changed);
        assert!(read_key_draft(Some("FAKE_KEY_ONE"), &[], false).changed);

        // A container that cannot be read leaves the saved list unknown, so any
        // typed list would replace it and has to stay a decision.
        assert!(read_key_draft(Some("FAKE_KEY_ONE\nFAKE_KEY_TWO"), &saved, true).changed);
        assert!(read_key_draft(Some(""), &saved, true).changed);

        // An unusable list always needs a decision, keeps the saved count and
        // names its line without any part of the key.
        let broken = read_key_draft(Some("FAKE_KEY_ONE\nFAKE KEY TWO"), &saved, false);
        assert!(broken.changed);
        assert_eq!(broken.count, saved.len());
        let error = broken.error.expect("the line that failed");
        assert_eq!(error.line, 2);
        assert!(
            !error.message.contains("FAKE KEY TWO"),
            "the message never quotes a key"
        );

        // A usable list shows its own count rather than the saved one.
        let replacement = read_key_draft(Some("FAKE_KEY_THREE"), &saved, false);
        assert_eq!(replacement.count, 1);
        assert!(replacement.error.is_none());
    }

    /// The general command's payload is the page's contract with the shell.
    /// Every managed value the general section owns travels under its own name
    /// - mode, the mute switch, the port and the chosen device - and the retired
    /// `compare_all` is not part of the contract any more, so a stale page that
    /// still sends it changes nothing. A payload that leaves any managed value
    /// out is refused rather than read as a default the page never chose: a
    /// missing mute switch would mean "off", a missing port and a missing
    /// device would silently undo values the user set.
    #[test]
    fn the_general_payload_carries_every_managed_value() {
        let sent: GeneralSettings = serde_json::from_str(
            r#"{"mode":"smart","mute_during_recording":true,"port":43118,"input_device":"wasapi:card-one"}"#,
        )
        .expect("the payload the settings page sends");
        assert_eq!(sent.mode, "smart");
        assert!(sent.mute_during_recording);
        assert_eq!(sent.port, 43118);
        assert_eq!(sent.input_device.as_deref(), Some("wasapi:card-one"));

        // `null` is the stored form of the system default device and a value
        // the page really sends, unlike an omitted field.
        let system: GeneralSettings = serde_json::from_str(
            r#"{"mode":"live","mute_during_recording":false,"port":4173,"input_device":null}"#,
        )
        .expect("the system default device");
        assert_eq!(system.port, 4173);
        assert_eq!(system.input_device, None);

        // A page from the comparison era still spells the retired field out; it
        // is not a value of this command and changes nothing.
        let stale: GeneralSettings = serde_json::from_str(
            r#"{"mode":"verbatim","compare_all":true,"mute_during_recording":false,"port":4173,"input_device":null}"#,
        )
        .expect("a stale payload");
        assert_eq!(stale.mode, "verbatim");
        assert!(!stale.mute_during_recording);

        // Each managed value is part of the contract: an omitted one is refused
        // instead of read as a default.
        for payload in [
            r#"{"mode":"smart"}"#,
            r#"{"mode":"smart","mute_during_recording":true}"#,
            r#"{"mode":"smart","mute_during_recording":true,"port":4173}"#,
            r#"{"mode":"smart","mute_during_recording":true,"input_device":null}"#,
        ] {
            assert!(
                serde_json::from_str::<GeneralSettings>(payload).is_err(),
                "the payload {payload} leaves a managed value out"
            );
        }
    }

    /// The general form's own values are checked against the rule the settings
    /// document is read with, before anything is planned or written: zero is not
    /// a port, a negative, a fraction, a string or a number above the range never
    /// deserializes into the command at all, and a device id is either the system
    /// default, an id `cpal::DeviceId` can parse, or a refusal that names the
    /// field and quotes nothing. A refused value therefore leaves the running
    /// port, the stored device and the files exactly as they were.
    #[test]
    fn the_general_form_refuses_a_port_or_a_device_it_cannot_store() {
        assert_eq!(usable_port(1).expect("the lowest port"), 1);
        assert_eq!(usable_port(65535).expect("the highest port"), 65535);

        // Zero is the only port a `u16` accepts that is not a port, so it is the
        // value this check is for.
        let zero: GeneralSettings = serde_json::from_str(
            r#"{"mode":"smart","mute_during_recording":true,"port":0,"input_device":null}"#,
        )
        .expect("zero is a u16");
        let error = usable_port(zero.port).expect_err("zero is not a port");
        assert_eq!(error.code, "PORT_INVALID");
        assert_eq!(error.field.as_deref(), Some("port"));

        // A value that is not a whole number in range never becomes a parsed
        // payload: the command is refused before its body runs.
        for payload in [
            r#"{"mode":"smart","mute_during_recording":true,"port":65536,"input_device":null}"#,
            r#"{"mode":"smart","mute_during_recording":true,"port":-1,"input_device":null}"#,
            r#"{"mode":"smart","mute_during_recording":true,"port":"4173","input_device":null}"#,
            r#"{"mode":"smart","mute_during_recording":true,"port":4173.5,"input_device":null}"#,
        ] {
            assert!(
                serde_json::from_str::<GeneralSettings>(payload).is_err(),
                "the port in {payload} is not a whole number in range"
            );
        }

        assert_eq!(usable_input_device(None).expect("the system device"), None);
        assert_eq!(
            usable_input_device(Some("wasapi:card-one".to_owned()))
                .expect("a device id")
                .as_deref(),
            Some("wasapi:card-one")
        );
        assert!(
            usable_input_device(Some(String::new())).is_err(),
            "an empty id is not a device"
        );
        for refused in ["not-a-device-id", "wasapi:"] {
            let error = usable_input_device(Some(refused.to_owned()))
                .expect_err("an id the document reader would refuse");
            assert_eq!(error.code, "INPUT_DEVICE_INVALID");
            assert_eq!(error.field.as_deref(), Some("input_device"));
            assert!(
                !error.message.contains(refused),
                "the message names the field, not the value"
            );
        }
    }

    /// A general change owns the port and the device the page shows, so saving a
    /// new microphone or a new port is exactly what travels; a chord or a key
    /// change keeps the ones that are running, so no other form can put back an
    /// old port or an old microphone behind the user's back.
    #[test]
    fn choosing_a_chord_or_keys_keeps_the_running_port_and_device() {
        let running = Settings {
            hotkey: "F2".to_owned(),
            mode: "smart".to_owned(),
            mute_during_recording: false,
            port: 43118,
            input_device: Some("wasapi:card-one".to_owned()),
        };
        let path = Path::new("settings.json");

        let general = planned_for_scope(
            ApplyScope::General,
            Settings {
                hotkey: "stale".to_owned(),
                mode: "verbatim".to_owned(),
                mute_during_recording: true,
                port: 43119,
                input_device: None,
            },
            &running,
            path,
        )
        .expect("a general plan");
        assert_eq!(
            general.hotkey, "F2",
            "the chord still comes from the running revision"
        );
        assert_eq!(general.port, 43119);
        assert_eq!(general.input_device, None);

        // A key change takes the whole running revision: a port or a device a
        // page might still be holding is not part of its business.
        let page_copy = Settings {
            hotkey: "stale".to_owned(),
            mode: "verbatim".to_owned(),
            mute_during_recording: true,
            port: 43119,
            input_device: None,
        };
        let keys = planned_for_scope(ApplyScope::Keys, page_copy.clone(), &running, path)
            .expect("a keys plan");
        assert_eq!(keys, running);

        // A chord change repairs the chord and keeps the running port and
        // device, even when the page sent something else.
        let hotkey = planned_for_scope(
            ApplyScope::Hotkey,
            Settings {
                hotkey: "Ctrl+Shift+F9".to_owned(),
                ..page_copy
            },
            &running,
            path,
        )
        .expect("a hotkey plan");
        assert_eq!(hotkey.hotkey, "Ctrl+Shift+F9");
        assert_eq!(hotkey.mode, "smart");
        assert_eq!(hotkey.port, 43118);
        assert_eq!(hotkey.input_device.as_deref(), Some("wasapi:card-one"));
    }

    /// The picker's list is a pure function of what the host enumerated: the
    /// default device is flagged by its id, the order follows the display name
    /// with the id as a tie-breaker, and two devices that share a name stay two
    /// entries with their own ids. The page reads `isDefault`, so the
    /// serialized shape is part of this contract too.
    #[test]
    fn the_input_device_list_marks_the_default_and_keeps_every_id() {
        let listed = describe_input_devices(
            vec![
                ("wasapi:beta".to_owned(), "Bravo".to_owned()),
                ("wasapi:alpha".to_owned(), "Alpha".to_owned()),
                ("wasapi:gamma".to_owned(), "Alpha".to_owned()),
            ],
            Some("wasapi:gamma"),
        );
        assert_eq!(
            listed
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            ["wasapi:alpha", "wasapi:gamma", "wasapi:beta"],
            "same names are ordered by id, and every id survives"
        );
        assert!(!listed[0].is_default);
        assert!(listed[1].is_default, "the default is flagged by its id");
        assert!(!listed[2].is_default);
        assert_eq!(
            serde_json::to_value(&listed[1]).expect("a serialized device")["isDefault"],
            serde_json::Value::Bool(true),
            "the page reads camelCase"
        );

        // No default device, or a default that is not among the inputs: nothing
        // is flagged, and the enumerated device is still offered.
        let one = describe_input_devices(vec![("wasapi:one".to_owned(), "One".to_owned())], None);
        assert!(!one[0].is_default);
        let elsewhere = describe_input_devices(
            vec![("wasapi:one".to_owned(), "One".to_owned())],
            Some("wasapi:elsewhere"),
        );
        assert!(!elsewhere[0].is_default);

        // An empty enumeration is an empty list, not a failure.
        assert!(describe_input_devices(Vec::new(), Some("wasapi:one")).is_empty());
    }

    /// The real write path migrates a document that still carries the retired
    /// `compare_all`: the property is gone from the bytes on disk, the mute
    /// switch a page chose is written, and the mark, the comment and the rest of
    /// the document survive byte for byte.
    #[test]
    fn a_save_migrates_the_retired_compare_all_away() {
        let scratch = Scratch::new("compare-migration");
        let config = scratch.config();
        let secrets_path = scratch.secrets();
        let original = document("live");
        fs::write(&config, &original).expect("the document");

        let planned = Settings {
            hotkey: "F2".to_owned(),
            mode: "verbatim".to_owned(),
            mute_during_recording: true,
            port: 43118,
            input_device: None,
        };
        let context = FileContext {
            config_path: &config,
            secrets_path: &secrets_path,
            previous_vault: None,
            secrets_unreadable: false,
        };
        let writes = FileWrites {
            settings: settings::patch_settings_document(&original, &planned)
                .expect("a patch"),
            container: None,
        };
        let outcome = transact_files(
            &context,
            &writes,
            &KeyPlan::Keep,
            &planned,
            undo_for(&config, &secrets_path),
        )
        .expect("the save");

        assert!(
            !outcome.container_verified,
            "a keep writes nothing to the container"
        );
        assert_eq!(
            fs::read_to_string(&config).expect("the written document"),
            "\u{feff}{\r\n  // режим\r\n  \"mute_during_recording\": true,\r\n  \"port\": 43118,\r\n  \"input_device\": null,\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"verbatim\"\r\n  \r\n}\r\n",
            "the retired property is gone and every other byte survived"
        );
    }

    /// An apply the document refuses puts the container back, so the running
    /// configuration and the files still agree.
    #[test]
    fn a_failed_document_write_leaves_the_previous_files() {
        let scratch = Scratch::new("rollback");
        let config = scratch.config();
        let secrets_path = scratch.secrets();
        let original = document("live");
        fs::write(&config, &original).expect("the document");
        fs::write(&secrets_path, container_of("FAKE_KEY_ONE")).expect("the container");
        let original_container = fs::read(&secrets_path).expect("the container bytes");
        let undo = undo_for(&config, &secrets_path);

        // The document is held from here: the transaction can write its
        // container, and its move onto the document has to fail.
        let held = hold_document(&config);
        let context = FileContext {
            config_path: &config,
            secrets_path: &secrets_path,
            previous_vault: None,
            secrets_unreadable: false,
        };
        let writes = FileWrites {
            settings: settings::patch_settings_document(&original, &settings_of("smart"))
                .expect("a patch"),
            container: Some(Some(container_of("FAKE_KEY_ONE\nFAKE_KEY_TWO"))),
        };
        let plan = KeyPlan::Replace(fake_keys("FAKE_KEY_ONE\nFAKE_KEY_TWO"));
        let failure = transact_files(&context, &writes, &plan, &settings_of("smart"), undo)
            .expect_err("a held document must stop the transaction");

        assert_eq!(failure.kind, FileFailureKind::DocumentWrite);
        assert!(
            failure.obligation.is_none(),
            "a clean rollback owes nothing"
        );
        drop(held);
        assert_eq!(
            fs::read(&config).expect("the document"),
            original.as_bytes(),
            "the document was never rewritten"
        );
        assert_eq!(
            fs::read(&secrets_path).expect("the container"),
            original_container,
            "the container holds its previous bytes again"
        );
        assert!(
            secrets::list_equal(
                secrets::load_secret_keys(&secrets_path)
                    .expect("the restored container")
                    .keys(),
                &fake_keys("FAKE_KEY_ONE")
            ),
            "the restored container decrypts to the list that was running"
        );
    }

    /// A rollback that could not be completed leaves an obligation, and the
    /// next apply discharges it: a store that was readable gets the running list
    /// back, a store that could not be read gets its own bytes back.
    #[test]
    fn a_failed_rollback_is_reconciled_by_the_next_save() {
        let scratch = Scratch::new("reconcile");
        let config = scratch.config();
        let secrets_path = scratch.secrets();
        let original = document("live");
        fs::write(&config, &original).expect("the document");
        let running = container_of("FAKE_KEY_ONE");

        // The restore cannot succeed: the container's name is a directory, so
        // the bytes the transaction wrote cannot be put back.
        fs::write(&secrets_path, &running).expect("the container");
        fs::remove_file(&secrets_path).expect("the container");
        fs::create_dir(&secrets_path).expect("a directory in its place");
        let undo = PersistUndo {
            settings_before: original.as_bytes().to_vec(),
            vault_before: Some(running.clone()),
            settings_replaced: false,
            secrets_replaced: true,
        };
        let context = FileContext {
            config_path: &config,
            secrets_path: &secrets_path,
            previous_vault: None,
            secrets_unreadable: false,
        };
        let failure = failed_files(&context, &undo, FileFailureKind::ContainerWrite);
        let owed = failure
            .obligation
            .expect("the container could not be put back");
        assert!(owed.secrets && !owed.settings);
        assert!(matches!(owed.vault_before, Some(VaultBefore::Usable)));

        // The obstruction goes, and the running list is written and verified.
        fs::remove_dir(&secrets_path).expect("the obstruction");
        let plan = repair_plan(&Some(VaultBefore::Usable), fake_keys("FAKE_KEY_ONE"));
        assert!(matches!(plan, KeyPlan::Replace(_)));
        let context = FileContext {
            config_path: &config,
            secrets_path: &secrets_path,
            previous_vault: Some(&VaultBefore::Usable),
            secrets_unreadable: false,
        };
        let writes = FileWrites {
            settings: settings::patch_settings_document(&original, &settings_of("live"))
                .expect("a patch"),
            container: Some(Some(running.clone())),
        };
        let outcome = transact_files(
            &context,
            &writes,
            &plan,
            &settings_of("live"),
            undo_for(&config, &secrets_path),
        )
        .expect("the reconciliation");
        assert!(
            outcome.container_verified,
            "the store holds the list the runtime is using again"
        );

        // A store that could not be read is put back exactly as it was, and is
        // not reported as a list this process wrote.
        let opaque = b"SPK1\x00\x01 not a container this process wrote".to_vec();
        fs::write(&secrets_path, b"something else entirely").expect("a foreign container");
        let plan = repair_plan(
            &Some(VaultBefore::Opaque(opaque.clone())),
            fake_keys("FAKE_KEY_ONE"),
        );
        assert!(matches!(plan, KeyPlan::RestoreOpaque(_)));
        let writes = FileWrites {
            settings: settings::patch_settings_document(&original, &settings_of("live"))
                .expect("a patch"),
            container: Some(Some(opaque.clone())),
        };
        let outcome = transact_files(
            &context,
            &writes,
            &plan,
            &settings_of("live"),
            undo_for(&config, &secrets_path),
        )
        .expect("the opaque restore");
        assert!(
            !outcome.container_verified,
            "an unreadable store is not a verified list"
        );
        assert_eq!(
            fs::read(&secrets_path).expect("the container"),
            opaque,
            "the bytes that were there before are back, byte for byte"
        );
    }

    /// A settings-only obligation is a fact about the document alone. A
    /// container failure that follows it has to keep its own before-image, so
    /// the reconciliation writes the running list back instead of deleting a
    /// container that was there all along.
    #[test]
    fn a_settings_only_obligation_does_not_lend_its_container_state() {
        let scratch = Scratch::new("vault-provenance");
        let config = scratch.config();
        let secrets_path = scratch.secrets();
        let original = document("live");
        fs::write(&config, &original).expect("the document");
        let running = container_of("FAKE_KEY_ONE\nFAKE_KEY_TWO");
        fs::write(&secrets_path, &running).expect("the container");

        // A general apply never reads the container, and its document rollback
        // fails. What it owes must say nothing about a container.
        fs::remove_file(&config).expect("the document");
        fs::create_dir(&config).expect("a directory in its place");
        let settings_only = failed_files(
            &FileContext {
                config_path: &config,
                secrets_path: &secrets_path,
                previous_vault: None,
                secrets_unreadable: false,
            },
            &PersistUndo {
                settings_before: original.as_bytes().to_vec(),
                vault_before: None,
                settings_replaced: true,
                secrets_replaced: false,
            },
            FileFailureKind::DocumentWrite,
        )
        .obligation
        .expect("the document could not be put back");
        assert!(settings_only.settings && !settings_only.secrets);
        assert!(
            settings_only.vault_before.is_none(),
            "an apply that never read the container has no container state to record"
        );
        assert!(
            matches!(
                repair_plan(&settings_only.vault_before, fake_keys("FAKE_KEY_ONE")),
                KeyPlan::Replace(_)
            ),
            "a settings-only obligation must not describe the container as absent"
        );

        // A keys apply then fails while writing the container and cannot put
        // the bytes back either. Its before-image is the container this process
        // really read.
        fs::remove_dir(&config).expect("the obstruction");
        fs::write(&config, &original).expect("the document again");
        fs::remove_file(&secrets_path).expect("the container");
        fs::create_dir(&secrets_path).expect("a directory in its place");
        let keys_failed = failed_files(
            &FileContext {
                config_path: &config,
                secrets_path: &secrets_path,
                previous_vault: None,
                secrets_unreadable: false,
            },
            &PersistUndo {
                settings_before: original.as_bytes().to_vec(),
                vault_before: Some(running.clone()),
                settings_replaced: false,
                secrets_replaced: true,
            },
            FileFailureKind::ContainerWrite,
        )
        .obligation
        .expect("the container could not be put back");
        assert!(keys_failed.secrets);
        assert!(matches!(
            keys_failed.vault_before,
            Some(VaultBefore::Usable)
        ));

        // The merge keeps the container state of the failure that really saw
        // it: the settings-only obligation must not overwrite it with nothing.
        let merged = fold_obligation(Some(&settings_only), keys_failed);
        assert!(merged.settings && merged.secrets);
        assert!(matches!(merged.vault_before, Some(VaultBefore::Usable)));

        // Reconciling therefore rewrites the list that was running, and leaves a
        // readable, non-empty container behind.
        let plan = repair_plan(
            &merged.vault_before,
            fake_keys("FAKE_KEY_ONE\nFAKE_KEY_TWO"),
        );
        assert!(
            matches!(plan, KeyPlan::Replace(_)),
            "a usable container is rewritten, never removed"
        );
        fs::remove_dir(&secrets_path).expect("the obstruction");
        let context = FileContext {
            config_path: &config,
            secrets_path: &secrets_path,
            previous_vault: merged.vault_before.as_ref(),
            secrets_unreadable: false,
        };
        let outcome = transact_files(
            &context,
            &FileWrites {
                settings: settings::patch_settings_document(&original, &settings_of("smart"))
                    .expect("a patch"),
                container: Some(Some(running.clone())),
            },
            &plan,
            &settings_of("smart"),
            undo_for(&config, &secrets_path),
        )
        .expect("the reconciliation");
        assert!(outcome.container_verified);
        let restored = secrets::load_secret_keys(&secrets_path).expect("the container is there");
        assert!(
            secrets::list_equal(restored.keys(), &fake_keys("FAKE_KEY_ONE\nFAKE_KEY_TWO")),
            "the ring that was running is on disk again"
        );
        assert!(
            !restored.is_empty(),
            "the container was not emptied by the reconciliation"
        );
    }

    /// A settings state of its own, with the scratch directory that holds the
    /// document it edits. No window and no runtime is started.
    fn preferences_state(name: &str) -> (Scratch, PreferencesState) {
        let scratch = Scratch::new(name);
        let runtime = Arc::new(SharedRuntime::new(Arc::new(RuntimeSnapshot {
            settings: settings_of("live"),
            keys: Arc::new(KeyRing::empty()),
            revision: 1,
        })));
        let state = PreferencesState::new(
            scratch.config(),
            runtime,
            Arc::new(Provider::new()),
            None,
            StartupPorts {
                configured: settings::DEFAULT_PORT,
                actual: settings::DEFAULT_PORT,
                fallback: false,
            },
        );
        (scratch, state)
    }

    /// The installer's exit latches before it does anything else, and its own
    /// latch is monotone: a late interactive quit may clear its own `closing`,
    /// but never this one, so no apply can begin behind the installer's
    /// decision.
    #[test]
    fn an_installer_quit_latches_every_writer_out() {
        let (_scratch, state) = preferences_state("installer-latch");

        // Before the exit, a command that would write is allowed.
        assert!(ensure_running(&state).is_ok());

        // The installer's latch and the ordinary closing one go up together,
        // before the exit waits for anything.
        installer_quit_latch(&state);
        assert!(
            ensure_running(&state).is_err(),
            "a late apply is refused once the installer latched the exit"
        );

        // A late interactive quit refuses and clears only its own latch; the
        // installer's stays up, so that refusal cannot re-open the shell.
        state.closing.store(false, Ordering::SeqCst);
        assert!(
            ensure_running(&state).is_err(),
            "the installer's latch is not cleared by another refusal"
        );
    }

    /// The installer's exit latches before it waits: an apply that is already
    /// running holds the session lock and finishes, while an apply that arrives
    /// after the exit decision is refused by the latch rather than waiting
    /// behind it.
    #[test]
    fn an_installer_exit_latches_before_it_waits_for_a_running_apply() {
        let (_scratch, state) = preferences_state("installer-barrier");
        let runtime = Arc::new(SharedRuntime::new(Arc::new(RuntimeSnapshot {
            settings: settings_of("live"),
            keys: Arc::new(KeyRing::empty()),
            revision: 1,
        })));
        let inner = crate::Shell::new(runtime, None).0;
        let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let entered = AtomicBool::new(false);

        std::thread::scope(|scope| {
            // An apply that is already running holds the session lock.
            let holder_inner = Arc::clone(&inner);
            let holder = scope.spawn(move || {
                let _guard = holder_inner.session.lock();
                let _ = locked_tx.send(());
                let _ = release_rx.recv();
            });
            let _ = locked_rx.recv();

            let waiter = scope.spawn(|| {
                let _guard = installer_exit_guard(&state, &inner);
                entered.store(true, Ordering::SeqCst);
            });

            // The latch is up while the exit is still waiting for the lock.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !state.installer_closing.load(Ordering::SeqCst)
                && std::time::Instant::now() < deadline
            {
                std::thread::yield_now();
            }
            assert!(
                state.installer_closing.load(Ordering::SeqCst),
                "the exit latches before it waits for the lock"
            );
            assert!(
                !entered.load(Ordering::SeqCst),
                "the exit waits for the apply it found running"
            );

            // The running apply finishes; only then does the exit proceed.
            let _ = release_tx.send(());
            holder.join().expect("the running apply ends");
            waiter.join().expect("the exit proceeds");
            assert!(
                entered.load(Ordering::SeqCst),
                "the exit proceeds once the apply finished"
            );
        });
    }
}
