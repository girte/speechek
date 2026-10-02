/*
 * Speechek's settings window.
 *
 * Two sections — the ordinary settings and the API keys — over the native
 * commands of `preferences.rs`. This page is a view of a native draft: every
 * keystroke in the key field travels to the shell as a revision-checked
 * compare-and-set, and nothing here keeps a secret after the field is hidden.
 *
 * Invariants worth knowing before editing:
 *
 *  - Listeners are registered before the first `settings_open`, because the
 *    shell may hand over a section request the moment the page is ready.
 *  - Views are accepted only for the draft they name, and commands carry the
 *    revision of the last accepted view, so a late answer can never overwrite
 *    a newer draft.
 *  - The ordinary settings apply themselves: a mode change is sent at once,
 *    and it never carries the launcher chord, so an old view cannot
 *    put a stale hotkey back. The key list is different — it stays a draft
 *    until «Применить ключи», and only that action writes the key store.
 *  - The launcher chord is typed, never captured: the field only holds text,
 *    Enter and leaving it send the completed string through the same serial
 *    queue, and no reply may repaint text typed after the snapshot it answers.
 *    A press of the running chord while the field is focused is written into
 *    the field by the shell, which Windows hands the key to instead of the
 *    page; the field's own state is reported back tagged with this document's
 *    identity and a sequence, so reordered or leftover signals cannot move it.
 *  - A revealed list lives in one textarea and nowhere else. `revealEpoch`
 *    grows synchronously before anything hides it, and a reveal answer that
 *    lost its epoch is dropped instead of drawn.
 *  - Text typed into the editor survives an IPC failure: the field stays
 *    visible with its text and an error, never silently cleared.
 *  - A close request is merged, not queued: a quit that arrives behind a close
 *    upgrades the one flow, and the flow re-reads the reason after each await.
 *  - Text from the shell and from the settings file is untrusted: it reaches
 *    the document through textContent or textarea.value only.
 */

/** The shell asks for a section; it is sent after the window is shown. */
const EVENT_OPEN_REQUEST = 'speechek:settings-open-request';
/** The window was asked to close (its own close button, or the app quitting). */
const EVENT_CLOSE_REQUEST = 'speechek:settings-close-request';
/** A dictation started or ended, so the hotkey must not be changed now. */
const EVENT_DICTATION_STATE = 'speechek:dictation-state';
/**
 * The launcher chord was pressed while the chord field holds the keyboard.
 * Windows hands such a press to the shell rather than to the page, so the
 * field cannot see the keys: the payload names the chord the shell is running,
 * and writing it here is the same gesture as typing it.
 */
const EVENT_HOTKEY_KEY = 'speechek:settings-hotkey-key';


const CMD = {
  open: 'settings_open',
  updateGeneral: 'settings_update_general',
  listInputDevices: 'settings_list_input_devices',
  updateHotkey: 'settings_update_hotkey',
  hotkeyCapture: 'settings_hotkey_capture',
  setKeys: 'settings_set_keys',
  reveal: 'settings_reveal_keys',
  clear: 'settings_clear_keys',
  check: 'settings_check_keys',
  applyKeys: 'settings_apply_keys',
  close: 'settings_close',
  action: 'settings_action',
  setAutostart: 'settings_set_autostart',
};

const SECTIONS = ['general', 'keys'];
const MODES = ['live', 'smart', 'verbatim'];
/** How long typing may rest before the draft is told about it. */
const TYPING_IDLE_MS = 350;
/**
 * Why the chord field is locked while a dictation runs; native
 * `DICTATION_ACTIVE` stays the backstop against the race.
 */
const DICTATION_HINT = 'Диктовка выполняется: горячую клавишу сейчас изменить нельзя.';
/** Why the port field was refused; the shell uses the same line for a zero port. */
const PORT_INVALID_MESSAGE = 'Укажите целое число от 1 до 65535.';
/**
 * Why the autostart switch is mixed and disabled; the shell could not read the
 * registration it would have to change.
 */
const AUTOSTART_UNKNOWN = 'Состояние автозагрузки определить не удалось: запись Windows недоступна. Изменить её сейчас нельзя.';

/** Fixed Russian texts for codes the shell may answer with. */
const FALLBACK_MESSAGES = {
  FORBIDDEN: 'Это окно не имеет доступа к настройкам.',
  STALE_DRAFT: 'Черновик изменился: данные обновлены, повторите действие.',
  UNSAVED_CHANGES: 'В черновике есть несохранённые изменения.',
  CONFIRM_REQUIRED: 'Действие требует подтверждения.',
  DICTATION_ACTIVE: 'Чтобы изменить горячую клавишу, завершите или отмените диктовку.',
  KEY_LIST_INVALID: 'В списке ключей есть ошибка.',
  SECRETS_UNAVAILABLE: 'Хранилище ключей недоступно: сохранённый список прочитать не удалось.',
  REVEAL_FAILED: 'Не удалось прочитать ключи для показа.',
  INVALID_MODE: 'Неизвестный режим распознавания.',
  HOTKEY_INVALID: 'Это сочетание не подходит для горячей клавиши.',
  HOTKEY_CONFLICT: 'Это сочетание занято другой программой. Выберите другое или закройте её.',
  CLOSING: 'Приложение завершает работу: настройки сейчас изменить нельзя.',
  INPUT_DEVICES_UNAVAILABLE: 'Не удалось получить список микрофонов. Проверьте доступ к устройствам записи.',
  INVALID_ACTION: 'Неизвестное действие.',
  SAVE_FAILED: 'Не удалось сохранить настройки: проверьте доступ к файлам.',
  SAVE_PARTIAL: 'Файлы могли измениться частично; приложение продолжает использовать прежние настройки. Устраните ошибку доступа и повторите.',
  CHECK_IN_PROGRESS: 'Проверка ключей уже выполняется.',
  INTERNAL: 'Внутренняя ошибка приложения.',
  AUTOSTART_UNAVAILABLE: 'Автозагрузку сейчас изменить нельзя: запись Windows занята другой программой или недоступна.',
  AUTOSTART_FAILED: 'Не удалось изменить автозагрузку: проверьте доступ к реестру и повторите.',
};

const tauri = globalThis.__TAURI__;
let shell = Boolean(tauri?.core?.invoke && tauri?.event?.listen);

const el = (id) => document.getElementById(id);

const notice = el('preview-notice');
const tabs = { general: el('tab-general'), keys: el('tab-keys') };
const panels = { general: el('panel-general'), keys: el('panel-keys') };
const modePicker = el('mode-picker');
const muteToggle = el('mute-during-recording');
const autostartToggle = el('autostart-enabled');
const autostartNotice = el('autostart-notice');
const autostartError = el('autostart-error');
const generalError = el('general-error');
const hotkeyInput = el('hotkey-input');
const hotkeyState = el('hotkey-state');
const hotkeyError = el('hotkey-error');
const portInput = el('port-input');
const portNotice = el('port-notice');
const portError = el('port-error');
const deviceSelect = el('input-device');
const deviceNotice = el('device-notice');
const deviceError = el('device-error');
const keysSummary = el('keys-summary');
const keysError = el('keys-error');
const keyReveal = el('keys-reveal');
const keyApply = el('keys-apply');
const keyClear = el('keys-clear');
const keyCheck = el('keys-check');
const keysEditor = el('keys-editor');
const keysEditorLabel = el('keys-editor-label');
const keysEditorHelp = el('keys-editor-help');
const keysText = el('keys-text');
const keysCheckSummary = el('keys-check-summary');
const keysResults = el('keys-check-results');
const statusLine = el('status');
const partialWarning = el('partial-warning');
const actionLab = el('action-lab');
const dialogClose = el('dialog-close');
const dialogCloseTitle = el('dialog-close-title');
const dialogCloseText = el('dialog-close-text');
const dialogCloseSave = el('dialog-close-save');
const dialogCloseDiscard = el('dialog-close-discard');
const dialogCloseReturn = el('dialog-close-return');
const dialogConfirm = el('dialog-confirm');
const dialogConfirmText = el('dialog-confirm-text');
const dialogConfirmOk = el('dialog-confirm-ok');
const dialogConfirmCancel = el('dialog-confirm-cancel');

/* -------------------------------------------------------------------------- */
/* State                                                                       */
/* -------------------------------------------------------------------------- */

/** Newest accepted SettingsView, or null before the first open / after close. */
let view = null;
/** Rendered section. Only this page moves it; the shell only requests one. */
let section = 'general';
/** Section named by the newest shell request, applied after the next open. */
let pendingSection = null;
/** Describes the editor currently on screen: hidden | revealed | replace. */
let editorMode = 'hidden';
/**
 * Edit generation of the field, and the generation the shell has accepted.
 * Acceptance is bound to the generation, never to a copy of the text: a reply
 * that arrives after the user typed again cannot mark the newer text as sent,
 * and nothing but the field itself ever holds the list.
 */
let editEpoch = 0;
let acceptedEpoch = -1;
/** True once a save in this session ended partially; it needs acknowledging. */
let partialObligation = false;
/** The last key-list failure echoed into the status line, if any. */
let keyErrorEcho = '';
/** True once the user typed into a replacement list, so a blank field is theirs. */
let replaceTouched = false;
/** Grows whenever a revealed list must stop being current. */
let revealEpoch = 0;
/** True while a reveal answer may still be drawn. */
let revealIntent = false;
/** The chord the shell last reported as applied; the fallback an error names. */
let appliedHotkey = '';
/** Grows with every submitted chord: a reply of an older intent reports nothing. */
let hotkeyIntent = 0;
/** True while the field holds text the shell has not taken; a view never erases it. */
let hotkeyDirty = false;
/** The last snapshot handed to the queue: { value, intent, fieldEpoch, pending, ok }. */
let hotkeySubmit = null;
/** Grows on every keystroke in the chord field, so a late answer cannot claim it. */
let hotkeyFieldEpoch = 0;
/** The wording of a refused chord, kept under the field until the user edits. */
let hotkeyFailure = '';
/** The text last handed to the queue; a blur of it is the same gesture. */
let hotkeyChangeBaseline = null;
/** The shell reports an ongoing dictation; the hotkey must not change now. */
let dictationActive = false;
/**
 * The identity of the settings document this page is, as the shell answered
 * it. Every field report is tagged with it, so a report that is still in
 * flight when the page reloads cannot move the field state of the document
 * that replaced it. `null` until the handshake has answered.
 */
let hotkeySignalDoc = null;
/** Grows with every report sent; the shell applies only a newer one. */
let hotkeySignalSeq = 0;
/** The handshake in flight, so a second focus does not start another one. */
let hotkeyHandshake = null;
/** A command that disables the whole form is in flight. */
let busy = false;
/** A close request is being resolved; further ones only merge into it. */
let closePending = false;
/** The close request this page still has to answer: null, 'close' or 'quit'. */
let pendingCloseReason = null;
/** The close reason whose partial-persistence warning was already agreed to. */
let partialConsentedFor = null;
/** Grows with every general interaction: an older answer must not repaint. */
let generalIntent = 0;
/** General sends that have not settled yet; their views must not repaint. */
let generalInFlight = 0;
/**
 * An autostart change that has not settled yet: its own optimistic value stays
 * on the switch until its answer lands, and a refusal rolls it back.
 */
let autostartInFlight = 0;
/** The wording of a refused autostart change, kept under the switch until the
 * next attempt; a repainted view never clears it on its own. */
let autostartFailure = '';
/** Grows with every activation re-read of the registration: only the newest
 * answer may update the value the page took from a focus. */
let autostartRefreshEpoch = 0;
/** The port field holds text the shell has not taken; a view never erases it. */
let portDirty = false;
/**
 * A port the user completed and the shell has not confirmed yet. It travels
 * with every later general change until an answer reports it, so toggling the
 * mute switch while this send is in flight cannot put the old port back.
 */
let pendingPort = null;
/**
 * The device picker holds a choice the shell has not confirmed yet. Like the
 * port, it travels with every later general change until an answer reports it;
 * `null` is the system default, and the flag says the value is the user's
 * rather than the draft's.
 */
let deviceDirty = false;
let pendingDevice = null;
/** The newest enumeration answer: null until one lands successfully. */
let deviceList = null;
/** Grows with every request, so an answer for an abandoned draft is dropped. */
let deviceEpoch = 0;
/** True while one enumeration is on its way; focus cannot stack more. */
let deviceLoading = false;
/** The wording of a refused device change, kept under the picker until the
 * user edits it; the enumeration has its own line so a repainted view may
 * clear one without the other. */
let deviceFailure = '';
/** Why the host's device list could not be read; cleared by a later success. */
let deviceListFailure = '';
/** Counts dictation events, so an older view cannot undo the newest one. */
let dictationEvents = 0;
/** The page dropped its draft after a successful close. */
let closed = true;

let typingTimer = null;
/** Every state-changing command is serialised through this chain. */
let queue = Promise.resolve();

const unlisteners = [];

/* -------------------------------------------------------------------------- */
/* Small helpers                                                               */
/* -------------------------------------------------------------------------- */

function payloadOf(event) {
  return event && typeof event === 'object' && 'payload' in event ? event.payload : event;
}

function errorOf(cause) {
  if (cause && typeof cause === 'object' && typeof cause.code === 'string') return cause;
  if (typeof cause === 'string') {
    try {
      const parsed = JSON.parse(cause);
      if (parsed && typeof parsed === 'object' && typeof parsed.code === 'string') return parsed;
    } catch {
      // The shell answered with a plain string; its text is the only detail.
      return { code: 'INTERNAL', message: cause };
    }
  }
  return { code: 'INTERNAL', message: '' };
}

function messageOf(error) {
  const own = typeof error.message === 'string' ? error.message.trim() : '';
  return own || FALLBACK_MESSAGES[error.code] || FALLBACK_MESSAGES.INTERNAL;
}

function setStatus(text) {
  statusLine.textContent = text;
}

function show(node) {
  node.hidden = false;
}

function hide(node) {
  node.hidden = true;
}

function clearChildren(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function setError(node, text) {
  node.textContent = text;
  node.hidden = text === '';
}

/** Russian count agreement, used for the key counter only. */
function keyCountText(count) {
  const mod100 = count % 100;
  const mod10 = count % 10;
  if (mod100 >= 11 && mod100 <= 14) return `${count} ключей`;
  if (mod10 === 1) return `${count} ключ`;
  if (mod10 >= 2 && mod10 <= 4) return `${count} ключа`;
  return `${count} ключей`;
}

function invoke(command, args) {
  return tauri.core.invoke(command, args);
}

/**
 * Tells the shell whether the chord field owns the keyboard. Fire and forget:
 * the flag only gates a new take, so a lost signal is corrected by the next
 * focus, blur or pagehide, and nothing on this page waits for an answer.
 *
 * Each report carries the identity of this document and a number of its own.
 * The shell applies only a report of the document it has loaded and only when
 * that number is newer than the last one it applied, so two `invoke`s that
 * arrive in the wrong order cannot write a focus state that is no longer true,
 * and a report left over from a page that was already replaced is dropped.
 */
function signalHotkeyField(active) {
  if (!shell) return;
  if (hotkeySignalDoc === null) {
    // The identity is not known yet (the handshake is still in flight, or it
    // failed): ask for it again, so a focus that happens meanwhile is reported
    // as soon as the answer arrives instead of being lost.
    void handshakeHotkeyField();
    return;
  }
  hotkeySignalSeq += 1;
  invoke(CMD.hotkeyCapture, {
    document: hotkeySignalDoc,
    sequence: hotkeySignalSeq,
    active: active === true,
  }).catch(() => {});
}

/**
 * Reads the identity of this settings document from the shell and reports the
 * field state that holds at that moment. A handshake, not a report: it changes
 * nothing natively, so a page that was replaced before its handshake arrived
 * cannot move the field of the document that replaced it.
 */
function handshakeHotkeyField() {
  if (!shell || hotkeyHandshake !== null) return hotkeyHandshake;
  hotkeyHandshake = invoke(CMD.hotkeyCapture, { document: 0, sequence: 0, active: false })
    .then((doc) => {
      if (typeof doc !== 'number') return;
      hotkeySignalDoc = doc;
      // What holds right now is what the shell has to have: a focus that
      // happened while the handshake was in flight is not lost.
      signalHotkeyField(hotkeyFieldHoldsKeyboard());
    })
    .catch(() => {
      // The shell keeps the field released until a report gets through; the
      // next focus, blur or pagehide starts this handshake over.
    })
    .finally(() => {
      hotkeyHandshake = null;
    });
  return hotkeyHandshake;
}

/** Whether the chord field is where this window's keystrokes go. */
function hotkeyFieldHoldsKeyboard() {
  return document.activeElement === hotkeyInput && !hotkeyInput.disabled;
}

/** The single writer of `queue`; nothing else may touch it. */
function enqueue(task) {
  const started = queue.then(task, task);
  queue = started.then(
    () => undefined,
    () => undefined,
  );
  return started;
}

/** Waits until every enqueued command has settled. */
async function drainQueue() {
  let seen = null;
  while (seen !== queue) {
    seen = queue;
    await seen;
  }
}

/* -------------------------------------------------------------------------- */
/* Rendering                                                                   */
/* -------------------------------------------------------------------------- */

function selectedMode() {
  const checked = modePicker.querySelector('input[name="transcription-mode"]:checked');
  return checked ? checked.value : 'live';
}

/** The port the general form should carry: a completed value waiting for its
 * answer, otherwise the one the shell reports. */
function desiredPort() {
  if (pendingPort !== null) return pendingPort;
  return Number.isInteger(view?.settings?.port) ? view.settings.port : 4173;
}

/** The chosen device the general form carries: the user's pick while the shell
 * has not confirmed it, otherwise the stored id, or `null` for the system
 * default. It never reads the select's display, so a list that failed to load
 * cannot turn a saved device into the system default behind the user's back. */
function currentInputDevice() {
  if (deviceDirty) return pendingDevice;
  const device = view?.settings?.input_device;
  return typeof device === 'string' && device !== '' ? device : null;
}

function renderSection() {
  for (const name of SECTIONS) {
    const on = name === section;
    tabs[name].setAttribute('aria-selected', on ? 'true' : 'false');
    tabs[name].tabIndex = on ? 0 : -1;
    panels[name].hidden = !on;
  }
}

function renderControls() {
  // A close flow in flight freezes the page too: its own settings_close answer
  // must not be raced by a new chord or general intent, and a late answer must
  // not revive the draft the close is dropping.
  const ready = view !== null && !busy && !closed && !closePending;
  // An explicit Apply keys or a close holds the page; a dictation forbids a
  // chord change. Neither one may touch the text already typed in the field.
  const usable = ready;
  keyReveal.disabled = !usable;
  keyApply.disabled = !usable;
  keyClear.disabled = !usable;
  keyCheck.disabled = !usable;
  muteToggle.disabled = !usable;
  // An unreadable registration leaves the switch mixed and disabled: a control
  // that cannot confirm its own state must not look actionable.
  autostartToggle.disabled = !usable || autostartState() === null;
  // The lab is a second window over a running take: it stays closed until the
  // dictation ends, while everything that does not touch the chord stays live.
  actionLab.disabled = !ready || dictationActive;
  modePicker.querySelectorAll('input').forEach((input) => {
    input.disabled = !usable;
  });
  // A field that is being disabled cannot hold the keyboard any more: the
  // shell has to hear about it even when no blur follows the disabling.
  const hotkeyWasHeld = hotkeyFieldHoldsKeyboard();
  hotkeyInput.disabled = !usable || dictationActive;
  if (hotkeyWasHeld && hotkeyInput.disabled) signalHotkeyField(false);
  portInput.disabled = !usable;
  deviceSelect.disabled = !usable;
}

function renderHotkey() {
  const hotkey = view?.settings && typeof view.settings.hotkey === 'string' ? view.settings.hotkey : '';
  // The applied chord is remembered even while the field is mid-edit: an error
  // line about it must name a chord the shell really has.
  appliedHotkey = hotkey;
  // The field belongs to the user the moment it holds text the shell has not
  // taken: a view that paints a general or key answer must not erase it.
  if (!hotkeyDirty && hotkeyInput.value !== hotkey) hotkeyInput.value = hotkey;
  const native = typeof view?.hotkeyError === 'string' ? view.hotkeyError : '';
  setError(hotkeyError, native || hotkeyFailure);
}

function renderGeneral() {
  const mode = view?.settings && MODES.includes(view.settings.mode) ? view.settings.mode : null;
  if (mode !== null) {
    const input = modePicker.querySelector(`input[name="transcription-mode"][value="${mode}"]`);
    if (input) input.checked = true;
  }
  // The mute switch is painted from the same snapshot the mode is: a view that
  // may repaint the general controls is the truth about both of them, and a
  // send in flight keeps this painting away until its own answer lands.
  muteToggle.checked = view?.settings?.mute_during_recording === true;
  // The autostart switch is painted from the same view as the other general
  // controls: a send in flight keeps this painting away until its answer lands.
  renderAutostart();
  // The port field belongs to the user while it holds text the shell has not
  // taken: a view that paints a general, chord or key answer must not erase
  // what was typed but not yet committed. A field that already holds the
  // reported value - after a commit, or before any edit - becomes clean again,
  // so a following view may paint it.
  const reported = Number.isInteger(view?.settings?.port) ? String(view.settings.port) : null;
  if (reported !== null) {
    if (pendingPort !== null && String(pendingPort) === reported) pendingPort = null;
    const text = portInput.value.trim();
    if (text === reported) portDirty = false;
    if (!portDirty && text !== reported) portInput.value = reported;
  }
  if (!portDirty) setError(portError, '');
  renderPortNotice();
  renderDevices();
  // A painted native view is the truth about the controls: a complaint about an
  // earlier attempt has been answered by it.
  setError(generalError, '');
}

/**
 * The port's own line under the field. A fallback this run started with
 * outranks everything - even a saved field the user has already changed, which
 * is exactly when the temporary port is easiest to forget. Then a saved port
 * that differs from the one being served, which only a restart applies. The
 * line is not the status line, so a partial-save warning never overwrites it
 * and it never overwrites one.
 */
function renderPortNotice() {
  if (view === null) {
    setPortNotice('');
    return;
  }
  if (view.portFallback === true) {
    setPortNotice(
      `При запуске порт ${view.startupPort} был занят, временно используется ${view.runningPort}. Он освободится при следующем запуске.`,
    );
    return;
  }
  const saved = Number.isInteger(view.settings?.port) ? view.settings.port : null;
  if (saved !== null && Number.isInteger(view.runningPort) && saved !== view.runningPort) {
    setPortNotice(`Порт ${saved} применится после перезапуска приложения.`);
    return;
  }
  setPortNotice('');
}

function setPortNotice(text) {
  portNotice.textContent = text;
}

/* -------------------------------------------------------------------------- */
/* Device picker                                                              */
/* -------------------------------------------------------------------------- */

/** Names the device failure and the enumeration failure, the user's own edit
 * first: the refusal of a change outranks a stale list, and a repainted view
 * clears only the first. */
function paintDeviceError() {
  setError(deviceError, deviceFailure || deviceListFailure);
}

/**
 * The picker's options and its own notice. The list is rebuilt from the newest
 * enumeration plus whatever the general form currently carries, so neither a
 * saved device the host no longer has nor a choice the shell has not confirmed
 * yet can vanish from the control while the document still names it. The
 * stored value itself is never rewritten here.
 */
function renderDevices() {
  if (view === null) return;
  const saved =
    typeof view.settings?.input_device === 'string' && view.settings.input_device !== ''
      ? view.settings.input_device
      : null;
  const listed = Array.isArray(deviceList) ? deviceList : [];
  // The shell's answer to a pending choice: the flag goes away the moment the
  // draft names the same device (or the same system default), and a view may
  // then repaint the picker freely.
  if (deviceDirty && (pendingDevice ?? '') === (saved ?? '')) {
    deviceDirty = false;
    pendingDevice = null;
  }
  const chosen = deviceDirty ? pendingDevice : saved;
  const options = new Map();
  options.set('', 'Системное устройство по умолчанию');
  for (const device of listed) {
    if (!device || typeof device.id !== 'string' || device.id === '') continue;
    const name = typeof device.name === 'string' && device.name !== '' ? device.name : device.id;
    options.set(device.id, device.isDefault === true ? `${name} (по умолчанию)` : name);
  }
  // A device the list does not know - the host's list is unreadable, or the
  // device is gone - stays on screen as its own entry, so the picker never
  // shows the system default for a document that names something else.
  for (const value of [saved, chosen]) {
    if (typeof value !== 'string' || value === '' || options.has(value)) continue;
    options.set(
      value,
      deviceList === null
        ? 'Текущее выбранное устройство (список недоступен)'
        : 'Выбранное устройство недоступно',
    );
  }
  clearChildren(deviceSelect);
  for (const [value, label] of options) {
    const option = document.createElement('option');
    option.value = value;
    option.textContent = label;
    deviceSelect.appendChild(option);
  }
  deviceSelect.value = typeof chosen === 'string' ? chosen : '';
  // A painted view is the shell's own answer to the last device change: the
  // refusal of an earlier attempt stops being current.
  if (!deviceDirty) {
    deviceFailure = '';
    paintDeviceError();
  }
  renderDeviceNotice(saved, listed);
}

/** The picker's own line: a saved device the host does not have is named, so
 * the silent fall back to the system microphone is not a surprise. The
 * enumeration failure has its own line under the select and is not repeated
 * here. */
function renderDeviceNotice(saved, listed) {
  const missing = saved !== null && Array.isArray(deviceList) && !listed.some((device) => device && device.id === saved);
  if (!missing) {
    deviceNotice.textContent = '';
    return;
  }
  deviceNotice.textContent =
    'Сохранённое устройство не найдено среди подключённых: запись пойдёт с системного микрофона. Сохранённый выбор не заменяется — выберите устройство заново, когда оно появится.';
}

/**
 * Re-reads the host's input devices. The request is not part of the command
 * queue: it changes nothing, and queuing it behind an apply would only delay
 * the answer. One read at a time, so a focus event that arrives while one is on
 * its way does not stack a second. A failed read keeps the options on screen
 * and the saved choice untouched, and names the failure under the field.
 */
async function refreshDevices() {
  if (view === null || closed || !shell || deviceLoading) return;
  deviceLoading = true;
  const epoch = ++deviceEpoch;
  try {
    const listed = await invoke(CMD.listInputDevices, {});
    if (epoch === deviceEpoch) {
      deviceList = Array.isArray(listed) ? listed : [];
      deviceListFailure = '';
    }
  } catch (cause) {
    if (epoch === deviceEpoch) deviceListFailure = messageOf(errorOf(cause));
  } finally {
    deviceLoading = false;
    if (epoch === deviceEpoch) {
      paintDeviceError();
      renderDevices();
    }
  }
}

/**
 * Wording of the one control that opens the key field. With the key store
 * unavailable there is nothing to reveal, so the same control opens an empty
 * editor instead of promising a list the shell cannot read.
 */
function revealLabel() {
  if (editorMode === 'revealed') return 'Скрыть ключи';
  if (editorMode === 'replace') return 'Ввести ключи';
  if (view !== null && view.secretsStatus === 'unavailable') return 'Ввести ключи';
  return 'Показать ключи';
}

function keysSummaryText() {
  const count = typeof view?.keyCount === 'number' ? view.keyCount : 0;
  if (view?.keyError) {
    return `Ранее сохранённых ключей: ${keyCountText(count)}. Исправьте список и примените ключи.`;
  }
  if (view.secretsStatus === 'unavailable') {
    if (view.keysChanged) {
      return `Хранилище ключей недоступно: сохранённый список прочитать не удалось. В черновике ${keyCountText(count)} — он заменит неизвестное содержимое только после подтверждённого применения.`;
    }
    return 'Хранилище ключей недоступно: сохранённый список прочитать не удалось. Показать или проверить сохранённые ключи нельзя.';
  }
  if (view.keysChanged) {
    return `В новом списке ${keyCountText(count)} — изменения не применены.`;
  }
  if (view.secretsStatus === 'missing') {
    return 'Сохранённых ключей нет: диктовка недоступна, пока вы не добавите ключ и не примените список.';
  }
  return `Сохранено ключей: ${keyCountText(count)}. Значения скрыты.`;
}

function renderKeys() {
  if (view === null) return;
  keysSummary.textContent = keysSummaryText();
  if (view.keyError) {
    const line = typeof view.keyError.line === 'number' ? `Строка ${view.keyError.line}: ` : '';
    setError(keysError, `${line}${view.keyError.message}`);
  } else {
    setError(keysError, '');
  }
  // The one control that opens the field carries the wording of what it will
  // do next: reveal the saved list, hide it, or open an empty editor when the
  // store cannot be read.
  keyReveal.textContent = revealLabel();
  const partial = typeof view.partialPersistence === 'string' ? view.partialPersistence : '';
  const flagged = partialObligation || (view.partialPersistence !== false && view.partialPersistence !== undefined);
  if (!flagged) {
    hide(partialWarning);
  } else {
    partialWarning.textContent =
      partial ||
      'Предыдущее сохранение завершилось частично: файлы могли измениться не полностью, при следующем запуске настройки могут отличаться от текущих. Устраните проблему доступа и повторите.';
    show(partialWarning);
  }
}

/**
 * Adopts a view. A page whose draft is gone is revived only by an explicit
 * open: an answer that arrives late for a draft that no longer exists must
 * never bring it back or repaint the window behind it.
 */
function applyView(next, { reopen = false, repaintGeneral } = {}) {
  if (!next || typeof next !== 'object') return;
  if (closed && !reopen) return;
  if (view !== null && next.draftId === view.draftId && typeof next.revision === 'number' && typeof view.revision === 'number') {
    // A view older than the one on screen says nothing new about the draft.
    if (next.revision < view.revision) return;
  }
  view = next;
  closed = false;
  // A view that names no obligation is the shell's own answer: the local note
  // made by a failed save has been superseded.
  if (next.partialPersistence === false) partialObligation = false;
  // The view may seed the dictation flag only until the first dictation event
  // arrives: an older view must not re-enable a control a newer event disabled.
  if (dictationEvents === 0 && typeof next.dictationActive === 'boolean') dictationActive = next.dictationActive;
  // A window opened while a dictation runs missed the event that started it, so
  // the view alone says why its field is locked. Only the active hint is painted
  // here: the line about the applied chord belongs to the next event to clear.
  if (dictationActive) hotkeyState.textContent = DICTATION_HINT;
  renderHotkey();
  // The decision belongs to this moment, not to the moment a caller's own await
  // began: a predicate is evaluated here, so an interaction that happened while
  // that await was in flight still wins. Without a caller's word, no other view
  // may repaint while a general send is outstanding.
  const repaint = typeof repaintGeneral === 'function' ? Boolean(repaintGeneral()) : repaintGeneral === undefined ? generalInFlight === 0 : repaintGeneral;
  if (repaint) renderGeneral();
  renderKeys();
  renderControls();
}

/* -------------------------------------------------------------------------- */
/* Secret editor                                                               */
/* -------------------------------------------------------------------------- */

function showEditor(mode, label, help) {
  editorMode = mode;
  keysEditorLabel.textContent = label;
  keysEditorHelp.textContent = help;
  show(keysEditor);
  keyReveal.textContent = revealLabel();
}

/**
 * Drops every local copy of a revealed list. Callers run it synchronously
 * before whatever would hide the field, and always before awaiting anything.
 * The acceptance flag goes with it: what the shell knows no longer describes
 * an empty field, so the next edit is always sent.
 */
function invalidateReveal() {
  revealEpoch += 1;
  revealIntent = false;
  editEpoch += 1;
  acceptedEpoch = -1;
  replaceTouched = false;
  editorMode = 'hidden';
  keysText.value = '';
  keysText.placeholder = '';
  hide(keysEditor);
  keyReveal.textContent = revealLabel();
}

/** True while an answer for this epoch and draft may still be drawn. */
function revealCurrent(epoch, draftId) {
  return (
    revealIntent &&
    !closed &&
    revealEpoch === epoch &&
    view !== null &&
    view.draftId === draftId &&
    section === 'keys'
  );
}

/* -------------------------------------------------------------------------- */
/* Command queue                                                               */
/* -------------------------------------------------------------------------- */

/**
 * Snapshots the editor for a send that may happen after the field is hidden.
 * Returns null when there is nothing to send: the field is hidden, an untouched
 * replacement field is still empty, or its content is exactly what the shell
 * already accepted. The snapshot pairs the text with the generation it was
 * taken at: a send waits in a queue, the field may hold something else by the
 * time it runs, and the text of one generation must never be accepted as
 * another. Nothing but the field itself holds the list once the send is done.
 */
function takeEditor() {
  if (editorMode === 'hidden' || acceptedEpoch === editEpoch) return null;
  const text = keysText.value;
  if (editorMode === 'replace' && text === '' && !replaceTouched) return null;
  return { text, epoch: editEpoch };
}

/**
 * Sends one snapshot and reports whether the shell took it. A refusal never
 * counts as sent: the caller must keep the text and stop what it was about to
 * do, or a failed send would quietly turn into a saved draft of the previous
 * list. The field is marked accepted only when it still holds the very
 * generation this send was taken from — a reply that arrives after the user
 * typed on must not declare the newer text sent.
 */
async function sendKeys(snapshot) {
  if (snapshot === null) return true;
  if (view === null || closed) return false;
  const { text, epoch } = snapshot;
  const draftId = view.draftId;
  const revision = view.revision;
  try {
    const next = await invoke(CMD.setKeys, { draftId, revision, text });
    applyView(next);
    acceptedEpoch = editorMode !== 'hidden' && editEpoch === epoch ? epoch : -1;
    if (keyErrorEcho !== '' && !view?.keyError && statusLine.textContent === keyErrorEcho) {
      setStatus('Черновик ключей обновлён: примените его кнопкой «Применить ключи».');
    }
    keyErrorEcho = '';
    return true;
  } catch (cause) {
    acceptedEpoch = -1;
    const error = errorOf(cause);
    if (error.code === 'STALE_DRAFT') {
      // Only a draft that is still on screen is re-read: an answer about a
      // draft that was closed or replaced must not revive it.
      if (draftLive(draftId)) {
        await resync({ keepKeysError: false });
        scheduleTyping();
      }
      return false;
    }
    // The text never reached the shell, and the field may have been hidden
    // since it was taken: bring back both, so a failed send cannot swallow
    // what was typed. The section it lives in decides whether it is on screen.
    if (editorMode === 'hidden') {
      keysText.value = text;
      editEpoch += 1;
      replaceTouched = true;
      keysText.placeholder = 'Новый полный список заменит прежний после применения';
      showEditor('replace', 'Новый список ключей', 'Введите полный список ключей: по одному в строке. Для замены недоступного хранилища потребуется подтверждение.');
    }
    // A failure leaves the field and its text alone: nothing is lost to it.
    showKeyError(error);
    return false;
  }
}

/**
 * Depth of read-only locks over the key field. Callers nest freely: a step that
 * flushes and then hides or redraws the field keeps it frozen for its whole
 * body, so no keystroke can slip in between the flush and the change.
 */
let editorLocks = 0;
/** Set when the page is unusable (no shell, forbidden); the field stays locked. */
let formDisabled = false;

function lockEditor() {
  editorLocks += 1;
  keysText.readOnly = true;
}

function unlockEditor() {
  editorLocks = Math.max(0, editorLocks - 1);
  if (editorLocks === 0 && !busy && !closed && !formDisabled) keysText.readOnly = false;
}

/**
 * Flushes the editor with the field frozen and reports whether the shell took
 * the list. A refusal leaves the field exactly as it is and says why: no step
 * in this page may discard text the shell never received.
 */
async function flushOrHold(message) {
  lockEditor();
  try {
    const flushed = await flushEditor();
    if (!flushed) setStatus(message);
    return flushed;
  } finally {
    unlockEditor();
  }
}

/**
 * Sends whatever was typed, then waits for every queued command. Returns false
 * when the draft could not be sent, so a caller that was about to save, check
 * or close stops instead of acting on a draft the shell never received.
 */
async function flushEditor() {
  clearTimeout(typingTimer);
  typingTimer = null;
  // The snapshot is taken before any await: a caller may hide the editor while
  // this is in flight, and the field must not be read after that.
  const snapshot = takeEditor();
  let ok = true;
  if (snapshot !== null) {
    await enqueue(async () => {
      ok = await sendKeys(snapshot);
    });
  }
  await drainQueue();
  return ok;
}

/**
 * Sends a snapshot a section switch took, without letting it delay the switch
 * itself. The snapshot was taken before the field went away, and nothing but
 * that send holds it.
 */
async function pipelineKeys(snapshot) {
  if (snapshot !== null) {
    await enqueue(async () => {
      await sendKeys(snapshot);
    });
  }
  await drainQueue();
}

/** True while an answer may still speak for the draft it was asked about. */
function draftLive(draftId) {
  return view !== null && !closed && view.draftId === draftId;
}

/** Re-reads the native draft; the editor and its text are left untouched. */
async function resync({ keepKeysError = true, repaintGeneral } = {}) {
  try {
    const next = await invoke(CMD.open, {});
    // A caller may pass a predicate: whether the general controls may be
    // repainted is then decided when this view is applied, not before the read
    // that may have taken longer than the next interaction.
    applyView(next, { reopen: true, repaintGeneral });
    return next;
  } catch (cause) {
    const error = errorOf(cause);
    if (!keepKeysError) setError(keysError, messageOf(error));
    setStatus(messageOf(error));
    if (error.code === 'FORBIDDEN') disableForm(messageOf(error));
    return null;
  }
}

/**
 * Queues a send of whatever the field holds right now. The snapshot is taken
 * here, not inside the queued task: a step that clears or replaces the field
 * while this send waits in line must not swallow the edit it was queued for.
 */
function queueEditorSend() {
  const snapshot = takeEditor();
  void enqueue(async () => {
    await sendKeys(snapshot);
  });
}

function scheduleTyping() {
  clearTimeout(typingTimer);
  typingTimer = setTimeout(() => {
    typingTimer = null;
    queueEditorSend();
  }, TYPING_IDLE_MS);
}

/* -------------------------------------------------------------------------- */
/* Errors and dialogs                                                          */
/* -------------------------------------------------------------------------- */

function showKeyError(error) {
  const line = typeof error.line === 'number' ? `Строка ${error.line}: ` : '';
  const message = messageOf(error);
  setError(keysError, `${line}${message}`);
  // Remembered so a later accepted send can replace the echoed failure text.
  keyErrorEcho = message;
  setStatus(message);
}

function askConfirm(title, text, confirmLabel) {
  return new Promise((resolve) => {
    let settled = false;
    const done = (answer) => {
      if (settled) return;
      settled = true;
      dialogConfirmOk.removeEventListener('click', onOk);
      dialogConfirmCancel.removeEventListener('click', onCancel);
      dialogConfirm.removeEventListener('cancel', onCancel);
      dialogConfirm.removeEventListener('close', onClose);
      dialogConfirm.close();
      resolve(answer);
      // A close request that arrived while this unrelated question was up is
      // answered now, in one flow; it was merged, not dropped.
      if (pendingCloseReason !== null && !closePending && !busy) void closeFlow();
    };
    const onOk = () => done(true);
    const onCancel = (event) => {
      if (event) event.preventDefault();
      done(false);
    };
    // A dialog closed by something other than these buttons is a refusal.
    const onClose = () => done(false);
    dialogConfirm.dataset.title = title;
    el('dialog-confirm-title').textContent = title;
    dialogConfirmText.textContent = text;
    dialogConfirmOk.textContent = confirmLabel || 'Продолжить';
    dialogConfirmOk.addEventListener('click', onOk);
    dialogConfirmCancel.addEventListener('click', onCancel);
    dialogConfirm.addEventListener('cancel', onCancel);
    dialogConfirm.addEventListener('close', onClose);
    dialogConfirm.showModal();
  });
}

/**
 * Apply keys / Do not apply / Return for a key draft. The wording names what is
 * about to happen, because closing the window and quitting are different ends.
 */
function askUnsaved(reason) {
  return new Promise((resolve) => {
    let settled = false;
    const done = (answer) => {
      if (settled) return;
      settled = true;
      dialogCloseSave.removeEventListener('click', onSave);
      dialogCloseDiscard.removeEventListener('click', onDiscard);
      dialogCloseReturn.removeEventListener('click', onReturn);
      dialogClose.removeEventListener('cancel', onCancel);
      dialogClose.removeEventListener('close', onClose);
      dialogClose.close();
      resolve(answer);
    };
    const onSave = () => done('apply');
    const onDiscard = () => done('discard');
    const onReturn = () => done('return');
    const onCancel = (event) => {
      if (event) event.preventDefault();
      done('return');
    };
    const onClose = () => done('return');
    dialogCloseTitle.textContent = 'Неприменённые изменения API-ключей';
    dialogCloseText.textContent =
      reason === 'quit'
        ? 'В черновике API-ключей есть изменения. Применить их перед выходом?'
        : 'В черновике API-ключей есть изменения. Применить их перед закрытием?';
    dialogCloseSave.addEventListener('click', onSave);
    dialogCloseDiscard.addEventListener('click', onDiscard);
    dialogCloseReturn.addEventListener('click', onReturn);
    dialogClose.addEventListener('cancel', onCancel);
    dialogClose.addEventListener('close', onClose);
    dialogClose.showModal();
  });
}

/* -------------------------------------------------------------------------- */
/* General settings                                                            */
/* -------------------------------------------------------------------------- */

/** The shell's read of the autostart registration: `true`, `false`, or `null`
 * when it could not be read or a foreign value owns the name. Anything else
 * means no view has answered yet. */
function autostartState() {
  return typeof view?.autostartEnabled === 'boolean' ? view.autostartEnabled : null;
}

/**
 * Paints the autostart switch. A registration the shell could not read stays
 * mixed and disabled instead of claiming a state Windows did not confirm, and
 * a change in flight keeps the user's own value until its answer lands.
 */
function renderAutostart() {
  if (autostartInFlight > 0) return;
  const state = autostartState();
  autostartToggle.checked = state === true;
  autostartToggle.indeterminate = state === null;
  autostartNotice.textContent = state === null && view !== null ? AUTOSTART_UNKNOWN : '';
  setError(autostartError, autostartFailure);
}

/**
 * Re-reads the autostart registration when the window is activated: Windows
 * Startup Apps and the registry can change it while the settings window stays
 * open in the background, and the switch would otherwise keep the value the
 * last open painted. The read changes nothing, so it is not queued; its answer
 * is merged into the newest accepted view only when it still speaks for the
 * draft on screen (activation epoch, draft id, revision) and the page is not
 * frozen or leaving. Every other field, every typed draft and every error line
 * stays untouched, and a change in flight keeps its own optimistic value: only
 * the registration this read was made for is adopted, and a failure leaves the
 * last value on screen rather than claiming a state Windows did not answer.
 */
async function refreshAutostartOnActivation() {
  if (view === null || closed || !shell || busy || closePending) return;
  const epoch = ++autostartRefreshEpoch;
  try {
    const next = await invoke(CMD.open, {});
    // A newer activation, a close flow or a frozen page owns the screen now: a
    // read for the state this window was in must not paint over it.
    if (epoch !== autostartRefreshEpoch || view === null || closed || busy || closePending) return;
    // Only an answer about the draft on screen and not older than the view it
    // answers may move the switch: a send that ran while this read was in
    // flight has already reported its own, newer registration.
    if (!next || typeof next !== 'object' || next.draftId !== view.draftId) return;
    if (typeof next.revision === 'number' && typeof view.revision === 'number' && next.revision < view.revision) return;
    // The shell answers `true`, `false` or `null`; anything else means this
    // page cannot tell Windows' state and must not invent one.
    if (next.autostartEnabled !== true && next.autostartEnabled !== false && next.autostartEnabled !== null) return;
    if (autostartState() === next.autostartEnabled) return;
    view = { ...view, autostartEnabled: next.autostartEnabled };
    renderAutostart();
    // The switch is disabled while the registration cannot be read: readability
    // may have changed on this read, so the control follows it.
    renderControls();
  } catch {
    // A failed re-read is not a state: the page keeps the value it last
    // accepted and waits for the next activation.
  }
}

/**
 * The autostart interaction, taken synchronously like the other general
 * settings: one change event is one intent and the switch's own value is read
 * here, while the send itself goes through the same serial queue so it cannot
 * race a chord or key command's revision.
 */
function onAutostartChange() {
  if (view === null || closed || busy) return;
  if (autostartState() === null) {
    // The registration cannot be read: the control takes no input and is repainted.
    renderAutostart();
    return;
  }
  const enabled = autostartToggle.checked;
  const draftId = view.draftId;
  autostartFailure = '';
  autostartInFlight += 1;
  setError(autostartError, '');
  setStatus('Применяем настройки…');
  void enqueue(() => sendAutostart(draftId, enabled));
}

/**
 * The general interaction, handled synchronously: the intent and the values are
 * taken here, before anything is queued, so two quick clicks can never share
 * one intent or one snapshot — the queue only decides when the send runs.
 */
function onGeneralChange() {
  if (view === null || closed || busy) return;
  const intent = generalIntent + 1;
  generalIntent = intent;
  // The chord is not part of this command: an old WebView view must not be able
  // to send a launcher key back, and a general change never touches the keys.
  // The mute switch, the port and the chosen device live in this section, so
  // their own current values travel: the change takes effect from the next
  // dictation, while a new port only after the next restart.
  const settings = {
    mode: selectedMode(),
    mute_during_recording: muteToggle.checked,
    port: desiredPort(),
    input_device: currentInputDevice(),
  };
  const draftId = view.draftId;
  // Counted until its answer lands: while a general send is outstanding, no
  // other view may repaint the controls, so a key-field or chord answer can
  // never rewrite what the user just chose and make the next snapshot wrong.
  generalInFlight += 1;
  setError(generalError, '');
  setStatus('Применяем настройки…');
  void enqueue(() => sendGeneral(intent, settings, draftId));
}

/**
 * Sends one snapshotted general change. The revision travels from the newest
 * accepted view at the moment this runs: a reply that belongs to an older
 * interaction may land, but it never repaints the choice made since.
 */
async function sendGeneral(intent, settings, draftId) {
  try {
    if (view === null || closed || view.draftId !== draftId) return;
    const revision = view.revision;
    try {
      const result = await invoke(CMD.updateGeneral, { draftId, revision, settings });
      if (result && result.view) applyView(result.view, { repaintGeneral: intent === generalIntent });
      if (intent !== generalIntent) return;
      if (result?.applied !== true) {
        setError(generalError, FALLBACK_MESSAGES.SAVE_FAILED);
        setStatus(FALLBACK_MESSAGES.SAVE_FAILED);
        return;
      }
      const warning = typeof result?.warning === 'string' ? result.warning : '';
      if (warning !== '') {
        setStatus(warning);
        show(partialWarning);
        partialWarning.textContent = warning;
        return;
      }
      setStatus('Настройки применены.');
    } catch (cause) {
      const error = errorOf(cause);
      // A stale revision means another answer already moved the draft on; the
      // newest view is the truth about the controls, and nothing failed.
      if (error.code !== 'STALE_DRAFT' && intent === generalIntent) {
        setGeneralFailure(error);
        setStatus(messageOf(error));
      }
      // The draft may have been closed while this answer was on its way: an
      // answer about it must not revive it in a hidden window.
      if (draftLive(draftId)) {
        await resync({ repaintGeneral: () => intent === generalIntent });
        if (error.code !== 'STALE_DRAFT' && intent === generalIntent) {
          setGeneralFailure(error);
        }
      }
    }
  } finally {
    generalInFlight = Math.max(0, generalInFlight - 1);
  }
}

/**
 * Sends one autostart change. The revision travels from the newest accepted
 * view: an answer that belongs to an older interaction may land, but the newest
 * view is the truth and a refusal rolls the switch back to it.
 */
async function sendAutostart(draftId, enabled) {
  try {
    if (view === null || closed || view.draftId !== draftId) return;
    const revision = view.revision;
    const next = await invoke(CMD.setAutostart, { draftId, revision, enabled });
    applyView(next);
    setStatus('Настройки применены.');
  } catch (cause) {
    const error = errorOf(cause);
    if (error.code === 'STALE_DRAFT') {
      // Another answer already moved the draft on; the newest view is the truth.
      if (draftLive(draftId)) await resync();
      return;
    }
    autostartFailure = messageOf(error);
    setStatus(autostartFailure);
    if (error.code === 'FORBIDDEN') disableForm(autostartFailure);
  } finally {
    autostartInFlight = Math.max(0, autostartInFlight - 1);
    // A refused choice is rolled back; an accepted one is already in `view`.
    renderAutostart();
  }
}

/** Puts a general refusal where the page can point at something: under the
 * port field, under the device picker, or on the section's own line. */
function setGeneralFailure(error) {
  const message = messageOf(error);
  if (error.field === 'port') {
    setError(portError, message);
    return;
  }
  if (error.field === 'input_device') {
    deviceFailure = message;
    paintDeviceError();
    return;
  }
  setError(generalError, message);
}

/* -------------------------------------------------------------------------- */
/* Port field                                                                 */
/* -------------------------------------------------------------------------- */

/**
 * The port field. Digits are typed, never captured: nothing is sent per
 * keystroke, and only a completed number — Enter, or leaving the field —
 * becomes a command. It travels through the same serial queue as every other
 * general change, so a port send cannot overtake a chord or a key send.
 *
 * A refused value leaves the text on screen, the running port and the previous
 * saved port exactly as they are. A committed value waiting for its answer
 * travels with every later general change too (`desiredPort`), so toggling the
 * mute switch while the port send is still in flight cannot put the old port
 * back.
 */
function submitPort() {
  if (view === null || closed || busy) return;
  const text = portInput.value.trim();
  const applied = Number.isInteger(view.settings.port) ? String(view.settings.port) : '';
  // The field already holds what the shell applies, and nothing is waiting:
  // there is no send and the field is clean again.
  if (text === applied && pendingPort === null) {
    portDirty = false;
    setError(portError, '');
    return;
  }
  const port = Number(text);
  if (!/^\d+$/.test(text) || port < 1 || port > 65535) {
    setError(portError, PORT_INVALID_MESSAGE);
    setStatus(PORT_INVALID_MESSAGE);
    return;
  }
  // The blur that follows Enter carries the value Enter just handed over: one
  // gesture, not a second intent.
  if (pendingPort !== null && pendingPort === port) return;
  pendingPort = port;
  setError(portError, '');
  onGeneralChange();
}

/**
 * Sends a completed port before the page is frozen or hidden. A native close,
 * a menu action or a section switch may never produce the blur that would
 * otherwise submit the text, so every lifecycle step calls this first; the
 * dedup above keeps it from duplicating an Enter or a blur that already sent
 * the value.
 */
function flushPort() {
  if (view === null || closed || busy) return;
  if (!portDirty) return;
  submitPort();
}

/* -------------------------------------------------------------------------- */
/* Hotkey field                                                                */
/* -------------------------------------------------------------------------- */

/**
 * The chord field. Text is typed, never captured: nothing is sent per
 * keystroke, and only a completed string — Enter, or leaving the field —
 * becomes a command. It travels through the same serial queue as every other
 * command, so a chord send cannot overtake a general or a key send.
 *
 * Dedup is by gesture, not by value: Enter and the blur it causes are one
 * intent, but a deliberate repeat is never swallowed. A chord the shell may
 * still owe a reconciliation for — a partial persistence left by a general or
 * key apply, or a chord error the newest view names — is always allowed to
 * travel again, so applying the same F3 can repair what a later answer broke.
 */
function submitHotkey(source) {
  if (view === null || closed || busy || dictationActive) return;
  const value = hotkeyInput.value.trim();
  const fieldEpoch = hotkeyFieldEpoch;
  // The blur that follows Enter carries the text Enter just handed over: that
  // is one gesture, not a second intent. It is dropped only while nothing is
  // owed, because an owed reconciliation must be reachable from the field.
  if (source === 'blur' && value === hotkeyChangeBaseline && !chordOwed()) return;
  // The very same snapshot is already waiting in line.
  if (hotkeySubmit !== null && hotkeySubmit.value === value && hotkeySubmit.fieldEpoch === fieldEpoch && hotkeySubmit.pending) {
    return;
  }
  const intent = hotkeyIntent + 1;
  hotkeyIntent = intent;
  const submit = { value, intent, fieldEpoch, pending: true };
  hotkeySubmit = submit;
  hotkeyChangeBaseline = value;
  hotkeyFailure = '';
  setError(hotkeyError, '');
  hotkeyState.textContent = '';
  setStatus('Применяем горячую клавишу…');
  void enqueue(() => sendHotkey(submit));
}

/**
 * True while the shell may still owe the files a reconciliation a chord send
 * can answer: a partial persistence left by any apply, or a chord error the
 * newest view still names. Only then is a repeated identical chord wanted.
 */
function chordOwed() {
  if (partialObligation) return true;
  if (view === null) return false;
  if (view.partialPersistence !== undefined && view.partialPersistence !== false) return true;
  return typeof view.hotkeyError === 'string' && view.hotkeyError !== '';
}

/**
 * Sends the completed chord the field is holding, when the shell does not have
 * it yet. Lifecycle steps — a tab switch, the comparison lab, a close or a quit
 * — call it before they freeze or hide the page: a native close or menu action
 * may never cause the blur that would otherwise submit the text. An unfinished
 * or invalid string is sent like any other and may be refused; that leaves the
 * previous chord working and never turns the text into a key draft. A value the
 * queue already has is not sent twice.
 */
function flushHotkey() {
  if (view === null || closed || busy || dictationActive) return;
  // A field nobody edited holds exactly what the shell already has; a flush is
  // for text the user completed, not for a fresh paint of the applied chord.
  if (!hotkeyDirty) return;
  const value = hotkeyInput.value.trim();
  if (value === hotkeyChangeBaseline && !chordOwed()) return;
  submitHotkey('flush');
}

/**
 * Sends one snapshotted chord. The revision travels from the newest accepted
 * view at the moment this runs; the answer may only report for the newest
 * intent, and never for text typed after the snapshot was taken.
 */
async function sendHotkey(submit) {
  try {
    if (view === null || closed) return;
    const draftId = view.draftId;
    try {
      const revision = view.revision;
      const result = await invoke(CMD.updateHotkey, { draftId, revision, hotkey: submit.value });
      if (result && result.view) applyView(result.view);
      if (submit.intent !== hotkeyIntent) return;
      // Newer text appeared while the command was in flight: it stays on
      // screen, the applied chord is named, and the newer text is not called
      // sent.
      if (hotkeyFieldEpoch !== submit.fieldEpoch) {
        const applied = result?.applied === true;
        setStatus(
          applied
            ? 'Горячая клавиша применена; в поле новый текст — нажмите Enter, чтобы применить его.'
            : 'Ввод изменился во время применения: нажмите Enter, чтобы применить новую строку.',
        );
        return;
      }
      if (result?.applied !== true) {
        // Nothing was applied, so no gesture of this text is spent: the field
        // keeps the chord and the next Enter or blur may try it again.
        hotkeyChangeBaseline = null;
        if (!draftLive(draftId)) return;
        // The newest view is read first: it names the chord that really works,
        // and the failure line under the field must not name a stale one.
        await resync();
        showHotkeyError(FALLBACK_MESSAGES.SAVE_FAILED);
        return;
      }
      // A draft that is gone (closed, or replaced by a newer one) must not be
      // painted from an answer to a question it never asked.
      if (!draftLive(draftId)) return;
      // The field now holds exactly the chord the shell reports, so the blur of
      // this very text is the tail of the gesture that applied it.
      hotkeyDirty = false;
      const applied = typeof view?.settings?.hotkey === 'string' ? view.settings.hotkey : submit.value;
      hotkeyInput.value = applied;
      submit.value = applied;
      hotkeyChangeBaseline = applied;
      appliedHotkey = applied;
      hotkeyFailure = '';
      setError(hotkeyError, '');
      hotkeyState.textContent = '';
      const warning = typeof result?.warning === 'string' ? result.warning : '';
      if (warning !== '') {
        setStatus(warning);
        show(partialWarning);
        partialWarning.textContent = warning;
      } else {
        setStatus('Горячая клавиша применена.');
      }
    } catch (cause) {
      const error = errorOf(cause);
      if (error.code === 'STALE_DRAFT') {
        hotkeyChangeBaseline = null;
        if (draftLive(draftId)) {
          await resync();
          if (submit.intent === hotkeyIntent && hotkeyFieldEpoch === submit.fieldEpoch) {
            setStatus('Черновик изменился: нажмите Enter, чтобы применить строку ещё раз.');
          }
        }
        return;
      }
      if (submit.intent !== hotkeyIntent) return;
      // A refused chord is not spent: the same text may be sent again.
      hotkeyChangeBaseline = null;
      if (!draftLive(draftId)) return;
      // The newest view is read first: it is what says which chord still works,
      // so the reason and the working chord are never painted from an older
      // answer than the one that refused this send.
      await resync();
      showHotkeyError(messageOf(error));
    }
  } finally {
    if (hotkeySubmit === submit) hotkeySubmit.pending = false;
  }
}

/**
 * Names the reason under the field and the chord that still works meanwhile.
 * The wording is kept until the user edits the field, so a later view that
 * paints a general or key answer does not silently drop it.
 */
function showHotkeyError(message) {
  hotkeyFailure = message;
  setError(hotkeyError, message);
  hotkeyState.textContent = appliedHotkey === '' ? '' : `Сейчас действует: ${appliedHotkey}.`;
  setStatus(message);
}

/* -------------------------------------------------------------------------- */
/* API keys                                                                    */
/* -------------------------------------------------------------------------- */

async function revealKeys() {
  if (view === null || closed) return;
  // The one control both opens and closes the field: it hides a revealed list
  // and it closes the empty editor the unavailable store opened.
  if (editorMode === 'revealed' || editorMode === 'replace') {
    await hideKeys();
    return;
  }
  // With the key store unavailable there is nothing to reveal: the same control
  // opens an empty editor and the unknown saved list is never loaded into it.
  if (view.secretsStatus === 'unavailable') {
    if (!(await flushOrHold('Список ключей не удалось отправить: исправьте ошибку и повторите.'))) return;
    invalidateReveal();
    replaceTouched = false;
    keysText.value = '';
    editEpoch += 1;
    keysText.placeholder = 'Новый полный список заменит содержимое хранилища после подтверждённого применения';
    showEditor('replace', 'Новый список ключей', 'Введите полный список ключей: по одному в строке. Для замены недоступного хранилища потребуется подтверждение.');
    keysText.focus();
    setStatus('Введите новый список: он заменит недоступное содержимое хранилища только после подтверждения и кнопки «Применить ключи».');
    return;
  }
  // The request is recorded on the click itself, before the first await: a
  // section switch or a close while the editor is flushing withdraws it, and a
  // later answer must not find a fresh reason to draw the keys.
  const epoch = revealEpoch;
  revealIntent = true;
  lockEditor();
  try {
    if (!(await flushOrHold('Список ключей не удалось отправить: исправьте ошибку и повторите.'))) {
      revealIntent = false;
      return;
    }
    if (!revealCurrent(epoch, view.draftId)) return;
    const draftId = view.draftId;
    const drawnEpoch = editEpoch;
    try {
      const text = await invoke(CMD.reveal, { draftId });
      if (!revealCurrent(epoch, draftId)) return;
      // The field is locked for this whole sequence, but its content could
      // still have been replaced from elsewhere: what changed is never
      // overwritten by an answer that was asked for before the change.
      if (editEpoch !== drawnEpoch) {
        revealIntent = false;
        setStatus('Список изменился, пока готовился показ: нажмите «Показать ключи» снова.');
        return;
      }
      keysText.value = typeof text === 'string' ? text : '';
      editEpoch += 1;
      // The shell may have answered with the saved list rather than the draft,
      // so what is drawn is not assumed to be what the shell holds: applying it
      // sends the list the user is looking at. A missing or empty saved list is
      // simply an empty editable field.
      acceptedEpoch = -1;
      replaceTouched = false;
      keysText.placeholder = '';
      showEditor('revealed', 'Список ключей', 'Список виден только в этом окне. Правки сохраняются по кнопке «Применить ключи».');
      keysText.focus();
      setStatus('Ключи показаны. Переключение раздела, применение или закрытие снова их скрывают.');
    } catch (cause) {
      if (!revealCurrent(epoch, draftId)) return;
      revealIntent = false;
      const error = errorOf(cause);
      setError(keysError, messageOf(error));
      setStatus(messageOf(error));
    }
  } finally {
    unlockEditor();
  }
}

/**
 * Hides a revealed list. What was typed still goes to the shell first, and a
 * failure keeps the field on screen instead of hiding the error with it.
 */
async function hideKeys() {
  if (!(await flushOrHold('Список ключей не удалось отправить: исправьте ошибку и повторите.'))) return;
  invalidateReveal();
  setStatus('Ключи скрыты.');
}

function clearKeys() {
  if (view === null || closed || busy || closePending) return;
  void (async () => {
    if (!(await flushOrHold('Список ключей не удалось отправить: исправьте ошибку и повторите.'))) return;
    const agreed = await askConfirm(
      'Очистить список ключей',
      'После применения диктовка отключится: приложению нужен хотя бы один ключ. Очистить черновик списка ключей?',
      'Очистить',
    );
    if (!agreed) return;
    // A close may have started while the question was up, and its own commands
    // are on their way: the clear is spent, not queued behind a hide.
    if (view === null || closed || closePending || busy) return;
    invalidateReveal();
    // The clear is a draft write like any other, so it goes through the same
    // serial queue: no general or key command can slip between the confirmed
    // click and the native call and leave it with a stale revision. The draft
    // is named at the moment it runs, not at the moment the button was clicked.
    busy = true;
    renderControls();
    try {
      await enqueue(async () => {
        if (view === null || closed) return;
        const draftId = view.draftId;
        try {
          const next = await invoke(CMD.clear, { draftId, revision: view.revision, confirmed: true });
          applyView(next);
          // The cleared draft is not the list the last report describes.
          resetCheckReport();
          setStatus('Черновик списка пуст: примените ключи, чтобы очистить сохранённый список.');
        } catch (cause) {
          const error = errorOf(cause);
          if (error.code === 'STALE_DRAFT') {
            // Another answer moved the draft on: the newest view is the truth,
            // and the clear is never reported as done.
            if (draftLive(draftId)) {
              await resync();
              resetCheckReport();
              setStatus('Черновик изменился: очистка не применена, повторите её.');
            }
            return;
          }
          if (draftLive(draftId)) showKeyError(error);
        }
      });
    } finally {
      busy = false;
      if (!closed) renderControls();
      // A close request that arrived while this held the page is answered now.
      if (pendingCloseReason !== null && !closePending) void closeFlow();
    }
  })();
}

/**
 * Drops a finished check report. It describes one list at one revision; the
 * moment the draft is edited or cleared it says nothing about what is typed,
 * so it goes rather than being read against newer text.
 */
function resetCheckReport() {
  clearChildren(keysResults);
  hide(keysResults);
  keysCheckSummary.textContent = '';
}

function renderCheckResults(results) {
  clearChildren(keysResults);
  const counts = { ok: 0, denied: 0, indeterminate: 0 };
  for (const item of results) {
    if (!counts[item.status]) counts[item.status] = 0;
    counts[item.status] += 1;
    const row = document.createElement('li');
    row.className = `settings-result settings-result-${item.status}`;
    const line = document.createElement('span');
    line.className = 'settings-result-line';
    line.textContent = `Строка ${item.line}`;
    const text = document.createElement('span');
    text.className = 'settings-result-text';
    text.textContent = item.message;
    row.append(line, text);
    keysResults.append(row);
  }
  keysResults.hidden = results.length === 0;
  keysCheckSummary.textContent = `Проверка завершена: доступ подтверждён — ${counts.ok || 0}, запрещён — ${counts.denied || 0}, не определено — ${counts.indeterminate || 0}. Проверка подтверждает только доступ к API.`;
}

function checkKeys() {
  if (view === null || closed) return;
  void (async () => {
    // Checking a list the shell has not received would report on the old one.
    if (!(await flushOrHold('Список ключей не удалось отправить: исправьте ошибку и запустите проверку снова.'))) return;
    // A replacement list is a transient field: it is finished once its text
    // has reached the shell, so it is hidden here; a revealed list stays.
    if (editorMode === 'replace') invalidateReveal();
    const draftId = view.draftId;
    const revision = view.revision;
    // The list generation this check was started for: an answer that arrives
    // after the user typed again describes text that is no longer on screen.
    const checkEdit = editEpoch;
    resetCheckReport();
    keysCheckSummary.textContent = 'Проверяем ключи по очереди…';
    try {
      const report = await invoke(CMD.check, { draftId, revision });
      // The draft may have been closed, reset or replaced while the keys were
      // being checked: an answer about another draft must not repaint this one,
      // and it must never open a fresh draft in a window that is already gone.
      if (closed || view === null || view.draftId !== draftId) return;
      if (editEpoch !== checkEdit) {
        // The list was edited while it was being checked: the report belongs to
        // the text that was sent, so it is dropped instead of being read as a
        // verdict on what is typed now.
        resetCheckReport();
        keysCheckSummary.textContent = 'Список изменился во время проверки: запустите проверку снова.';
        return;
      }
      if (report.draftId !== draftId || report.stale === true || report.revision !== view.revision) {
        keysCheckSummary.textContent = 'Черновик изменился во время проверки: запустите проверку снова.';
        // Only the same live draft is re-read; a different one is left alone.
        if (report.draftId === draftId) await resync();
        return;
      }
      renderCheckResults(Array.isArray(report.results) ? report.results : []);
    } catch (cause) {
      if (closed || view === null || view.draftId !== draftId) return;
      const error = errorOf(cause);
      if (error.code === 'STALE_DRAFT') {
        keysCheckSummary.textContent = 'Черновик изменился во время проверки: запустите проверку снова.';
        await resync();
        return;
      }
      keysCheckSummary.textContent = messageOf(error);
      setStatus(messageOf(error));
    }
  })();
}

/* -------------------------------------------------------------------------- */
/* Applying keys, closing and quitting                                         */
/* -------------------------------------------------------------------------- */

/**
 * Applies the key draft. The field is frozen for the whole sequence: a
 * keystroke typed while the list is being written cannot be erased by the
 * field being cleared afterwards. The list is sent first, and only a list the
 * shell actually received is applied. General settings never wait for this.
 */
async function applyKeys() {
  if (view === null || closed) return false;
  busy = true;
  keysText.readOnly = true;
  renderControls();
  try {
    const flushed = await flushEditor();
    if (!flushed) {
      // The list never reached the shell: applying now would write the previous
      // one. The field keeps the text and the error for a retry.
      setStatus('Список ключей не удалось отправить: исправьте ошибку и примените снова.');
      return false;
    }
    let confirmedReplace = false;
    for (;;) {
      if (view === null || closed) return false;
      const draftId = view.draftId;
      const revision = view.revision;
      try {
        const result = await invoke(CMD.applyKeys, { draftId, revision, confirmedReplace });
        if (result && result.view) applyView(result.view);
        const applied = result?.applied === true;
        const warning = typeof result?.warning === 'string' ? result.warning : '';
        if (applied) {
          partialObligation = false;
          // An applied list leaves nothing transient behind in this page.
          invalidateReveal();
        }
        if (warning !== '') {
          setStatus(warning);
          show(partialWarning);
          partialWarning.textContent = warning;
        } else {
          setStatus(applied ? 'Ключи применены.' : 'Ключи не применены.');
        }
        return applied;
      } catch (cause) {
        const error = errorOf(cause);
        if (error.code === 'STALE_DRAFT') {
          setStatus('Черновик изменился: список обновлён, примените ключи снова.');
          if (draftLive(draftId)) await resync();
          return false;
        }
        if (error.code === 'CONFIRM_REQUIRED') {
          const empty = view.keyCount === 0 && view.keysChanged === true;
          let text;
          if (view.secretsStatus === 'unavailable') {
            text =
              'Хранилище ключей недоступно: сохранённый список прочитать не удалось, и применение полностью заменит его. Продолжить?';
          } else if (empty) {
            text = 'Пустой список отключит диктовку: приложению нужен хотя бы один ключ. Применить пустой список?';
          } else {
            text = messageOf(error);
          }
          const agreed = await askConfirm('Подтверждение применения', text, 'Применить ключи');
          if (!agreed) {
            setStatus('Применение ключей отменено.');
            return false;
          }
          confirmedReplace = true;
          continue;
        }
        if (error.code === 'SAVE_PARTIAL') {
          partialObligation = true;
          partialWarning.textContent = messageOf(error);
          show(partialWarning);
          setStatus(messageOf(error));
          return false;
        }
        if (error.code === 'KEY_LIST_INVALID') {
          showKeyError(error);
          return false;
        }
        setStatus(messageOf(error));
        if (error.code === 'DICTATION_ACTIVE' || error.code === 'HOTKEY_CONFLICT') {
          setError(hotkeyError, messageOf(error));
        }
        return false;
      }
    }
  } finally {
    busy = false;
    keysText.readOnly = false;
    if (!closed) renderControls();
    // A close request that arrived while this held the page is answered now;
    // inside a close flow that request is already being handled.
    if (pendingCloseReason !== null && !closePending) void closeFlow();
  }
}

/**
 * Closes the draft. A plain close drops the draft and hides the window; a quit
 * (`forQuit`) drops the key draft and the secrets but leaves the window up
 * until the native quit really starts. All of that is the shell's decision.
 */
async function closeDraft({ discard, forQuit = false }) {
  if (view === null || closed) return true;
  try {
    await invoke(CMD.close, { draftId: view.draftId, discard: discard === true, forQuit });
    forgetDraft();
    return true;
  } catch (cause) {
    const error = errorOf(cause);
    if (error.code === 'UNSAVED_CHANGES' || error.code === 'SAVE_PARTIAL') {
      setStatus(messageOf(error));
      if (error.code === 'SAVE_PARTIAL') {
        partialObligation = true;
        partialWarning.textContent = messageOf(error);
        show(partialWarning);
      }
      return false;
    }
    setStatus(messageOf(error));
    return false;
  }
}

/** Drops every local trace of a draft the shell has already closed. */
function forgetDraft() {
  view = null;
  closed = true;
  // The chord field belongs to the draft that just went away: nothing typed
  // for it may still be sent, and the next open paints a fresh field.
  hotkeyIntent += 1;
  hotkeySubmit = null;
  hotkeyFieldEpoch += 1;
  hotkeyChangeBaseline = null;
  hotkeyDirty = false;
  hotkeyFailure = '';
  // The port field belongs to the draft that just went away too.
  pendingPort = null;
  portDirty = false;
  setError(portError, '');
  setPortNotice('');
  portInput.value = '';
  // The device choice belongs to the draft that went away too, and a read still
  // on its way must not paint the next one.
  deviceEpoch += 1;
  deviceDirty = false;
  pendingDevice = null;
  deviceFailure = '';
  deviceListFailure = '';
  paintDeviceError();
  deviceNotice.textContent = '';
  clearChildren(deviceSelect);
  invalidateReveal();
  resetCheckReport();
  keysSummary.textContent = '';
  setError(keysError, '');
  setError(hotkeyError, '');
  hotkeyState.textContent = '';
  hotkeyInput.value = '';
  renderControls();
}

/** The reason a running close flow must answer, with anything newer merged. */
function closeReasonNow() {
  return pendingCloseReason === 'quit' ? 'quit' : 'close';
}

/** Drops the merged close request and its consent: nothing of it may fire. */
function clearCloseIntent() {
  pendingCloseReason = null;
  partialConsentedFor = null;
}

/**
 * Resolves the close sequence: a key draft is offered Apply keys / Do not apply
 * / Return, a clean view closes at once. Returns true when the window was
 * closed (or, for a quit, handed over to the native quit action). Everything
 * that needs the window visible — the dialog and the partial warning — happens
 * before settings_close would hide it.
 *
 * The reason is re-read after every await: a tray quit that arrives while a
 * close dialog is up upgrades this very flow instead of being lost behind it.
 */
async function resolveClose() {
  if (view === null) {
    await openSettings();
    if (view === null) return false;
  }
  // A native close may never blur the chord field, so the completed text is
  // submitted here — before the page is frozen — and the drain below waits for
  // its answer: a partial flag such a send raised is then known to the warning
  // instead of being missed in a window that is about to hide.
  flushHotkey();
  flushPort();
  // The editor is flushed first: it decides whether there is anything to apply,
  // and a refused send leaves the text on screen for the dialog below.
  const draftId = view.draftId;
  keysText.readOnly = true;
  let flushed;
  try {
    flushed = await flushEditor();
  } finally {
    keysText.readOnly = false;
  }
  // The close contract: the sent hotkey update and every queued answer are
  // waited out, and the decision below is then made on a freshly read
  // non-secret view — a draft a failed apply left untouched, or a partial flag
  // a late answer raised, is seen here instead of being decided on the view
  // that predates it. The editor keeps its text (resync never touches it), and
  // a draft that is gone by now is not revived.
  await drainQueue();
  if (draftLive(draftId)) await resync();
  const unsent = !flushed && takeEditor() !== null;
  let discard = false;
  // Only a list that really differs from the saved one - or text the shell
  // never received - is worth a question. The native draft's own flag is not
  // asked about on its own: a revealed list that was merely viewed, or retyped
  // as it is, is not a change.
  if (view.keysChanged === true || unsent) {
    const answer = await askUnsaved(closeReasonNow());
    if (answer === 'return') {
      clearCloseIntent();
      setStatus(closeReasonNow() === 'quit' ? 'Выход отменён.' : 'Закрытие отменено.');
      return false;
    }
    if (answer === 'apply') {
      if (!(await applyKeys())) {
        // A failed apply leaves the window and the request behind: nothing of
        // this intent may fire later on its own.
        clearCloseIntent();
        return false;
      }
    } else {
      discard = true;
    }
  }
  // A chord send or a terminal answer may have queued a re-read of the
  // non-secret view; it lands before the questions below, so the flags they ask
  // about are the newest ones the shell has and a late partial flag is not
  // skipped in a window that is about to hide.
  await drainQueue();
  // The reason may have been upgraded while the dialog was open, so the
  // question below is asked for what is about to happen now — and consent to a
  // plain close never counts as consent to quit.
  const reason = closeReasonNow();
  if (!(await acknowledgePartial(reason))) {
    clearCloseIntent();
    return false;
  }
  const closed = await closeWithReason(discard);
  // A refusal of settings_close keeps the window up; the request is spent, so
  // only a new one from the shell may start a fresh flow.
  if (!closed) clearCloseIntent();
  return closed;
}

/**
 * Ends the session. The reason is re-read after the close command: a tray quit
 * that arrived while settings_close was in flight is merged, not lost, and the
 * quit still happens. The merged intent is kept until the quit really starts.
 */
async function closeWithReason(discard) {
  const forQuit = closeReasonNow() === 'quit';
  if (!(await closeDraft({ discard, forQuit }))) return false;
  if (closeReasonNow() !== 'quit') {
    clearCloseIntent();
    return true;
  }
  // The consent is read before the intent is dropped: it is what the native
  // quit action is told, so it must survive this flow's own bookkeeping.
  const consented = partialConsentedFor === 'quit';
  const done = await quitThroughPage(consented);
  clearCloseIntent();
  return done;
}

/**
 * Asks about the partial-persistence obligation while the window is still on
 * screen. Returns false only when the user refused to go on. Consent is kept
 * per reason: agreeing to close the window is not agreeing to quit.
 */
async function acknowledgePartial(reason) {
  const fromView = view !== null && view.partialPersistence !== false && view.partialPersistence !== undefined;
  if (!partialObligation && !fromView) return true;
  if (partialConsentedFor === reason) return true;
  const own = typeof view?.partialPersistence === 'string' && view.partialPersistence !== '' ? view.partialPersistence : '';
  const base =
    own ||
    'Предыдущее сохранение завершилось частично: файлы могли измениться не полностью, и при следующем запуске настройки могут отличаться от текущих.';
  const quitting = reason === 'quit';
  const agreed = await askConfirm(
    quitting ? 'Выход' : 'Закрытие окна',
    quitting ? `${base} Завершить работу?` : `${base} Закрыть окно?`,
    quitting ? 'Завершить работу' : 'Закрыть окно',
  );
  if (!agreed) {
    setStatus(quitting ? 'Выход отменён.' : 'Закрытие отменено.');
    return false;
  }
  partialConsentedFor = reason;
  return true;
}

/**
 * A one-shot wait for the open request the shell sends after showing the window
 * again. Registration completes before this returns, so the caller invokes the
 * command that may need the event only once the listener is really in place;
 * there is no timer and no way to show the window from this page.
 */
async function waitForOpenRequest() {
  const holder = { unlisten: null, settled: false };
  let resolvePromise;
  const promise = new Promise((resolve) => {
    resolvePromise = resolve;
  });
  const settle = () => {
    if (holder.settled) return;
    holder.settled = true;
    if (holder.unlisten) void holder.unlisten();
    resolvePromise();
  };
  try {
    const off = await tauri.event.listen(EVENT_OPEN_REQUEST, () => settle());
    holder.unlisten = off;
    // The event may already have arrived while registration was in flight.
    if (holder.settled) void off();
  } catch {
    // Without a shell that can register a listener there is nothing to wait
    // for; the caller's own error handling still runs.
    settle();
  }
  return { promise, cancel: () => settle() };
}

/**
 * Runs the native quit action. A refusal asks the shell to bring the window
 * back first; the one-shot listener above catches that, the fresh non-secret
 * view is re-read, and the quit-specific partial warning is asked before the
 * action is repeated with the consent it was waiting for.
 */
async function quitThroughPage(acknowledgedPartial, attempt = 0) {
  // Awaited, not started: the listener is in place before the action can refuse.
  const reopened = await waitForOpenRequest();
  try {
    await invoke(CMD.action, { action: 'quit', acknowledgedPartial });
    setStatus('Завершаем работу…');
    reopened.cancel();
    return true;
  } catch (cause) {
    const error = errorOf(cause);
    if ((error.code === 'SAVE_PARTIAL' || error.code === 'UNSAVED_CHANGES') && attempt < 2) {
      if (error.code === 'SAVE_PARTIAL') {
        partialObligation = true;
        partialWarning.textContent = messageOf(error);
        show(partialWarning);
      }
      setStatus(messageOf(error));
      // The shell asked for the window before refusing; wait for the request it
      // sent after showing it, then read the newest view from the page that is
      // visible again. Nothing is asked in a hidden window.
      await reopened.promise;
      const fresh = await resync();
      if (fresh === null) return false;
      if (error.code === 'UNSAVED_CHANGES') {
        // The shell still sees a draft this page does not: drop it, then ask
        // the question that the refusal was really about.
        if (!(await closeDraft({ discard: true, forQuit: true }))) return false;
      }
      partialConsentedFor = null;
      if (!(await acknowledgePartial('quit'))) return false;
      return quitThroughPage(partialConsentedFor === 'quit', attempt + 1);
    }
    reopened.cancel();
    setStatus(messageOf(error));
    return false;
  }
}

/** Runs the comparison-lab action; it never touches the key draft. */
async function runLab() {
  try {
    await invoke(CMD.action, { action: 'lab' });
    setStatus('Открываем сравнение.');
    return true;
  } catch (cause) {
    setStatus(messageOf(errorOf(cause)));
    return false;
  }
}

async function openLab() {
  // A dictation owns the microphone and the foreground: the lab is not opened
  // over one, exactly as its disabled button says.
  if (view === null || dictationActive) return;
  flushHotkey();
  flushPort();
  if (!(await flushOrHold('Список ключей не удалось отправить: исправьте ошибку и повторите.'))) return;
  invalidateReveal();
  await runLab();
}

/* -------------------------------------------------------------------------- */
/* Sections                                                                    */
/* -------------------------------------------------------------------------- */

async function switchSection(name, { focus = false } = {}) {
  if (!SECTIONS.includes(name)) return;
  // The card describes the section being left; it goes before anything of the
  // next section can be painted under it.
  closeHelp();
  // The completed chord is sent before anything is hidden or locked: a tab
  // click may never blur the field, and the section switch must not lose text
  // the shell has not received. The dedup keeps this from duplicating the
  // Enter or the blur that already sent the same text.
  flushHotkey();
  flushPort();
  // The intent to reveal dies before anything can hide the field, and the
  // field's text is taken before it is cleared, so a switch can never lose a
  // keystroke that had not reached the shell yet — and an untouched blank
  // replacement field is never sent as an empty list.
  const pending = takeEditor();
  revealIntent = false;
  if (name === section) {
    if (focus) tabs[name].focus();
    return;
  }
  if (section === 'keys') invalidateReveal();
  section = name;
  renderSection();
  if (focus) tabs[name].focus();
  void pipelineKeys(pending);
}

async function openSettings() {
  // A card left open when the window was hidden must not greet the next open.
  closeHelp();
  const wanted = pendingSection;
  pendingSection = null;
  const next = await resync();
  if (next === null) return null;
  const requested = wanted ?? (SECTIONS.includes(next.selectedSection) ? next.selectedSection : 'last');
  // "last" keeps the section on screen; anything else goes through the very
  // same switch the tabs use, so no keystroke is left behind on the way.
  await switchSection(requested);
  // The window is open and takes focus: the host's device list is read here,
  // and again on every later activation.
  void refreshDevices();
  return next;
}

/* -------------------------------------------------------------------------- */
/* Events                                                                      */
/* -------------------------------------------------------------------------- */

function onOpenRequest(event) {
  const payload = payloadOf(event);
  const wanted = payload && typeof payload === 'object' ? payload.section : null;
  pendingSection = SECTIONS.includes(wanted) ? wanted : 'last';
  void openSettings();
}

/**
 * The shell may hand the same close request over more than once until
 * settings_close answers it — the window's own close button, the tray's quit,
 * or both. The reason is merged before anything else, so a quit can never be
 * lost behind the close it upgrades, and a second dialog is never stacked on
 * the first: the running flow re-reads the merged reason after each await.
 */
function onCloseRequest(event) {
  const payload = payloadOf(event);
  const raw = payload && typeof payload === 'object' ? payload.reason : null;
  const reason = raw === 'quit' ? 'quit' : 'close';
  pendingCloseReason = reason === 'quit' ? 'quit' : pendingCloseReason === 'quit' ? 'quit' : 'close';
  if (dialogClose.open || dialogConfirm.open || closePending || busy) return;
  void closeFlow();
}

async function closeFlow() {
  closePending = true;
  // The freeze is painted at once: the chord field and the general controls
  // stop taking new intents while settings_close is on its way.
  renderControls();
  try {
    await resolveClose();
  } finally {
    closePending = false;
    if (!closed) renderControls();
  }
}

function onDictationState(event) {
  const payload = payloadOf(event);
  const active = Boolean(payload && typeof payload === 'object' ? payload.active : payload);
  // Counted, not just assigned: a view that answers an older question may seed
  // the flag, but it may never undo the newest dictation event.
  dictationEvents += 1;
  dictationActive = active;
  // The field is disabled while a dictation runs and the reason is named;
  // native DICTATION_ACTIVE stays the backstop against the race.
  hotkeyState.textContent = active ? DICTATION_HINT : '';
  renderControls();
}

/**
 * A press of the launcher chord that belongs to the chord field.
 *
 * Windows hands the chord to the shell instead of the page, so the field never
 * sees the keys and its own `keydown`/`input` cannot write the chord. The
 * shell therefore sends the chord it is running, and this writes it into the
 * field exactly as a typed edit would: the text is the user's until Enter or
 * leaving the field sends it, and no keystroke here applies anything.
 */
function onHotkeyKey(event) {
  if (view === null || closed || busy || dictationActive) return;
  if (!hotkeyFieldHoldsKeyboard()) return;
  const payload = payloadOf(event);
  const chord = payload && typeof payload === 'object' && typeof payload.hotkey === 'string'
    ? payload.hotkey.trim()
    : '';
  if (chord === '' || hotkeyInput.value === chord) return;
  hotkeyInput.value = chord;
  hotkeyInput.setSelectionRange(chord.length, chord.length);
  hotkeyDirty = true;
  hotkeyFieldEpoch += 1;
  hotkeyChangeBaseline = null;
  hotkeyFailure = '';
  setError(hotkeyError, '');
  hotkeyState.textContent = '';
}

/* -------------------------------------------------------------------------- */
/* Wiring                                                                      */
/* -------------------------------------------------------------------------- */
function disableForm(message) {
  const hotkeyWasHeld = hotkeyFieldHoldsKeyboard();
  for (const node of [keyReveal, keyApply, keyClear, keyCheck, actionLab, hotkeyInput, portInput, deviceSelect, muteToggle, autostartToggle]) {
    node.disabled = true;
  }
  if (hotkeyWasHeld) signalHotkeyField(false);
  modePicker.querySelectorAll('input').forEach((input) => {
    input.disabled = true;
  });
  keysText.readOnly = true;
  if (message) setStatus(message);
}

/** The help card on screen: its trigger and the panel it opened, or null. */
let helpTrigger = null;
let helpPanel = null;

/**
 * Places the open card inside the window: under its trigger by default, above
 * it when there is no room below, and never across the 8 px viewport margin.
 * Its height is capped to the window so a long hint cannot leave the screen,
 * and the left edge is pulled back when the card would cross the right margin.
 */
function placeHelp() {
  if (!helpPanel || !helpTrigger || helpPanel.hidden) return;
  const margin = 8;
  const gap = 6;
  const rect = helpTrigger.getBoundingClientRect();
  const viewportWidth = document.documentElement.clientWidth;
  const viewportHeight = document.documentElement.clientHeight;
  helpPanel.style.maxHeight = `${Math.max(0, viewportHeight - margin * 2)}px`;
  const width = helpPanel.offsetWidth;
  const height = helpPanel.offsetHeight;
  const below = rect.bottom + gap;
  const above = rect.top - gap - height;
  // Under the trigger by default, above it when there is no room below, and
  // inside the viewport even when its trigger has been scrolled out of it.
  const preferred = below + height <= viewportHeight - margin || above < margin
    ? below
    : above;
  const top = Math.min(Math.max(preferred, margin), Math.max(margin, viewportHeight - margin - height));
  const left = Math.min(Math.max(rect.left, margin), Math.max(margin, viewportWidth - margin - width));
  helpPanel.style.left = `${Math.round(left)}px`;
  helpPanel.style.top = `${Math.round(top)}px`;
}

/** Takes the open card off the screen and frees its trigger. */
function closeHelp() {
  if (helpTrigger) helpTrigger.setAttribute('aria-expanded', 'false');
  if (helpPanel) helpPanel.hidden = true;
  helpTrigger = null;
  helpPanel = null;
}

/**
 * Shows one card at a time. The panel moves into the body for good: the
 * scrollable settings area can no longer clip it, and the panel's own geometry
 * can never add to that area's scroll size. Its id stays, so the trigger's
 * `aria-controls` and `aria-describedby` keep naming the same element, and
 * `[hidden]` still takes it out of the page.
 */
function openHelp(trigger, panel) {
  if (helpTrigger === trigger && helpPanel === panel) {
    placeHelp();
    return;
  }
  closeHelp();
  document.body.appendChild(panel);
  panel.style.position = 'fixed';
  panel.hidden = false;
  trigger.setAttribute('aria-expanded', 'true');
  helpTrigger = trigger;
  helpPanel = panel;
  placeHelp();
}

/**
 * The question-mark buttons beside the blocks. Every trigger names its panel by
 * `aria-controls`; the panel is what the trigger points at with
 * `aria-describedby`, and `aria-expanded` says whether it is on screen now.
 * Hovering the trigger opens its card and the pointer leaving closes it; Tab
 * focus opens it for a keyboard reader and blur takes it away. A click never
 * pins: the focus it leaves is not `:focus-visible`, so taking the pointer off
 * the trigger closes the card. Escape closes it, and nothing brings it back
 * until the pointer enters a trigger again or Tab focuses one anew. Only one
 * card is on screen: opening another closes the first, and a section switch
 * closes the open one. A panel never touches the draft: it may be read while a
 * command runs.
 */
function wireHelp() {
  for (const trigger of document.querySelectorAll('.settings-help')) {
    const panel = el(trigger.getAttribute('aria-controls'));
    if (!panel) continue;
    trigger.addEventListener('pointerenter', () => openHelp(trigger, panel));
    trigger.addEventListener('pointerleave', () => {
      // A keyboard-focused trigger keeps its card: the focus ring is still on
      // it, and Tab or the pointer will take the card away.
      if (helpTrigger === trigger && !trigger.matches(':focus-visible')) closeHelp();
    });
    trigger.addEventListener('focus', () => {
      // Focus opens the card only when the keyboard put it there: a mouse
      // click focuses the trigger too, and that card must not outlive the
      // hover that showed it.
      if (trigger.matches(':focus-visible')) openHelp(trigger, panel);
    });
    trigger.addEventListener('blur', () => {
      if (helpTrigger === trigger) closeHelp();
    });
  }
  // Escape closes the open card wherever the focus is; the next hover or Tab
  // focus is what may bring a card back.
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && helpTrigger) closeHelp();
  });
  // The card is placed from geometry, so scrolling the settings area or
  // resizing the window moves it with its trigger.
  document.addEventListener('scroll', () => placeHelp(), { capture: true, passive: true });
  window.addEventListener('resize', () => placeHelp());
}

function wire() {
  wireHelp();
  tabs.general.addEventListener('click', () => void switchSection('general'));
  tabs.keys.addEventListener('click', () => void switchSection('keys'));
  for (const name of SECTIONS) {
    tabs[name].addEventListener('keydown', (event) => {
      if (event.key !== 'ArrowRight' && event.key !== 'ArrowLeft' && event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
      event.preventDefault();
      const index = SECTIONS.indexOf(name);
      const step = event.key === 'ArrowRight' || event.key === 'ArrowDown' ? 1 : SECTIONS.length - 1;
      const next = SECTIONS[(index + step) % SECTIONS.length];
      void switchSection(next, { focus: true });
    });
  }
  modePicker.addEventListener('change', (event) => {
    if (event.target.name !== 'transcription-mode') return;
    onGeneralChange();
  });
  // Clicking the mode that is already in effect is still a general action: a
  // radio fires no `change` for it, and it is the way a draft owed a partial
  // write is reconciled with what is running now.
  modePicker.addEventListener('click', (event) => {
    const input = event.target;
    if (!(input instanceof HTMLInputElement) || input.name !== 'transcription-mode' || !input.checked) return;
    const current = view?.settings && typeof view.settings.mode === 'string' ? view.settings.mode : null;
    if (input.value !== current) return;
    const owed = partialObligation || (view.partialPersistence !== false && view.partialPersistence !== undefined);
    if (!owed) return;
    onGeneralChange();
  });

  // The chord is typed: a keystroke only edits the field, and only a completed
  // string — Enter, or leaving the field — is sent. Enter and the blur that
  // follows share one snapshot, so the two gestures cannot race each other.
  hotkeyInput.addEventListener('input', () => {
    hotkeyDirty = true;
    hotkeyFieldEpoch += 1;
    // An edit is a new gesture: whatever was sent before says nothing about it,
    // even when the text ends up the same.
    hotkeyChangeBaseline = null;
    // The user is answering the complaint by typing: it stops being current.
    hotkeyFailure = '';
    setError(hotkeyError, '');
    hotkeyState.textContent = '';
  });
  hotkeyInput.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter' || event.isComposing) return;
    event.preventDefault();
    submitHotkey('enter');
  });
  hotkeyInput.addEventListener('change', () => submitHotkey('blur'));
  // The chord field is exactly what the launcher chord must not steal: the
  // shell hears when it takes the keyboard and when it gives it up, so a press
  // with the field focused stays text while a press without it starts a take.
  hotkeyInput.addEventListener('focus', () => signalHotkeyField(true));
  hotkeyInput.addEventListener('blur', () => signalHotkeyField(false));

  // The mute switch is an ordinary general setting: one change event is one
  // intent, and the value is read from the control itself when it is sent.
  muteToggle.addEventListener('change', () => onGeneralChange());

  // The autostart switch is an ordinary general setting with its own command:
  // one change event is one intent, and the value is read from the control here.
  autostartToggle.addEventListener('change', () => onAutostartChange());

  // The port is typed like the chord: a keystroke only edits the field and
  // takes the complaint about the previous value away, and only a completed
  // number — Enter, or leaving the field — is sent.
  portInput.addEventListener('input', () => {
    portDirty = true;
    setError(portError, '');
  });
  portInput.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter' || event.isComposing) return;
    event.preventDefault();
    submitPort();
  });
  portInput.addEventListener('change', () => submitPort());

  // The picker is a completed gesture too: one selection is one general change,
  // and the value is read from the control itself when it is sent.
  deviceSelect.addEventListener('change', () => {
    if (view === null || closed || busy) return;
    deviceDirty = true;
    pendingDevice = deviceSelect.value === '' ? null : deviceSelect.value;
    deviceFailure = '';
    paintDeviceError();
    onGeneralChange();
  });

  keyReveal.addEventListener('click', () => void revealKeys());
  keyApply.addEventListener('click', () => void applyKeys());
  keyClear.addEventListener('click', () => clearKeys());
  keyCheck.addEventListener('click', () => checkKeys());

  keysText.addEventListener('input', () => {
    replaceTouched = true;
    // The field no longer holds the text the shell last accepted.
    editEpoch += 1;
    // A report about the list that was there a moment ago would be read as a
    // verdict on this new text: it goes at once, before any debounce.
    resetCheckReport();
    scheduleTyping();
  });
  keysText.addEventListener('blur', () => queueEditorSend());

  actionLab.addEventListener('click', () => void openLab());

  // The window is shown and activated again: the host may have gained or lost
  // a device while it was hidden, and Windows may have changed the autostart
  // registration in the meantime, so both are re-read on every focus — and the
  // chord field may hold the keyboard again, which the shell has to know.
  window.addEventListener('focus', () => {
    void refreshDevices();
    void refreshAutostartOnActivation();
    signalHotkeyField(hotkeyFieldHoldsKeyboard());
  });

  window.addEventListener('pagehide', () => {
    // The page is going away with whatever held its keyboard: the shell must
    // not keep a chord gate that no field will ever release.
    signalHotkeyField(false);
    revealIntent = false;
    revealEpoch += 1;
    editEpoch += 1;
    acceptedEpoch = -1;
    keysText.value = '';
  });
}

/* -------------------------------------------------------------------------- */
/* Boot                                                                        */
/* -------------------------------------------------------------------------- */

async function boot() {
  wire();
  renderSection();
  renderControls();

  if (!shell) {
    show(notice);
    disableForm('Настройки можно изменить только в окне Speechek.');
    setStatus('Доступна только эта страница-объяснение.');
    return;
  }

  try {
    // The listeners come first: a section request or a close request may be
    // waiting for this page, and it must not miss either of them.
    unlisteners.push(await tauri.event.listen(EVENT_OPEN_REQUEST, onOpenRequest));
    unlisteners.push(await tauri.event.listen(EVENT_CLOSE_REQUEST, onCloseRequest));
    unlisteners.push(await tauri.event.listen(EVENT_DICTATION_STATE, onDictationState));
    unlisteners.push(await tauri.event.listen(EVENT_HOTKEY_KEY, onHotkeyKey));
    // The document identity comes before any report: the shell drops a report
    // it cannot place, and a focus that arrives while this is in flight is
    // reported by the handshake itself.
    void handshakeHotkeyField();
  } catch (cause) {
    shell = false;
    show(notice);
    disableForm('Окно не смогло связаться с приложением.');
    console.error('speechek settings: the shell did not accept an event listener', cause);
    return;
  }

  setStatus('Загрузка настроек…');
  await openSettings();
  if (statusLine.textContent === 'Загрузка настроек…') {
    setStatus('Настройки загружены. Общие настройки применяются сразу, порт — после перезапуска; ключи — кнопкой «Применить ключи».');
  }
}

void boot();
