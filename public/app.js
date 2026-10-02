import { createRecorder } from './recorder.js';

/** The laboratory always runs all three modes on one take; nothing selects a subset. */
const MODES = ['live', 'smart', 'verbatim'];
const BATCH_MODES = ['smart', 'verbatim'];

/** The Tauri global the shell injects; absent when the page runs in a browser. */
const tauri = globalThis.__TAURI__;
/** True only when the page really can reach the shell, so the native path is used. */
const shell = Boolean(tauri?.core?.invoke && tauri?.event?.listen);
/** True inside any Speechek window, whether or not the shell answers this page. */
const embedded = Boolean(tauri);

/** The shell's native capture: one audio frame, its failure, and its window closing. */
const EVENT_PCM_FRAME = 'speechek:pcm-frame';
const EVENT_MIC_ERROR = 'speechek:mic-error';
const EVENT_LAB_CLOSED = 'speechek:lab-closed';

/** Ten minutes, the cap the lab always applied; an external take owns its own time. */
const LAB_CAP_MS = 600000;
/** Grace for the frames the shell had queued when it answered the stop. */
const LAB_TAIL_MS = 1500;

const MIC_FAILED = 'Запись не началась';
const TAIL_LOST = 'Потеряны кадры записи — текст не расшифрован';
const LAB_CLOSED = 'Окно лаборатории закрыто';

const $ = (id) => document.getElementById(id);
const button = $('record-button');
const label = $('record-label');
const status = $('status');
const error = $('error');
const warning = $('warning');
const browserNote = $('browser-note');
const timer = $('timer');
const panels = {
  live: [$('live-text'), $('live-meta')],
  smart: [$('smart-text'), $('smart-meta')],
  verbatim: [$('verbatim-text'), $('verbatim-meta')],
};

let sessionStarted = false;
let recording = false;
let halting = false;
let tick = null;
let tickFrom = 0;
/** Confirmed text per mode, for a take the recorder stopped on its own. */
const confirmed = new Map();
/** Reported failures per mode ('', for a session error). */
const failures = new Map();
/** When each batch mode's request started, for the footer timing. */
const batchBegan = new Map();

/** The recorder of the take that runs now; rebuilt after a take is dropped. */
let recorder = null;
/** The shell's native laboratory take; null while no native take runs. */
let native = null;
/** Every lab event subscription, released with the page. */
const unlisten = [];
/**
 * Resolves once the lab events are subscribed. A native take awaits this before
 * it asks the shell to reserve a generation, so no frame of it can be missed.
 */
const listening = shell
  ? Promise.all([
    [EVENT_PCM_FRAME, onPcmFrame],
    [EVENT_MIC_ERROR, onMicError],
    [EVENT_LAB_CLOSED, onLabClosed],
  ].map(([name, handler]) => tauri.event.listen(name, handler)
    .then((off) => { if (typeof off === 'function') unlisten.push(off); })
    .catch(() => { /* the page still works, without the shell's frames */ })))
  : Promise.resolve();

// The device chosen in Speechek's settings belongs to the shell's capture. A page
// in a plain browser has no shell and always records through the browser's own
// microphone, so it says that instead of pretending the choice applies.
if (browserNote && !embedded) browserNote.hidden = false;

function setResult(name, text, meta, empty = false) {
  const [body, footer] = panels[name];
  body.textContent = text;
  body.classList.toggle('empty', empty);
  footer.textContent = meta;
}
function showError(message) { error.textContent = message; error.hidden = false; }
function clearError() { error.hidden = true; error.textContent = ''; }
function setStatus(message) { status.textContent = message; }
function showWarning(message) {
  if (!warning) return;
  warning.textContent = message;
  warning.hidden = false;
}
function clearWarning() {
  if (!warning) return;
  warning.textContent = '';
  warning.hidden = true;
}
function timeText(seconds) {
  return `${String(Math.floor(seconds / 60)).padStart(2, '0')}:${String(seconds % 60).padStart(2, '0')}`;
}
function stopTick() { clearInterval(tick); tick = null; }

/** Status line after a take, from the recorder's report or from the live callbacks. */
function summarize(report) {
  if (!sessionStarted) { setStatus('Запись отменена'); return; }
  const text = (name) => (report ? report.results[name] : confirmed.get(name));
  const failure = (name) => (report ? report.errors[name] : failures.get(name));
  const live = text('live');
  if (live) setResult('live', live, 'Готово');
  const succeeded = MODES.filter((name) => text(name));
  if (succeeded.length === MODES.length) { setStatus('Расшифровка готова'); return; }
  if (failure('')) { setStatus('Не удалось получить расшифровку'); return; }
  if (succeeded.length) { setStatus('Часть результатов недоступна — проверьте ошибки ниже'); return; }
  setStatus('Не удалось получить расшифровку');
}

/* -------------------------------------------------------------------------- */
/* Recorder                                                                    */
/* -------------------------------------------------------------------------- */

/**
 * Builds the recorder of the next take. One take at a time: a dropped take (a
 * failed or closed native capture) takes its recorder with it, so the take after
 * it starts from an empty buffer instead of inheriting a damaged one.
 */
function ensureRecorder() {
  if (!recorder) {
    recorder = createRecorder({
      onState(next) {
        if (next === 'arming') {
          confirmed.clear();
          failures.clear();
          batchBegan.clear();
          clearWarning();
          button.disabled = true;
          label.textContent = 'Начать запись';
          clearError();
          timer.textContent = '00:00';
          setStatus(shell ? 'Открываем микрофон' : 'Запрашиваем доступ к микрофону');
          for (const name of MODES) setResult(name, 'Ожидание записи…', 'Ожидание', true);
          return;
        }
        if (next === 'recording') {
          recording = true;
          sessionStarted = true;
          button.disabled = false;
          button.classList.add('recording');
          label.textContent = 'Остановить';
          tickFrom = Date.now();
          stopTick();
          tick = setInterval(() => { timer.textContent = timeText(Math.floor((Date.now() - tickFrom) / 1000)); }, 250);
          setStatus('Идёт запись и потоковая расшифровка');
          return;
        }
        if (next === 'stopping') {
          recording = false;
          stopTick();
          button.disabled = true;
          label.textContent = 'Обработка…';
          setStatus('Завершаем потоковую расшифровку');
          for (const name of BATCH_MODES) {
            batchBegan.set(name, performance.now());
            setResult(name, 'Обработка записи…', 'Запрос к gemini-3.5-transcribe', true);
          }
          return;
        }
        if (next === 'idle') {
          recording = false;
          stopTick();
          button.disabled = false;
          button.classList.remove('recording');
          label.textContent = sessionStarted ? 'Новая запись' : 'Начать запись';
          // A stop the page did not ask for (the ten-minute cap) still needs a status.
          if (!halting) summarize();
        }
      },
      onLevel() { /* level meter is not part of this layout */ },
      onInterim(name, text) {
        setResult(name, text, 'Предварительный текст');
      },
      onFinal(name, text) {
        confirmed.set(name, text);
        failures.delete(name);
        const began = batchBegan.get(name);
        setResult(name, text, name === 'live' ? 'Подтверждённый текст' : began === undefined ? 'Готово' : `Готово · ${((performance.now() - began) / 1000).toFixed(1)} с`);
      },
      onError(name, message) {
        if (!confirmed.has(name)) failures.set(name, message);
        if (panels[name]) setResult(name, message, 'Ошибка — результат не сравнивается');
        showError(message);
      },
    });
  }
  return recorder;
}

/** Drops the take and its recorder; the next take builds a fresh one. */
function dropRecorder() {
  const dying = recorder;
  recorder = null;
  dying?.dispose();
}

/* -------------------------------------------------------------------------- */
/* Native capture                                                              */
/* -------------------------------------------------------------------------- */

/** Calls a shell command and answers undefined instead of throwing. */
async function callShell(command, args) {
  try {
    return await tauri.core.invoke(command, args);
  } catch (cause) {
    console.error(`speechek lab: ${command} failed`, cause);
    return undefined;
  }
}

function messageOf(cause) {
  return cause instanceof Error ? cause.message : String(cause ?? '');
}

/** Best-effort release of the shell's reservation; the answer decides nothing. */
function stopNative(take) {
  if (!take || take.generation === null) return Promise.resolve();
  return callShell('lab_stop_capture', { generation: take.generation });
}

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
 * right now, numbered right after the last one seen, reaches the recorder: a
 * replayed or late frame is never spoken twice, and a skipped or unreadable one
 * marks the take as damaged so its tail is never transcribed as if it were whole.
 */
function onPcmFrame(event) {
  const take = native;
  const payload = event?.payload;
  if (!take || !payload || payload.generation !== take.generation) return;
  const sequence = payload.sequence;
  if (!Number.isSafeInteger(sequence) || sequence <= take.received) return;
  if (sequence !== take.received + 1) { take.damaged = true; return; }
  const samples = decodePcm(payload.data);
  if (!samples) { take.damaged = true; return; }
  take.received = sequence;
  recorder?.acceptPcmChunk(samples);
  take.waiting?.();
}

/**
 * The shell's native capture failed. The reason the shell sent is the one the
 * user reads, and the take ends without transcription: the live text that
 * arrived before the failure is not a result of it either.
 */
function onMicError(event) {
  const take = native;
  const payload = event?.payload;
  if (!take || !payload || payload.generation !== take.generation) return;
  const reason = typeof payload.message === 'string' ? payload.message.trim() : '';
  dropNativeTake(take, reason || MIC_FAILED);
}

/**
 * The laboratory window is going away. The shell stops its capture with the
 * window, so nothing is asked of it here and no part of the take is transcribed.
 */
function onLabClosed() {
  const take = native;
  if (!take) return;
  dropNativeTake(take, LAB_CLOSED);
}

/**
 * Waits for the frames the shell had queued when it answered the stop. Answers
 * false when they did not arrive in time or the take was dropped meanwhile.
 */
function waitLabTail(take, total) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      take.waiting = null;
      resolve(false);
    }, LAB_TAIL_MS);
    take.waiting = () => {
      if (!take.dropped && take.received < total) return;
      take.waiting = null;
      clearTimeout(timer);
      resolve(!take.dropped && take.received >= total);
    };
  });
}

/**
 * Ends a native take that cannot be transcribed: the shell's capture is released
 * first (best effort), then the recorder is disposed, so nothing of the take can
 * reach the server, and the message that caused it is the one left on screen.
 * Only the first cause is reported; a later one describes the same take.
 */
function dropNativeTake(take, message) {
  // Only the take that is still current may touch the recorder and the screen: a
  // late answer of a settled take belongs to a take nobody is showing.
  if (take.dropped || native !== take) return;
  take.dropped = true;
  clearTimeout(take.capTimer);
  take.capTimer = null;
  native = null;
  take.waiting?.();
  if (!take.stopping) {
    take.stopping = true;
    void stopNative(take);
  }
  // Disposing reports the recorder's own idle state; that summary would claim a
  // result this take does not have, so it is suppressed for the one call.
  const narrating = halting;
  halting = true;
  try {
    dropRecorder();
  } finally {
    halting = narrating;
  }
  confirmed.clear();
  failures.clear();
  batchBegan.clear();
  clearWarning();
  for (const name of MODES) setResult(name, 'Текст не расшифрован', 'Ошибка — результат не сравнивается', true);
  showError(message);
  setStatus('Расшифровка не начата');
}

/**
 * Starts the shell's native laboratory take. The events are subscribed before
 * the shell reserves a generation, the recorder is armed externally and awaited
 * before the shell may open a microphone, and only then is the generation the
 * frames belong to fixed.
 */
async function beginNative() {
  if (native) return;
  const take = { generation: null, received: 0, damaged: false, stopping: false, dropped: false, waiting: null, capTimer: null };
  native = take;
  try {
    await listening;
    if (take.dropped || take.stopping || native !== take) return;
    let prepared;
    try {
      prepared = await tauri.core.invoke('lab_prepare_capture');
    } catch (cause) {
      if (!take.dropped) dropNativeTake(take, messageOf(cause).trim() || 'Лаборатория сейчас занята другой записью.');
      return;
    }
    if (take.dropped || native !== take || take.stopping) {
      // Stop can finish before prepare answers. Release the late reservation;
      // it must not block the next take or open a microphone behind this page.
      if (Number.isSafeInteger(prepared) && prepared > 0) void stopNative({ generation: prepared });
      return;
    }
    if (!Number.isSafeInteger(prepared) || prepared <= 0) {
      dropNativeTake(take, 'Лаборатория сейчас занята другой записью.');
      return;
    }
    take.generation = prepared;
    // External first, and awaited: a shell that opened a device before the
    // recorder accepted the frames of it would drop the beginning of the take.
    await ensureRecorder().start({ modes: MODES, external: true });
    if (take.dropped || take.stopping || native !== take) return;
    setStatus('Открываем микрофон устройства');
    let answer;
    try {
      answer = await tauri.core.invoke('lab_start_capture', { generation: take.generation });
    } catch (cause) {
      if (!take.dropped && !take.stopping && native === take) dropNativeTake(take, messageOf(cause).trim() || MIC_FAILED);
      return;
    }
    if (take.dropped || take.stopping || native !== take) return;
    if (typeof answer?.fallbackDevice === 'string' && answer.fallbackDevice) {
      showWarning(fallbackLine(answer.fallbackDevice));
    }
    // The recorder announced the recording before the device was open; the take
    // is only really running once the shell answers.
    setStatus('Идёт запись и потоковая расшифровка');
    // An external take has no recorder timer; the laboratory keeps its ten minutes.
    take.capTimer = setTimeout(() => { void haltNative(); }, LAB_CAP_MS);
  } catch (cause) {
    if (!take.dropped && !take.stopping && native === take) dropNativeTake(take, messageOf(cause).trim() || MIC_FAILED);
  }
}

/**
 * Stops the shell's native take: the microphone is closed and its whole tail is
 * waited for, and only a complete one is handed to the recorder. A damaged or
 * missing tail drops the take without a single transcription request.
 */
async function haltNative() {
  const take = native;
  if (!take || take.stopping) return;
  take.stopping = true;
  clearTimeout(take.capTimer);
  take.capTimer = null;
  halting = true;
  button.disabled = true;
  label.textContent = 'Обработка…';
  setStatus('Завершаем запись и дожидаемся последних кадров');
  try {
    let last;
    try {
      last = await tauri.core.invoke('lab_stop_capture', { generation: take.generation });
    } catch (cause) {
      if (!take.dropped) dropNativeTake(take, messageOf(cause).trim() || TAIL_LOST);
      return;
    }
    if (take.dropped) return;
    let whole = !take.damaged && Number.isSafeInteger(last) && take.received <= last;
    if (whole && take.received < last) whole = await waitLabTail(take, last);
    if (!whole || take.damaged || take.dropped) {
      dropNativeTake(take, TAIL_LOST);
      return;
    }
    // Keep this take current while the recorder drains and uploads: closing the
    // window must still be able to dispose it and abort in-flight requests.
    const result = await ensureRecorder().stop();
    if (take.dropped || native !== take) return;
    native = null;
    clearWarning();
    summarize(result);
  } catch (cause) {
    if (!take.dropped && native === take) dropNativeTake(take, `Не удалось обработать запись: ${messageOf(cause)}`);
  } finally {
    halting = false;
  }
}

/**
 * The shell's capsule for a take that had to record from the system microphone
 * because the chosen one was not there. It names the device that recorded for
 * this take only: the settings keep the device the user chose.
 */
function fallbackLine(device) {
  return `Выбранный микрофон недоступен; запись с системного: ${device}`;
}

/* -------------------------------------------------------------------------- */
/* Takes                                                                       */
/* -------------------------------------------------------------------------- */

/** Opens the microphone the laboratory records with, from the shell or the browser. */
async function begin() {
  sessionStarted = false;
  if (shell) {
    await beginNative();
    return;
  }
  try {
    await ensureRecorder().start({ modes: MODES });
  } catch (cause) {
    showError(cause.message);
    setStatus('Запись не началась');
  }
}

/** Ends the running take and shows whatever it produced. */
async function halt() {
  if (shell && native) {
    await haltNative();
    return;
  }
  halting = true;
  try {
    summarize(await ensureRecorder().stop());
  } catch (cause) {
    showError(`Не удалось обработать запись: ${cause.message}`);
    setStatus('Ошибка обработки');
  } finally {
    halting = false;
  }
}

button.addEventListener('click', () => { if (recording) void halt(); else void begin(); });

// Leaving the page takes the shell's capture with it, best effort: the frames of
// the take have nowhere to go after this page does, so it is never transcribed.
window.addEventListener('pagehide', () => {
  const take = native;
  if (take) {
    take.dropped = true;
    clearTimeout(take.capTimer);
    take.capTimer = null;
    take.waiting?.();
    if (!take.stopping) {
      take.stopping = true;
      void stopNative(take);
    }
    native = null;
  }
  dropRecorder();
  for (const off of unlisten) off?.();
});
