/*
 * Speechek's recording overlay.
 *
 * The shell owns the dictation: its F2 binding shows this window and sends
 * `speechek:toggle`, tagged with the generation of the dictation. Nothing here
 * starts a recording on its own, and this file never opens a microphone: the
 * desktop capture belongs to the shell, which streams 16 kHz mono PCM16 frames
 * to this page as `speechek:pcm-frame`. They are handed to the shared recorder,
 * started with `external: true`, which is the only thing talking to the server.
 *
 * A dictation, as this page sees it:
 *
 *   toggle ─► arming ──first frame──► recording ──toggle──► transcribing
 *                │                                              │
 *                └─ start failed ─► error                        ├─ text ─► insert_text ─► finish_session success
 *                                                                └─ no text ─► error ─► finish_session error
 *
 * `speechek:cancel` takes any of those states out of the dictation: Escape was
 * pressed, the take is dropped, and no request of it is made or awaited — the
 * recorder it belonged to is disposed, so nothing of the take is transcribed,
 * uploaded or inserted, and the generation cannot start again.
 *
 * The pill stays visible while recognition runs, with a processing status and
 * a spinner instead of the recording meter. No transcript is drawn in any
 * state. The shell hides the pill before insertion; a failure leaves its reason
 * visible briefly. `finish_session` reports which ending happened.
 *
 * One shell event is a warning and not an ending: `speechek:mute-warning`, the
 * system sound the shell tried to silence for this take and could not work
 * with. It takes over the second line of the pill and the dictation records on.
 *
 * The arming state is held until the shell delivers its first real frame, so
 * the bars never move on their own. The two shell events one dictation can
 * produce — the arm itself and the stop the shell queued while it was arming —
 * share a generation, so a generation is accepted at most twice and never after
 * a newer one has arrived. A stop waits for every frame the shell had already
 * queued before the recorder is closed, so the tail reaches Live and the WAV.
 */

import { createRecorder } from './recorder.js';

/** The only event the shell sends to the overlay, and the only recording trigger. */
const EVENT_TOGGLE = 'speechek:toggle';
/** The shell's native capture: one audio frame, and one capture failure. */
const EVENT_PCM_FRAME = 'speechek:pcm-frame';
const EVENT_MIC_ERROR = 'speechek:mic-error';
/**
 * The shell could not silence the system for this take, or could not give the
 * sound back. It is a warning, not a failure: the recording goes on, and the
 * sentence only takes over the second line of the pill. A warning marked as the
 * take's last arrives after Escape cancelled it, with its sound still silenced:
 * that one is painted on its own and the shell brings the pill back for it.
 */
const EVENT_MUTE_WARNING = 'speechek:mute-warning';
/**
 * The shell cancelled the dictation in progress: Escape was pressed while it
 * ran. Nothing of the take may survive the event — no transcription, no
 * insertion, and no late answer of it either.
 */
const EVENT_CANCEL = 'speechek:cancel';
/**
 * The launcher settings were saved. The idle line follows the new binding; the
 * mode of a take never does, because the shell pins that per dictation.
 */
const EVENT_SETTINGS_CHANGED = 'speechek:settings-changed';
const SETTINGS_URL = '/api/settings';

const MODES = new Set(['live', 'smart', 'verbatim']);
/** The binding shown while idle, until the launcher settings name another one. */
const DEFAULT_HOTKEY = 'F2';

/** The recorder's level reports arrive at most ~30 times a second. */
const METER_MIN = 3;
const METER_MAX = 18;
/** Centre-weighted bar shape: the middle bar is the tall one. */
const METER_SHAPE = [0.42, 0.58, 0.76, 0.9, 1, 0.9, 0.76, 0.58, 0.42];
/** Smoothing of one bar towards the level of the newest sample: fast rise, slow fall. */
const METER_RISE = 0.8;
const METER_FALL = 0.18;

const TIMER_TICK_MS = 200;
const CAP_MS = 600000;
/** Grace for the frames the shell had queued when it answered the stop. */
const NATIVE_TAIL_MS = 1500;

/** The second line of the pill: which mode records, in the lab's own words. */
const MODE_LABELS = { live: 'Live Smart', smart: 'Smart', verbatim: 'Дословно' };

/** Shown verbatim when the shell had to leave the text on the clipboard. */
const COPIED_FALLBACK = 'Скопировано — вставьте вручную';
/** Windows blocks the microphone in its own privacy settings, not in a page prompt. */
const MIC_DENIED = 'Разрешите микрофон: Параметры Windows → Конфиденциальность → Микрофон';
/** The shell could not start capturing, and its reason is the whole story. */
const MIC_FAILED = 'Запись не началась';
/** The shell had already queued these frames, so the take is not trustworthy. */
const TAIL_LOST = 'Потеряны последние кадры записи — текст не вставлен';
const NO_TEXT = 'Не удалось распознать речь';

const overlay = document.getElementById('overlay');
const statusLine = document.getElementById('status');
const elapsedLine = document.getElementById('elapsed');
const modeLine = document.getElementById('mode');
const bars = Array.from(document.querySelectorAll('.bar'));

/** The Tauri global the shell injects; absent in a plain browser preview. */
const tauri = globalThis.__TAURI__;
/** True only when the page really can reach the shell. */
let shell = Boolean(tauri?.event?.listen && tauri?.core?.invoke);

/** The binding to name while idle. */
let hotkey = DEFAULT_HOTKEY;
/** Set once the shell pushed a settings change: the boot fetch is older then. */
let settingsPushed = false;

/** Every shell event this page listens to; registered once, released with the page. */
const unlisteners = [];
let recorder = null;
/** Last state the recorder reported, used to spot a start that was refused. */
let recorderState = 'idle';
/** Resolves once a take that is winding down has released the microphone. */
let released = Promise.resolve();
let releaseDone = null;
let capTimer = null;

/** The dictation this page is showing, finished or not. */
let session = null;
/** The shell's capture whose frames feed the recorder; null while none runs. */
let capture = null;
/** Newest generation whose toggle has been applied; nothing older is repeated. */
let handledGeneration = -1;
/** A toggle that arrived while another was still being applied. */
let queuedGeneration = null;
let draining = false;

/* -------------------------------------------------------------------------- */
/* Boot                                                                        */
/* -------------------------------------------------------------------------- */

/**
 * The launcher settings name one thing on this page: the binding written into
 * the idle line. The mode of a take comes from the shell with the dictation, so
 * a settings change can never reach a recording that has already started.
 */
void (async () => {
  if (!shell) return;
  try {
    const response = await fetch(SETTINGS_URL, { cache: 'no-store', signal: AbortSignal.timeout(2000) });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const settings = await response.json();
    if (!settingsPushed && typeof settings?.hotkey === 'string') hotkey = settings.hotkey;
  } catch {
    // The idle line keeps the default binding; nothing else reads the response.
    return;
  }
  // The binding is known only now, and only the idle line names it: every other
  // state belongs to a dictation, and that state is left alone.
  if (overlay.dataset.state === 'idle') render('idle');
})();

/**
 * The launcher settings were saved: the idle line names the binding they chose.
 * Only that line follows them, and only while no dictation is on screen — the
 * mode and session of a take are the shell's answer to `dictation_context`,
 * pinned to the generation, and this event never touches them.
 */
function onSettingsChanged(event) {
  settingsPushed = true;
  const named = event?.payload?.hotkey;
  if (typeof named === 'string' && named.trim()) hotkey = named.trim();
  if (overlay.dataset.state === 'idle') render('idle');
}

async function boot() {
  if (!shell) {
    render('preview');
    return;
  }
  try {
    // The listeners come first: `overlay_ready` is what hands over a dictation
    // the shell queued while this page was still loading, and that event must
    // find a listener already waiting. The frame listener has to be in place
    // before the shell is told the page is ready, or the first frames of that
    // dictation would be spoken to nobody.
    unlisteners.push(await tauri.event.listen(EVENT_TOGGLE, onToggle));
    unlisteners.push(await tauri.event.listen(EVENT_CANCEL, onCancel));
    unlisteners.push(await tauri.event.listen(EVENT_PCM_FRAME, onPcmFrame));
    unlisteners.push(await tauri.event.listen(EVENT_MIC_ERROR, onMicError));
    unlisteners.push(await tauri.event.listen(EVENT_MUTE_WARNING, onMuteWarning));
    unlisteners.push(await tauri.event.listen(EVENT_SETTINGS_CHANGED, onSettingsChanged));
  } catch (error) {
    shell = false;
    console.error('speechek overlay: the shell did not accept an event listener', error);
    render('preview');
    return;
  }
  try {
    await tauri.core.invoke('overlay_ready');
  } catch (error) {
    console.error('speechek overlay: overlay_ready failed', error);
  }
  // `overlay_ready` is where the shell hands over a dictation that was waiting
  // for this listener, so its arming state must not be painted over here.
  if (!isLive(session)) render('idle');
}

/* -------------------------------------------------------------------------- */
/* Toggle intake                                                               */
/* -------------------------------------------------------------------------- */

function generationOf(event) {
  const payload = event && typeof event === 'object' && 'payload' in event ? event.payload : event;
  return typeof payload === 'number' && Number.isSafeInteger(payload) && payload > 0 ? payload : null;
}

/**
 * One dictation can produce two events with the same generation: the arm, and
 * the stop the shell queued while the recorder was still starting. Anything
 * older than the newest generation is a late event from a finished dictation.
 */
function onToggle(event) {
  const generation = generationOf(event);
  if (generation === null || generation < handledGeneration) return;
  queuedGeneration = generation;
  // Events are applied one at a time: a stop that arrives while the start is
  // still awaiting the microphone waits here instead of re-entering it.
  if (!draining) void drain();
}

async function drain() {
  draining = true;
  try {
    while (queuedGeneration !== null) {
      const generation = queuedGeneration;
      queuedGeneration = null;
      await applyToggle(generation);
    }
  } finally {
    draining = false;
  }
}

async function applyToggle(generation) {
  const active = session;
  if (isLive(active)) {
    // The same generation again means the same dictation, and the shell only
    // repeats it to stop it.
    await endSession(active);
    return;
  }
  if (active && !active.finished) return; // a newer dictation cannot start over one that runs
  if (generation <= handledGeneration) return; // a repeat of something already handled
  await beginSession(generation);
}

/**
 * The shell cancelled the dictation: Escape was pressed while it ran, so the
 * pill is already hidden and nothing of the take may be kept. The generation is
 * marked handled here as well, which is what keeps a toggle of that dictation
 * that is still queued — or that arrives after this event — from starting it.
 */
function onCancel(event) {
  const generation = generationOf(event);
  if (generation === null || generation < handledGeneration) return;
  const active = session;
  // A dictation newer than the cancelled one owns this page; its arrival is not
  // this event's business.
  if (isLive(active) && active.generation > generation) return;
  if (queuedGeneration !== null && queuedGeneration <= generation) queuedGeneration = null;
  handledGeneration = Math.max(handledGeneration, generation);
  if (isLive(active) && active.generation === generation) cancelSession(active);
}

/* -------------------------------------------------------------------------- */
/* One dictation                                                               */
/* -------------------------------------------------------------------------- */

/** True while `active` is the dictation this page is showing and still runs. */
function isLive(active) {
  return Boolean(active) && active === session && !active.finished;
}

async function beginSession(generation) {
  handledGeneration = generation;
  const active = {
    generation,
    mode: '',
    sessionId: null,
    phase: 'starting',
    sampled: false,
    settled: false,
    finished: false,
    error: '',
    // A non-fatal line of this take, e.g. the system mute that did not work.
    warning: '',
  };
  session = active;

  stopMeter();
  stopTimer();
  elapsedLine.textContent = '00:00';
  render('arming');

  if (!shell) {
    // Only reachable if a preview page is fed a synthetic event: no shell, no
    // recording, and the informative idle state stays as it is.
    render('preview');
    return;
  }

  // The shell names this take: the mode it records in, and the session whose
  // settings it pinned. Both arrive before the microphone is opened, and a
  // dictation cancelled while the answer was in flight never starts one.
  const context = await callShell('dictation_context', { generation });
  if (!isLive(active)) return;
  if (context === undefined) {
    finishError(active, 'Не удалось получить настройки диктовки', 'Запись не началась');
    return;
  }
  if (!MODES.has(context?.mode) || !Number.isSafeInteger(context.sessionId) || context.sessionId <= 0) {
    finishError(active, 'Оболочка назвала неизвестный режим диктовки', 'Запись не началась');
    return;
  }
  active.mode = context.mode;
  active.sessionId = context.sessionId;
  // The mode is known only now, so the arming line is repainted with it.
  render('arming');

  // A take that is still winding down owns the microphone; the shell is not
  // told this one is over before that, but a stop it decided on its own can be.
  await waitReleased();
  if (!isLive(active)) return;

  const before = recorderState;
  try {
    // The desktop microphone belongs to the shell, so the recorder must not
    // open one of its own: `external` makes `acceptPcmChunk` the only way audio
    // reaches it, and no capture of this page ever touches a device. The session
    // tag keeps every request of this take on the snapshot the shell pinned.
    await ensureRecorder().start({ modes: [active.mode], external: true, sessionId: active.sessionId });
  } catch (cause) {
    if (!isLive(active)) return;
    // The capture never opened: nothing was recorded, so nothing is inserted.
    finishError(active, looksDenied(cause) ? MIC_DENIED : `Запись не началась: ${messageOf(cause)}`, 'Запись не началась');
    return;
  }
  if (!isLive(active)) return;
  if (recorderState === before) {
    // `start` does nothing while the recorder still holds a previous take, so
    // this dictation never began and must not pretend it did.
    finishError(active, 'Предыдущая диктовка ещё завершается', 'Запись не началась');
    return;
  }

  // The shell's microphone runs for this generation and no other, and it is
  // started before the shell is told the capture began: the stop it may answer
  // with then always has a capture to close. The answer names the device that
  // really captured, so a take that fell back to the system microphone says so
  // instead of pretending it recorded from the chosen one.
  capture = { generation, seen: 0, missing: false, waiting: null };
  const started = await startCapture(generation);
  if (started.capture === undefined) {
    capture = null;
    // The shell could not open a microphone at all, and said why. A page cannot
    // fix that: Windows' privacy settings are named only when the reason really
    // is a refusal of access.
    await abortSession(active, started.reason || MIC_FAILED, captureFix(started.reason));
    return;
  }
  if (!isLive(active)) {
    // A stop that ended the dictation while the shell was still arming it left
    // frames in flight; they belong to nothing now, so the capture is closed.
    void closeNative();
    return;
  }
  const chosen = started.capture.fallbackDevice;
  if (typeof chosen === 'string' && chosen.trim()) {
    // The chosen device was not there, so the system one recorded this take.
    // The first frame may already have painted the pill, so the line is put up
    // on whatever live state it is in.
    active.warning = fallbackLine(chosen.trim());
    const painted = overlay.dataset.state;
    if (painted === 'arming' || painted === 'recording' || painted === 'transcribing') {
      renderProgress(active, painted);
    }
  }

  // The shell moves to Recording here, and this is what delivers a stop the
  // user pressed while the recorder was still starting.
  await invoke('capture_started', { generation });
  if (!isLive(active)) return;
  active.phase = 'live';
  capTimer = setTimeout(() => { void endSession(active); }, CAP_MS);
}

async function endSession(active) {
  if (!isLive(active) || active.phase !== 'live') return;
  clearTimeout(capTimer);
  capTimer = null;

  active.phase = 'transcribing';
  stopMeter();
  stopTimer();
  renderProgress(active, 'transcribing');

  await invoke('capture_finalizing', { generation: active.generation });
  // The shell answers with the last frame it emitted, so the recorder waits for
  // every frame it had queued before the take is closed behind it.
  const whole = await closeNative();
  if (!isLive(active)) return;
  let report = null;
  let failure = '';
  try {
    report = await ensureRecorder().stop();
  } catch (cause) {
    failure = messageOf(cause);
  }
  if (!isLive(active)) return;
  if (!whole) {
    // Frames the shell had already queued never arrived: the take is missing the
    // words that ended it, so nothing is inserted.
    finishError(active, TAIL_LOST);
    return;
  }
  settle(active, report, failure);
}

/**
 * Drops the take of a cancelled dictation and leaves nothing behind: no timer,
 * no bars, no capture, no recorder, and no shell call that could still insert.
 *
 * The recorder is disposed rather than stopped — `stop()` is what transcribes
 * what was recorded, and a cancelled take must not be transcribed, uploaded or
 * inserted. Disposing also aborts the requests it had in flight and closes its
 * Live socket, so a cancelled dictation has no live connection left; the next
 * dictation builds a fresh recorder with an empty buffer.
 *
 * Only the pill's last paint stays in the page: the shell hid it before this
 * event was sent, and the next dictation repaints it.
 */
function cancelSession(active) {
  if (!active || active.finished) return;
  active.finished = true;
  clearTimeout(capTimer);
  capTimer = null;
  stopMeter();
  stopTimer();
  // The native capture is already closed by the shell; the slot goes with the
  // dictation so that a frame still in flight belongs to nothing.
  capture = null;
  const dropping = recorder;
  recorder = null;
  recorderState = 'idle';
  releaseDone?.();
  releaseDone = null;
  released = Promise.resolve();
  dropping?.dispose();
}

/**
 * Ends a dictation that cannot go on. The shell's capture is released first, so
 * a failure never leaves the microphone running, and the message that caused it
 * is the one left on screen.
 */
async function abortSession(active, message, detail) {
  clearTimeout(capTimer);
  capTimer = null;
  if (!isLive(active) || active.phase === 'transcribing') return;
  active.phase = 'transcribing';
  // The message goes up before the teardown: releasing the capture takes a
  // moment, and a failure is not something to keep the user waiting for.
  active.error = message;
  stopMeter();
  stopTimer();
  renderProgress(active, 'transcribing');
  await closeNative();
  if (!isLive(active)) return;
  try {
    await ensureRecorder().stop();
  } catch {
    // The failure already on screen is the one worth keeping.
  }
  if (isLive(active)) finishError(active, message, detail);
}

function settle(active, report, failure) {
  if (!isLive(active) || active.settled) return;
  active.settled = true;
  const results = report && typeof report === 'object' ? report.results : null;
  const errors = report && typeof report === 'object' ? report.errors : null;
  // Only the configured mode is a result: a dictation carries exactly one mode.
  const text = typeof results?.[active.mode] === 'string' ? results[active.mode].trim() : '';
  if (text) {
    void insertText(active, text);
    return;
  }
  finishError(active, reportedError(errors, active) || failure || active.error || NO_TEXT);
}

function reportedError(errors, active) {
  for (const name of [active.mode, '']) {
    const message = errors?.[name];
    if (typeof message === 'string' && message.trim()) return message.trim();
  }
  return active.error.trim();
}

async function insertText(active, text) {
  let outcome;
  try {
    outcome = await tauri.core.invoke('insert_text', { generation: active.generation, text });
  } catch (cause) {
    if (isLive(active)) finishError(active, messageOf(cause) || 'Текст не удалось передать в активное окно', 'Текст не вставлен');
    return;
  }
  if (!isLive(active)) return;
  // The shell types the text, or leaves it on the clipboard for one manual paste.
  if (outcome === 'copied_fallback') finishCopied(active);
  else if (outcome === 'inserted') finishSuccess(active);
  else finishError(active, 'Неизвестный результат вставки', 'Текст не вставлен');
}

/** Common end of a dictation: no more timer, no more bars, one last state. */
function finish(active) {
  if (!isLive(active) || active.finished) return false;
  active.finished = true;
  clearTimeout(capTimer);
  capTimer = null;
  stopMeter();
  stopTimer();
  return true;
}

/**
 * The text was inserted. The pill is already hidden and stays that way: the
 * shell only needs to know that this dictation ended well.
 */
function finishSuccess(active) {
  if (!finish(active)) return;
  void invoke('finish_session', { generation: active.generation, outcome: 'success' });
}

/**
 * The text is on the clipboard for one manual paste. That is a visible ending,
 * so the shell brings the pill back with this line for a few seconds.
 */
function finishCopied(active) {
  if (!finish(active)) return;
  render('copied');
  void invoke('finish_session', { generation: active.generation, outcome: 'error' });
}

/**
 * The dictation failed: the reason replaces the processing status, and the
 * shell keeps it visible briefly (or re-shows it after an insertion failure).
 */
function finishError(active, message, detail = 'Текст не вставлен') {
  if (!finish(active)) return;
  render('error', { message, detail });
  void invoke('finish_session', { generation: active.generation, outcome: 'error' });
}

/* -------------------------------------------------------------------------- */
/* Recorder                                                                    */
/* -------------------------------------------------------------------------- */

/** Follows the recorder's state: a take is over the moment it reports idle. */
function trackRelease(next) {
  recorderState = next;
  if (next === 'idle') {
    releaseDone?.();
    releaseDone = null;
    return;
  }
  if (!releaseDone) released = new Promise((resolve) => { releaseDone = resolve; });
}

function waitReleased() {
  return released;
}

/**
 * The recorder of this page: one microphone, one session at a time. A
 * cancelled dictation takes its recorder with it, so the session after a cancel
 * builds a fresh one with an empty buffer.
 */
function ensureRecorder() {
  if (!recorder) {
    recorder = createRecorder({
      onState(next) {
        trackRelease(next);
      },
      onLevel(level) {
        // Only a level: the transition out of arming belongs to the first frame
        // the shell delivers, so bars that never move cannot end it either.
        if (!isLive(session)) return;
        aim(level);
      },
      // Provisional and confirmed text exist only as transcription: this pill
      // never draws a transcript, and the result of a dictation is the
      // recorder's own report at the end, so neither line has anything to show.
      onInterim() {},
      onFinal() {},
      onError(name, message) {
        const active = session;
        if (!isLive(active) || typeof message !== 'string' || !message) return;
        // An error never becomes an insertion, and a take that failed has no
        // text left to wait for: the capture behind it is closed here.
        active.error = message;
        render('error', { message });
        void abortSession(active, message);
      },
    });
  }
  return recorder;
}

/* -------------------------------------------------------------------------- */
/* Native capture                                                              */
/* -------------------------------------------------------------------------- */

/**
 * The shell sends 16 kHz mono PCM16 as base64, little-endian, and WebView2 only
 * runs on x86-64: the bytes are already in the order the samples are read in.
 */
function decodePcm(data) {
  if (typeof data !== 'string' || data.length === 0) return null;
  let binary;
  try {
    binary = atob(data);
  } catch {
    return null;
  }
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return new Int16Array(bytes.buffer, 0, bytes.length >> 1);
}

/**
 * One frame of the shell's capture. Only a frame of the generation that runs
 * right now, with a sequence newer than every frame already seen for it, reaches
 * the recorder: a replayed or late event is never spoken twice.
 */
function onPcmFrame(event) {
  const running = capture;
  const payload = event?.payload;
  if (!running || !payload || payload.generation !== running.generation) return;
  const sequence = payload.sequence;
  if (!Number.isSafeInteger(sequence) || sequence <= running.seen) return;
  if (sequence !== running.seen + 1) {
    running.missing = true;
    return;
  }
  const samples = decodePcm(payload.data);
  if (!samples) {
    running.missing = true;
    return;
  }

  running.seen = sequence;
  const active = session;
  if (
    isLive(active) && active.generation === running.generation && !active.sampled
    && (active.phase === 'starting' || active.phase === 'live')
  ) {
    // The first frame the shell really captured is the only thing that ends the
    // arming visual: a capture that never delivers one keeps waiting.
    active.sampled = true;
    startMeter();
    startTimer();
    renderProgress(active, 'recording');
  }
  ensureRecorder().acceptPcmChunk(samples);
  running.waiting?.();
}

/**
 * The shell's capture stopped or could not go on. The reason the shell sent is
 * the one the user reads, and Windows' own privacy settings are named on the
 * second line only when that reason really is a refusal of access: a desktop
 * page is not the thing that asks for the microphone, and a device that was
 * unplugged is not a permission problem.
 */
function onMicError(event) {
  const running = capture;
  const payload = event?.payload;
  if (!running || !payload || payload.generation !== running.generation) return;
  const reason = typeof payload.message === 'string' ? payload.message.trim() : '';
  void abortSession(session, reason || MIC_FAILED, captureFix(reason));
}

/**
 * The shell's system mute of this take did not work out, one way or the other.
 * Nothing is dropped for it: the sentence is kept on the second line of the pill
 * while the take runs. A warning marked as the take's last — the shell cancelled
 * it and the sound it silenced was not given back — would say that into a hidden
 * pill, so it is painted on its own and the shell is asked to bring the pill
 * back for it; nothing of the take is revived for that, and a newer dictation
 * owns this page by then or the shell refuses to show anything.
 */
function onMuteWarning(event) {
  const payload = event?.payload;
  if (!payload) return;
  const message = typeof payload.message === 'string' ? payload.message.trim() : '';
  if (!message) return;
  const active = session;
  if (!active || payload.generation !== active.generation) return;
  if (payload.last === true) {
    // The take is over: the shell hid the pill, and this line is what may bring
    // it back. Its answer decides nothing here, so the call is not awaited.
    render('warning', { message, detail: '' });
    void invoke('mute_warning_shown', { generation: active.generation });
    return;
  }
  if (!isLive(active)) return;
  active.warning = message;
  if (active.phase === 'live') renderProgress(active, 'recording');
  else if (active.phase === 'transcribing') renderProgress(active, 'transcribing');
  else renderProgress(active, 'arming');
}

/**
 * Closes the shell's capture and waits for the frames it had queued when it
 * answered: the newest sequence reaching the returned count is what tells this
 * page that nothing of the take is still in flight. Answers false when those
 * frames did not arrive in time.
 */
async function closeNative() {
  const running = capture;
  if (!running) return true;
  const total = await callShell('stop_native_capture', { generation: running.generation });
  // The slot stays reachable while the tail is in flight: the frames the shell
  // had already queued still belong to this capture, and only generation and
  // sequence decide what reaches the recorder.
  let whole = Number.isSafeInteger(total) && total >= running.seen && !running.missing;
  if (whole && running.seen < total) whole = await waitTail(running, total);
  if (capture === running) capture = null;
  return whole;
}

/**
 * Waits for the frames the shell had queued when it answered, and answers false
 * when they did not arrive in time.
 */
function waitTail(running, total) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      running.waiting = null;
      resolve(false);
    }, NATIVE_TAIL_MS);
    running.waiting = () => {
      if (running.seen < total) return;
      running.waiting = null;
      clearTimeout(timer);
      resolve(true);
    };
  });
}

/* -------------------------------------------------------------------------- */
/* Meter                                                                       */
/* -------------------------------------------------------------------------- */

const barHeights = new Array(bars.length).fill(METER_MIN);
/** Drawn level of the newest sample, in 0..1. */
let aimLevel = 0;
let metering = false;

/** One step of the meter: every bar moves towards the sample, fast up and slowly down. */
function paint() {
  for (let index = 0; index < bars.length; index += 1) {
    const wanted = METER_MIN + aimLevel * METER_SHAPE[index] * (METER_MAX - METER_MIN);
    const ease = wanted > barHeights[index] ? METER_RISE : METER_FALL;
    const next = barHeights[index] * (1 - ease) + wanted * ease;
    barHeights[index] = next;
    bars[index].style.height = `${next.toFixed(1)}px`;
  }
}


function startMeter() {
  if (metering) return;
  metering = true;
  aimLevel = 0;
}

/** Rest: every bar sits at its lowest height and the loop is gone. */
function stopMeter() {
  metering = false;
  aimLevel = 0;
  for (let index = 0; index < bars.length; index += 1) {
    barHeights[index] = METER_MIN;
    bars[index].style.height = `${METER_MIN}px`;
  }
}

/**
 * Map the recorder's peak for drawing only. The former 0.012 gate kept every
 * peak below that threshold at rest; the smaller visual floor lets quiet
 * input move the bars without changing recorded samples.
 */
function aim(level) {
  const peak = Math.max(0, Math.min(1, Number(level) || 0));
  const mapped = Math.log1p(300 * Math.max(0, peak - 0.003)) / Math.log1p(35);
  aimLevel = Math.min(1, mapped);
  if (metering) paint();
}

/* -------------------------------------------------------------------------- */
/* Timer                                                                       */
/* -------------------------------------------------------------------------- */

let timerId = null;
let startedAt = 0;

function startTimer() {
  if (timerId) return;
  startedAt = performance.now();
  showElapsed();
  timerId = setInterval(showElapsed, TIMER_TICK_MS);
}

function stopTimer() {
  if (!timerId) return;
  clearInterval(timerId);
  timerId = null;
}

function showElapsed() {
  const total = Math.floor((performance.now() - startedAt) / 1000);
  const minutes = Math.floor(total / 60);
  elapsedLine.textContent = `${String(minutes).padStart(2, '0')}:${String(total % 60).padStart(2, '0')}`;
}

/* -------------------------------------------------------------------------- */
/* Copy and rendering                                                          */
/* -------------------------------------------------------------------------- */

const COPY = {
  preview: 'Предпросмотр без оболочки',
  arming: 'Слушаю микрофон…',
  recording: 'Идёт запись',
  transcribing: 'Идёт обработка',
  copied: COPIED_FALLBACK,
};

function message(state) {
  if (state === 'idle') return `${hotkey} — диктовка`;
  return COPY[state] ?? COPY.arming;
}

/** The second line: the mode that records, or what ended the dictation. */
function detail(state) {
  if (state === 'error') return 'Текст не вставлен';
  if (state === 'copied') return 'Текст в буфере обмена';
  // The mode belongs to the take: the shell names it with each dictation, and
  // no settings response is drawn here.
  return MODE_LABELS[session?.mode] ?? MODE_LABELS.live;
}

/**
 * A warning of the running take, e.g. the system mute the shell could not work
 * with. It is a line of the take and is shown while the take runs; the states
 * that already say what happened are not overwritten by it.
 */
function warningLine(state) {
  if (state !== 'arming' && state !== 'recording' && state !== 'transcribing') return null;
  const active = session;
  return isLive(active) && active.warning ? active.warning : null;
}

function render(state, options = {}) {
  // Only a real change is written: repeated renders of one state are not
  // transitions, and nothing else should have to filter them out.
  if (overlay.dataset.state !== state) overlay.dataset.state = state;
  statusLine.textContent = options.message ?? message(state);
  modeLine.textContent = options.detail ?? warningLine(state) ?? detail(state);
}

/** A running capture keeps running, but an error already reported stays visible. */
function renderProgress(active, state) {
  if (active.error) {
    render('error', { message: active.error });
    return;
  }
  render(state);
}

/* -------------------------------------------------------------------------- */
/* Shell calls                                                                 */
/* -------------------------------------------------------------------------- */

function invoke(command, args) {
  if (!shell) return Promise.resolve(null);
  return tauri.core.invoke(command, args).catch((error) => {
    console.error(`speechek overlay: ${command} failed`, error);
    return null;
  });
}

/**
 * Calls a shell command whose answer is a value. Unlike `invoke`, a failure is
 * not flattened into `null`: the caller learns that there was no answer at all.
 */
async function callShell(command, args) {
  if (!shell) return undefined;
  try {
    return await tauri.core.invoke(command, args);
  } catch (error) {
    console.error(`speechek overlay: ${command} failed`, error);
    return undefined;
  }
}

function messageOf(cause) {
  return cause instanceof Error ? cause.message : String(cause ?? '');
}

/** A microphone the system refused looks like this, whatever the browser calls it. */
function looksDenied(cause) {
  const name = cause?.name ?? '';
  return name === 'NotAllowedError' || name === 'SecurityError' || /not.?allowed|denied|permission|доступ к микрофону запрещ|микрофон.*запрещ/i.test(messageOf(cause));
}

/** What the user can do about a capture failure: it is only a refusal when the
 * shell's own reason really says so. */
function captureFix(reason) {
  return looksDenied(reason) ? MIC_DENIED : MIC_FAILED;
}

/**
 * Starts the shell's capture of one dictation and answers what it recorded with.
 *
 * Unlike a plain `callShell`, the reason the shell refused the command survives:
 * a capture that never started must be reported with what really went wrong,
 * not with a guess about Windows' privacy settings. An answer of `undefined`
 * is that refusal; `reason` is what the shell said about it, possibly empty.
 */
async function startCapture(generation) {
  if (!shell) return { capture: undefined, reason: '' };
  try {
    return { capture: await tauri.core.invoke('start_native_capture', { generation }), reason: '' };
  } catch (error) {
    console.error('speechek overlay: start_native_capture failed', error);
    return { capture: undefined, reason: messageOf(error).trim() };
  }
}

/**
 * The shell's capsule for a take that had to record from the system microphone
 * because the chosen one was not there. It names the device that recorded for
 * this generation only: the settings keep the device the user chose, and the
 * next dictation asks for it again.
 */
function fallbackLine(device) {
  return `Выбранный микрофон недоступен; запись с системного: ${device}`;
}

/* -------------------------------------------------------------------------- */
/* Lifecycle                                                                   */
/* -------------------------------------------------------------------------- */

// Leaving the page takes the capture with it: the shell's microphone is closed
// best effort, the recorder releases its Live socket and buffers, and every
// listener goes. Nothing else is reported, because the whole window this page
// belongs to is going away with it.
window.addEventListener('pagehide', () => {
  clearTimeout(capTimer);
  stopMeter();
  stopTimer();
  void closeNative();
  recorder?.dispose();
  for (const off of unlisteners) off?.();
});

void boot();
