//! Speechek's desktop shell.
//!
//! The shell owns everything that is not the overlay itself: the launcher
//! settings, the global dictation hotkey, the tray icon, the settings window,
//! the comparison window, the commands the overlay calls, and the insertion of
//! the dictated text into whichever window is active once the text is ready.
//!
//! The tray exposes exactly «Настройка» and «Выход». The settings window is
//! opened on demand, whether or not a dictation is running: during a take it
//! only locks the chord field and the comparison button. Quitting goes through
//! the same key-draft and partial-persistence coordinator as the settings page,
//! never directly through a menu callback.
//!
//! A dictation is a small state machine. The hotkey never reaches the recorder
//! twice: it opens the overlay, waits for the recorder to say it is running, and
//! only then lets a second press stop it.
//!
//! Escape cancels the dictation instead of finishing it, in any of the three
//! states after Idle and before its text is inserted: the pill goes away, the
//! microphone is released, the take is dropped unread, and nothing of it is
//! transcribed or typed. Escape is a shortcut of the shell only while such a
//! dictation exists, so it keeps its ordinary meaning everywhere else.
//!
//! ```text
//!          press                capture_started          press (stop)
//!   Idle ──────────► Arming ──────────────────────► Recording ──────────► Finalizing
//!     ▲               │  │                              │                    │
//!     │               │  └─ queued stop ────────────────┘                    │
//!     └───────────────┴───────────────────────────────────────────────────────┘
//!              the pill stays up while the take is closed and recognized, and
//!              is hidden with the hand-over of the text; finish_session brings
//!              it back only for a failure, for 4000 ms
//! ```
//!
//! The pill stays up for the whole dictation: the hotkey only stops the
//! recorder, and the window comes down in the same step that hands the
//! transcript to the window it belongs to. The user therefore keeps seeing that
//! the text is still on its way, and only a failure is worth a window of its own.
//!
//! The microphone is the shell's, not the renderer's: the overlay never takes
//! the foreground, and Windows answers the renderer's microphone request only
//! while its window is active. The shell therefore captures the default input
//! device itself and hands the overlay 16 kHz mono PCM16 frames to record. The
//! built-in comparison window records through the same single capture — one take
//! of either window at a time, and never one for the other — while the external
//! browser page keeps the browser's own microphone.
//!
//! The launcher chord is a registration with a gate of its own. Applying it registers
//! the chord the user chose with the gate closed, publishes the configuration,
//! and only then opens that gate and closes the one of the chord it replaced —
//! so there is no moment without a launcher, and a press that was already queued
//! can never start a dictation through a chord that is no longer the launcher. A
//! chord that cannot be registered at startup is not fatal: the shell runs with
//! the launcher disabled and opens the window on the section that repairs it.
//!
//! Configuration lives in the active flavor's profile: Production uses
//! `%APPDATA%/Speechek`, Development uses the directory beside its executable,
//! and Test uses its isolated profile or explicit test override. Settings are read
//! once while the shell is starting up, and the key list travels in
//! `secrets.bin` beside it, sealed for the current Windows user. A first run
//! writes the annotated settings template only; keys arrive with the first explicit key apply
//! of the settings window. A missing or unreadable container is not a startup
//! failure: the shell runs with an empty key ring and remembers why, so the
//! settings window can repair it and a press can name the reason instead of
//! recording a take nothing can recognize. The released shell is one executable
//! with no console, so every remaining startup failure — an unreadable settings
//! document, a loopback port taken by another program — is shown in a native
//! message box.

mod audio;
mod autostart;
mod backend;
mod capture;
mod installer;
mod live;
mod overlay;
mod paste;
mod preferences;
mod profile;
mod provider;
mod secrets;
mod settings;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use settings::{RuntimeSnapshot, SharedRuntime};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    webview::{NewWindowResponse, PageLoadEvent, WebviewWindowBuilder},
    AppHandle, Emitter, Manager, RunEvent, Url, WebviewUrl, WebviewWindow, WindowEvent,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_single_instance::init as single_instance;
use windows::core::HSTRING;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MESSAGEBOX_STYLE,
};

use backend::ReservedListener;
use preferences::{PreferencesState, SettingsUiError, StartupPorts};

/// Name the shell uses when it has to speak for itself: the active flavor's
/// title, so a development or test build does not present itself as the
/// released shell.
const APP_TITLE: &str = profile::defaults(profile::ACTIVE).title;

/// Window labels. The overlay label is also the only label allowed to call the
/// renderer-facing commands. `preferences::SETTINGS_LABEL` names the settings
/// window, and this module is the only one that creates it.
pub(crate) const OVERLAY_LABEL: &str = "overlay";
pub(crate) const LAB_LABEL: &str = "lab";

/// The `WebviewUrl::App` paths the restricted windows load. The port they are
/// served on is not part of them: [`is_app_page`] derives the real URL from the
/// running `build.dev_url`, which `run` pins to the port the reservation
/// claimed — the configured one, or the temporary one it fell back to.
///
/// The laboratory names its page with a leading slash on purpose: Tauri skips
/// the join for the literal path `index.html` and would load the base itself,
/// while an explicit `/index.html` resolves to exactly the document the
/// laboratory's gate accepts.
pub(crate) const SETTINGS_PAGE_PATH: &str = "settings.html";
const OVERLAY_PAGE_PATH: &str = "overlay.html";
const LAB_PAGE_PATH: &str = "/index.html";

/// The settings window: one window, opened on demand from the tray icon, and
/// only ever on the page its own navigation gate accepts.
const SETTINGS_TITLE: &str = "Speechek — Настройки";
/// The window a right click on the tray icon gets, and the smallest size that
/// still shows both sections without covering the fields with the footer.
const SETTINGS_SIZE: (f64, f64) = (920.0, 720.0);
const SETTINGS_MIN_SIZE: (f64, f64) = (640.0, 480.0);

/// Sent to the settings window after it is shown again: which section to open.
/// A first load never needs it, because the page asks with `settings_open` by
/// itself and gets the section from the pending request.
const EVENT_SETTINGS_OPEN: &str = "speechek:settings-open-request";
/// Sent when the window's close button was pressed. The window stays up until
/// the page says what to do with the draft: neither half of an editing session
/// is thrown away by the native side on its own.
const EVENT_SETTINGS_CLOSE: &str = "speechek:settings-close-request";
/// Sent when a dictation starts and when it ends, so the page stops offering a
/// chord change that would stop a take.
const EVENT_DICTATION_STATE: &str = "speechek:dictation-state";
/// Sent when the launcher chord is pressed while the settings window's chord
/// field holds the keyboard. Windows hands such a press to the process rather
/// than to the page, so the field cannot see the keys themselves: the event
/// carries the chord that is running, and the page writes it into the field as
/// text — only Enter or leaving the field may apply it.
const EVENT_SETTINGS_HOTKEY_KEY: &str = "speechek:settings-hotkey-key";

/// The sections a settings request can name, as the value the watcher reads.
/// The names the page sees are `general`, `keys` and `last`.
const SECTION_LAST: u8 = 0;
const SECTION_GENERAL: u8 = 1;
const SECTION_KEYS: u8 = 2;

/// The launcher chord the settings document names could not be registered while
/// the shell started. The shell then runs with the launcher disabled and opens
/// the window, so the chord can be repaired instead of refusing to start.
const HOTKEY_NOT_REGISTERED: &str =
    "Горячую клавишу не удалось зарегистрировать: её удерживает другая программа или система. Выберите другое сочетание в разделе «Общие настройки».";

/// The chord the settings window asked for is taken by another program. The
/// registration the shell is already running is left exactly as it is.
const HOTKEY_CONFLICT_MESSAGE: &str =
    "Это сочетание клавиш уже занято другой программой или системой. Выберите другое.";

/// The chord the settings window asked for is not one the grammar can express.
const HOTKEY_INVALID_MESSAGE: &str =
    "Сочетание клавиш не поддерживается. Примеры: F2, Ctrl+Shift+Space.";

/// The chord apply succeeded, but the chord it replaced could not be
/// released. The new chord works; only leaving the process gives the old one
/// back, so the user is told before the next start finds it taken.
const HOTKEY_UNRELEASED_MESSAGE: &str =
    "Новые настройки применены, но прежнюю клавишу не удалось освободить. Завершите Speechek через «Выход» перед повторным запуском.";

/// The chord apply failed before anything was published, and its candidate
/// just taken could not be given back either. The running chord is untouched;
/// the candidate is silenced and named so the user knows what still holds it.
const HOTKEY_CANDIDATE_UNRELEASED_MESSAGE: &str =
    "Прежняя настройка осталась в силе, но выбранное сочетание клавиш не удалось освободить: его удерживает сам Speechek до выхода. Выберите другое сочетание или завершите приложение.";

/// Emitted to the overlay to start recording and to stop it again. Both use the
/// same event and the same generation, so the renderer can tell which dictation
/// it belongs to. This is the only recording trigger the shell sends.
const EVENT_TOGGLE: &str = "speechek:toggle";

/// Sent to the overlay when Escape cancels the dictation in progress. The
/// renderer drops the take and never asks for a transcript of it, so nothing of
/// a cancelled dictation can be inserted.
const EVENT_CANCEL: &str = "speechek:cancel";

/// Sent to the laboratory when its window is closing. The page stays loaded
/// while the window is only hidden, so this is its only notice that the shell
/// has already ended its capture: it destroys its recorder and resets its
/// controls, and asks the shell for nothing — the capture is closed natively.
const EVENT_LAB_CLOSED: &str = "speechek:lab-closed";

/// A key that arrives twice back to back must not open two dictations.
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(300);

/// How often the shell checks the one thing no event can report.
const TICK: Duration = Duration::from_millis(250);

/// An overlay that never reports a running capture is put away again instead of
/// leaving the shell unable to start another dictation.
const ARM_TIMEOUT: Duration = Duration::from_secs(30);

/// The comparison window, opened on demand from the settings window.
const LAB_SIZE: (f64, f64) = (1_100.0, 800.0);

/// The renderer never hands over a long dictation in one piece: this is the
/// ceiling the shell accepts, in bytes of UTF-8.
const MAX_TEXT_BYTES: usize = 64 * 1024;

/* -------------------------------------------------------------------------- */
/* Window pages                                                                */
/* -------------------------------------------------------------------------- */

/// The exact URL `WebviewUrl::App(page)` resolves to for the running shell: the
/// page joined onto `build.dev_url`, which `run` pins to the loopback origin the
/// reservation claimed, or onto the custom-protocol base when no dev URL is
/// configured. This mirrors the join Tauri performs when it opens a window, so a
/// gate and the document it guards agree about the one URL that is allowed.
///
/// The literal `index.html` stays the base itself, exactly as Tauri skips the
/// join for it; every other path — the laboratory's explicit `/index.html`
/// included — is joined onto the base.
fn resolved_app_page(dev_url: Option<&Url>, page: &str) -> Url {
    let base = match dev_url {
        Some(dev_url) => dev_url.clone(),
        None if cfg!(windows) || cfg!(target_os = "android") => {
            Url::parse("http://tauri.localhost").expect("a static URL parses")
        }
        None => Url::parse("tauri://localhost").expect("a static URL parses"),
    };
    if page == "index.html" {
        base
    } else {
        base.join(page).expect("a page path never breaks a base URL")
    }
}

/// Whether a window labelled `window_label` is on the exact page
/// `WebviewUrl::App(page)` names, under `dev_url`.
///
/// Both halves are checked: the label, because a label is not a secret, and the
/// whole URL — scheme, host, port and path, with no query and no fragment — so a
/// lookalike path or the URL of a previous run's port never matches. These
/// windows drive the microphone, the clipboard and the key store.
fn app_page_matches(
    label: &str,
    page: &str,
    window_label: &str,
    dev_url: Option<&Url>,
    url: &Url,
) -> bool {
    window_label == label && *url == resolved_app_page(dev_url, page)
}

/// Whether `window` carries `label` and sits on the page `WebviewUrl::App(page)`
/// resolves to right now. Used by every gate that decides whether a command may
/// run in this window.
pub(crate) fn is_app_page(window: &WebviewWindow, label: &str, page: &str) -> bool {
    let Ok(url) = window.url() else {
        return false;
    };
    app_page_matches(
        label,
        page,
        window.label(),
        window.app_handle().config().build.dev_url.as_ref(),
        &url,
    )
}

/* -------------------------------------------------------------------------- */
/* State                                                                       */
/* -------------------------------------------------------------------------- */

/// Where a dictation currently is. Only the hotkey and the renderer move it.
/// `preferences` reads it to decide whether a chord may be replaced right now,
/// and the settings window is told [`Phase::is_active`] as its own flag: a
/// dictation keeps that window away from nothing, it only refuses a chord
/// change and disables the lab button while it runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Phase {
    /// Nothing is going on; a press opens the overlay.
    #[default]
    Idle,
    /// The overlay was asked to record but the recorder has not started yet, so
    /// a press is remembered instead of being sent to a recorder that is deaf.
    Arming,
    /// The recorder is running; a press stops it.
    Recording,
    /// The recorder is closing and the take is being recognized; the pill stays
    /// up meanwhile, and the hotkey is ignored until the renderer reports the
    /// outcome. The hand-over hides the pill; a failure brings it back with the
    /// reason.
    Finalizing,
    /// Clipboard ownership has begun — the pill is already down — and Escape can
    /// no longer undo a paste chord.
    Pasting,
}

impl Phase {
    /// Whether a dictation occupies the shell right now.
    ///
    /// The settings window is told this as `dictation_active`: the chord field
    /// is locked and the lab button is disabled for as long as it is true, from
    /// the arming until the hand-over is over. Everything else the window does —
    /// viewing, mode, keys, closing — stays available during a take.
    pub(crate) fn is_active(self) -> bool {
        match self {
            Self::Idle => false,
            Self::Arming | Self::Recording | Self::Finalizing | Self::Pasting => true,
        }
    }
}

/// State the shell keeps for the overlay.
#[derive(Default)]
struct ShellState {
    phase: Phase,
    /// The configuration revision the current dictation was started under:
    /// captured while the phase moves to Arming and pinned to the generation
    /// once the pill is shown, so every request of the take uses the same key
    /// ring and mode even when an apply publishes another revision meanwhile.
    snapshot: Option<Arc<RuntimeSnapshot>>,
    /// Final text of the last dictation that was handed over, kept until the
    /// next one replaces it.
    text: Option<Arc<String>>,
    /// Exactly one hand-off per dictation, including a clipboard fallback.
    inserted: bool,
}

/// Everything shared between the hotkey callback, the renderer commands, the
/// watcher thread and the settings commands. Handed to Tauri as managed state.
pub(crate) struct Inner {
    /// The configuration the shell runs on. A dictation captures the revision
    /// it starts under; an apply publishes a replacement that the following
    /// dictation picks up.
    runtime: Arc<SharedRuntime>,
    /// Why the key container could not be read while the shell started, if it
    /// could not. The runtime carries an empty ring either way, and a press is
    /// refused with this reason until a key list is saved — never with a
    /// fallback to a plain-text key file.
    secrets_error: Option<String>,
    /// The chord that is allowed to start a dictation, and the gate every
    /// registration the shell has replaced carries. Written only while the
    /// session lock is held, so a chord apply cannot race a press
    /// that is deciding what to do.
    hotkey: Mutex<Option<RegisteredHotkey>>,
    state: Mutex<ShellState>,
    /// Generation of the current dictation; the overlay gets it with every
    /// toggle and sends it back when asking the overlay to go away.
    generation: AtomicU64,
    /// A press that arrived while the overlay was still loading the page.
    pending_toggle: AtomicBool,
    /// A stop pressed while the recorder was not running yet, delivered to it
    /// exactly once when it reports that it started.
    queued_stop: AtomicBool,
    /// Whether the overlay has registered its event listener.
    overlay_listening: AtomicBool,
    /// When the last press was accepted, for the debounce.
    last_toggle: AtomicU64,
    /// When the current arming started, for the arming timeout.
    arm_started: AtomicU64,
    /// A tray click, a startup repair or a repeated launch asked for the
    /// settings window. The watcher opens it at its next tick, whether or not a
    /// dictation is running: the window takes the foreground, and the take
    /// pastes into whatever owns it when the text is ready.
    pending_settings: AtomicBool,
    /// Which section that request named, as one of the `SECTION_*` values. An
    /// atomic because the tray callback that sets it may not take a lock the
    /// watcher could be holding.
    pending_section: AtomicU8,
    /// The settings window is being built and focused on the main thread: a
    /// dictation must not start into a window that is still receiving the focus.
    settings_activating: AtomicBool,
    /// The settings window holds the foreground, so a press belongs to it.
    settings_focused: AtomicBool,
    /// The settings window's chord field holds the keyboard, so the chord
    /// typed there is text for that field, never a take to start here. Mirrors
    /// `hotkey_signal` for the press path, which reads it lock-free.
    hotkey_field_focused: AtomicBool,
    /// The report stream that owns `hotkey_field_focused`: which settings
    /// document is reporting and the newest sequence applied. Its own leaf
    /// lock, never the session lock — see [`HotkeySignal`].
    hotkey_signal: Mutex<HotkeySignal>,
    /// The settings page finished loading at least once, so it is listening for
    /// the events the shell sends it.
    settings_page_ready: AtomicBool,
    /// Bumped for every activation and for every close that answers one. The
    /// closure an activation dispatched to the main thread shows the window only
    /// while its own token is still the current one, so a close that arrives
    /// while the window is on its way cannot be undone by it being shown.
    activation_token: AtomicU64,
    /// A close the window's button asked for before the page was listening: the
    /// page is the only one that may decide about a draft, so the click is
    /// remembered and handed over once the page asks to open — see
    /// [`settings_ui_ready`], which the settings state calls for it.
    pending_close: AtomicU8,
    /// Set while the shell is leaving: the watcher stops asking for windows and
    /// no press may open anything any more.
    pub(crate) shutdown: AtomicBool,
    /// Set when the installer asked this process to leave. Separate from
    /// [`Self::shutdown`] because the exit path picks a different cleanup for
    /// it: a capture is given up without waiting for its driver exactly when
    /// this is set, while an ordinary quit keeps its own draft decisions.
    pub(crate) installer_quit: AtomicBool,
    /// Held for the whole of one key decision. A press of Escape that arrives
    /// while a launch is still placing the pill waits for it, so the cancel sees
    /// the generation the launch created and not the one before it.
    pub(crate) session: Mutex<()>,
    /// Monotonic session ticket, captured by the Escape handler at registration.
    session_id: AtomicU64,
    /// Generation of the dictation Escape cancelled, or zero when none was. A
    /// command of that generation is refused however late it arrives, so a
    /// cancelled dictation can never reach the clipboard.
    cancelled: AtomicU64,
    /// Generation whose system mute could not be given back, or zero when none
    /// failed. Its pill is already hidden, so this is the sentence the renderer
    /// may bring the pill back for — see [`mute_warning_shown`].
    pending_mute_warning: AtomicU64,
    /// The laboratory's reservation of the microphone, or `None` when it has
    /// none. Only the laboratory commands and the window's close path touch it,
    /// always under the session lock and before any capture of it is stopped, so
    /// a laboratory take can never be reserved while a dictation runs, a
    /// dictation can never start while a take is reserved, and a take that was
    /// closed can never regain a microphone.
    lab: Mutex<Option<LabCapture>>,
    /// Whether the laboratory window is closed (hidden) right now. A closed
    /// window may reserve nothing: a prepare that was still in flight when the
    /// window went away must not leave a take nobody can stop, and a hidden
    /// page must never hold a microphone. Cleared when the window is shown
    /// again.
    lab_closed: AtomicBool,
    /// Source of the laboratory's own generation numbers. Separate from the
    /// dictation generations on purpose: neither space has to know the other,
    /// and a laboratory number can never name a dictation's take.
    lab_next: AtomicU64,
}

/// One laboratory take the shell has reserved the microphone for, from
/// `lab_prepare_capture` until its capture is stopped.
///
/// The snapshot is the configuration the take records with, pinned when the
/// reservation was made: a setting saved while the take runs takes effect with
/// the next one, exactly as a dictation's own pin does. `start_requested` makes
/// the reservation one-shot — the first `lab_start_capture` of this generation
/// takes the right to open the device, so a repeated command can never open a
/// second microphone behind the same take.
struct LabCapture {
    /// The laboratory's own number for this take. It only ever names events of
    /// the `lab` window; dictation generations are a separate space.
    generation: u64,
    /// The configuration this take records with, pinned when it was reserved.
    snapshot: Arc<RuntimeSnapshot>,
    /// Set by the first `lab_start_capture` of this generation, under the
    /// session lock.
    start_requested: bool,
}

/// One registered launcher chord, with the two pieces of state its own handler
/// needs: the gate that decides whether its presses count, and whether that key
/// is currently down.
///
/// Both belong to the registration and not to the shell, so a chord that is
/// being replaced cannot disturb the chord that replaces it — a press of the old
/// chord that was already in the plugin's hands when the swap happened can set
/// only its own key state, and its release can only clear its own, while the
/// chord that now owns the launcher starts from a key that is not held.
///
/// A chord apply carries one of these from `prepare_settings_hotkey` to
/// `commit_settings_hotkey` — or to `rollback_settings_hotkey` when the files
/// could not be written — so it is the shell's handle on a registration the
/// settings window cannot see.
pub(crate) struct RegisteredHotkey {
    shortcut: Shortcut,
    enabled: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
}

impl RegisteredHotkey {
    /// The chord the settings document named: it is the launcher from the start.
    fn launcher(shortcut: Shortcut) -> Self {
        Self {
            shortcut,
            enabled: Arc::new(AtomicBool::new(true)),
            held: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A registration whose presses are ignored until it is activated.
    fn candidate(shortcut: Shortcut) -> Self {
        Self {
            shortcut,
            enabled: Arc::new(AtomicBool::new(false)),
            held: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Registers this chord with the plugin, wired to its own gate and its own
    /// key state.
    fn install(&self, app: &AppHandle) -> Result<(), tauri_plugin_global_shortcut::Error> {
        register_gated(
            app,
            self.shortcut,
            Arc::clone(&self.enabled),
            Arc::clone(&self.held),
        )
    }
}

/// The settings chord field's report stream, as the shell keeps it.
///
/// The page reports when the field takes and when it gives the keyboard up,
/// and two fire-and-forget reports can arrive in either order. Every report
/// carries the identity of the document that sent it — read from the shell
/// with a handshake before the first report — and a number of its own; only a
/// report of the current document whose number is newer than the last one
/// applied may change what the field owns. A report that lost a race, or one
/// that belonged to a page that was already replaced, can therefore never put
/// a stale focus state back, and a released gate can never be re-opened by the
/// blur that was issued before it.
#[derive(Default)]
struct HotkeySignal {
    /// Identity of the document whose reports count. Bumped when a document
    /// starts loading and when the window is destroyed, so nothing a previous
    /// page still has in flight reaches the state of the one that replaced it.
    document: u64,
    /// The highest report sequence applied for that document.
    sequence: u64,
    /// Whether the field of that document holds the keyboard. Mirrored into
    /// `Inner::hotkey_field_focused` for the press path, which reads it on
    /// every chord without taking this lock.
    focused: bool,
}

impl HotkeySignal {
    /// Applies one report and answers the document the page has to echo back.
    ///
    /// A sequence of zero is the handshake: it only reads that identity, so a
    /// page that just loaded learns which document its reports belong to, and
    /// a page that was already replaced cannot use it to write anything.
    fn record(&mut self, document: u64, sequence: u64, active: bool, visible: bool) -> u64 {
        if sequence == 0 || document != self.document {
            return self.document;
        }
        if sequence <= self.sequence {
            return self.document;
        }
        self.sequence = sequence;
        // A window that is not on screen holds no keyboard: the report may
        // only release, so a page that has not noticed the hide cannot silence
        // the launcher of a window nobody can see.
        self.focused = active && visible;
        self.document
    }

    /// Gives the field's claim on the keyboard up without touching the
    /// document or its sequence: the same page may report its field again when
    /// the window comes back, and a report that is older than the last applied
    /// one still cannot re-open the gate.
    fn release(&mut self) {
        self.focused = false;
    }

    /// Starts a new document: nothing a page already has in flight can reach
    /// it, and its own report numbering begins again after its handshake.
    fn replace_document(&mut self) {
        self.document = self.document.wrapping_add(1);
        self.sequence = 0;
        self.focused = false;
    }
}

/// Applies one report of the settings chord field and answers with the
/// document identity the page has to echo back with every following report.
///
/// The state is kept under the signal's own leaf lock, never the session lock:
/// the window's events run on the main thread, which a save worker may be
/// waiting for while it holds the session lock, so no path taken on the main
/// thread may need that one.
pub(crate) fn record_hotkey_field_report(
    inner: &Inner,
    document: u64,
    sequence: u64,
    active: bool,
    visible: bool,
) -> u64 {
    let mut signal = inner.hotkey_signal.lock();
    let document = signal.record(document, sequence, active, visible);
    inner
        .hotkey_field_focused
        .store(signal.focused, Ordering::SeqCst);
    document
}

/// Gives up the chord field's claim on the keyboard for every way the window
/// can lose it — hidden, out of the foreground, or gone.
pub(crate) fn clear_hotkey_field(inner: &Inner) {
    let mut signal = inner.hotkey_signal.lock();
    signal.release();
    inner.hotkey_field_focused.store(false, Ordering::SeqCst);
}

/// Starts a new report document: a page that is loading, or a window that was
/// destroyed, cannot answer for the field any more, and the reports of the
/// document that is going away are dropped from here on.
pub(crate) fn reset_hotkey_field_document(inner: &Inner) {
    let mut signal = inner.hotkey_signal.lock();
    signal.replace_document();
    inner.hotkey_field_focused.store(false, Ordering::SeqCst);
}

/// Managed state wrapper; `Arc` so every callback can hold on to the shell.
struct Shell(Arc<Inner>);

impl Shell {
    fn new(runtime: Arc<SharedRuntime>, secrets_error: Option<String>) -> Self {
        Self(Arc::new(Inner {
            runtime,
            secrets_error,
            hotkey: Mutex::new(None),
            state: Mutex::new(ShellState::default()),
            generation: AtomicU64::new(0),
            pending_toggle: AtomicBool::new(false),
            queued_stop: AtomicBool::new(false),
            overlay_listening: AtomicBool::new(false),
            last_toggle: AtomicU64::new(0),
            arm_started: AtomicU64::new(0),
            pending_settings: AtomicBool::new(false),
            pending_section: AtomicU8::new(SECTION_LAST),
            settings_activating: AtomicBool::new(false),
            settings_focused: AtomicBool::new(false),
            hotkey_field_focused: AtomicBool::new(false),
            hotkey_signal: Mutex::new(HotkeySignal::default()),
            settings_page_ready: AtomicBool::new(false),
            activation_token: AtomicU64::new(0),
            pending_close: AtomicU8::new(0),
            shutdown: AtomicBool::new(false),
            installer_quit: AtomicBool::new(false),
            session: Mutex::new(()),
            session_id: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            pending_mute_warning: AtomicU64::new(0),
            lab: Mutex::new(None),
            lab_closed: AtomicBool::new(false),
            lab_next: AtomicU64::new(0),
        }))
    }
}

pub(crate) fn inner(app: &AppHandle) -> Arc<Inner> {
    app.state::<Shell>().0.clone()
}

pub(crate) fn phase_of(inner: &Inner) -> Phase {
    inner.state.lock().phase
}

fn set_phase(inner: &Inner, phase: Phase) {
    inner.state.lock().phase = phase;
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Releases the configuration a dictation pinned, so a finished take stops
/// holding its key ring alive.
///
/// The caller holds the session lock. Both halves belong to that lock: the pin
/// must not be dropped by a timeout of a dictation that is already over, and
/// the captured revision must not be cleared out from under the arming that is
/// waiting for the lock. The generation gate keeps an ended dictation from
/// touching the one that replaced it.
fn retire_dictation_locked(inner: &Inner, generation: u64) {
    inner.runtime.retire(generation);
    let mut state = inner.state.lock();
    if inner.generation.load(Ordering::SeqCst) == generation {
        state.snapshot = None;
    }
}

/// Whether the microphone of `generation` may still be opened.
///
/// A dictation is admitted while it is the one arming, its pinned revision is
/// still there, and it was neither cancelled nor shut down. The command and
/// every way a dictation ends take the same session lock, so this is read under
/// that lock before the device is opened — a pin that is gone means the take was
/// cancelled or replaced since the request — and read again under it once the
/// device is up, where a change means the dictation is over and the capture must
/// be closed instead of admitted. The lock is never held across the open itself:
/// a device that waits on its driver has to stay cancellable.
fn admits_capture(inner: &Inner, generation: u64) -> bool {
    inner.runtime.resolve(Some(generation)).is_some()
        && inner.generation.load(Ordering::SeqCst) == generation
        && phase_of(inner) == Phase::Arming
        && inner.cancelled.load(Ordering::SeqCst) != generation
        && !inner.shutdown.load(Ordering::SeqCst)
}

/* -------------------------------------------------------------------------- */
/* Laboratory state                                                            */
/* -------------------------------------------------------------------------- */

/// Why the laboratory cannot take the microphone right now, if it cannot.
/// Called with the session lock held: the phase it reads is decided under that
/// same lock by every dictation transition.
fn lab_refusal(inner: &Inner) -> Option<String> {
    if inner.shutdown.load(Ordering::SeqCst) {
        return Some("Speechek завершается — запись невозможна.".to_string());
    }
    match phase_of(inner) {
        Phase::Idle => None,
        _ => Some("Идёт диктовка — дождитесь её окончания.".to_string()),
    }
}

/// Reserves the microphone for one laboratory take and answers the generation
/// it was numbered with. Called with the session lock held.
///
/// A dictation in any phase but Idle, a shell that is leaving, a window that is
/// closed and a take that is already reserved all refuse this: the laboratory
/// never shares the microphone with a dictation, one take of its own is prepared
/// at a time, and a closed window can never leave a take nobody is there to
/// stop.
fn reserve_lab_capture(inner: &Inner) -> Result<u64, String> {
    if let Some(message) = lab_refusal(inner) {
        return Err(message);
    }
    if inner.lab_closed.load(Ordering::SeqCst) {
        return Err("Окно лаборатории закрыто — откройте его заново.".to_string());
    }
    let mut lab = inner.lab.lock();
    if lab.is_some() {
        return Err("Лаборатория уже готовит запись.".to_string());
    }
    let generation = inner.lab_next.fetch_add(1, Ordering::SeqCst) + 1;
    *lab = Some(LabCapture {
        generation,
        snapshot: inner.runtime.snapshot(),
        start_requested: false,
    });
    Ok(generation)
}

/// Takes the one-shot right of a reserved take to open its microphone, and
/// answers the configuration it was reserved with. Called with the session lock
/// held.
///
/// A generation that is not the reserved one — a repeated command, a late start
/// of a take that is already over — opens nothing, and the right is spent by the
/// first start that takes it.
fn request_lab_start(inner: &Inner, generation: u64) -> Result<Arc<RuntimeSnapshot>, String> {
    let mut lab = inner.lab.lock();
    match lab.as_mut() {
        Some(reserved) if reserved.generation == generation => {
            if reserved.start_requested {
                return Err("Микрофон этой записи уже открывается.".to_string());
            }
            reserved.start_requested = true;
            Ok(Arc::clone(&reserved.snapshot))
        }
        _ => Err("Эта запись уже завершена — микрофон не открывался.".to_string()),
    }
}

/// Whether `generation` is still the laboratory take the shell holds. Called
/// with the session lock held.
fn lab_capture_is_current(inner: &Inner, generation: u64) -> bool {
    matches!(
        inner.lab.lock().as_ref(),
        Some(reserved) if reserved.generation == generation
    )
}

/// Whether the laboratory holds the microphone right now, so a dictation press
/// must not take it. Read under the session lock, which is what makes the
/// decision the same one the laboratory's own door takes.
fn lab_holds_microphone(inner: &Inner) -> bool {
    inner.lab.lock().is_some()
}

/// Releases the reservation of `generation`, and answers whether it was the one
/// held. Called with the session lock held.
///
/// The reservation is gone before anything is stopped, so the generation can
/// never open — or install — a microphone from here on, and a later take gets a
/// generation of its own.
fn release_lab_capture(inner: &Inner, generation: u64) -> bool {
    let mut lab = inner.lab.lock();
    if matches!(lab.as_ref(), Some(reserved) if reserved.generation == generation) {
        *lab = None;
        true
    } else {
        false
    }
}

/// Releases whatever reservation the laboratory holds and answers its
/// generation. Called with the session lock held, by the window's close path.
fn take_lab_capture(inner: &Inner) -> Option<u64> {
    inner
        .lab
        .lock()
        .take()
        .map(|reserved| reserved.generation)
}

/// Releases a reservation whose take failed before its microphone was open, so
/// the laboratory may prepare another one. Called after a failed start; a
/// reservation that is already gone — the take was stopped, or its window was
/// closed — is left exactly as it is.
fn release_failed_lab_capture(app: &AppHandle, generation: u64) {
    let inner = inner(app);
    let _session = inner.session.lock();
    if release_lab_capture(&inner, generation) {
        capture::close_admission(app);
    }
}

/* -------------------------------------------------------------------------- */
/* Configuration                                                               */
/* -------------------------------------------------------------------------- */

/// The running loopback backend, kept only so the exit path can release the port
/// and retire its runtime thread once the windows are gone. The slot is emptied
/// on shutdown, so the port is given back exactly once.
struct BackendState(Mutex<Option<backend::Backend>>);

/// Shows a message in a native Windows dialog and waits for it to be dismissed.
///
/// This is the only channel the shell has before a window exists: the released
/// executable is built without a console, so a printed message would never be
/// read. The dialog needs no owner, and it is put in front so a startup failure
/// cannot hide behind another application.
fn show_message(message: &str, icon: MESSAGEBOX_STYLE) {
    let text = HSTRING::from(message);
    let caption = HSTRING::from(APP_TITLE);
    // SAFETY: both strings are NUL-terminated and outlive the call; a message
    // box takes no other pointer. The return value only names the button, which
    // "OK" fixes.
    unsafe {
        MessageBoxW(
            None,
            &text,
            &caption,
            MB_OK | icon | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// Fatal startup failure: the user has to see why speechek did not start, so the
/// reason is a modal dialog and the process stops once it is dismissed.
fn fatal(message: String) -> ! {
    show_message(&message, MB_ICONERROR);
    std::process::exit(1)
}

/// Reads the configuration, serves the loopback socket this run reserved and
/// creates the shell.
///
/// Called from the Tauri `setup` hook, which runs once the plugins are
/// initialized: the single-instance plugin has already sent a second launch away
/// by then, so it never reaches the port. Everything that can fail here is fatal
/// and shows its reason in a native dialog, because the released executable has
/// no console — with two exceptions, both repairable in the settings window: the
/// key container (a missing or unreadable one leaves an empty ring and the
/// reason behind) and the launcher chord (a chord that cannot be registered
/// leaves the launcher disabled, never a silently chosen one).
///
/// The socket is passed in already bound, because `run` claimed it before the
/// shell was built; the port is not chosen here. The document read here is the
/// one the preflight read, so a saved port that changed in between — the
/// first-run template written, a hand edit — stops the run with a native dialog
/// before the backend or any window exists rather than serving another port.
///
/// The order is what the rest of the shell needs:
/// * the managed state exists before the first webview, or an overlay that loads
///   faster than the shell would find nothing on its first command;
/// * the backend is up before the overlay, the tray and the hotkey, so nothing
///   can ask for a transcript before something can serve it;
/// * the hotkey is registered once the shell can actually answer a press, and
///   the window that repairs it is asked for last, so its page finds a shell
///   that is completely up when it loads.
fn startup(app: &AppHandle, reservation: ReservedListener) {
    let config_path = match settings::settings_path() {
        Ok(path) => path,
        Err(err) => fatal(err.to_string()),
    };

    // A first run gets the annotated settings document on the path this launch
    // actually selected — the debug override included — and startup continues
    // with it: the key list that is still missing is reported to the user, not
    // turned into a dialog that keeps the shell from starting.
    if let Err(err) = settings::create_default_if_missing(&config_path) {
        fatal(err.to_string());
    }

    let settings = match settings::load_settings(&config_path) {
        Ok(settings) => settings,
        Err(err) => fatal(err.to_string()),
    };

    // The socket is already listening on the port the preflight read, so the
    // settings document has to agree: writing the first-run template or a hand
    // edit between the reads can change the saved port. Refuse before the backend
    // and windows exist instead of running elsewhere than the file says;
    // a restart applies the value currently in the file.
    if settings.port != reservation.configured_port() {
        fatal(format!(
            "Speechek cannot start: the settings file names port {}, but this start claimed port {}. Start Speechek again to serve the saved port.",
            settings.port,
            reservation.configured_port()
        ));
    }

    // The key list travels beside the settings document, sealed for this
    // Windows user. A container that is absent, foreign or unreadable is not a
    // startup failure: the shell runs with an empty ring and remembers why, so
    // the settings window can report it and a press can refuse with it. Nothing
    // falls back to a plain-text key file.
    let secrets_path = secrets::secrets_path(&config_path);
    let (keys, secrets_error) = match secrets::load_secret_keys(&secrets_path) {
        Ok(keys) => (keys, None),
        Err(error) => (Arc::new(secrets::KeyRing::empty()), Some(error)),
    };

    // What the launcher will be asked to register, and whether a take could be
    // recognized at all. Both decide where the window opens if this start needs
    // one; neither is decided by the wrapped hotkey text alone.
    let hotkey = settings.hotkey.clone();
    let usable_keys = secrets_error.is_none() && !keys.is_empty();
    let secrets_message = secrets_error.as_ref().map(|error| error.to_string());

    // One coherent configuration, built once: the backend serves every request
    // from it, and a dictation pins the revision it started under.
    let runtime = Arc::new(SharedRuntime::new(Arc::new(RuntimeSnapshot {
        settings,
        keys,
        revision: 1,
    })));
    let provider = Arc::new(provider::Provider::new());

    // Before any webview: the renderer-facing commands reach for this state on
    // their first call, and the overlay is created a few lines below.
    app.manage(Shell::new(Arc::clone(&runtime), secrets_message));
    app.manage(capture::NativeCapture::new());
    // The installer's own way to ask this shell to leave: it signals a named
    // event instead of flashing a console, and the watcher polls it on every
    // tick. A process that cannot create the event still runs - an installer
    // that finds none waits out its own deadline and forces the process.
    match installer::QuitSignal::new() {
        Ok(signal) => { app.manage(signal); },
        Err(err) => eprintln!("{APP_TITLE}: {err}."),
    }
    // The port facts come from the reservation, before the backend takes it:
    // the settings window reports the port this start asked for and the port
    // that is really served, and neither follows the editable document.
    let ports = StartupPorts {
        configured: reservation.configured_port(),
        actual: reservation.actual_port(),
        fallback: reservation.fallback(),
    };
    app.manage(PreferencesState::new(
        config_path,
        Arc::clone(&runtime),
        Arc::clone(&provider),
        secrets_error,
        ports,
    ));

    // The in-process backend serves the socket this run reserved before a single
    // window exists; `start` returns once that socket is accepting. A start that
    // fell back serves the temporary port until the next restart, and keeps the
    // saved one for that restart.
    if reservation.fallback() {
        eprintln!(
            "speechek: port {} was in use at start; serving http://127.0.0.1:{} until the next start",
            reservation.configured_port(),
            reservation.actual_port()
        );
    }
    let running = match backend::Backend::start(
        reservation,
        Arc::clone(&runtime),
        Arc::clone(&provider),
    ) {
        Ok(running) => running,
        Err(err) => fatal(err),
    };
    app.manage(BackendState(Mutex::new(Some(running))));

    if let Err(err) = overlay::create(app) {
        fatal(format!("cannot create the hidden overlay: {err}"));
    }
    if let Err(err) = tray(app) {
        fatal(format!("cannot create the tray icon: {err}"));
    }

    // Last, once the overlay exists: the first press has to find a page that can
    // receive it. A chord that cannot be registered is kept as the reason the
    // window shows, not as a dialog that keeps the shell from starting.
    let launcher_error = register_launcher(app, &hotkey);
    if let Some(message) = &launcher_error {
        app.state::<PreferencesState>()
            .hotkey_error
            .lock()
            .replace(message.clone());
    }

    let handle = app.clone();
    std::thread::spawn(move || watch(handle));

    // Where this start has to be repaired: a key list that cannot recognize a
    // take comes first, because without it nothing can be dictated at all; then
    // a launcher that is not registered. A start with both in order asks for no
    // window, and the tray is the only way in.
    match (usable_keys, launcher_error.is_some()) {
        (false, _) => request_settings(app, "keys"),
        (true, true) => request_settings(app, "general"),
        (true, false) => {}
    }
}

/* -------------------------------------------------------------------------- */
/* Hotkey                                                                      */
/* -------------------------------------------------------------------------- */

/// Registers one chord with a gate of its own.
///
/// The handler is only a gate check and an atomic swap: the plugin holds its own
/// shortcut map while invoking it, so nothing else may run there — neither a
/// registration (which would re-enter the plugin) nor the decision itself, which
/// takes the session lock an apply worker may be holding while it waits for this
/// very registration to be made on the main thread.
fn register_gated(
    app: &AppHandle,
    shortcut: Shortcut,
    enabled: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
) -> Result<(), tauri_plugin_global_shortcut::Error> {
    let handle = app.clone();
    app.global_shortcut()
        .on_shortcut(shortcut, move |_app, _shortcut, event| {
            if !enabled.load(Ordering::SeqCst) {
                return;
            }
            if event.state == ShortcutState::Released {
                held.store(false, Ordering::SeqCst);
            } else if !held.swap(true, Ordering::SeqCst) {
                let handle = handle.clone();
                let gate = Arc::clone(&enabled);
                std::thread::spawn(move || on_press(&handle, gate));
            }
        })
}

/// Registers the launcher chord the settings document names.
///
/// One attempt, and nothing more: a chord that cannot be registered leaves the
/// launcher disabled with the reason kept for the settings window, instead of a
/// dialog that keeps the shell from starting. Nothing is retried at runtime
/// either — not here, and not on a press — so the only way a chord the user
/// wants starts working is an explicit chord apply that registers it.
fn register_launcher(app: &AppHandle, hotkey: &str) -> Option<String> {
    // The document's chord is checked against the same grammar the settings
    // window enforces: `"Escape"` and `"F12"` must not be registered as the
    // launcher, because one belongs to cancelling a dictation and the other is
    // reserved. A chord the grammar refuses is a repair, not a registration.
    let path = app.state::<PreferencesState>().config_path.clone();
    let Ok(hotkey) = settings::validate_hotkey(hotkey, &path) else {
        return Some(HOTKEY_NOT_REGISTERED.to_owned());
    };
    let Ok(shortcut) = hotkey.parse::<Shortcut>() else {
        return Some(HOTKEY_NOT_REGISTERED.to_owned());
    };
    let registration = RegisteredHotkey::launcher(shortcut);
    match registration.install(app) {
        Ok(()) => {
            let shell = inner(app);
            let _session = shell.session.lock();
            *shell.hotkey.lock() = Some(registration);
            None
        }
        Err(_) => Some(HOTKEY_NOT_REGISTERED.to_owned()),
    }
}

/// Whether the launcher slot already holds exactly this chord.
///
/// The id of the parsed chord decides, not its spelling: `Shortcut::id()` is the
/// hash the plugin registers under, so `"f2"` and `"F2"`, or `"control+shift+space"`
/// and `"Ctrl+Shift+Space"`, are one registration. A slot without an entry means
/// nothing is registered — including a saved chord that could not be registered
/// while the shell started — and the next chord apply must install it.
fn holds_chord(inner: &Inner, shortcut: Shortcut) -> bool {
    inner
        .hotkey
        .lock()
        .as_ref()
        .is_some_and(|current| current.shortcut.id() == shortcut.id())
}

/// Whether the running launcher is already the chord the given text names.
///
/// The same parsed identity check that lets `prepare_settings_hotkey` return
/// `Ok(None)`: equivalent spellings describe one registration, while an empty
/// slot always needs registration recovery. Only a hotkey-scoped apply asks
/// this; general and key applies never touch the launcher. Called with the
/// session lock held; only the hotkey slot itself is locked here.
pub(crate) fn settings_hotkey_is_current(app: &AppHandle, hotkey: &str) -> bool {
    let inner = inner(app);
    let path = app.state::<PreferencesState>().config_path.clone();
    let Some(shortcut) = parse_registration_chord(hotkey, &path) else {
        return false;
    };
    holds_chord(&inner, shortcut)
}

/// The chord a document's text names, validated and parsed the one way this
/// module registers chords.
fn parse_registration_chord(hotkey: &str, settings_path: &Path) -> Option<Shortcut> {
    let hotkey = settings::validate_hotkey(hotkey, settings_path).ok()?;
    hotkey.parse::<Shortcut>().ok()
}

/// Registers the chord the settings window wants as the launcher, with its gate
/// still closed, and says whether there is anything to do at all.
///
/// Called by a chord apply before any write, with the session lock held: the
/// candidate is registered while the chord that is actually running keeps
/// working, so the files may still fail to be written and the launcher is never
/// left without a chord. `Ok(None)` means the wanted chord is already the working
/// registration, so nothing has to change — not even when the user only fixed
/// the mode or the keys.
pub(crate) fn prepare_settings_hotkey(
    app: &AppHandle,
    hotkey: &str,
) -> Result<Option<RegisteredHotkey>, SettingsUiError> {
    let inner = inner(app);
    let path = app.state::<PreferencesState>().config_path.clone();
    let invalid = || SettingsUiError::new("HOTKEY_INVALID", HOTKEY_INVALID_MESSAGE);
    let hotkey = settings::validate_hotkey(hotkey, &path).map_err(|_| invalid())?;
    let shortcut = hotkey.parse::<Shortcut>().map_err(|_| invalid())?;
    if holds_chord(&inner, shortcut) {
        return Ok(None);
    }
    let candidate = RegisteredHotkey::candidate(shortcut);
    candidate
        .install(app)
        .map_err(|_| SettingsUiError::new("HOTKEY_CONFLICT", HOTKEY_CONFLICT_MESSAGE))?;
    Ok(Some(candidate))
}

/// Opens the gate of the chord just published and closes the one of the
/// chord it replaced, then gives the old chord back to Windows.
///
/// Called with the session lock held, after the single runtime publish: from here
/// on the new chord is the launcher. A chord that cannot be given back is a
/// warning and not a failure — the new one works, the files and the runtime agree,
/// and only leaving the process releases the old one.
pub(crate) fn commit_settings_hotkey(
    app: &AppHandle,
    candidate: Option<RegisteredHotkey>,
) -> Option<String> {
    let candidate = candidate?;
    let inner = inner(app);
    let previous = {
        let mut slot = inner.hotkey.lock();
        let gate = Arc::clone(&candidate.enabled);
        let previous = slot.replace(candidate);
        // The candidate becomes the launcher and the chord it replaced stops
        // being one. Nothing else has to be reset: the key state belongs to each
        // registration, so the chord that now owns the launcher starts from a
        // key that is not held, however long the old chord's key is kept down.
        gate.store(true, Ordering::SeqCst);
        if let Some(previous) = &previous {
            previous.enabled.store(false, Ordering::SeqCst);
        }
        previous
    };
    // What the window shows follows the registration: a chord that is running
    // now is not a reason for the repair banner any more.
    app.state::<PreferencesState>().hotkey_error.lock().take();
    let previous = previous?;
    match app.global_shortcut().unregister(previous.shortcut) {
        Ok(()) => None,
        Err(_) => Some(HOTKEY_UNRELEASED_MESSAGE.to_owned()),
    }
}

/// Gives back a candidate chord that was registered but never published.
///
/// Called with the session lock held, on the failure path before the runtime is
/// published: the chord that is actually running was never touched, so the
/// candidate is simply unregistered again. A candidate that cannot be released
/// keeps its gate closed and is named in the warning; it can never start a
/// dictation, and the running chord is untouched.
pub(crate) fn rollback_settings_hotkey(
    app: &AppHandle,
    candidate: Option<RegisteredHotkey>,
) -> Option<String> {
    let candidate = candidate?;
    candidate.enabled.store(false, Ordering::SeqCst);
    match app.global_shortcut().unregister(candidate.shortcut) {
        Ok(()) => None,
        Err(_) => Some(HOTKEY_CANDIDATE_UNRELEASED_MESSAGE.to_owned()),
    }
}

/// One press of the launcher hotkey.
fn on_press(app: &AppHandle, gate: Arc<AtomicBool>) {
    let inner = inner(app);

    // A key that repeats, or arrives again at once, is still one dictation.
    let now = now_ms();
    if now.saturating_sub(inner.last_toggle.load(Ordering::SeqCst))
        < TOGGLE_DEBOUNCE.as_millis() as u64
    {
        return;
    }

    // Stopping and finalizing are not replaced by another press. The window the
    // text goes to is decided when the text is ready, not now: a dictation lands
    // in whichever window holds the foreground by the time it can be pasted.
    if matches!(phase_of(&inner), Phase::Finalizing | Phase::Pasting) {
        return;
    }
    inner.last_toggle.store(now, Ordering::SeqCst);
    toggle(app, &gate);
}

/// Opens, arms or stops the dictation. Every accepted press ends up here.
///
/// The session lock is held for the whole decision, so a press of Escape that
/// arrives while this one is still placing the pill waits for the generation
/// this press creates instead of cancelling the dictation before it. The lock is
/// the outer one of the whole shell: each settings apply takes it first
/// and then the draft, so nothing here may take the draft lock.
fn toggle(app: &AppHandle, gate: &Arc<AtomicBool>) {
    let inner = inner(app);
    let _session = inner.session.lock();
    // A press that was already queued when the chord was replaced belongs to the
    // registration that is no longer the launcher.
    if !gate.load(Ordering::SeqCst) {
        return;
    }
    match phase_of(&inner) {
        Phase::Idle => {
            // The settings window holds the foreground: a press there belongs
            // to the window while its chord field has the keyboard — the chord
            // typed into that field is text and must never start a take that
            // steals the focus. With the field out of focus the chord is the
            // launcher again, so a take may start over the open window.
            //
            // The flags are kept by the window's own focus events and by the
            // page's report of its field; a window that went away without
            // reporting either loss must not silence the launcher forever, so
            // flags that outlive the window are dropped here, on the cheap map
            // lookup and nothing else.
            let mut in_front = inner.settings_focused.load(Ordering::SeqCst);
            let mut field_focused = inner.hotkey_field_focused.load(Ordering::SeqCst);
            if (in_front || field_focused)
                && app
                    .get_webview_window(preferences::SETTINGS_LABEL)
                    .is_none()
            {
                inner.settings_focused.store(false, Ordering::SeqCst);
                clear_hotkey_field(&inner);
                in_front = false;
                field_focused = false;
            }
            if inner.settings_activating.load(Ordering::SeqCst)
                || inner.shutdown.load(Ordering::SeqCst)
            {
                return;
            }
            // Windows hands the press to this process instead of the page, so
            // the focused field cannot see the keys at all. The page is told
            // the chord that is running and writes it into the field as text;
            // Enter or leaving the field is what may apply it. This runs under
            // the session lock a chord apply holds while it replaces this very
            // registration, so the text written is never one that was just
            // replaced.
            if in_front && field_focused {
                let hotkey = inner.runtime.snapshot().settings.hotkey.clone();
                emit_hotkey_key(app, &hotkey);
                return;
            }
            // The laboratory holds the microphone for a take it has prepared or
            // is recording: a press must not take it away. The reservation is
            // read under the session lock this press holds, so a take that is
            // stopping right now has its reservation gone already, and this
            // press may start.
            if lab_holds_microphone(&inner) {
                return;
            }
            // The take is bound to the configuration it starts under: this
            // revision is captured while the phase moves, and pinned to the
            // generation once the pill is really shown. An apply that lands in
            // between publishes a newer revision for the next take and never
            // reaches this one.
            let snapshot = inner.runtime.snapshot();
            if snapshot.keys.is_empty() {
                // Nothing could recognize a take, so none is started: no
                // microphone, no pill and no dictation to cancel. Instead the
                // window is asked for on the section that ends the state — the
                // press is the user saying the launcher is not doing anything,
                // and a release build has no console where the reason could be
                // read. The request is queued by the shell and shown by the
                // watcher, so it costs a press no lock but the one it holds.
                match &inner.secrets_error {
                    Some(reason) => eprintln!("{APP_TITLE}: {reason} A dictation cannot start."),
                    None => eprintln!(
                        "{APP_TITLE}: no API key is saved yet, so a dictation cannot start."
                    ),
                }
                request_settings(app, "keys");
                return;
            }
            {
                let mut state = inner.state.lock();
                state.inserted = false;
                state.snapshot = Some(snapshot);
            }
            inner.arm_started.store(now_ms(), Ordering::SeqCst);
            set_phase(&inner, Phase::Arming);
            // Escape belongs to the dictation from here on; it is registered
            // before the pill is placed, so the arming itself can be cancelled.
            let session_id = inner.session_id.fetch_add(1, Ordering::SeqCst) + 1;
            if let Err(err) = guard_cancel_key(app, session_id) {
                cancel_arming_locked(app, err.clone());
                show_message(&err, MB_ICONERROR);
                return;
            }
            // The dictation exists now, so the settings window stops offering a
            // chord change that would stop the take. The event is sent from
            // under the session lock on purpose: it has to arrive after this
            // transition and before the one that ends it, and the window's page
            // never waits for that lock.
            emit_dictation_state(app, true);
            if !inner.overlay_listening.load(Ordering::SeqCst) {
                // The overlay page is still loading; the dictation is handed to
                // it the moment it says its listener is up.
                inner.pending_toggle.store(true, Ordering::SeqCst);
                return;
            }
            if let Err(err) = arm_overlay(app) {
                cancel_arming_locked(app, err);
            }
        }
        Phase::Arming => {
            // The recorder is not listening yet: remember exactly one stop and
            // deliver it when the recorder reports that it started. The pill
            // stays up while the recorder closes and the take is readied; it
            // comes down with the hand-over of the text.
            inner.queued_stop.store(true, Ordering::SeqCst);
        }
        Phase::Recording => {
            let generation = inner.generation.load(Ordering::SeqCst);
            set_phase(&inner, Phase::Finalizing);
            // Stop only: the pill stays up while the recorder closes and the take
            // is recognized, and comes down with the hand-over of the text.
            emit_toggle(app, generation);
        }
        // The renderer is closing the recorder; the outcome decides what
        // happens next, and this press is simply lost.
        Phase::Finalizing | Phase::Pasting => {}
    }
}

/// Reveals the overlay and tells the renderer to start recording. The overlay
/// is shown without taking the foreground, so the target window keeps it.
fn arm_overlay(app: &AppHandle) -> Result<(), String> {
    let inner = inner(app);
    if !inner.overlay_listening.load(Ordering::SeqCst) {
        return Err("the overlay page did not report that it is listening".to_string());
    }
    let snapshot = inner
        .state
        .lock()
        .snapshot
        .clone()
        .ok_or_else(|| "the dictation was started without a configuration".to_string())?;
    let generation = overlay::show(app)?;
    inner.generation.store(generation, Ordering::SeqCst);
    // Pinned before the renderer hears about the dictation and before any
    // request of it can arrive: the take uses this revision's ring and mode
    // even when an apply publishes another one while it is running.
    inner.runtime.pin(generation, snapshot);
    emit_toggle(app, generation);
    Ok(())
}

/// A dictation that could not be handed to the overlay leaves no state behind.
///
/// The caller holds the session lock: the phase, the captured revision and the
/// pin this clears belong to the arming that lock covers, so a press waiting
/// for the lock cannot find them gone and a stale timeout cannot retire the
/// configuration of the dictation that replaced it.
fn cancel_arming_locked(app: &AppHandle, err: String) {
    let inner = inner(app);
    inner.pending_toggle.store(false, Ordering::SeqCst);
    inner.queued_stop.store(false, Ordering::SeqCst);
    let generation = inner.generation.load(Ordering::SeqCst);
    inner.state.lock().phase = Phase::Idle;
    retire_dictation_locked(&inner, generation);
    // No dictation is left to cancel, so Escape goes back to Windows.
    release_cancel_key(app);
    // A stream this dictation opened on the way is not left running.
    capture::shutdown(app);
    // The dictation is over before it ever became one, so the settings window
    // may offer a chord change again.
    emit_dictation_state(app, false);
    eprintln!("{APP_TITLE}: {err}.");
}

/// Puts away an overlay that never reported a running capture.
///
/// The condition is checked again under the session lock instead of being taken
/// from the watcher's earlier look: an arming that ended, or that started its
/// capture, while the watcher was waking is not put away by a stale timeout.
fn cancel_arming_timeout(app: &AppHandle) {
    let generation = {
        let inner = inner(app);
        let _session = inner.session.lock();
        if phase_of(&inner) != Phase::Arming
            || now_ms().saturating_sub(inner.arm_started.load(Ordering::SeqCst))
                < ARM_TIMEOUT.as_millis() as u64
        {
            return;
        }
        cancel_arming_locked(
            app,
            "the overlay did not report a running capture; putting it away".to_string(),
        );
        inner.generation.load(Ordering::SeqCst)
    };
    overlay::hide_now(app, generation);
}

/// Sends the renderer the one event it acts on, tagged with the dictation it
/// belongs to so a late event cannot disturb the next one.
fn emit_toggle(app: &AppHandle, generation: u64) {
    if let Err(err) = app.emit_to(OVERLAY_LABEL, EVENT_TOGGLE, generation) {
        eprintln!("{APP_TITLE}: cannot reach the overlay: {err}.");
    }
}

/// What the settings window is told about a dictation.
#[derive(Clone, Copy, Serialize)]
struct DictationState {
    active: bool,
}

/// Tells the settings window whether a dictation is running.
///
/// The page disables its chord capture while one is: a capture would have to
/// take the foreground of a window the take is running under, and the chord
/// being replaced is the one that stops the take. A missing settings window is
/// not a failure — the event is a fact about the shell, and the answer to
/// `settings_open` carries the same fact.
fn emit_dictation_state(app: &AppHandle, active: bool) {
    let _ = app.emit_to(
        preferences::SETTINGS_LABEL,
        EVENT_DICTATION_STATE,
        DictationState { active },
    );
}

/// What the settings window is told when a press of the launcher chord belongs
/// to its chord field.
#[derive(Clone, Serialize)]
struct SettingsHotkeyKey {
    hotkey: String,
}

/// Tells the settings window to write the running chord into its field.
///
/// The page reacts only while the field really holds the keyboard, and writing
/// the chord there is the same gesture as typing it: nothing is applied until
/// Enter or leaving the field, and the launcher itself is untouched.
fn emit_hotkey_key(app: &AppHandle, hotkey: &str) {
    let _ = app.emit_to(
        preferences::SETTINGS_LABEL,
        EVENT_SETTINGS_HOTKEY_KEY,
        SettingsHotkeyKey {
            hotkey: hotkey.to_owned(),
        },
    );
}

/// Hands a remembered close to the settings page, if there is one.
///
/// The flag is taken with a swap, so exactly one of the two sides that can call
/// this — the window's own close button and the page reporting that it is
/// reachable — ever sends the request.
fn flush_settings_close(app: &AppHandle) -> bool {
    let inner = inner(app);
    // A quit warning must not run in a hidden WebView while the watcher is
    // still bringing the window forward. A dictation does not defer it either:
    // the window may stand in front of a running take, and the page then decides
    // about its draft exactly as it would in any other phase. A close never
    // cancels, stops or re-generates the dictation it was asked under.
    if !inner.settings_page_ready.load(Ordering::SeqCst)
        || inner.pending_settings.load(Ordering::SeqCst)
        || inner.settings_activating.load(Ordering::SeqCst)
        || !app
            .get_webview_window(preferences::SETTINGS_LABEL)
            .is_some_and(|window| window.is_visible().unwrap_or(false))
    {
        return false;
    }
    let reason = inner.pending_close.swap(0, Ordering::SeqCst);
    if reason == 0 {
        return false;
    }
    let payload = SettingsCloseRequest {
        reason: if reason == 2 { "quit" } else { "close" },
    };
    if app
        .emit_to(preferences::SETTINGS_LABEL, EVENT_SETTINGS_CLOSE, payload)
        .is_err()
    {
        inner.pending_close.fetch_max(reason, Ordering::SeqCst);
        return false;
    }
    true
}

#[derive(Clone, Copy, Serialize)]
struct SettingsCloseRequest {
    reason: &'static str,
}

fn request_settings_close(app: &AppHandle) {
    inner(app).pending_close.fetch_max(1, Ordering::SeqCst);
    flush_settings_close(app);
}

pub(crate) fn request_settings_quit(app: &AppHandle) {
    inner(app).pending_close.fetch_max(2, Ordering::SeqCst);
    if flush_settings_close(app) {
        return;
    }
    request_settings(app, "last");
}

/// Marks the settings page as reachable and hands over a close it could not hear.
///
/// Called by the settings state once the page has asked to open — the page
/// registers its listeners before that call, so this is the first moment the
/// shell can send it anything at all; the load of its document says nothing
/// about listeners of its own being in place. A close that was asked for before
/// this is delivered here, exactly once, with the same targeted event the
/// window's own close button sends.
pub(crate) fn settings_ui_ready(app: &AppHandle) {
    inner(app).settings_page_ready.store(true, Ordering::SeqCst);
    flush_settings_close(app);
}

/* -------------------------------------------------------------------------- */
/* Cancelling a dictation                                                      */
/* -------------------------------------------------------------------------- */

/// Escape, the key the shell takes for itself only while a dictation exists.
///
/// It is registered on the way into a dictation and given back on the way out,
/// so no other application loses its Escape while speechek is idle. A bare
/// Escape can never be the launcher hotkey - the settings read refuses it - so
/// the two never have to share the key.
fn cancel_key() -> Shortcut {
    Shortcut::new(None, Code::Escape)
}

/// A session without its cancel key must not start. Each handler captures the
/// session ticket, so a queued Escape cannot cancel the next dictation.
fn guard_cancel_key(app: &AppHandle, session_id: u64) -> Result<(), String> {
    let shortcuts = app.global_shortcut();
    let key = cancel_key();
    if shortcuts.is_registered(key) {
        return Err(
            "Escape ещё занят предыдущей записью; повторите после её завершения.".to_string(),
        );
    }
    let handle = app.clone();
    shortcuts.on_shortcut(key, move |_app, _shortcut, event| {
        // Only the press matters. Cancelling is idempotent, and the release of
        // Escape is deliberately not waited for: cancelling gives the key back
        // to Windows, and the release of a key the shell no longer holds is not
        // delivered here at all.
        if event.state == ShortcutState::Pressed {
            // Unregistering Escape inside this callback would re-enter the
            // plugin's locked shortcut map.
            let handle = handle.clone();
            std::thread::spawn(move || cancel_dictation(&handle, session_id));
        }
    })
    .map_err(|err| format!("Не удалось зарегистрировать Escape для отмены записи: {err}. Освободите клавишу в другой программе и повторите."))
}

/// Gives Escape back to Windows once no dictation can be cancelled any more.
fn release_cancel_key(app: &AppHandle) {
    let shortcuts = app.global_shortcut();
    let key = cancel_key();
    if !shortcuts.is_registered(key) {
        return;
    }
    if let Err(err) = shortcuts.unregister(key) {
        eprintln!("{APP_TITLE}: Escape could not be given back to Windows: {err}.");
    }
}

/// Cancels the dictation in progress: Escape was pressed while one ran.
///
/// Nothing of it is kept. The pill goes away before the microphone is even
/// closed, the native capture is stopped without waiting for the frames of a
/// take nobody wants, and the renderer is told to drop the take, so neither its
/// Live socket nor a batch upload of it survives. The phase returns to Idle and
/// the generation is marked cancelled, which is what refuses every late answer
/// of this dictation: no transcription, no insertion, no window.
///
/// Escape is given back to Windows here as well: from this press on it belongs
/// to whichever application the user is typing into.
///
/// Idempotent: a key repeat, or a press that raced the end of the dictation,
/// finds nothing to cancel and changes nothing.
fn cancel_dictation(app: &AppHandle, session_id: u64) {
    let inner = inner(app);
    let _session = inner.session.lock();
    if inner.session_id.load(Ordering::SeqCst) != session_id
        || matches!(phase_of(&inner), Phase::Idle | Phase::Pasting)
    {
        return;
    }
    let generation = inner.generation.load(Ordering::SeqCst);
    inner.state.lock().phase = Phase::Idle;
    inner.pending_toggle.store(false, Ordering::SeqCst);
    inner.queued_stop.store(false, Ordering::SeqCst);
    // The take is gone, so nothing may use the revision it pinned: the next
    // dictation starts from whatever the runtime holds by then.
    retire_dictation_locked(&inner, generation);
    // Before the renderer is told anything: from here on a command that belongs
    // to this generation is refused, however late it arrives.
    inner.cancelled.store(generation, Ordering::SeqCst);
    release_cancel_key(app);
    overlay::hide_now(app, generation);
    // The renderer hears about it before the device is closed: the take must be
    // dropped and its requests cancelled while the microphone is still winding
    // down. The frames a stopped capture would still deliver belong to the
    // cancelled generation, and the renderer no longer has a capture to take
    // them.
    emit_cancel(app, generation);
    capture::shutdown(app);
    // The microphone is closed by now, so the system sound of the take has been
    // given back — or it has not. A restore that failed leaves the mute in the
    // worker's hands, so this release is both the second attempt and the
    // answer: a failure here is the one thing a cancelled take still has to
    // say, and it is said through the pill the renderer brings back for it.
    if let Err(cause) = audio::release(generation) {
        eprintln!("speechek: {cause}.");
        inner.pending_mute_warning.store(generation, Ordering::SeqCst);
        capture::mute_warning_after_take(app, generation);
    }
    // The take is gone, so the settings window may offer a chord change again.
    emit_dictation_state(app, false);
}

/// Tells the renderer that the take was cancelled. The generation is the one the
/// dictation was announced with, so a late cancel cannot touch the dictation
/// that replaced it.
fn emit_cancel(app: &AppHandle, generation: u64) {
    if let Err(err) = app.emit_to(OVERLAY_LABEL, EVENT_CANCEL, generation) {
        eprintln!("{APP_TITLE}: cannot reach the overlay to cancel a dictation: {err}.");
    }
}

/// Whether the installer signaled this process to leave.
///
/// A build that never created the event has no installer channel at all: the
/// lookup answers `None` and the watcher simply keeps running, while the
/// installer waits out its own deadline.
fn installer_requested(app: &AppHandle) -> bool {
    app.try_state::<installer::QuitSignal>()
        .is_some_and(|signal| signal.is_signaled())
}

/// Watches the two things nothing reports: an overlay that never started a
/// capture, and a pending request for the settings window.
fn watch(app: AppHandle) {
    loop {
        std::thread::sleep(TICK);
        // The installer's request is read before anything that can wait: a
        // dictation or a settings activation must not stand between the signal
        // and the exit it asked for.
        if installer_requested(&app) {
            preferences::request_installer_quit(&app);
            return;
        }
        let inner = inner(&app);
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // The conditions are only a cheap filter; the timeout itself checks them
        // again under the session lock before it retires anything.
        if phase_of(&inner) == Phase::Arming
            && now_ms().saturating_sub(inner.arm_started.load(Ordering::SeqCst))
                >= ARM_TIMEOUT.as_millis() as u64
        {
            cancel_arming_timeout(&app);
        }

        // The settings window takes the foreground, so it is only ever opened
        // for a request that is still pending. A dictation that is running does
        // not hold the request back: the window is shown and focused at once,
        // and the take pastes into whatever owns the foreground when its text
        // is ready. The session lock is the one a press waits for as well, so
        // the window cannot appear in the middle of a decision about a
        // dictation.
        if inner.pending_settings.load(Ordering::SeqCst)
            && !inner.settings_activating.load(Ordering::SeqCst)
        {
            let _session = inner.session.lock();
            // Both flags are read again under the lock: a close that answered
            // the request while this thread was waiting for it has cleared them
            // already, and a request must never be served after it was answered.
            if inner.pending_settings.load(Ordering::SeqCst)
                && !inner.settings_activating.load(Ordering::SeqCst)
                && !inner.shutdown.load(Ordering::SeqCst)
            {
                activate_settings(&app);
            }
        }
        if inner.pending_close.load(Ordering::SeqCst) != 0 {
            flush_settings_close(&app);
        }
    }
}

/* -------------------------------------------------------------------------- */
/* Tray and windows                                                            */
/* -------------------------------------------------------------------------- */

/// The tray icon: the way into the settings window, and the only thing the shell
/// shows while no dictation runs.
///
/// A right click opens the native menu only. The two actions preserve the
/// deferred settings activation and the page's unflushed key editor.
fn tray(app: &AppHandle) -> tauri::Result<()> {
    let settings = MenuItem::with_id(app, "settings", "Настройка", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&settings, &quit])?;
    TrayIconBuilder::with_id("speechek")
        .icon(Image::from_bytes(include_bytes!("../icons/icon.png"))?)
        .tooltip(APP_TITLE)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "settings" => request_settings(app, "last"),
            "quit" => {
                let handle = app.clone();
                std::thread::spawn(move || preferences::request_quit(&handle));
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// The section a request named, as the page spells it.
fn section_name(section: u8) -> &'static str {
    match section {
        SECTION_GENERAL => "general",
        SECTION_KEYS => "keys",
        _ => "last",
    }
}

/// Asks for the settings window, or answers the page that the window may go.
///
/// Both halves are deliberately lock-free: the tray callback runs on the thread
/// that delivers tray events, and `"close"` comes from a settings command body.
/// The window is shown by the watcher, whether or not a dictation is running:
/// it is put in front as soon as a request asks for it.
pub(crate) fn request_settings(app: &AppHandle, request: &str) {
    // A second launch asks for the window before this process has a shell of its
    // own; there is nothing to ask then, and no state to look up.
    let Some(shell) = app.try_state::<Shell>() else {
        return;
    };
    let inner = shell.0.clone();
    match request {
        "close" => {
            // A request that was still waiting is answered by this: the page has
            // decided the window may go, so it must not be shown instead — not
            // even by an activation that is already on its way to the main
            // thread, which the bumped token stops. A close remembered from
            // before the page was listening is answered by this too.
            inner.pending_settings.store(false, Ordering::SeqCst);
            inner.activation_token.fetch_add(1, Ordering::SeqCst);
            inner.pending_close.store(0, Ordering::SeqCst);
            // The barrier stays up until the window is really hidden: a press
            // that arrives in between must not start a take under a window that
            // is still in front of everything. The hide itself is window work,
            // so it runs on the main thread rather than on whichever command
            // body called this.
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Some(window) = handle.get_webview_window(preferences::SETTINGS_LABEL) {
                    let _ = window.hide();
                }
                let inner = crate::inner(&handle);
                // A hidden window cannot be the one the next press belongs to,
                // even if no focus event follows the hide, and a field that is
                // off screen holds no keyboard.
                inner.settings_focused.store(false, Ordering::SeqCst);
                clear_hotkey_field(&inner);
                inner.settings_activating.store(false, Ordering::SeqCst);
            });
        }
        section => {
            // The shell is leaving: nothing may ask for a window that is about
            // to be torn down.
            if inner.shutdown.load(Ordering::SeqCst) {
                return;
            }
            inner.pending_section.store(
                match section {
                    "general" => SECTION_GENERAL,
                    "keys" => SECTION_KEYS,
                    _ => SECTION_LAST,
                },
                Ordering::SeqCst,
            );
            inner.pending_settings.store(true, Ordering::SeqCst);
        }
    }
}

/// The section a re-shown window is told to open on.
#[derive(Clone, Serialize)]
struct SettingsOpenRequest {
    section: &'static str,
}

/// Shows the settings window and puts it in front, on the main thread.
///
/// Called by the watcher with the session lock held. A dictation that is running
/// does not hold it back: the window is shown and focused at once, and the take
/// pastes into whatever owns the foreground when its text is ready. The closure
/// this dispatches touches neither the session, the draft nor the runtime — only
/// the window and the two flags — so an apply worker that is waiting for the
/// main thread to register a chord can never be waiting behind this. The pending request is answered whatever happens, including a
/// window that cannot be created: the shell would otherwise ask for it at every
/// tick.
fn activate_settings(app: &AppHandle) {
    let shell = inner(app);
    shell.settings_activating.store(true, Ordering::SeqCst);
    // Every activation gets a token of its own. A close that arrives while this
    // one is still on its way to the main thread bumps the token, and the
    // closure below then leaves the window alone: a request the page has already
    // answered must not be undone by a window appearing after the answer.
    let token = shell.activation_token.fetch_add(1, Ordering::SeqCst) + 1;
    let section = section_name(shell.pending_section.load(Ordering::SeqCst));
    // Before the window is shown: a page that loads now reads the section with
    // its first `settings_open`, and an already loaded one hears it below.
    if let Some(state) = app.try_state::<PreferencesState>() {
        *state.pending_section.lock() = section.to_owned();
    }
    let handle = app.clone();
    let dispatched = app.run_on_main_thread(move || {
        let inner = inner(&handle);
        // The request has to be the current one and still unanswered: a close
        // that arrived while this closure was queued has already cancelled it,
        // and a window must never appear after the page answered for it.
        if inner.activation_token.load(Ordering::SeqCst) != token
            || !inner.pending_settings.load(Ordering::SeqCst)
        {
            return;
        }
        match handle.get_webview_window(preferences::SETTINGS_LABEL) {
            Some(window) => {
                let _ = window.show();
                let _ = window.set_focus();
                // A loaded page has already been through `settings_open`, so
                // the section it should switch to is only reachable as an
                // event.
                if inner.settings_page_ready.load(Ordering::SeqCst) {
                    let _ = window.emit(EVENT_SETTINGS_OPEN, SettingsOpenRequest { section });
                }
            }
            None => create_settings_window(&handle),
        }
        inner.settings_activating.store(false, Ordering::SeqCst);
        inner.pending_settings.store(false, Ordering::SeqCst);
        flush_settings_close(&handle);
    });
    if dispatched.is_err() {
        // The token says whether this activation is still the one the flags
        // belong to: an activation a close has already cancelled leaves nothing
        // to clear, and no error is worth showing for it.
        if shell.activation_token.load(Ordering::SeqCst) == token {
            shell.settings_activating.store(false, Ordering::SeqCst);
            shell.pending_settings.store(false, Ordering::SeqCst);
            show_message(
                "Не удалось открыть окно настроек: приложение не смогло обратиться к главному потоку.",
                MB_ICONERROR,
            );
        }
    }
}

/// Creates the one settings window.
///
/// It is the only window that may drive the settings commands, so it is also the
/// only one pinned to a single page: any other navigation is refused, as are new
/// windows and downloads, and the WebView is told not to offer to save or check
/// the spelling of what is typed into it — that form may hold a key.
fn create_settings_window(app: &AppHandle) {
    // A page that finished loading is a page that has registered its listeners,
    // which is what decides between "the window is told which section to open"
    // and "the page asks by itself". The builder takes the only handler the
    // window offers for that.
    let loaded = app.clone();
    // The one URL this window may be on, derived from the running dev URL so a
    // start on the temporary port of a fallback is matched too.
    let nav_url = resolved_app_page(app.config().build.dev_url.as_ref(), SETTINGS_PAGE_PATH);
    let load_url = nav_url.clone();
    let window = WebviewWindowBuilder::new(
        app,
        preferences::SETTINGS_LABEL,
        WebviewUrl::App(SETTINGS_PAGE_PATH.into()),
    )
    .title(SETTINGS_TITLE)
    .inner_size(SETTINGS_SIZE.0, SETTINGS_SIZE.1)
    .min_inner_size(SETTINGS_MIN_SIZE.0, SETTINGS_MIN_SIZE.1)
    .resizable(true)
    .minimizable(true)
    .maximizable(true)
    .decorations(true)
    .visible(false)
    .general_autofill_enabled(false)
    .on_navigation(move |url| url == &nav_url)
    .on_new_window(|_url, _features| NewWindowResponse::Deny)
    .on_download(|_webview, _event| false)
    .on_page_load(move |_window, payload| {
        // A document that starts loading has no listener of its own in place
        // yet, so nothing of the previous page is reachable any more: readiness
        // comes back when the page asks to open, which it does after its
        // listeners are registered.
        if matches!(payload.event(), PageLoadEvent::Started) && payload.url() == &load_url {
            let inner = inner(&loaded);
            inner.settings_page_ready.store(false, Ordering::SeqCst);
            // A document that is starting to load has no focused field yet, and
            // nothing the page it replaces still has in flight may reach it:
            // the report document is replaced here, and the new page reads its
            // own identity with a handshake before its first report.
            reset_hotkey_field_document(&inner);
        }
    })
    .build();

    let window = match window {
        Ok(window) => window,
        Err(err) => {
            show_message(
                &format!("Не удалось открыть окно настроек: {err}."),
                MB_ICONERROR,
            );
            return;
        }
    };
    let handle = app.clone();
    window.on_window_event(move |event| match event {
        // The close button asks the page what to do with the draft. The window
        // is not closed here, and no lock is taken: the page answers with
        // `settings_close` (or with a Save first), and only that answer hides it.
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let handle = handle.clone();
            std::thread::spawn(move || request_settings_close(&handle));
        }
        // The window is in front, so the shell knows the focus it must not steal
        // with a take, and the focus the settings commands require of it.
        WindowEvent::Focused(true) => {
            let inner = inner(&handle);
            inner.settings_focused.store(true, Ordering::SeqCst);
            inner.settings_activating.store(false, Ordering::SeqCst);
        }
        // It lost the foreground, so nothing there belongs to it any more, and
        // a field cannot hold the keyboard of a window that does not have it.
        WindowEvent::Focused(false) => {
            let inner = inner(&handle);
            inner.settings_focused.store(false, Ordering::SeqCst);
            clear_hotkey_field(&inner);
        }
        WindowEvent::Destroyed => {
            // The window is gone, so its draft is too; whoever only learns this
            // must not wait for a command body that may hold the draft lock
            // across a registration on the main thread. A close nobody answered
            // dies with the window it belonged to.
            let inner = inner(&handle);
            inner.pending_close.store(0, Ordering::SeqCst);
            inner.settings_activating.store(false, Ordering::SeqCst);
            inner.settings_focused.store(false, Ordering::SeqCst);
            reset_hotkey_field_document(&inner);
            inner.settings_page_ready.store(false, Ordering::SeqCst);
            invalidate_preferences(&handle);
        }
        _ => {}
    });

    let _ = window.show();
    let _ = window.set_focus();
}

/// Drops the settings draft, off the UI thread.
fn invalidate_preferences(app: &AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || {
        if let Some(state) = handle.try_state::<PreferencesState>() {
            state.invalidate();
        }
    });
}

/// Opens the comparison window, creating it the first time the settings window
/// asks for it. Its close button hides it — the comparison stays loaded and the
/// shell keeps living in the tray — and ends whatever take it was recording
/// natively: the microphone is never left to a hidden page, which `pagehide`
/// alone would not guarantee.
///
/// The window work is queued to the main thread: the settings window's action
/// calls this from a command body, and neither creating nor showing a window
/// belongs there.
pub(crate) fn open_lab(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || open_lab_now(&handle));
}

fn open_lab_now(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LAB_LABEL) {
        let _ = window.show();
        let _ = window.set_focus();
        // The window is visible again, so the laboratory may reserve the
        // microphone again — and a prepare it sent while it was closed is
        // refused until this very moment, never left as a take nobody stops.
        inner(app).lab_closed.store(false, Ordering::SeqCst);
        return;
    }

    let window = match WebviewWindowBuilder::new(app, LAB_LABEL, WebviewUrl::App(LAB_PAGE_PATH.into()))
        .title("Speechek — сравнение")
        .inner_size(LAB_SIZE.0, LAB_SIZE.1)
        .visible(false)
        .build()
    {
        Ok(window) => window,
        Err(err) => {
            eprintln!("{APP_TITLE}: cannot open the comparison window: {err}.");
            return;
        }
    };

    let handle = window.clone();
    let closing = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = handle.hide();
            lab_window_closed(&closing);
        }
    });

    let _ = window.show();
    let _ = window.set_focus();
    // A window that exists is a window that is shown: the laboratory may
    // reserve the microphone again.
    inner(app).lab_closed.store(false, Ordering::SeqCst);
}

/// The laboratory window is closing: its take is ended natively and its page is
/// told, because the window is only hidden and the page stays loaded.
///
/// `pagehide` is not guaranteed when a window is hidden, so the event is the
/// page's only notice: it destroys its recorder and resets its controls, and
/// asks the shell for nothing — everything is already closed here.
///
/// The window is marked closed, its reservation released and the capture
/// admission moved under the session lock before the device is stopped: a take
/// whose microphone is still opening can never install itself, a prepare that
/// was still in flight when the window went away is refused instead of leaving a
/// take nobody can stop, and a hidden page can never hold the microphone. The
/// device itself is closed off the window thread, so the UI never waits for a
/// driver.
fn lab_window_closed(app: &AppHandle) {
    let inner = inner(app);
    let generation = {
        let _session = inner.session.lock();
        inner.lab_closed.store(true, Ordering::SeqCst);
        let generation = take_lab_capture(&inner);
        if generation.is_some() {
            // The generation is closed before its capture is stopped: whatever
            // its worker is still doing cannot install a recording any more.
            capture::close_admission(app);
        }
        generation
    };
    if let Err(err) = app.emit_to(LAB_LABEL, EVENT_LAB_CLOSED, ()) {
        eprintln!("{APP_TITLE}: cannot tell the comparison window it is closing: {err}.");
    }
    let Some(generation) = generation else {
        return;
    };
    let handle = app.clone();
    if let Err(err) = std::thread::Builder::new()
        .name(format!("speechek-lab-stop-{generation}"))
        .spawn(move || {
            if let Err(err) = capture::stop(&handle, capture::CaptureOwner::Lab(generation)) {
                eprintln!("{APP_TITLE}: the comparison capture did not stop cleanly: {err}.");
            }
        })
    {
        eprintln!("{APP_TITLE}: cannot start the comparison stop thread: {err}.");
    }
}

/// Leaves the application.
///
/// The worker that decided to leave marks the shell as closing first: from here
/// on a press starts nothing, the launcher asks for no window, and the
/// configuration a dictation pinned is released under the session lock. Only
/// then is the exit requested. The exit path itself — the port, the hook, the
/// microphone, the chords — must not have to wait for the session lock, because
/// a Save worker may be holding it while it waits for the main thread, so this
/// is the last place the lock is taken.
pub(crate) fn quit_from_settings(app: &AppHandle) {
    if let Some(shell) = app.try_state::<Shell>() {
        let inner = shell.0.clone();
        inner.shutdown.store(true, Ordering::SeqCst);
        inner.pending_settings.store(false, Ordering::SeqCst);
        let _session = inner.session.lock();
        let generation = inner.generation.load(Ordering::SeqCst);
        inner.state.lock().phase = Phase::Idle;
        retire_dictation_locked(&inner, generation);
        // Escape belongs to Windows from here on; whatever dictation was left is
        // not one the shell can cancel any more.
        release_cancel_key(app);
    }
    app.exit(0);
}

/// The bounded cleanup of an exit the installer asked for.
///
/// Not [`quit_from_settings`]'s path: the installer already confirmed that
/// unapplied key changes are lost, so nothing is asked, offered back or
/// written. The caller holds the session lock and latched the shell as leaving
/// before it took it, so no new decision can start behind this one.
///
/// A dictation whose text was never handed to Windows is cancelled exactly like
/// an Escape: the generation is refused from here on, the pinned revision is
/// retired and the renderer is told to drop the take. A hand-over that already
/// began is a Windows transfer and is not pretended away; the system sound is
/// left to [`audio`], which gives back only what the shell itself applied.
pub(crate) fn installer_exit_cleanup(app: &AppHandle, inner: &Inner) {
    let generation = inner.generation.load(Ordering::SeqCst);
    let pasting = phase_of(inner) == Phase::Pasting;
    inner.pending_toggle.store(false, Ordering::SeqCst);
    inner.queued_stop.store(false, Ordering::SeqCst);
    if !pasting {
        inner.cancelled.store(generation, Ordering::SeqCst);
    }
    retire_dictation_locked(inner, generation);
    {
        let mut state = inner.state.lock();
        state.phase = Phase::Idle;
        state.text = None;
    }
    release_cancel_key(app);
    overlay::hide_now(app, generation);
    if !pasting {
        // A `Pasting` take is a Windows transfer that already began; nothing
        // may tell its renderer the take was cancelled, while a take that never
        // reached the clipboard is dropped exactly like a cancelled one.
        emit_cancel(app, generation);
    }
}

/// Leaves nothing behind: the hotkey is given back, the microphone is released,
/// the overlay is put away and the last dictation is dropped.
fn cleanup(app: &AppHandle) {
    let inner = inner(app);
    inner.shutdown.store(true, Ordering::SeqCst);
    inner.pending_settings.store(false, Ordering::SeqCst);
    inner.pending_close.store(0, Ordering::SeqCst);
    inner.settings_activating.store(false, Ordering::SeqCst);
    inner.settings_focused.store(false, Ordering::SeqCst);
    clear_hotkey_field(&inner);
    let _ = app.global_shortcut().unregister_all();
    if let Some(slot) = inner.hotkey.lock().take() {
        slot.enabled.store(false, Ordering::SeqCst);
    }
    capture::shutdown(app);
    // The laboratory's reservation goes with the shell: there is no window left
    // to stop it, and `capture::shutdown` above closed whatever device it held.
    // The lab slot is a leaf lock, so the exit path takes it without the session
    // lock a Save worker may still hold.
    inner.lab.lock().take();
    // A capture that was still winding down may not have reached its own
    // release; the shell takes the system mute back here as a last resort.
    audio::shutdown();
    overlay::hide_now(app, inner.generation.load(Ordering::SeqCst));
    invalidate_preferences(app);
    let mut state = inner.state.lock();
    state.phase = Phase::Idle;
    state.text = None;
}

/* -------------------------------------------------------------------------- */
/* Renderer commands                                                           */
/* -------------------------------------------------------------------------- */

/// Only the overlay page may drive the shell; every command checks it. The
/// check is [`is_app_page`], so both the label and the exact document URL must
/// hold, and the URL follows the dev URL the shell is really serving from.
fn is_overlay(window: &WebviewWindow) -> bool {
    is_app_page(window, OVERLAY_LABEL, OVERLAY_PAGE_PATH)
}

/// The overlay reports that its listener is registered, and receives the native
/// window handle the shell and the renderer share. A dictation asked for while
/// the page was still loading is handed over now.
///
/// The dictation is armed under the session lock, which a Save worker may hold
/// while it waits for the UI thread to register a hotkey, so the work runs off
/// the UI thread.
#[tauri::command]
async fn overlay_ready(window: WebviewWindow, app: AppHandle) -> i64 {
    if !is_overlay(&window) {
        return 0;
    }
    tauri::async_runtime::spawn_blocking(move || {
        let inner = inner(&app);
        // The page is listening from now on, and a dictation that was waiting
        // for it is handed over under the session lock, exactly once: the swap
        // happens there, so a press that took the lock first and an Escape that
        // cancelled the arming while this command was on its way are both
        // visible here — a cancelled dictation is never armed, and a dictation
        // that armed itself in the meantime is not armed a second time.
        {
            let _session = inner.session.lock();
            inner.overlay_listening.store(true, Ordering::SeqCst);
            if inner.pending_toggle.swap(false, Ordering::SeqCst)
                && phase_of(&inner) == Phase::Arming
            {
                if let Err(err) = arm_overlay(&app) {
                    cancel_arming_locked(&app, err);
                }
            }
        }
        native_handle(&app)
    })
    .await
    .unwrap_or(0)
}

/// The overlay's native window handle, as the renderer sees it; zero when there
/// is no overlay window to hand out.
fn native_handle(app: &AppHandle) -> i64 {
    match app
        .get_webview_window(OVERLAY_LABEL)
        .map(|window| window.hwnd())
    {
        Some(Ok(hwnd)) => hwnd.0 as isize as i64,
        _ => 0,
    }
}

/// What the overlay needs to record one dictation: the mode it records in, and
/// the session tag every request of the take carries. Both belong to the
/// revision the dictation pinned, not to whatever the runtime holds when the
/// answer is built.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DictationContext {
    mode: String,
    session_id: u64,
}

/// Names the dictation the overlay has just been told to record.
///
/// The answer is the shell's promise for the take: the mode of the pinned
/// revision and the session tag, which is the generation the shell accepts
/// requests under. A generation that was cancelled, ended or replaced since the
/// toggle is refused, so a late answer cannot have a recorder started for a
/// dictation the shell is no longer running.
///
/// Reading the state takes the session lock, which the Save worker may hold
/// while it waits for the UI thread to register a hotkey, so the work runs off
/// the UI thread.
#[tauri::command]
async fn dictation_context(
    window: WebviewWindow,
    app: AppHandle,
    generation: u64,
) -> Result<DictationContext, preferences::SettingsUiError> {
    if !is_overlay(&window) {
        return Err(preferences::SettingsUiError::new(
            "FORBIDDEN",
            "Команда доступна только плашке диктовки Speechek.",
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let inner = inner(&app);
        let _session = inner.session.lock();
        let snapshot = inner.runtime.resolve(Some(generation)).ok_or_else(|| {
            preferences::SettingsUiError::new(
                "STALE_SESSION",
                "Эта диктовка уже завершена или отменена.",
            )
        })?;
        {
            let state = inner.state.lock();
            if inner.generation.load(Ordering::SeqCst) != generation
                || !matches!(
                    state.phase,
                    Phase::Arming | Phase::Recording | Phase::Finalizing
                )
                || inner.cancelled.load(Ordering::SeqCst) == generation
                || inner.shutdown.load(Ordering::SeqCst)
            {
                return Err(preferences::SettingsUiError::new(
                    "STALE_SESSION",
                    "Эта диктовка уже завершена или отменена.",
                ));
            }
        }
        Ok(DictationContext {
            mode: snapshot.settings.mode.clone(),
            session_id: generation,
        })
    })
    .await
    .map_err(|_| {
        preferences::SettingsUiError::new("INTERNAL", "Не удалось получить параметры диктовки.")
    })?
}

/// The overlay reports that its recorder is running. A stop pressed while it was
/// still starting is delivered here, exactly once; the pill has stayed up since
/// that press and comes down with the hand-over of the text.
#[tauri::command]
async fn capture_started(window: WebviewWindow, app: AppHandle, generation: u64) {
    if !is_overlay(&window) {
        return;
    }
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let inner = inner(&app);
        // The arming timeout decides whether this dictation is still arming
        // under this same lock: a capture that reports itself running while the
        // watcher is waking must not be put away as if it never started, and a
        // timeout that already decided to put it away must not be overruled by a
        // recorder that started in the same instant.
        let _session = inner.session.lock();
        let queued_stop = {
            let mut state = inner.state.lock();
            if generation != inner.generation.load(Ordering::SeqCst)
                || state.phase != Phase::Arming
                || inner.cancelled.load(Ordering::SeqCst) == generation
            {
                return;
            }
            let queued = inner.queued_stop.swap(false, Ordering::SeqCst);
            state.phase = if queued {
                Phase::Finalizing
            } else {
                Phase::Recording
            };
            queued
        };
        if queued_stop {
            emit_toggle(&app, generation);
        }
    })
    .await;
}

/// The overlay reports that it is closing the recorder: the hotkey stops
/// toggling recording from here, while the pill stays up and shows the take
/// being readied. The ten-minute cap and a failed arm both close the recorder
/// without a hotkey press, and neither takes the pill down by itself: the
/// hand-over hides it, and a failure brings it back with its reason.
#[tauri::command]
async fn capture_finalizing(window: WebviewWindow, app: AppHandle, generation: u64) {
    if !is_overlay(&window) {
        return;
    }
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let inner = inner(&app);
        // The same lock the timeout and the press path take: closing the
        // recorder is part of the dictation's state, not a fact about it.
        let _session = inner.session.lock();
        let mut state = inner.state.lock();
        if generation == inner.generation.load(Ordering::SeqCst)
            && matches!(state.phase, Phase::Arming | Phase::Recording)
        {
            state.phase = Phase::Finalizing;
        }
    })
    .await;
}

/// Ends the dictation only for an explicit `"success"` or `"error"` outcome.
///
/// A successful dictation leaves nothing on screen: the hand-over already hid
/// the pill, and this only makes sure it is gone. A failure brings the pill back
/// with the reason the renderer has already written into it, for four seconds.
/// Only the dictation the shell is on right now may do either — a result that
/// arrives after a newer dictation took over is dropped, so a stale error cannot
/// cover the recording that replaced it.
#[tauri::command]
async fn finish_session(
    window: WebviewWindow,
    app: AppHandle,
    generation: u64,
    outcome: String,
) -> Result<(), String> {
    if outcome != "success" && outcome != "error" {
        return Err("invalid dictation outcome".to_string());
    }
    if !is_overlay(&window) {
        return Err("finish_session is only available to the overlay window.".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let inner = inner(&app);
        let _session = inner.session.lock();
        if generation != inner.generation.load(Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut state = inner.state.lock();
            if state.phase == Phase::Idle {
                return Ok(());
            }
            state.phase = Phase::Idle;
        }
        inner.queued_stop.store(false, Ordering::SeqCst);
        // The take is over, so the revision it pinned is released: nothing that
        // belongs to it may be asked for again, and its ring stops being held.
        retire_dictation_locked(&inner, generation);
        // The dictation is over: Escape is nobody's to cancel any more.
        release_cancel_key(&app);
        capture::shutdown(&app);
        if outcome == "error" {
            overlay::show_error(&app, generation);
        } else {
            overlay::hide_now(&app, generation);
        }
        // The take is over, so the settings window may offer a chord change
        // again; the event is sent under the session lock so it cannot overtake
        // the one a dictation that is starting right now sends.
        emit_dictation_state(&app, false);
        Ok(())
    })
    .await
    .map_err(|err| format!("the dictation outcome could not be applied ({err})"))?
}

/* -------------------------------------------------------------------------- */
/* Native capture commands                                                     */
/* -------------------------------------------------------------------------- */

/// The overlay reports that it is opening the native capture of a dictation.
///
/// The stream is opened for the dictation the shell is on right now: an event
/// that arrives late from a finished dictation cannot open a microphone for the
/// next one. Only one stream is ever open, and the comparison window never has a
/// dictation, so it can never open one either.
///
/// The mute and the device of the take both come from the revision this
/// dictation pinned, never from whatever an apply published meanwhile: a setting
/// changed during a take takes effect with the next one. A pin that is gone is a
/// dictation that was cancelled or replaced, and no microphone is opened for it
/// at all.
///
/// The answer is what the renderer records with: [`capture::CaptureStart`] names
/// the device that really captured, which is the system one when the pinned
/// device is gone, and it only arrives once the stream has delivered its first
/// frame. A capture that never delivers one is this command's error, never a
/// late event about a microphone the renderer does not have yet.
///
/// Opening a device can wait on its driver, so the command does not block the UI
/// and the session lock is not held across it: admission is read under that lock
/// before the device is opened and read again under it when the device is up, so
/// a cancellation that lands in between closes the capture again instead of
/// letting it join a dictation that is already over. The promise that admission
/// carries travels into the capture itself as a token, so a cancellation that
/// lands while the driver is still answering is seen by the capture thread — it
/// opens nothing, records nothing and silences nothing — and not only by this
/// command.
#[tauri::command]
async fn start_native_capture(
    window: WebviewWindow,
    app: AppHandle,
    generation: u64,
) -> Result<capture::CaptureStart, String> {
    if !is_overlay(&window) {
        return Err("start_native_capture is only available to the overlay window.".to_string());
    }
    let state = inner(&app);
    let (mute_audio, input_device, admission) = {
        let _session = state.session.lock();
        let ended = || "this dictation is no longer arming its microphone.".to_string();
        if !admits_capture(&state, generation) {
            return Err(ended());
        }
        let snapshot = state.runtime.resolve(Some(generation)).ok_or_else(ended)?;
        // The capture's admission token is read here, while the lock every end
        // of a dictation takes is held: an end that lands after this point moves
        // the token before it closes anything, and the capture compares it again
        // before a device is opened, at every step until the first frame, and
        // once the stream runs — see `capture::start`.
        (
            snapshot.settings.mute_during_recording,
            snapshot.settings.input_device.clone(),
            capture::admission(&app),
        )
    };
    let opened = capture::start(
        &app,
        capture::CaptureOwner::Dictation(generation),
        input_device.as_deref(),
        mute_audio,
        admission,
    );
    let admitted = {
        let _session = state.session.lock();
        admits_capture(&state, generation)
    };
    if !admitted {
        // The device took seconds to open and the dictation is over by now. The
        // capture is closed through its own generation, so a newer take that
        // owns the microphone in the meantime is not touched.
        let _ = capture::stop(&app, capture::CaptureOwner::Dictation(generation));
        return Err("this dictation ended while its microphone was opening.".to_string());
    }
    if opened.is_err() {
        // A start that failed leaves nothing behind: a device attempt that was
        // still finishing when the command gave up is closed through its own
        // generation, so the microphone is free for the next take — of this
        // window or of the laboratory. A newer take is never touched.
        let _ = capture::stop(&app, capture::CaptureOwner::Dictation(generation));
    }
    opened
}

/// The overlay reports that its recorder is closed, and gets back the sequence
/// of the last frame the shell captured for this dictation.
///
/// Every frame up to that sequence is already on its way to the overlay, so the
/// renderer can wait for the tail instead of losing the last words.
#[tauri::command]
async fn stop_native_capture(
    window: WebviewWindow,
    app: AppHandle,
    generation: u64,
) -> Result<u64, String> {
    if !is_overlay(&window) {
        return Err("stop_native_capture is only available to the overlay window.".to_string());
    }
    let state = inner(&app);
    if generation != state.generation.load(Ordering::SeqCst)
        || !matches!(phase_of(&state), Phase::Recording | Phase::Finalizing)
    {
        return Err("this dictation is no longer capturing.".to_string());
    }
    capture::stop(&app, capture::CaptureOwner::Dictation(generation))
}

/* -------------------------------------------------------------------------- */
/* Laboratory commands                                                         */
/* -------------------------------------------------------------------------- */

/// Whether the window is the built-in laboratory, on the exact document the
/// shell opened for it. The page drives the microphone, so a lookalike path, a
/// query, a fragment or the URL of a previous run's port is refused.
fn is_lab(window: &WebviewWindow) -> bool {
    is_app_page(window, LAB_LABEL, LAB_PAGE_PATH)
}

/// The refusal every laboratory command answers when it is not called by the
/// laboratory's own page.
fn lab_forbidden() -> String {
    "Команда доступна только встроенной лаборатории Speechek.".to_string()
}

/// The laboratory reserves the microphone for one take.
///
/// The reservation exists while the page is still preparing its recorder, so
/// the frames of the take have somewhere to go the moment the device starts, and
/// so a dictation cannot take the microphone in between. Only the `lab` window
/// on its own page may ask. A dictation running right now, a second reservation
/// and a shell that is leaving all refuse it; the answer is the generation the
/// take is numbered with, which every later command of it names.
///
/// The reservation pins the configuration the take records with, so a setting
/// saved while it runs takes effect with the next take.
#[tauri::command]
async fn lab_prepare_capture(window: WebviewWindow, app: AppHandle) -> Result<u64, String> {
    if !is_lab(&window) {
        return Err(lab_forbidden());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let inner = inner(&app);
        let _session = inner.session.lock();
        reserve_lab_capture(&inner)
    })
    .await
    .unwrap_or_else(|_| Err("Не удалось подготовить запись: рабочий поток недоступен.".to_string()))
}

/// Opens the microphone of a prepared laboratory take.
///
/// The session lock is taken only to spend the take's one-shot right and read
/// the snapshot it was reserved with; the device is opened without that lock, so
/// the take can be stopped while its driver is still answering. A take whose
/// reservation is gone by the time its device answers — its window was closed, a
/// stop arrived, the shell left — is closed through its own owner and never
/// reaches the page.
///
/// The answer is [`capture::CaptureStart`], exactly as a dictation's start
/// command answers it, so the renderer knows which device really captured.
#[tauri::command]
async fn lab_start_capture(
    window: WebviewWindow,
    app: AppHandle,
    generation: u64,
) -> Result<capture::CaptureStart, String> {
    if !is_lab(&window) {
        return Err(lab_forbidden());
    }
    tauri::async_runtime::spawn_blocking(move || start_lab_capture(&app, generation))
        .await
        .unwrap_or_else(|_| Err("Не удалось открыть микрофон: рабочий поток недоступен.".to_string()))
}

fn start_lab_capture(app: &AppHandle, generation: u64) -> Result<capture::CaptureStart, String> {
    let inner = inner(app);
    let owner = capture::CaptureOwner::Lab(generation);
    let (snapshot, admission) = {
        let _session = inner.session.lock();
        let snapshot = request_lab_start(&inner, generation)?;
        // Read under the same lock that closes it: a stop that lands after this
        // point moves the token before it stops anything, and the capture
        // compares it again before it installs its recording.
        (snapshot, capture::admission(app))
    };
    let opened = match capture::start(
        app,
        owner,
        snapshot.settings.input_device.as_deref(),
        // A laboratory take never touches the system mute: its bookkeeping is
        // numbered by dictation generations.
        false,
        admission,
    ) {
        Ok(opened) => opened,
        Err(cause) => {
            eprintln!("{APP_TITLE}: the comparison take could not open a microphone: {cause}.");
            // The reservation is given back so the page may prepare another
            // take, and whatever the attempt left behind — a recording that was
            // installed in the instant the command gave up — is closed through
            // its own owner: a take of either window is never touched by it.
            release_failed_lab_capture(app, generation);
            let _ = capture::stop(app, owner);
            return Err(format!("Не удалось открыть микрофон: {cause}"));
        }
    };
    // The take was stopped, or the shell closed its capture, while the device
    // was opening: its own stop ends it — through its owner alone, so a newer
    // take of either window is never touched.
    let current = {
        let _session = inner.session.lock();
        lab_capture_is_current(&inner, generation)
    };
    if !current {
        let _ = capture::stop(app, owner);
        return Err("Запись была остановлена, пока открывался микрофон.".to_string());
    }
    Ok(opened)
}

/// Ends the laboratory take `generation` and answers the sequence of its last
/// frame, or zero when its microphone never opened.
///
/// The reservation is released and the shell's capture admission moved under the
/// session lock *before* the capture is stopped: a take whose device is still
/// opening can therefore never install itself, and the generation that was
/// closed can never regain a microphone. A generation that is not the reserved
/// one — a late stop of a take that is already over — changes nothing, touches
/// no newer take and answers zero.
#[tauri::command]
async fn lab_stop_capture(
    window: WebviewWindow,
    app: AppHandle,
    generation: u64,
) -> Result<u64, String> {
    if !is_lab(&window) {
        return Err(lab_forbidden());
    }
    tauri::async_runtime::spawn_blocking(move || stop_lab_capture(&app, generation))
        .await
        .unwrap_or_else(|_| Err("Не удалось остановить запись: рабочий поток недоступен.".to_string()))
}

fn stop_lab_capture(app: &AppHandle, generation: u64) -> Result<u64, String> {
    let inner = inner(app);
    let current = {
        let _session = inner.session.lock();
        let current = release_lab_capture(&inner, generation);
        if current {
            capture::close_admission(app);
        }
        current
    };
    if !current {
        return Ok(0);
    }
    match capture::stop(app, capture::CaptureOwner::Lab(generation)) {
        Ok(sequence) => Ok(sequence),
        Err(cause) => {
            eprintln!("{APP_TITLE}: the comparison capture did not stop cleanly: {cause}.");
            Err(format!(
                "Не удалось дождаться последних кадров записи: {cause}"
            ))
        }
    }
}

/// The renderer reports that it painted the warning of a take that is over.
///
/// A cancelled dictation takes its pill down immediately, so the sentence about
/// a system sound the shell could not give back would never be read. The shell
/// queues that sentence for the cancelled generation (see [`cancel_dictation`])
/// and brings the pill back here — after the page has painted it, and only while
/// the sentence is still the pending one. A newer dictation owns the pill by
/// then, or a release that succeeded in the meantime cleared the sentence, and
/// neither is touched.
#[tauri::command]
async fn mute_warning_shown(window: WebviewWindow, app: AppHandle, generation: u64) {
    if !is_overlay(&window) {
        return;
    }
    let state = inner(&app);
    if state.pending_mute_warning.load(Ordering::SeqCst) != generation {
        return;
    }
    state.pending_mute_warning.store(0, Ordering::SeqCst);
    overlay::show_warning(&app, generation);
}

/// Whether the hand-over of `generation` may still become a Windows paste.
///
/// The caller holds the session lock, so nothing can move between this answer
/// and the transition it authorizes. A cancelled take, one that was replaced,
/// a shell that is leaving - the installer's exit latches `shutdown` before it
/// ever waits - and any phase but `Finalizing` all refuse the late answer.
fn paste_may_begin(inner: &Inner, generation: u64) -> bool {
    inner.cancelled.load(Ordering::SeqCst) != generation
        && inner.generation.load(Ordering::SeqCst) == generation
        && !inner.shutdown.load(Ordering::SeqCst)
        && phase_of(inner) == Phase::Finalizing
}

/// Inserts the dictated text into whichever window owns the foreground when the
/// text is ready.
///
/// The transcript travels through the clipboard: it is published as a delayed
/// render promise, the `Ctrl+V` chord is sent, and only a read Windows reports
/// back to the owner — after the chord, with the foreground staying on the target
/// across it — counts as a paste. A window that takes the foreground while the
/// dictation runs therefore receives the text instead of the window the hotkey
/// was pressed in; a target that never reads the clipboard leaves the transcript
/// in the clipboard for one manual paste.
///
/// The pill comes down here, in the same step that turns the dictation into a
/// paste: it stayed up while the take was recorded and recognized, and nothing of
/// the hand-over is meant to be visible.
///
/// Returns `inserted` or `copied_fallback` — both leave the text in the clipboard
/// — or an `Err` when somebody else owns the clipboard by then (their copy is
/// left untouched) or the text could not be written at all. The text stays in
/// memory until the next dictation replaces it.
#[tauri::command]
async fn insert_text(
    window: WebviewWindow,
    app: AppHandle,
    generation: u64,
    text: String,
) -> Result<&'static str, String> {
    if !is_overlay(&window) {
        return Err("insert_text is only available to the overlay window.".to_string());
    }
    if text.trim().is_empty() {
        return Err("the dictation was empty, so there was nothing to insert.".to_string());
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "the dictation is longer than {} KiB.",
            MAX_TEXT_BYTES / 1024
        ));
    }
    let inner = inner(&app);
    {
        let mut state = inner.state.lock();
        if generation != inner.generation.load(Ordering::SeqCst)
            || state.phase != Phase::Finalizing
            || state.inserted
        {
            return Err("this dictation is no longer waiting for text.".to_string());
        }
        state.inserted = true;
    }

    // The clipboard transaction blocks off the UI thread. Share ownership of
    // the original String with its worker and the last successful dictation.
    let own_process = std::process::id();
    let text = Arc::new(text);
    let kept = Arc::clone(&text);
    // Cancellation and clipboard hand-off share one session lock. Escape wins
    // before this transition; once Pasting begins the recording is complete,
    // and an installer's exit refuses the hand-over exactly like a cancel.
    let gate = Arc::clone(&inner);
    let paste_app = app.clone();
    let pasted = tauri::async_runtime::spawn_blocking(move || {
        {
            let _session = gate.session.lock();
            if !paste_may_begin(&gate, generation) {
                return Err("this dictation was cancelled.".to_string());
            }
            gate.state.lock().phase = Phase::Pasting;
            // The dictation is going through, so the pill comes down in the same
            // step, before the clipboard transaction: nothing of the hand-over is
            // meant to be visible, and Escape is past winning.
            overlay::hide_now(&paste_app, generation);
        }
        release_cancel_key(&paste_app);
        paste::transact_foreground(text, own_process)
    })
    .await
    .map_err(|err| format!("the text could not be handed over ({err})"))??;
    inner.state.lock().text = Some(kept);
    Ok(pasted.as_str())
}

/* -------------------------------------------------------------------------- */
/* Startup                                                                     */
/* -------------------------------------------------------------------------- */

/// Builds the shell: the single-instance guard, then configuration, backend,
/// launcher, tray, hidden overlay and the watcher, and the exit path that gives
/// the backend's port back.
///
/// The loopback port is decided before the shell is built — the settings
/// document is read, and the socket bound, while no window can exist — so the
/// origin the three windows are served from and the socket the backend answers
/// on are one decision, with no gap another process could bind into. A failure
/// to preflight or reserve is not reported here: Tauri is built first so the
/// single-instance plugin can hand a second launch to the shell that is running,
/// and the failure becomes the native dialog in `setup` below, before any window
/// exists.
///
/// A startup problem that cannot be repaired is fatal and shows a native
/// dialog: a shell that runs without the backend its pages are served from is
/// worse than no shell at all. The launcher chord and the key container are not
/// among them — both are repaired in the settings window. The work itself lives
/// in `startup`, which the `setup` hook calls once the plugins are initialized.
///
/// The command list below is the same inventory the build script declares to
/// Tauri: a label capability only closes the commands the manifest names, so both
/// lists have to agree with each other and with `src-tauri/build.rs`.
pub fn run() {
    let reservation = settings::preflight_port()
        .map_err(|error| error.to_string())
        .and_then(backend::reserve);

    let mut context = tauri::generate_context!();
    // The flavor owns the identity the shell runs under: a debug build must not
    // share the production shell's single-instance guard, its WebView2 data
    // directory or its profile, and the tray has to say which build it is. Both
    // values come from the compile-time table (`crate::profile`) and are copied
    // in before the Builder exists, so the plugins and the runtime see this
    // build's own identity even when the executable was started by
    // double-clicking it.
    {
        let defaults = profile::defaults(profile::ACTIVE);
        let config = context.config_mut();
        config.identifier = defaults.identifier.to_owned();
        config.product_name = Some(defaults.title.to_owned());
    }
    if let Ok(reserved) = &reservation {
        // Built without `custom-protocol`, this shell resolves `WebviewUrl::App`
        // against `build.dev_url` and treats those URLs as local to the ACL, so
        // overriding it here is what makes the three windows load from the
        // backend that owns the port — the configured one, or the temporary one
        // it fell back to.
        let origin = format!("http://127.0.0.1:{}/", reserved.actual_port());
        context.config_mut().build.dev_url =
            Some(Url::parse(&origin).expect("a loopback origin is always a valid URL"));
    }

    let builder = tauri::Builder::default()
        // A second launch is a mistake: this shell already owns the launcher, the
        // backend port and the tray, so the newcomer only brings the comparison
        // window forward. The plugin decides during plugin setup, before this
        // app's own setup runs, so the second process never reaches the port.
        .plugin(single_instance(|app, _argv, _cwd| {
            if app.get_webview_window(LAB_LABEL).is_some() {
                open_lab(app);
            }
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            overlay_ready,
            dictation_context,
            capture_started,
            capture_finalizing,
            finish_session,
            insert_text,
            start_native_capture,
            stop_native_capture,
            mute_warning_shown,
            lab_prepare_capture,
            lab_start_capture,
            lab_stop_capture,
            preferences::settings_open,
            preferences::settings_update_general,
            preferences::settings_list_input_devices,
            preferences::settings_set_keys,
            preferences::settings_reveal_keys,
            preferences::settings_clear_keys,
            preferences::settings_check_keys,
            preferences::settings_apply_keys,
            preferences::settings_close,
            preferences::settings_action,
            preferences::settings_update_hotkey,
            preferences::settings_hotkey_capture,
            preferences::settings_set_autostart
        ])
        .setup(move |app| {
            // The plugins have run by now, a second launch is already gone, and
            // no window exists yet, so the deferred failure is reported here.
            let reservation = match reservation {
                Ok(reservation) => reservation,
                Err(message) => fatal(message),
            };
            startup(app.handle(), reservation);
            Ok(())
        });

    let app = match builder.build(context) {
        Ok(app) => app,
        Err(err) => fatal(format!("cannot start the Tauri runtime: {err}")),
    };

    app.run(|app, event| match event {
        // Closing either window hides it (the settings window asks its page
        // first), and the shell lives on in the tray: only the window's own
        // "Выход" stops it.
        RunEvent::ExitRequested { api, code, .. } if code.is_none() => api.prevent_exit(),
        RunEvent::Exit => {
            // An exit the installer asked for is bounded by the installer's own
            // deadline: the system sound goes back before anything can wait on
            // a capture worker, and the capture itself is released without
            // waiting for a driver - instead of the five-second finish the
            // ordinary teardown may take. The cleanup below then finds nothing
            // left to wait for; its own audio and capture calls are no-ops.
            let installer_quit = app
                .try_state::<Shell>()
                .is_some_and(|shell| shell.0.installer_quit.load(Ordering::SeqCst));
            if installer_quit {
                audio::shutdown();
                capture::shutdown_for_exit(app);
            }
            // The port and the backend's runtime thread go first; the windows and
            // the microphone are already gone by the time this event arrives.
            let running = app
                .try_state::<BackendState>()
                .and_then(|state| state.0.lock().take());
            if let Some(mut backend) = running {
                backend.shutdown();
            }
            cleanup(app);
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::{
        admits_capture, app_page_matches, clear_hotkey_field, lab_capture_is_current,
        lab_holds_microphone, lab_refusal, paste_may_begin, record_hotkey_field_report,
        release_lab_capture,
        request_lab_start, reserve_lab_capture, reset_hotkey_field_document, resolved_app_page,
        take_lab_capture, Inner, Phase, Shell, LAB_LABEL, LAB_PAGE_PATH,
    };
    use crate::secrets::KeyRing;
    use crate::settings::{RuntimeSnapshot, Settings, SharedRuntime};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tauri::Url;

    /// The settings window is told `dictation_active`, and its lab button is
    /// disabled, for exactly the phases a dictation is in — from the arming until
    /// the hand-over is over, and never for `Idle`. A phase added later has to be
    /// classified in `Phase::is_active` on purpose, where this test is what makes
    /// the decision visible instead of silently making the window look idle.
    #[test]
    fn dictation_is_active_in_every_phase_but_idle() {
        assert!(!Phase::Idle.is_active());
        for phase in [
            Phase::Arming,
            Phase::Recording,
            Phase::Finalizing,
            Phase::Pasting,
        ] {
            assert!(phase.is_active(), "{phase:?} runs a dictation");
        }
    }

    /// A microphone is opened only for the dictation that still holds its pinned
    /// revision and is still arming. A cancellation that retires the pin or
    /// clears the phase while the device is opening leaves nothing to admit, and
    /// a take that was replaced is refused with the phase left untouched.
    #[test]
    fn a_capture_is_admitted_only_while_its_pinned_dictation_is_arming() {
        let runtime = SharedRuntime::new(Arc::new(RuntimeSnapshot {
            settings: Settings {
                hotkey: "F2".to_owned(),
                mode: "live".to_owned(),
                mute_during_recording: false,
                port: crate::settings::DEFAULT_PORT,
                input_device: None,
            },
            keys: Arc::new(KeyRing::empty()),
            revision: 1,
        }));
        let inner = Shell::new(Arc::new(runtime), None).0;
        inner.generation.store(7, Ordering::SeqCst);
        inner.state.lock().phase = Phase::Arming;
        inner.runtime.pin(7, inner.runtime.snapshot());
        assert!(
            admits_capture(&inner, 7),
            "the arming take that holds its revision is admitted"
        );

        // Escape retires the pin and clears the phase before the device is up.
        inner.runtime.retire(7);
        inner.state.lock().phase = Phase::Idle;
        assert!(!admits_capture(&inner, 7), "a cancelled take is not admitted");

        // A take that was replaced while its device opened is refused as well.
        inner.state.lock().phase = Phase::Arming;
        inner.runtime.pin(7, inner.runtime.snapshot());
        inner.generation.store(8, Ordering::SeqCst);
        assert!(!admits_capture(&inner, 7), "a replaced take is not admitted");

        // And one whose generation is the cancelled one.
        inner.generation.store(7, Ordering::SeqCst);
        inner.cancelled.store(7, Ordering::SeqCst);
        assert!(!admits_capture(&inner, 7), "a cancelled generation stays refused");

        // The shell that is leaving opens nothing.
        inner.cancelled.store(0, Ordering::SeqCst);
        inner.shutdown.store(true, Ordering::SeqCst);
        assert!(!admits_capture(&inner, 7), "a shell that is leaving admits nothing");
    }

    /// A hand-over may begin only for the dictation that is still the one
    /// finalizing: a cancellation, a replacement or an installer's exit refuses
    /// the late answer before it can touch the clipboard.
    #[test]
    fn a_late_hand_over_cannot_outlive_a_cancellation_or_an_exit() {
        let inner = idle_inner();
        inner.generation.store(9, Ordering::SeqCst);
        inner.state.lock().phase = Phase::Finalizing;
        assert!(
            paste_may_begin(&inner, 9),
            "the dictation that is waiting for its text may pass"
        );

        // Escape cancels it: the generation is refused before any paste.
        inner.cancelled.store(9, Ordering::SeqCst);
        assert!(!paste_may_begin(&inner, 9), "a cancelled take does not paste");
        inner.cancelled.store(0, Ordering::SeqCst);

        // A take that was replaced never pastes into its successor's place.
        inner.generation.store(10, Ordering::SeqCst);
        assert!(!paste_may_begin(&inner, 9), "a replaced take does not paste");
        inner.generation.store(9, Ordering::SeqCst);

        // An installer's exit refuses it even while the phase still reads
        // `Finalizing`: the exit latches `shutdown` before it waits for the
        // session lock, so a response that lost that race never pastes.
        inner.shutdown.store(true, Ordering::SeqCst);
        assert!(
            !paste_may_begin(&inner, 9),
            "a shell that is leaving pastes nothing"
        );
    }

    /// A page gate resolves its URL from the dev URL the shell is really serving
    /// from, not from a fixed port: the page of a start that fell back to a
    /// temporary port is accepted, and the same page on any other port, under
    /// another host, with a query, a fragment or a lookalike path is refused.
    #[test]
    fn a_page_gate_follows_the_running_dev_url() {
        let dev_url = Url::parse("http://127.0.0.1:43117/").expect("a loopback dev URL");
        let served = Url::parse("http://127.0.0.1:43117/settings.html").expect("the served page");

        assert!(app_page_matches(
            "settings",
            "settings.html",
            "settings",
            Some(&dev_url),
            &served
        ));

        for foreign in [
            "http://127.0.0.1:4173/settings.html", // the old fixed port
            "http://127.0.0.1:43118/settings.html", // a neighbouring port
            "http://localhost:43117/settings.html", // another loopback host
            "http://127.0.0.1:43117/overlay.html",  // the other restricted page
            "http://127.0.0.1:43117/settings.html?section=general",
            "http://127.0.0.1:43117/settings.html#draft",
            "http://127.0.0.1:43117/settings.html.evil",
            "http://127.0.0.1:43117/evil/settings.html",
            "https://127.0.0.1:43117/settings.html",
        ] {
            assert!(
                !app_page_matches(
                    "settings",
                    "settings.html",
                    "settings",
                    Some(&dev_url),
                    &Url::parse(foreign).expect("a test URL")
                ),
                "{foreign} is not the page the settings window may be on"
            );
        }

        // The label is not a secret: the same URL in another window is refused.
        assert!(
            !app_page_matches("settings", "settings.html", LAB_LABEL, Some(&dev_url), &served),
            "the lab window is not the settings window"
        );
    }

    /// Without a dev URL the shell resolves `WebviewUrl::App` against the custom
    /// protocol base, and `index.html` stays the base itself — the two rules the
    /// gate mirrors to agree with Tauri about the opened document.
    #[test]
    fn a_page_without_a_dev_url_resolves_like_webview_url_app() {
        let expected = if cfg!(windows) || cfg!(target_os = "android") {
            "http://tauri.localhost/settings.html"
        } else {
            "tauri://localhost/settings.html"
        };
        let resolved = resolved_app_page(None, "settings.html");
        assert_eq!(resolved.as_str(), expected);

        let index = resolved_app_page(None, "index.html");
        assert_eq!(index.path(), "/", "index.html stays the base itself");
        assert!(
            !index.as_str().ends_with("index.html"),
            "Tauri does not join index.html onto the base: {index}"
        );

        // The laboratory names its page explicitly, so the join really happens
        // and the document Tauri opens is the one its gate accepts.
        let explicit = resolved_app_page(None, "/index.html");
        assert_eq!(explicit.path(), "/index.html");
        assert!(
            explicit.as_str().ends_with("/index.html"),
            "an explicit index page is joined onto the base: {explicit}"
        );
    }

    /// One shell with a valid configuration and no keys, for the state tests
    /// that never touch a window.
    fn idle_inner() -> Arc<super::Inner> {
        let runtime = SharedRuntime::new(Arc::new(RuntimeSnapshot {
            settings: Settings {
                hotkey: "F2".to_owned(),
                mode: "live".to_owned(),
                mute_during_recording: false,
                port: crate::settings::DEFAULT_PORT,
                input_device: None,
            },
            keys: Arc::new(KeyRing::empty()),
            revision: 1,
        }));
        Shell::new(Arc::new(runtime), None).0
    }

    /// The laboratory and a dictation never hold the microphone at the same
    /// time: a reservation refuses a press, a dictation in any phase refuses a
    /// reservation, and the take's start right is spent exactly once.
    #[test]
    fn the_laboratory_and_a_dictation_never_share_the_microphone() {
        let inner = idle_inner();

        // Idle: the laboratory may reserve one take, numbered in its own space,
        // and the press gate now sees the microphone taken.
        let generation = reserve_lab_capture(&inner).expect("an idle shell reserves a take");
        assert_eq!(generation, 1, "the laboratory numbers its own takes");
        assert_eq!(
            reserve_lab_capture(&inner).unwrap_err(),
            "Лаборатория уже готовит запись.",
            "one take is prepared at a time"
        );
        assert!(lab_holds_microphone(&inner));

        // The take is one-shot: only its own first start may open the device,
        // and it pins the configuration the shell holds now.
        let snapshot = request_lab_start(&inner, generation).expect("its own first start");
        assert_eq!(snapshot.settings.mode, "live");
        assert!(request_lab_start(&inner, generation).is_err(), "one start per take");
        assert!(
            request_lab_start(&inner, generation + 1).is_err(),
            "a generation the shell does not hold opens nothing"
        );
        assert!(lab_capture_is_current(&inner, generation));

        // A dictation in any phase but Idle refuses the laboratory, and a shell
        // that is leaving refuses it whatever the phase says.
        inner.state.lock().phase = Phase::Arming;
        assert!(reserve_lab_capture(&inner).is_err(), "a dictation is running");
        assert!(lab_refusal(&inner).is_some());
        inner.state.lock().phase = Phase::Idle;
        assert!(lab_refusal(&inner).is_none());
        inner.shutdown.store(true, Ordering::SeqCst);
        assert!(reserve_lab_capture(&inner).is_err(), "a shell that is leaving");
        assert!(lab_refusal(&inner).is_some());
        inner.shutdown.store(false, Ordering::SeqCst);

        // A window that is closed may reserve nothing: a prepare that was still
        // in flight when it went away must not leave a take nobody can stop.
        inner.lab_closed.store(true, Ordering::SeqCst);
        assert_eq!(
            reserve_lab_capture(&inner).unwrap_err(),
            "Окно лаборатории закрыто — откройте его заново."
        );
        inner.lab_closed.store(false, Ordering::SeqCst);

        // The stop releases the reservation before anything is stopped, and a
        // late stop of the same generation changes nothing.
        assert!(release_lab_capture(&inner, generation));
        assert!(!release_lab_capture(&inner, generation), "a late stop has nothing to take");
        assert!(!lab_holds_microphone(&inner), "the press may start again");
        assert!(!lab_capture_is_current(&inner, generation));
        assert!(take_lab_capture(&inner).is_none());
    }

    /// Closing the laboratory's window takes its reservation before anything is
    /// stopped: a late start can never install itself, and the next take gets a
    /// generation of its own.
    #[test]
    fn closing_the_lab_window_takes_the_reservation_first() {
        let inner = idle_inner();
        let generation = reserve_lab_capture(&inner).expect("an idle shell reserves a take");
        assert!(request_lab_start(&inner, generation).is_ok());

        assert_eq!(take_lab_capture(&inner), Some(generation));
        assert!(take_lab_capture(&inner).is_none(), "only one reservation ever exists");
        assert!(
            !lab_capture_is_current(&inner, generation),
            "the closed generation is not the shell's take any more"
        );
        // A start that answers after the close opens nothing: its generation is
        // not the reserved one, and the closed number is never handed out again.
        assert!(request_lab_start(&inner, generation).is_err());

        let next = reserve_lab_capture(&inner).expect("the next take is prepared right away");
        assert_ne!(next, generation, "a new take gets a new generation");
        assert!(lab_capture_is_current(&inner, next));
        assert!(!lab_capture_is_current(&inner, generation));
    }

    /// The laboratory's gate accepts exactly the document Tauri opens for it:
    /// the explicit index page of the origin this run serves, and nothing else.
    #[test]
    fn the_lab_gate_accepts_only_the_explicit_index_page() {
        let dev_url = Url::parse("http://127.0.0.1:43117/").expect("a loopback dev URL");
        let served = Url::parse("http://127.0.0.1:43117/index.html").expect("the served page");

        assert!(app_page_matches(
            LAB_LABEL,
            LAB_PAGE_PATH,
            LAB_LABEL,
            Some(&dev_url),
            &served
        ));

        for foreign in [
            // The base Tauri loads for the literal "index.html": the page the
            // laboratory must not be recognised on.
            "http://127.0.0.1:43117/",
            "http://127.0.0.1:43117/index.html?take=1",
            "http://127.0.0.1:43117/index.html#take",
            "http://127.0.0.1:43117/overlay.html",
            "http://127.0.0.1:43117/settings.html",
            "http://127.0.0.1:43117/sub/index.html",
            "http://127.0.0.1:4173/index.html",
            "http://localhost:43117/index.html",
            "https://127.0.0.1:43117/index.html",
        ] {
            assert!(
                !app_page_matches(
                    LAB_LABEL,
                    LAB_PAGE_PATH,
                    LAB_LABEL,
                    Some(&dev_url),
                    &Url::parse(foreign).expect("a test URL")
                ),
                "{foreign} is not the page the laboratory may drive the microphone from"
            );
        }

        // The label is not a secret, and the page names the gate: the same
        // document in another window is refused, whatever that window is.
        assert!(
            !app_page_matches(LAB_LABEL, LAB_PAGE_PATH, "settings", Some(&dev_url), &served),
            "the settings window is not the laboratory"
        );
        assert!(
            !app_page_matches(LAB_LABEL, LAB_PAGE_PATH, "overlay", Some(&dev_url), &served),
            "the overlay window is not the laboratory"
        );
    }

    /// The settings window's chord-field reports arrive over fire-and-forget
    /// IPC, so two of them can land in either order and a page that was already
    /// replaced can still have one in flight. Only a newer report of the
    /// document that is loaded may move the field, and a late report can never
    /// re-open a gate that was released: the launcher must not be silenced by
    /// its own field's stale `true`.
    #[test]
    fn chord_field_reports_cannot_undo_a_newer_state() {
        let inner = settings_shell();
        let focused = |inner: &Arc<Inner>| inner.hotkey_field_focused.load(Ordering::SeqCst);

        // The handshake only reads the document identity: it must not claim
        // the keyboard by itself.
        assert_eq!(record_hotkey_field_report(&inner, 0, 0, true, true), 0);
        assert!(!focused(&inner));

        // A focus and the blur after it, applied in the order they arrive.
        assert_eq!(record_hotkey_field_report(&inner, 0, 1, true, true), 0);
        assert!(focused(&inner));
        assert_eq!(record_hotkey_field_report(&inner, 0, 2, false, true), 0);
        assert!(!focused(&inner));
        // The focus that was issued before that blur arrives after it: it is
        // dropped, so the released gate stays released.
        record_hotkey_field_report(&inner, 0, 1, true, true);
        assert!(!focused(&inner), "the older report must not come back");

        // Only a newer report of the same document may hold the field again.
        record_hotkey_field_report(&inner, 0, 3, true, true);
        assert!(focused(&inner));

        // A page that is loading replaces the document: nothing the previous
        // document still has in flight reaches the new one, and the new page
        // reads its identity with the handshake.
        reset_hotkey_field_document(&inner);
        assert!(!focused(&inner));
        assert_eq!(record_hotkey_field_report(&inner, 0, 99, true, true), 1);
        assert!(
            !focused(&inner),
            "the replaced document is not the one reporting"
        );
        assert_eq!(record_hotkey_field_report(&inner, 1, 0, false, true), 1);
        assert_eq!(record_hotkey_field_report(&inner, 1, 1, true, true), 1);
        assert!(focused(&inner));

        // A window that is not on screen may only release: nothing there holds
        // a keyboard, and a stale `true` must not silence the launcher.
        record_hotkey_field_report(&inner, 1, 2, true, false);
        assert!(!focused(&inner));

        // Clearing for a hidden or unfocused window keeps the sequence: a
        // report the page sent before it was released cannot re-open it.
        record_hotkey_field_report(&inner, 1, 3, true, true);
        clear_hotkey_field(&inner);
        assert!(!focused(&inner));
        record_hotkey_field_report(&inner, 1, 2, true, true);
        assert!(!focused(&inner), "a straggler must not re-open the gate");

        // Every delivery order of one document's reports ends at the state of
        // the newest of them, whichever order the IPC happened to use.
        let reports: Vec<(u64, bool, bool)> = vec![
            (1, true, true),
            (2, false, true),
            (3, true, true),
            (4, true, false),
        ];
        let newest = *reports
            .iter()
            .max_by_key(|report| report.0)
            .expect("reports");
        let mut indices: Vec<usize> = (0..reports.len()).collect();
        let mut orders = Vec::new();
        permutations(&mut indices, 0, &mut orders);
        assert_eq!(orders.len(), 24, "every order of four reports");
        for order in &orders {
            let inner = settings_shell();
            record_hotkey_field_report(&inner, 0, 0, true, true);
            for index in order {
                let (sequence, active, visible) = reports[*index];
                assert_eq!(
                    record_hotkey_field_report(&inner, 0, sequence, active, visible),
                    0
                );
            }
            assert_eq!(focused(&inner), newest.1 && newest.2, "order {order:?}");
        }
    }

    /// A shell whose chord-field report stream is still empty.
    fn settings_shell() -> Arc<Inner> {
        let runtime = SharedRuntime::new(Arc::new(RuntimeSnapshot {
            settings: Settings {
                hotkey: "F2".to_owned(),
                mode: "live".to_owned(),
                mute_during_recording: false,
                port: crate::settings::DEFAULT_PORT,
                input_device: None,
            },
            keys: Arc::new(KeyRing::empty()),
            revision: 1,
        }));
        Shell::new(Arc::new(runtime), None).0
    }

    /// Every ordering of `items[start..]`, for the delivery-order property.
    fn permutations(items: &mut Vec<usize>, start: usize, out: &mut Vec<Vec<usize>>) {
        if start == items.len() {
            out.push(items.clone());
            return;
        }
        for index in start..items.len() {
            items.swap(start, index);
            permutations(items, start + 1, out);
            items.swap(start, index);
        }
    }
}
