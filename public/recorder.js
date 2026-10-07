/**
 * Shared capture and transcription client for the lab page and the desktop overlay.
 *
 * The module owns everything that touches the microphone or the server: 16 kHz mono
 * PCM16 capture through the `pcm-16k` worklet, the Live Smart WebSocket, batch WAV
 * uploads, the ten-minute cap and every bit of cleanup. Callers only see state,
 * level and text.
 *
 * The Gemini key never reaches this code: the shell keeps it encrypted for the
 * current Windows user and the settings window is where it is entered and
 * replaced. Nothing here persists anything to storage.
 *
 * Callbacks:
 *   onState(state)          'idle' | 'arming' | 'recording' | 'stopping'
 *   onLevel(level)          peak level 0..1, at most ~30 reports per second
 *   onInterim(mode, text)   provisional text; never a result
 *   onFinal(mode, text)     confirmed text only
 *   onError(mode, message, ui)  mode is 'live' | 'smart' | 'verbatim', or '' for
 *                           session errors (capture, cap, short recording) that
 *                           belong to no single mode. `message` is the report
 *                           rendered in the language current at that moment;
 *                           `ui` is the `{ key, args }` descriptor pages keep to
 *                           re-render the same report after a language change.
 *
 * `start`, `stop`, `acceptPcmChunk` and `dispose` are the whole surface. `stop`
 * is what produces the text; `dispose` is the opposite: it drops the take and
 * its instance, aborting a batch upload that is already in flight and closing
 * the Live socket, so nothing of that take is transcribed, uploaded or
 * reported, and a caller that wants the recording again builds a new recorder.
 *
 * `stop` resolves with `{ results, errors, errorDetails }`: `errors` maps each
 * failed mode to the message rendered when it was reported, `errorDetails` to
 * its descriptor. A failed `start` rejects with an `Error` whose `.ui` is that
 * descriptor, `.diagnosticMessage` the original raw text and `.cause` the
 * original exception, whose `name` is preserved on it.
 */

import { formatError } from './i18n.js';

const WORKLET_URL = '/pcm-worklet.js';
const LIVE_URL = '/api/live';
const TRANSCRIBE_URL = '/api/transcribe';

const LIVE_MODE = 'live';
const BATCH_MODES = ['smart', 'verbatim'];
const VALID_MODES = new Set([LIVE_MODE, ...BATCH_MODES]);
/** Every mode a session may run, in the order its results belong in. */
const MODE_ORDER = [LIVE_MODE, ...BATCH_MODES];

/** Ten minutes, the cap the lab page always applied to a microphone take. */
const CAP_MS = 600000;
/** Frames held while Live has not answered `ready` yet. */
const QUEUE_LIMIT = 100;
/** Grace period for Live `done` after `end`, as before. */
const LIVE_DRAIN_MS = 20000;
/** Grace period for a start that is still arming when `stop` arrives. */
const ARM_SETTLE_MS = 250;
/** Grace period for the worklet's final PCM chunk after `stop`. */
const WORKLET_DRAIN_MS = 500;
/** 0.1 s at 16 kHz: below this there is nothing to transcribe. */
const MIN_SAMPLES = 1600;
/** Level reports stay at ~30 fps. */
const LEVEL_INTERVAL_MS = 33;
/** Full scale for the 16-bit samples, used to normalise peaks into 0..1. */
const PCM16_PEAK = 32768;

const noop = () => {};

const emptyResult = () => ({ results: {}, errors: {}, errorDetails: {} });

/** A catalog descriptor: `{ key, args? }`, the wire shape of a UiMessage. */
function uiMsg(key, args) {
  return args === undefined ? { key } : { key, args };
}

/** Narrows a value to a descriptor without depending on i18n internals. */
function isDescriptor(value) {
  return typeof value === 'object' && value !== null && typeof value.key === 'string' && value.key.length > 0;
}

/**
 * Raw detail of any thrown value, safe to show as data: an `Error` yields its
 * original diagnostic text, never `String(object)`.
 */
function diagnosticOf(cause) {
  if (cause instanceof Error) {
    if (typeof cause.diagnosticMessage === 'string' && cause.diagnosticMessage) return cause.diagnosticMessage;
    return typeof cause.message === 'string' ? cause.message : '';
  }
  if (cause && typeof cause === 'object') {
    if (typeof cause.diagnosticMessage === 'string' && cause.diagnosticMessage) return cause.diagnosticMessage;
    return typeof cause.message === 'string' ? cause.message : '';
  }
  if (typeof cause === 'string') return cause;
  if (typeof cause === 'number' || typeof cause === 'boolean') return String(cause);
  return '';
}

/** Renders a descriptor in the current language; the raw detail is a last resort. */
function renderUi(ui, diagnostic = '') {
  return formatError({ ui, message: diagnostic }, diagnostic);
}

/**
 * An `Error` that keeps its catalog descriptor, the raw diagnostic and the
 * original failure, so no layer has to parse translated prose.
 */
function uiFailure(ui, diagnostic = '', cause = null) {
  const failure = new Error(renderUi(ui, diagnostic));
  failure.ui = ui;
  failure.diagnosticMessage = diagnostic;
  failure.cause = cause;
  return failure;
}

/** Turns a failed HTTP answer into an `Error` carrying the catalog descriptor. */
function httpFailure(result, status, fallbackDiagnostic = '') {
  const error = result?.error;
  const structured = isDescriptor(error?.ui) ? error.ui : null;
  const body = typeof error?.message === 'string' && error.message ? error.message : '';
  const diagnostic = body || fallbackDiagnostic;
  const code = typeof error?.code === 'string' && error.code ? error.code : null;
  const ui = structured ?? (code === 'MISSING_API_KEY'
    ? uiMsg('RecorderMissingApiKey')
    : uiMsg('RecorderRequestFailed', { detail: diagnostic || uiMsg('RecorderHttpStatus', { status }) }));
  const failure = uiFailure(ui, diagnostic);
  failure.code = code;
  return failure;
}

export function createRecorder(handlers = {}) {
  const onState = typeof handlers.onState === 'function' ? handlers.onState : () => {};
  const onLevel = typeof handlers.onLevel === 'function' ? handlers.onLevel : () => {};
  const onInterim = typeof handlers.onInterim === 'function' ? handlers.onInterim : () => {};
  const onFinal = typeof handlers.onFinal === 'function' ? handlers.onFinal : () => {};
  const onError = typeof handlers.onError === 'function' ? handlers.onError : () => {};

  let state = 'idle';
  let active = Promise.resolve();
  let stopping = null;
  let disposed = false;

  /**
   * Monotonic session ticket. Only the newest `start` may publish capture
   * resources, so a permission prompt answered after a later start can never
   * build a second parallel capture.
   */
  let session = 0;
  /** Resources of a start that has not published yet, released when it loses. */
  let pending = null;

  let stream = null;
  let context = null;
  let processor = null;
  let socket = null;
  let capTimer = null;
  /**
   * The session's cancellation handle. Aborting it stops a batch upload that is
   * already in flight, so a take the caller dropped cannot be transcribed by an
   * answer nobody is waiting for.
   */
  let abort = null;

  /**
   * The server session this take belongs to. The desktop shell pins the
   * settings snapshot of a dictation to this id, and every request of the take
   * names it, so a Save during a recording cannot change the mode or the key
   * behind a take that already started. The lab page starts takes without one.
   */
  let sessionId = null;

  /** Batch PCM frames; stays null unless the session needs a WAV. */
  let chunks = null;
  let samples = 0;
  let queue = [];
  let ready = false;
  let liveFailed = false;
  let liveFinal = [];
  let liveInterim = '';
  let liveFinish = null;
  let liveDone = Promise.resolve();

  /** Modes this session transcribes, canonical order; at least one. */
  let modes = [LIVE_MODE];
  let needsLive = false;
  let needsBatch = false;
  let errors = {};
  /** Descriptors behind `errors`, kept so a page can re-render after a switch. */
  let errorDetails = {};

  let levelAt = 0;
  let levelPeak = 0;

  /* ---------------------------------------------------------------------- */
  /* small helpers                                                          */
  /* ---------------------------------------------------------------------- */

  function setState(next) {
    if (disposed || state === next) return;
    state = next;
    onState(next);
  }

  function emitLevel(value) {
    if (disposed) return;
    onLevel(Math.max(0, Math.min(1, value)));
  }

  /**
   * Reports each mode at most once: `errors` keeps the text rendered at report
   * time, `errorDetails` the descriptor pages keep for a later re-render.
   */
  function reportError(name, ui, diagnostic = '') {
    if (name in errors) return;
    const message = renderUi(ui, diagnostic);
    errors[name] = message;
    errorDetails[name] = ui;
    if (!disposed) onError(name, message, ui);
  }

  function delay(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  function recordingActive() {
    return state === 'recording';
  }

  /** Adds the take's session tag to a route; an untagged take keeps it as it was. */
  function withSession(url) {
    if (sessionId === null) return url;
    return `${url}${url.includes('?') ? '&' : '?'}session=${sessionId}`;
  }

  /* ---------------------------------------------------------------------- */
  /* Live Smart WebSocket                                                   */
  /* ---------------------------------------------------------------------- */

  function bytesToBase64(bytes) {
    let binary = '';
    for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
    return btoa(binary);
  }

  function sendAudio(chunk) {
    if (socket?.readyState !== WebSocket.OPEN) return;
    socket.send(JSON.stringify({ type: 'audio', data: bytesToBase64(new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength)) }));
  }

  function sendOrQueue(chunk) {
    if (ready) {
      sendAudio(chunk);
      return;
    }
    if (queue.length < QUEUE_LIMIT) {
      queue.push(chunk);
      return;
    }
    queue = [];
    ready = false;
    liveFailed = true;
    reportError(LIVE_MODE, uiMsg('RecorderLiveServerTimeout'));
    socket?.close();
  }

  function liveEnd() {
    if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify({ type: 'end' }));
  }

  function finishLive() {
    const finish = liveFinish;
    liveFinish = null;
    liveDone = Promise.resolve();
    if (finish) finish();
  }

  function openLive() {
    ready = false;
    queue = [];
    liveFailed = false;
    liveFinal = [];
    liveInterim = '';
    liveDone = new Promise((resolve) => { liveFinish = resolve; });
    socket = new WebSocket(`${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}${withSession(LIVE_URL)}`);
    socket.onopen = () => socket.send(JSON.stringify({ type: 'start', mode: 'smart' }));
    socket.onmessage = (event) => {
      let message;
      try { message = JSON.parse(event.data); } catch { return; }
      if (message.type === 'ready') {
        ready = true;
        const pending = queue;
        queue = [];
        for (const chunk of pending) sendAudio(chunk);
        // Reached when stop() ran while the server was still arming.
        if (!recordingActive()) liveEnd();
      } else if (message.type === 'interim') {
        liveInterim = message.text || '';
        onInterim(LIVE_MODE, [liveFinal.filter(Boolean).join(' '), liveInterim].filter(Boolean).join(' '));
      } else if (message.type === 'final') {
        liveFinal.push(message.text || '');
        liveInterim = '';
        onFinal(LIVE_MODE, liveFinal.filter(Boolean).join(' '));
      } else if (message.type === 'error') {
        liveFailed = true;
        ready = false;
        queue = [];
        const structured = isDescriptor(message.ui) ? message.ui : null;
        const raw = typeof message.message === 'string' && message.message ? message.message : '';
        reportError(LIVE_MODE, structured ?? uiMsg('RecorderLiveServiceError', {
          detail: raw || uiMsg('RecorderLiveServiceUnknown'),
        }), raw);
        finishLive();
      } else if (message.type === 'done') {
        if (!liveFailed && !liveFinal.some(Boolean)) {
          reportError(LIVE_MODE, uiMsg('RecorderLiveNoText'));
        }
        finishLive();
      }
    };
    socket.onerror = () => {
      liveFailed = true;
      reportError(LIVE_MODE, uiMsg('RecorderLiveConnectionFailed'));
      finishLive();
    };
    socket.onclose = () => {
      ready = false;
      if (!liveFinish) return;
      liveFailed = true;
      reportError(LIVE_MODE, uiMsg('RecorderLiveInterrupted'));
      finishLive();
    };
  }

  /* ---------------------------------------------------------------------- */
  /* capture                                                                */
  /* ---------------------------------------------------------------------- */

  function ingest(chunk) {
    if (disposed) return;
    samples += chunk.length;
    if (needsBatch) chunks.push(chunk);
    if (!processor) {
      // External frames carry their own level: peak per ~33 ms window.
      let peak = levelPeak;
      for (let i = 0; i < chunk.length; i++) {
        const magnitude = Math.abs(chunk[i]);
        if (magnitude > peak) peak = magnitude;
      }
      levelPeak = peak;
      const now = performance.now();
      if (now - levelAt >= LEVEL_INTERVAL_MS) {
        levelAt = now;
        levelPeak = 0;
        emitLevel(peak / PCM16_PEAK);
      }
    }
    if (needsLive) sendOrQueue(chunk);
  }

  function handleWorklet(data) {
    if (!data) return;
    if (data.type === 'audio') ingest(new Int16Array(data.buffer));
    else if (data.type === 'level') emitLevel(data.level);
  }

  /** Waits for a worklet to hand over its final PCM chunk. */
  function drainWorklet(worklet) {
    if (!worklet) return Promise.resolve();
    return new Promise((resolve) => {
      const previous = worklet.port.onmessage;
      let settled = false;
      const timer = setTimeout(() => settle(), WORKLET_DRAIN_MS);
      function settle() {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve();
      }
      worklet.port.onmessage = (event) => {
        if (event.data?.type === 'ended') settle();
        else previous?.(event);
      };
      worklet.port.postMessage('stop');
    });
  }

  async function closeContext(closing) {
    if (!closing) return;
    try {
      await closing.close();
    } catch { /* already closed, or the page is going away */ }
  }

  async function closeSocket() {
    const closing = socket;
    socket = null;
    ready = false;
    if (!closing) return;
    try {
      closing.close();
    } catch { /* already closing */ }
  }

  /**
   * Releases the capture resources held by `owned`. Bags are emptied as they are
   * released, so a start that lost its ticket gives back only what it built and a
   * second release is a no-op.
   */
  async function releaseOwned(owned) {
    if (!owned) return;
    owned.stream?.getTracks().forEach((track) => track.stop());
    owned.stream = null;
    if (owned.processor) {
      // The worklet's tail has to reach Live before `end` and the WAV builder.
      await drainWorklet(owned.processor);
      owned.processor.disconnect();
      owned.processor = null;
    }
    const closing = owned.context;
    owned.context = null;
    await closeContext(closing);
  }

  /** Releases the running session and any start that is still arming. */
  async function releaseCapture() {
    clearTimeout(capTimer);
    capTimer = null;
    const running = { stream, context, processor };
    stream = null;
    context = null;
    processor = null;
    const arming = pending;
    pending = null;
    await releaseOwned(running);
    await releaseOwned(arming);
  }

  /* ---------------------------------------------------------------------- */
  /* arming and stopping                                                    */
  /* ---------------------------------------------------------------------- */

  async function arm(options) {
    const ticket = ++session;
    const stale = () => ticket !== session;
    const mine = { stream: null, context: null, processor: null };
    pending = mine;

    const requested = Array.isArray(options?.modes) ? options.modes : [];
    modes = MODE_ORDER.filter((name) => VALID_MODES.has(name) && requested.includes(name));
    // A take without a usable mode list still records: it transcribes Live.
    if (modes.length === 0) modes = [LIVE_MODE];
    sessionId = Number.isSafeInteger(options?.sessionId) && options.sessionId > 0 ? options.sessionId : null;
    needsLive = modes.includes(LIVE_MODE);
    needsBatch = modes.some((name) => name !== LIVE_MODE);
    const external = options?.external === true;

    chunks = needsBatch ? [] : null;
    samples = 0;
    queue = [];
    ready = false;
    liveFailed = false;
    liveFinal = [];
    liveInterim = '';
    errors = {};
    errorDetails = {};
    levelAt = 0;
    levelPeak = 0;
    abort = new AbortController();

    setState('arming');
    try {
      if (!external) {
        mine.stream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: false, noiseSuppression: false, autoGainControl: false } });
        if (stale()) return releaseOwned(mine);
        const audio = new AudioContext();
        mine.context = audio;
        await audio.audioWorklet.addModule(WORKLET_URL);
        if (stale()) return releaseOwned(mine);
        const worklet = new AudioWorkletNode(audio, 'pcm-16k');
        mine.processor = worklet;
        worklet.port.onmessage = (event) => handleWorklet(event.data);
        const source = audio.createMediaStreamSource(mine.stream);
        const silent = audio.createGain();
        silent.gain.value = 0;
        source.connect(worklet).connect(silent).connect(audio.destination);
        await audio.resume();
        if (stale()) return releaseOwned(mine);
      }
      if (stale()) return releaseOwned(mine);

      // The ticket still stands: this start owns the module's capture slots.
      stream = mine.stream;
      context = mine.context;
      processor = mine.processor;
      mine.stream = null;
      mine.context = null;
      mine.processor = null;
      if (pending === mine) pending = null;

      setState('recording');
      if (needsLive) openLive();
      // The microphone path owns the ten-minute cap. An external host (native cpal
      // capture) runs its own timer, drains its tail and then calls stop(), so the
      // recorder must not cut the take short underneath it.
      if (!external) capTimer = setTimeout(() => { void stop(); }, CAP_MS);
    } catch (cause) {
      await releaseOwned(mine);
      if (pending === mine) pending = null;
      if (stale()) return;
      setState('idle');
      const diagnostic = diagnosticOf(cause);
      const ui = isDescriptor(cause?.ui) ? cause.ui : cause?.name === 'NotAllowedError'
        ? uiMsg('RecorderMicDenied')
        : uiMsg('RecorderStartFailed', { detail: diagnostic || uiMsg('RecorderUnknownCause') });
      reportError('', ui, diagnostic);
      throw uiFailure(ui, diagnostic, cause);
    }
  }

  function makeWave() {
    const wave = new ArrayBuffer(44 + samples * 2);
    const view = new DataView(wave);
    const ascii = (offset, text) => { for (let i = 0; i < text.length; i++) view.setUint8(offset + i, text.charCodeAt(i)); };
    ascii(0, 'RIFF'); view.setUint32(4, 36 + samples * 2, true);
    ascii(8, 'WAVE'); ascii(12, 'fmt '); view.setUint32(16, 16, true);
    view.setUint16(20, 1, true); view.setUint16(22, 1, true);
    view.setUint32(24, 16000, true); view.setUint32(28, 32000, true);
    view.setUint16(32, 2, true); view.setUint16(34, 16, true);
    ascii(36, 'data'); view.setUint32(40, samples * 2, true);
    let offset = 44;
    for (const chunk of chunks ?? []) for (const sample of chunk) { view.setInt16(offset, sample, true); offset += 2; }
    return wave;
  }

  async function transcribe(name, wave) {
    const signal = abort?.signal;
    try {
      const response = await fetch(withSession(`${TRANSCRIBE_URL}?mode=${name}`), {
        method: 'POST',
        headers: { 'Content-Type': 'audio/wav' },
        body: wave,
        signal,
      });
      let parseError = null;
      const result = await response.json().catch((error) => {
        // An aborted take keeps its own meaning; a broken body is reported by status.
        if (error?.name === 'AbortError') throw error;
        parseError = error;
        return null;
      });
      if (!response.ok) throw httpFailure(result, response.status);
      if (result === null) throw httpFailure(null, response.status, diagnosticOf(parseError));
      if (typeof result.text !== 'string' || !result.text.trim()) throw uiFailure(uiMsg('RecorderEmptyText'));
      return { text: result.text };
    } catch (cause) {
      // A request disposed with its take is not a transcription failure: the
      // session it belonged to is gone and no answer of it has anywhere to go.
      if (cause?.name === 'AbortError') return { aborted: true };
      if (cause instanceof Error && isDescriptor(cause.ui)) {
        return { ui: cause.ui, diagnostic: typeof cause.diagnosticMessage === 'string' ? cause.diagnosticMessage : '' };
      }
      const diagnostic = diagnosticOf(cause);
      return { ui: uiMsg('RecorderRequestFailed', { detail: diagnostic || uiMsg('RecorderUnknownCause') }), diagnostic };
    }
  }

  async function runStop() {
    if (state === 'idle') return emptyResult();
    let arming = false;
    if (state === 'arming') {
      // A pending permission prompt must not hold the stop: the in-flight start
      // loses its ticket and its resources are released now, not when it resumes.
      arming = true;
      session += 1;
      await releaseCapture();
      await Promise.race([active.then(noop, noop), delay(ARM_SETTLE_MS)]);
    }
    if (arming && state !== 'recording') {
      await releaseCapture();
      await closeSocket();
      setState('idle');
      return emptyResult();
    }

    setState('stopping');
    clearTimeout(capTimer);
    capTimer = null;
    const results = {};
    try {
      await releaseCapture(); // final worklet chunk lands here, before Live end
      if (disposed) return emptyResult();
      if (needsLive) {
        if (ready && socket?.readyState === WebSocket.OPEN) liveEnd();
        else if (socket?.readyState !== WebSocket.OPEN) finishLive();
        await Promise.race([liveDone, delay(LIVE_DRAIN_MS)]);
        if (liveFinish) {
          liveFailed = true;
          reportError(LIVE_MODE, uiMsg('RecorderLiveTimeout'));
          finishLive();
        }
        await closeSocket();
      }
      // A partial segment from a stream that then failed is not a result.
      if (liveFinal.some(Boolean) && !liveFailed) results.live = liveFinal.filter(Boolean).join(' ');

      if (samples < MIN_SAMPLES) {
        reportError('', uiMsg('RecorderTooShort'));
        return { results, errors, errorDetails };
      }

      if (needsBatch) {
        const wave = makeWave();
        const wanted = BATCH_MODES.filter((name) => modes.includes(name));
        const outcomes = await Promise.all(wanted.map((name) => transcribe(name, wave)));
        if (disposed) return emptyResult();
        wanted.forEach((name, index) => {
          const outcome = outcomes[index];
          if (outcome.text) {
            results[name] = outcome.text;
            onFinal(name, outcome.text);
          } else if (!outcome.aborted) {
            reportError(name, outcome.ui, outcome.diagnostic);
          }
        });
      }
      return { results, errors, errorDetails };
    } catch (cause) {
      const diagnostic = diagnosticOf(cause);
      const ui = isDescriptor(cause?.ui) ? cause.ui : uiMsg('RecorderStopFailed', { detail: diagnostic || uiMsg('RecorderUnknownCause') });
      reportError('', ui, diagnostic);
      return { results, errors, errorDetails };
    } finally {
      chunks = null;
      samples = 0;
      queue = [];
      liveInterim = '';
      emitLevel(0);
      await closeSocket();
      await closeContext();
      setState('idle');
    }
  }

  /* ---------------------------------------------------------------------- */
  /* public surface                                                         */
  /* ---------------------------------------------------------------------- */

  /**
   * Idempotent while a session is arming or recording.
   * `modes` is the exact set of transcriptions a take produces - one entry for
   * an ordinary dictation, all three for the laboratory - in canonical order
   * live, smart, verbatim; no other mode is ever sent.
   * `external: true` skips the microphone; the host then feeds `acceptPcmChunk`.
   * `sessionId`, when given, tags every request of the take with the session the
   * desktop shell pinned its settings to.
   */
  function start(options = {}) {
    if (state !== 'idle' || disposed) return active;
    active = arm(options);
    return active;
  }

  function stop() {
    if (disposed || state === 'idle') return Promise.resolve(emptyResult());
    if (!stopping) stopping = runStop().finally(() => { stopping = null; });
    return stopping;
  }

  /**
   * Frame intake for capture owned elsewhere (a future native cpal source in the
   * desktop webview). Frames are ignored while the internal microphone is
   * attached, so two sources can never be mixed into one recording.
   */
  function acceptPcmChunk(chunk) {
    if (disposed || stream || !(chunk instanceof Int16Array) || chunk.length === 0) return;
    if (state !== 'recording' && state !== 'stopping') return;
    ingest(chunk);
  }

  /**
   * Drops this instance and its take: nothing is transcribed, nothing is
   * inserted, a batch upload already in flight is aborted, the Live socket is
   * closed as soon as the capture is released, and no further callback of this
   * instance ever fires. A new recorder is what the next dictation needs.
   */
  function dispose() {
    if (disposed) return;
    session += 1;
    clearTimeout(capTimer);
    capTimer = null;
    // A batch upload already in flight is cancelled here: the take it belongs to
    // is being dropped, so its answer must not arrive at all.
    abort?.abort();
    abort = null;
    // The session tag belongs to the take that is being dropped.
    sessionId = null;
    // The socket goes before anything else: not even a frame that was still
    // queued leaves the page after the dictation was cancelled.
    finishLive();
    void closeSocket();
    // The take itself is dropped, so nothing of it can be built into a WAV later.
    chunks = null;
    samples = 0;
    queue = [];
    emitLevel(0);
    setState('idle');
    disposed = true;
    void releaseCapture();
  }

  return { start, stop, acceptPcmChunk, dispose };
}
