/**
 * public/dev-recorder.js — Development-only diagnostics decorator.
 *
 * The page keeps loading `/recorder.js`, and in a Development build with the
 * `dev-diagnostics` feature the shell serves this file for that route while
 * `/recorder-core.js` serves the untouched `public/recorder.js`. Everything the
 * pages depend on stays exactly as it was: the same `createRecorder(handlers)`
 * surface, the same returned promises, results, callbacks, options and
 * exceptions, because every call is forwarded to the core unchanged.
 *
 * On top of that, and only when the page bound a native take with the global
 * `__speechekDevDiagnostics` marker, the decorator observes the take and posts
 * reports to `POST /api/dev-diagnostics`: the bounded counters and boundary
 * events, and the take's content — the error reports the core really handed
 * over, and, on the terminal report, the exact object `stop()` resolved with.
 * Content is never truncated: a report that cannot carry it whole is refused
 * and accounted for instead.
 *
 *   - `setContext(owner, generation)` — one-shot pending binding, called by the
 *     page right before the genuine `start` of a take. A `start` that the core
 *     answers as a no-op neither consumes nor changes it, and a browser take
 *     (one without a context) clears it instead of inheriting a native key.
 *   - `nativeGap(owner, generation, expected, received, reason)` — a skipped or
 *     unreadable native frame, with the key of the capture it happened in.
 *   - `nativeStop(owner, generation, last, seen, damaged)` — the tail the shell
 *     answered with, after it was waited for; `last` is null when it is unknown.
 *
 * Nothing here is a user-facing feature: no visible text, no UI state, no new
 * per-frame work in a release. The marker is absent there, no take is ever
 * bound, and the WebSocket and fetch wrappers only decorate requests that
 * belong to a bound take of this same origin.
 *
 * What a report carries is what the take really produced: the counters and
 * codes below, the error reports the core delivered (mode, rendered message,
 * catalog descriptor, the elapsed time of the callback that raised it and,
 * only when the core handed one over, the raw diagnostic), and the three maps
 * `stop()` resolved with — results, errors and errorDetails. No URL, host, key,
 * header, audio frame or setting ever travels, and nothing is stored: a report
 * is only built once a take is bound.
 */

import { createRecorder as createCoreRecorder } from './recorder-core.js';

/* -------------------------------------------------------------------------- */
/* constants                                                                   */
/* -------------------------------------------------------------------------- */

/** The global the pages use to bind a take; absent in production. */
const MARKER_NAME = '__speechekDevDiagnostics';

const LIVE_URL = '/api/live';
const TRANSCRIBE_URL = '/api/transcribe';
const REPORT_URL = '/api/dev-diagnostics';

/** The endpoint refuses a bigger body, so a report is never built past this. */
const BODY_LIMIT = 4096;
/**
 * A report that carries content travels under its own cap, because the content
 * is what the archive exists for: text that a take really produced is never
 * cut. It matches the endpoint's content limit; a body beyond it is refused
 * and accounted for, never sent short.
 */
const CONTENT_BODY_LIMIT = 1048576;
/** The modes a result or an error report may name; '' is a session error. */
const MODES = new Set(['', 'live', 'smart', 'verbatim']);
/** One report carries at most this many boundary events. */
const EVENTS_LIMIT = 16;
/** Counters are coalesced into a report at most once per second. */
const REPORT_INTERVAL_MS = 1000;
/** After the terminal outcome, a report is retried for at most this long. */
const TERMINAL_GRACE_MS = 2000;

const OWNERS = new Set(['dictation', 'lab']);
const STATES = new Set(['idle', 'arming', 'recording', 'stopping', 'disposed']);
const TERMINAL_OUTCOMES = new Set(['success', 'error', 'cancelled']);

/** Gap reasons are mapped to a numeric code; no free-form string is sent. */
const REASON_CODES = { sequence: 1, decode: 2 };

/** The page's own Live route: the one socket this decorator may observe. */
const LIVE_PATH = LIVE_URL;
/** Exact prefixes of the three frames the core sends; nothing else is sent. */
const AUDIO_PREFIX = '{"type":"audio","data":"';
const START_PREFIX = '{"type":"start"';
const END_PREFIX = '{"type":"end"';

/* -------------------------------------------------------------------------- */
/* captured originals                                                          */
/* -------------------------------------------------------------------------- */

/**
 * The globals are read once, at import time, before anything is replaced: the
 * reporter always posts through the fetch the page really started with, so its
 * own requests can never be observed, tagged or retried by the wrapper.
 */
const nativeFetch = typeof globalThis.fetch === 'function' ? globalThis.fetch : null;
const NativeWebSocket = typeof globalThis.WebSocket === 'function' ? globalThis.WebSocket : null;
const TextEncoderCtor = typeof globalThis.TextEncoder === 'function' ? globalThis.TextEncoder : null;
const encoder = TextEncoderCtor ? new TextEncoderCtor() : null;

/* -------------------------------------------------------------------------- */
/* small helpers                                                               */
/* -------------------------------------------------------------------------- */

function nowMs() {
  const perf = globalThis.performance;
  if (perf && typeof perf.now === 'function') return perf.now();
  return Date.now();
}

/** UTF-8 length of a string, exactly as the endpoint counts it. */
function utf8Bytes(text) {
  if (encoder) return encoder.encode(text).length;
  let bytes = 0;
  for (let index = 0; index < text.length; index++) {
    const code = text.charCodeAt(index);
    if (code < 0x80) bytes += 1;
    else if (code < 0x800) bytes += 2;
    else if (code >= 0xd800 && code <= 0xdbff && index + 1 < text.length) { bytes += 4; index++; }
    else bytes += 3;
  }
  return bytes;
}

/** Non-negative safe integers only; anything else becomes null. */
function safeCount(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function originOf() {
  const location = globalThis.location;
  return location && typeof location.origin === 'string' && location.origin ? location.origin : null;
}

/** Resolves a request target against the page origin; null when unusable. */
function absoluteUrl(raw) {
  const origin = originOf();
  if (origin === null) return null;
  try {
    return new URL(raw, origin);
  } catch {
    return null;
  }
}

/** Adds the take's diagnostic tag; the value keeps its raw `owner:generation`. */
function tagUrl(url, owner, generation) {
  const tag = `diagnostics=${owner}:${generation}`;
  return `${url}${url.includes('?') ? '&' : '?'}${tag}`;
}

/** Same-origin check for a request target; the observer only touches those. */
function isOwnPath(resolved) {
  const origin = originOf();
  return origin !== null && resolved.origin === origin;
}

/** The frame kind of a core send, without parsing or copying the payload. */
function sendKind(data) {
  if (typeof data !== 'string') return null;
  if (data.startsWith(AUDIO_PREFIX)) return 'audio';
  if (data.startsWith(START_PREFIX)) return 'start';
  if (data.startsWith(END_PREFIX)) return 'end';
  return null;
}

/** PCM bytes behind the base64 of an audio frame, computed from its length. */
function audioBytes(json) {
  const length = json.length - AUDIO_PREFIX.length - 2; // trailing `"}`
  if (length <= 0) return 0;
  let padding = 0;
  if (json.charCodeAt(json.length - 3) === 61) padding += 1; // '='
  if (json.charCodeAt(json.length - 4) === 61) padding += 1;
  return Math.floor(length / 4) * 3 - padding;
}

/* -------------------------------------------------------------------------- */
/* content helpers                                                             */
/* -------------------------------------------------------------------------- */

/**
 * A value JSON cannot carry as it is: a cycle, a function, a symbol, an object
 * whose prototype is not a plain one (a Date, a Map, a class instance — its own
 * fields are not the value), an array hole or a number that is not finite. It
 * is never replaced by something else and never dropped: whoever meets it
 * reports the content as lost, so a report never travels with a piece of the
 * take quietly missing.
 */
const UNSERIALIZABLE = Symbol('unserializable');

/**
 * A complete copy of one value, at whatever depth it really has: strings,
 * booleans, finite numbers and null as they are, and every own enumerable
 * property of a plain object or element of an array rebuilt in full. An
 * ancestor repeat (a cycle), a non-plain prototype, and a value with no JSON
 * form are answered with `UNSERIALIZABLE` rather than copied short.
 */
function snapshotValue(value, ancestors) {
  if (value === null) return null;
  if (typeof value === 'string' || typeof value === 'boolean') return value;
  if (typeof value === 'number') return Number.isFinite(value) ? value : UNSERIALIZABLE;
  if (typeof value !== 'object') return UNSERIALIZABLE;
  if (ancestors.includes(value)) return UNSERIALIZABLE;
  const array = Array.isArray(value);
  if (!array) {
    const prototype = Object.getPrototypeOf(value);
    if (prototype !== null && prototype !== Object.prototype) return UNSERIALIZABLE;
  }
  ancestors.push(value);
  let copy;
  if (array) {
    copy = [];
    for (const item of value) {
      const nested = snapshotValue(item, ancestors);
      if (nested === UNSERIALIZABLE) {
        ancestors.pop();
        return UNSERIALIZABLE;
      }
      copy.push(nested);
    }
  } else {
    copy = {};
    for (const [name, item] of Object.entries(value)) {
      const nested = snapshotValue(item, ancestors);
      if (nested === UNSERIALIZABLE) {
        ancestors.pop();
        return UNSERIALIZABLE;
      }
      // An own property named `__proto__` is defined, never assigned: assigning
      // it would reach the copy's prototype instead of the copy, which both
      // drops the property from the report and pollutes Object.prototype.
      Object.defineProperty(copy, name, {
        value: nested,
        enumerable: true,
        writable: true,
        configurable: true,
      });
    }
  }
  ancestors.pop();
  return copy;
}

/**
 * The frozen whole copy of a value, or null when it has no bounded JSON form.
 * A structure too deep for the stack is answered the same way: the content is
 * accounted for as lost instead of thrown into the caller's own callback.
 */
function snapshot(value) {
  try {
    const copy = snapshotValue(value, []);
    if (copy === UNSERIALIZABLE) return null;
    return deepFreeze(copy);
  } catch {
    return null;
  }
}

/** Freezes a snapshot to its leaves, so what was recorded cannot be changed. */
function deepFreeze(value) {
  if (value === null || typeof value !== 'object') return value;
  for (const nested of Object.values(value)) deepFreeze(nested);
  return Object.freeze(value);
}

/**
 * The catalog descriptor a page re-renders from, copied in full. A value that
 * is not a descriptor at all — no key, or something that has no JSON form — is
 * refused, so the report that needed it is accounted for as lost instead.
 */
function copyDescriptor(ui) {
  if (!ui || typeof ui !== 'object' || Array.isArray(ui)) return null;
  if (typeof ui.key !== 'string' || ui.key === '') return null;
  return snapshot(ui);
}

/**
 * One error report of a take, exactly as the core delivered it. `mode` is a
 * known mode or '' for a session error; `message` is the text rendered at that
 * moment; `ui` is the descriptor the page keeps; `diagnostic` carries the raw
 * text only when the core really handed one over — it is never invented. A
 * report whose text is not text at all is refused rather than replaced, so the
 * caller can account for it instead of recording a report that was not made.
 */
function errorEntry(mode, message, ui, diagnostic) {
  const descriptor = copyDescriptor(ui);
  let text = '';
  if (message !== undefined && message !== null) {
    if (typeof message !== 'string') return null;
    text = message;
  }
  if (text === '' && descriptor === null) return null;
  const entry = {
    mode: typeof mode === 'string' && MODES.has(mode) ? mode : '',
    message: text,
  };
  if (descriptor !== null) entry.ui = descriptor;
  if (typeof diagnostic === 'string' && diagnostic !== '') entry.diagnostic = diagnostic;
  return Object.freeze(entry);
}

/**
 * The object `stop()` resolved with, copied whole while the core still holds
 * it, so the terminal report and the caller see the same values and no later
 * mutation of either can reach a report that was already built. The three maps
 * are the whole resolution: a shape the core does not promise is answered with
 * null, and the caller accounts for that content as lost rather than sending a
 * repaired or partial resolution.
 */
function copyResult(result) {
  if (!result || typeof result !== 'object' || Array.isArray(result)) return null;
  const results = {};
  const errors = {};
  const errorDetails = {};
  // Results are per mode; errors and their descriptors also name the whole
  // session with '', the key the core uses for a capture or session failure.
  for (const [name, target, session] of [['results', results, false], ['errors', errors, true]]) {
    const map = result[name];
    if (!map || typeof map !== 'object' || Array.isArray(map)) return null;
    for (const [mode, text] of Object.entries(map)) {
      if (!MODES.has(mode) || (!session && mode === '')) return null;
      if (typeof text !== 'string') return null;
      target[mode] = text;
    }
  }
  const details = result.errorDetails;
  if (!details || typeof details !== 'object' || Array.isArray(details)) return null;
  for (const [mode, ui] of Object.entries(details)) {
    if (!MODES.has(mode)) return null;
    const descriptor = copyDescriptor(ui);
    if (descriptor === null) return null;
    errorDetails[mode] = descriptor;
  }
  return Object.freeze({
    results: Object.freeze(results),
    errors: Object.freeze(errors),
    errorDetails: Object.freeze(errorDetails),
  });
}

/* -------------------------------------------------------------------------- */
/* take reporter                                                               */
/* -------------------------------------------------------------------------- */

/**
 * One bound take. It owns the counters the plan lists, the bounded event queue
 * and the single report in flight; every payload is frozen once it is submitted
 * and only replaced after the endpoint acknowledged it.
 */
class TakeReporter {
  constructor(owner, generation) {
    this.owner = owner;
    this.generation = generation;
    this.startedAt = nowMs();
    this.state = 'idle';
    this.socket = null;

    this.handedFrames = 0;
    this.handedSamples = 0;
    this.ignoredFrames = 0;
    this.sentFrames = 0;
    this.sentPcmBytes = 0;
    this.readyCount = 0;
    this.interimCount = 0;
    this.finalCount = 0;
    this.errorCount = 0;
    this.doneCount = 0;
    this.eventsDropped = 0;
    this.lostReports = 0;
    this.lastSendMs = null;
    this.lastReceiveMs = null;

    this.events = [];
    /**
     * The content of this take: the error reports the core delivered and, once
     * it resolved, the object `stop()` returned. Both are copied verbatim and
     * only ever dropped as a whole, never cut down.
     */
    this.errors = [];
    this.result = null;
    /** True once this take's content was accounted for as lost. */
    this.contentDisabled = false;
    this.reportIndex = 0;
    /** The immutable payload in flight, and whether one is. */
    this.submitted = null;
    this.inflight = false;
    /** Coalesced counters waiting for the next report. */
    this.dirty = false;
    this.terminalOutcome = null;
    /** True once a submitted payload carried the terminal outcome. */
    this.terminalSent = false;
    this.deadline = 0;
    /** The coalescing tick; `expiry` is the independent terminal deadline. */
    this.timer = null;
    this.expiry = null;
    this.closed = false;
    this.stopped = false;
    this.abandoned = false;
  }

  elapsed() {
    return Math.max(0, nowMs() - this.startedAt);
  }

  /* ---------------------------------------------------------------- */
  /* counters                                                          */
  /* ---------------------------------------------------------------- */

  setState(next) {
    if (this.closed || !STATES.has(next) || this.state === next) return;
    this.state = next;
    this.dirty = true;
    this.schedule();
  }

  handed(chunk) {
    this.handedFrames += 1;
    this.handedSamples += chunk.length;
    this.dirty = true;
    this.schedule();
  }

  ignored() {
    this.ignoredFrames += 1;
    this.dirty = true;
    this.schedule();
  }

  sentAudio(bytes) {
    this.sentFrames += 1;
    this.sentPcmBytes += bytes;
    this.lastSendMs = Math.round(this.elapsed());
    this.dirty = true;
    this.schedule();
  }

  sentControl() {
    this.lastSendMs = Math.round(this.elapsed());
    this.dirty = true;
    this.schedule();
  }

  received(kind) {
    if (kind === 'ready') { this.readyCount += 1; this.emit('socket_ready'); }
    else if (kind === 'interim') this.interimCount += 1;
    else if (kind === 'final') this.finalCount += 1;
    else if (kind === 'error') this.emit('socket_error');
    else if (kind === 'done') { this.doneCount += 1; this.emit('socket_done'); }
    this.lastReceiveMs = Math.round(this.elapsed());
    this.dirty = true;
    this.schedule();
  }

  /** Core `onError` reports only; a WebSocket error is not counted twice. */
  error() {
    if (this.closed || this.stopped) return;
    this.errorCount += 1;
    this.dirty = true;
    this.schedule();
  }

  attachSocket(socket) {
    this.socket = socket;
  }

  /* ---------------------------------------------------------------- */
  /* content                                                           */
  /* ---------------------------------------------------------------- */

  /**
   * One error report the core really delivered. It is kept verbatim, stamped
   * with the moment of the callback that raised it — the take's own monotonic
   * elapsed milliseconds — and the index it was kept at is answered, so the raw
   * diagnostic a failed start carries in its rejection can complete exactly
   * this report. A take whose content was already accounted for as lost counts
   * it there instead of accumulating text that will never leave.
   */
  addError(entry) {
    if (this.closed || this.stopped || this.abandoned) return -1;
    if (this.contentDisabled) {
      this.lostReports += 1;
      this.dirty = true;
      this.schedule(0);
      return -1;
    }
    const atMs = Math.round(this.elapsed());
    this.errors.push(Object.freeze({
      ...entry,
      atMs: Number.isFinite(atMs) && atMs >= 0 ? atMs : 0,
    }));
    this.dirty = true;
    this.schedule(0);
    return this.errors.length - 1;
  }

  /**
   * Completes one recorded error report with the raw diagnostic the failure
   * itself handed over. The text is taken from the core's own rejection, so a
   * report never carries a diagnostic that was not really produced, and the
   * moment the report was recorded stays exactly the one it was stamped with.
   */
  attachDiagnostic(index, diagnostic) {
    if (this.closed || this.stopped || this.abandoned || this.contentDisabled) return;
    const entry = this.errors[index];
    if (!entry || entry.diagnostic !== undefined) return;
    const completed = errorEntry(entry.mode, entry.message, entry.ui, diagnostic);
    if (completed === null) return;
    this.errors[index] = entry.atMs === undefined
      ? completed
      : Object.freeze({ ...completed, atMs: entry.atMs });
    this.dirty = true;
    this.schedule(0);
  }

  /**
   * The object `stop()` resolved with, recorded before the terminal outcome is
   * settled: the terminal report carries the resolution of the very take whose
   * outcome it announces. The core promises exactly these three maps, so a
   * resolution that is not one can only be the observer meeting something it
   * did not expect: it is accounted for as lost content, never sent repaired or
   * partial, and the caller's own resolution is not touched either way.
   */
  setResult(result) {
    if (this.closed || this.stopped || this.abandoned || this.contentDisabled) return;
    const copy = copyResult(result);
    if (copy === null) {
      this.loseContent('content_invalid');
      return;
    }
    this.result = copy;
    // No report is scheduled here: the resolution is carried by the terminal
    // report, which the take hands over right after this and which is built
    // from these fields. A body sent before that would carry the resolution
    // without the outcome it belongs to.
    this.dirty = true;
  }

  /** The content this report carries, or null when the take produced none. */
  content() {
    if (this.contentDisabled) return null;
    if (this.result === null && this.errors.length === 0) return null;
    const content = {};
    if (this.result !== null) content.result = this.result;
    if (this.errors.length > 0) content.errors = Object.freeze([...this.errors]);
    return content;
  }

  /* ---------------------------------------------------------------- */
  /* events                                                            */
  /* ---------------------------------------------------------------- */

  /**
   * Boundary events are flushed right away, in a batch of at most
   * `EVENTS_LIMIT`; an event that does not fit is counted as dropped.
   */
  emit(event, fields) {
    if (this.closed) return;
    const entry = { event, atMs: Math.round(this.elapsed()) };
    if (fields) {
      if (safeCount(fields.last) !== null) entry.last = fields.last;
      if (safeCount(fields.seen) !== null) entry.seen = fields.seen;
      if (safeCount(fields.expected) !== null) entry.expected = fields.expected;
      if (safeCount(fields.received) !== null) entry.received = fields.received;
      if (Number.isSafeInteger(fields.code) && fields.code >= 0 && fields.code <= 0xffff) entry.code = fields.code;
      if (fields.damaged === true) entry.damaged = true;
    }
    // A batch that is full, or a take whose reporting is already over, cannot
    // deliver this event: it is accounted for instead of being accumulated.
    if (this.stopped || this.abandoned || this.events.length >= EVENTS_LIMIT) {
      this.eventsDropped += 1;
      this.dirty = true;
      this.schedule(0);
      return;
    }

    this.events.push(entry);
    this.dirty = true;
    this.schedule(0);
  }

  /** Only a genuine bound start, stop or dispose reaches this. */
  terminal(outcome) {
    if (this.closed || !TERMINAL_OUTCOMES.has(outcome)) return;
    this.terminalOutcome = outcome;
    this.deadline = nowMs() + TERMINAL_GRACE_MS;
    this.dirty = true;
    // The terminal report does not wait for the next second tick, and its grace
    // is measured by its own timer: an endpoint that never answers must not be
    // able to keep the payload, the timer or the callbacks alive.
    this.clearTimer();
    this.armExpiry();
    this.schedule(0);
  }

  /* ---------------------------------------------------------------- */
  /* scheduling                                                        */
  /* ---------------------------------------------------------------- */

  /** The independent terminal deadline; armed once per take. */
  armExpiry() {
    if (this.expiry !== null) return;
    try {
      this.expiry = setTimeout(() => {
        this.expiry = null;
        this.expire();
      }, TERMINAL_GRACE_MS);
    } catch { /* no timers left: the take is over anyway */ }
  }

  /**
   * The terminal grace is over. Reporting stops for good: whatever was still
   * pending is accounted for as lost, and no later callback, acknowledgement or
   * socket event can schedule another attempt for a take that is already over.
   */
  expire() {
    if (this.closed || this.stopped || this.abandoned) return;
    this.abandoned = true;
    if (this.submitted !== null || this.inflight) {
      this.lostReports += 1;
      this.submitted = null;
    }
    this.clearTimer();
    this.clearExpiry();
  }

  clearExpiry() {
    if (this.expiry !== null) {
      clearTimeout(this.expiry);
      this.expiry = null;
    }
  }

  /** Ends every schedule of this take, once and for all. */
  stopReporting() {
    this.stopped = true;
    this.submitted = null;
    this.clearTimer();
    this.clearExpiry();
  }

  schedule(delay) {
    if (this.closed || this.stopped || this.abandoned || this.timer !== null) return;
    // Past the terminal grace nothing is scheduled again, whatever asked.
    if (this.terminalOutcome !== null && nowMs() >= this.deadline) {
      this.expire();
      return;
    }
    const wait = delay === undefined ? REPORT_INTERVAL_MS : delay;
    try {
      this.timer = setTimeout(() => {
        this.timer = null;
        this.tick();
      }, wait);
    } catch { /* no timers left; the take is over anyway */ }
  }

  clearTimer() {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  tick() {
    if (this.closed || this.stopped || this.abandoned) return;
    if (this.inflight) return;
    if (this.submitted === null && !this.dirty) return;
    this.send();
  }

  flush() {
    this.clearTimer();
    this.tick();
  }

  send() {
    if (this.closed || this.stopped || this.abandoned || this.inflight) return;
    let entry = this.submitted;
    if (entry === null) {
      entry = this.build();
      if (entry === null) return;
      this.submitted = entry;
      this.dirty = false;
    }
    this.inflight = true;
    postReport(this.owner, this.generation, entry.body).then(
      (status) => this.ack(status),
      () => this.ack(0),
    );
  }

  ack(status) {
    this.inflight = false;
    if (this.closed) return;
    if (this.abandoned) {
      this.submitted = null;
      return;
    }
    // 202 accepted and 200 stale_report both acknowledge this report.
    if (status === 202 || status === 200) {
      this.submitted = null;
      if (this.terminalOutcome !== null) {
        // The take is over. Only a terminal report that is still waiting its
        // turn is sent inside the grace; once it was admitted, reporting ends
        // and no later callback may schedule anything again.
        if (this.terminalSent) this.stopReporting();
        else this.schedule(0);
        return;
      }
      // Coalesced counters: at most one snapshot per second while it runs.
      this.schedule();
      return;
    }
    // A queue that is full and a network failure keep the same payload and
    // index: the very next tick tries the identical report again.
    if (status === 429 || status === 0) {
      this.retry();
      return;
    }
    // The take or the sink is gone: scheduling stops, the audio does not care.
    if (status === 410 || status === 503) {
      this.stopReporting();
      return;
    }
    // 400/403/405/413/415 and anything unexpected: one technical warning.
    this.warn(status);
    this.stopReporting();
  }

  retry() {
    // Past the terminal grace the take is accounted for, once, and ends.
    if (this.terminalOutcome !== null && nowMs() >= this.deadline) {
      this.expire();
      return;
    }
    this.schedule();
  }

  /* ---------------------------------------------------------------- */
  /* payload                                                           */
  /* ---------------------------------------------------------------- */

  /**
   * Builds the next report. The index is assigned only after the payload was
   * proven to fit, so an oversized body is never sent and never retried. A
   * report that carries content takes the wide cap; the content itself is
   * copied once, verbatim, and is never the part that gets cut.
   */
  build() {
    let scalars = this.scalars();
    if (!validReport(scalars)) {
      this.disable();
      return null;
    }
    // The content of this take is built only from what the core really
    // delivered, and a piece of it that could not be copied was already
    // accounted for as lost: what is here is whole and of the promised shape.
    const content = this.content();
    if (content !== null) {
      const wide = this.buildContent(scalars, content);
      if (wide !== null) return wide;
      // Not one byte of the content may be cut, so the whole report is
      // accounted for as lost and the take keeps to its counters.
      this.loseContent('content_overflow');
      // The loss belongs to the report that follows it: the counters are read
      // again, so the body that is really sent advertises the lost content.
      scalars = this.scalars();
      if (!validReport(scalars)) {
        this.disable();
        return null;
      }
    }
    return this.buildNumeric(scalars);
  }

  /** Every counter, in the order the endpoint's report carries them. */
  scalars() {
    return {
      owner: this.owner,
      generation: this.generation,
      reportIndex: this.reportIndex + 1,
      clientElapsedMs: Math.round(this.elapsed()),
      state: this.state,
      handedFrames: this.handedFrames,
      handedSamples: this.handedSamples,
      ignoredFrames: this.ignoredFrames,
      sentFrames: this.sentFrames,
      sentPcmBytes: this.sentPcmBytes,
      unsentFrames: Math.max(0, this.handedFrames - this.sentFrames),
      bufferedBytes: this.bufferedBytes(),
      readyCount: this.readyCount,
      interimCount: this.interimCount,
      finalCount: this.finalCount,
      errorCount: this.errorCount,
      doneCount: this.doneCount,
      eventsDropped: this.eventsDropped,
      lostReports: this.lostReports,
      lastSendMs: safeCount(this.lastSendMs),
      lastReceiveMs: safeCount(this.lastReceiveMs),
      terminalOutcome: this.terminalOutcome,
      events: this.events,
    };
  }

  /**
   * The counters-only report, under the ordinary cap: the largest prefix of
   * the bounded events that fits, the rest waiting for the next batch.
   */
  buildNumeric(scalars) {
    let events = this.events;
    let body = JSON.stringify({ ...scalars, events });
    if (utf8Bytes(body) > BODY_LIMIT) {
      let low = 0;
      let high = events.length;
      while (low < high) {
        const middle = Math.ceil((low + high) / 2);
        const candidate = JSON.stringify({ ...scalars, events: events.slice(0, middle) });
        if (utf8Bytes(candidate) <= BODY_LIMIT) low = middle;
        else high = middle - 1;
      }
      if (low === 0 && events.length > 0) {
        // Not one event fits next to the counters: they cannot be delivered at
        // all, so they are accounted for instead of retried forever.
        this.eventsDropped += events.length;
        events = [];
      } else {
        events = events.slice(0, low);
      }
      body = JSON.stringify({ ...scalars, events });
      if (utf8Bytes(body) > BODY_LIMIT) {
        this.disable();
        return null;
      }
    }
    return this.submit(scalars, events, body, null);
  }

  /**
   * The report that carries the take's content, under the wide cap. The events
   * are the droppable part — they are bounded and counted — while the content
   * is not: a body that cannot hold it whole is refused here, never cut.
   */
  buildContent(scalars, content) {
    let events = this.events;
    let body = JSON.stringify({ ...scalars, events, ...content });
    if (utf8Bytes(body) > CONTENT_BODY_LIMIT) {
      this.eventsDropped += events.length;
      events = [];
      body = JSON.stringify({ ...scalars, events, ...content });
      if (utf8Bytes(body) > CONTENT_BODY_LIMIT) return null;
    }
    return this.submit(scalars, events, body, content);
  }

  /**
   * Takes the built body: the batch of events it carried is consumed, the index
   * is spent and the terminal report is the one that ends the take. The payload
   * is frozen once, so a retry sends exactly these bytes again.
   */
  submit(scalars, events, body, content) {
    this.events = this.events.slice(events.length);
    this.reportIndex += 1;
    if (this.terminalOutcome !== null) this.terminalSent = true;
    const payload = { ...scalars, events: Object.freeze(events) };
    if (content !== null) {
      if (content.result !== undefined) payload.result = content.result;
      if (content.errors !== undefined) payload.errors = content.errors;
    }
    return Object.freeze({
      index: this.reportIndex,
      body,
      payload: Object.freeze(payload),
    });
  }

  bufferedBytes() {
    const socket = this.socket;
    const amount = socket && typeof socket.bufferedAmount === 'number' ? socket.bufferedAmount : 0;
    if (!Number.isFinite(amount) || amount <= 0) return 0;
    return Math.min(Number.MAX_SAFE_INTEGER, Math.floor(amount));
  }

  /**
   * Content that cannot be delivered whole is lost, never cut: the take is
   * marked as incomplete through its own counters, one technical warning names
   * the reason, and no later report of this take may carry content again. The
   * take advertises that incompleteness once: a repeated signal, or one raised
   * after the take or its reporting already ended, adds nothing to it.
   */
  loseContent(code) {
    if (this.closed || this.stopped || this.abandoned || this.contentDisabled) return;
    this.result = null;
    this.errors = [];
    this.contentDisabled = true;
    this.lostReports += 1;
    this.dirty = true;
    this.schedule(0);
    this.warn(0, code);
  }

  /** The reporter turns itself off; the capture it observed goes on. */
  disable() {
    this.closed = true;
    this.submitted = null;
    this.clearTimer();
    this.clearExpiry();
    this.warn(0, 'invalid_report');
  }

  warn(status, code) {
    const name = code ?? (typeof status === 'number' && status > 0 ? `status_${status}` : 'invalid_report');
    try {
      console.warn(`speechek diagnostics: report ${name}`);
    } catch { /* a console that refuses a warning decides nothing */ }
  }

  /**
   * The take is over. A terminal report that is still on its way keeps its
   * bounded retry loop and its independent expiry timer; everything else is
   * cleared, so no timer of this take can outlive it.
   */
  finish() {
    if (!this.stopped && !this.abandoned && this.terminalOutcome !== null
      && (this.submitted !== null || this.inflight)) return;
    this.clearTimer();
    this.clearExpiry();
  }
}

/** Every scalar is present and inside the endpoint's grammar. */
function validReport(report) {
  if (!OWNERS.has(report.owner)) return false;
  if (!Number.isSafeInteger(report.generation) || report.generation <= 0) return false;
  if (!Number.isSafeInteger(report.reportIndex) || report.reportIndex <= 0) return false;
  if (!Number.isFinite(report.clientElapsedMs) || report.clientElapsedMs < 0) return false;
  if (!STATES.has(report.state)) return false;
  if (report.terminalOutcome !== null && !TERMINAL_OUTCOMES.has(report.terminalOutcome)) return false;
  const counters = [
    report.handedFrames, report.handedSamples, report.ignoredFrames, report.sentFrames,
    report.sentPcmBytes, report.unsentFrames, report.bufferedBytes, report.readyCount,
    report.interimCount, report.finalCount, report.errorCount, report.doneCount,
    report.eventsDropped, report.lostReports,
  ];
  for (const value of counters) {
    if (!Number.isSafeInteger(value) || value < 0) return false;
  }
  if (report.lastSendMs !== null && (!Number.isFinite(report.lastSendMs) || report.lastSendMs < 0)) return false;
  if (report.lastReceiveMs !== null && (!Number.isFinite(report.lastReceiveMs) || report.lastReceiveMs < 0)) return false;
  return Array.isArray(report.events) && report.events.length <= EVENTS_LIMIT;
}

/**
 * Posts one report through the fetch the page started with, keepalive so a page
 * that is going away still delivers it. The answer is reduced to its numeric
 * status; no body, header or error text is ever read.
 */
function postReport(owner, generation, body) {
  if (nativeFetch === null) return Promise.resolve(0);
  const origin = originOf();
  if (origin === null) return Promise.resolve(0);
  try {
    const target = `${origin}${REPORT_URL}?diagnostics=${owner}:${generation}`;
    const response = nativeFetch.call(globalThis, target, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body,
      keepalive: true,
    });
    return response.then(
      (answer) => (answer && typeof answer.status === 'number' ? answer.status : 0),
      () => 0,
    );
  } catch {
    return Promise.resolve(0);
  }
}

/* -------------------------------------------------------------------------- */
/* take binding                                                                */
/* -------------------------------------------------------------------------- */

/** A binding handed over by the page and not consumed yet. */
let pendingBinding = null;
/** The take a socket being constructed right now belongs to. */
let activeToken = null;
/** Newest genuine take, kept so a late native gap or stop still finds it. */
let lastToken = null;

/**
 * An immutable take token: a start either consumed a pending native binding or
 * is a browser take, and nothing afterwards can change which one it is.
 *
 * A native key belongs to the shell's own capture, so it is only ever honoured
 * for an external start: a genuine start of any kind clears the pending
 * binding, because a browser take that inherited it would report against a
 * native archive it never fed and settle that session with its own outcome.
 */
function beginTake(options) {
  const bound = pendingBinding;
  pendingBinding = null;
  const genuine = bound !== null && options?.external === true;
  const token = {
    owner: genuine ? bound.owner : null,
    generation: genuine ? bound.generation : null,
    genuine,
    reporter: genuine ? new TakeReporter(bound.owner, bound.generation) : null,
    settled: false,
    stopObserved: false,
  };
  activeToken = token;
  if (genuine) lastToken = token;
  return token;
}

/** The reporter of the take a native hook names, never of a later one. */
function reporterFor(owner, generation) {
  if (!OWNERS.has(owner)) return null;
  if (!Number.isSafeInteger(generation) || generation <= 0) return null;
  const token = lastToken;
  if (!token || !token.genuine || !token.reporter) return null;
  if (token.owner !== owner || token.generation !== generation) return null;
  const reporter = token.reporter;
  if (reporter.closed || reporter.stopped) return null;
  return reporter;
}

/* -------------------------------------------------------------------------- */
/* global marker                                                               */
/* -------------------------------------------------------------------------- */

/**
 * One-shot pending binding for the next genuine start of this page. A repeated
 * `start` the core answers as a no-op leaves it in place, and a browser take
 * clears it instead of inheriting the native generation.
 */
function setContext(owner, generation) {
  if (!OWNERS.has(owner)) return;
  if (!Number.isSafeInteger(generation) || generation <= 0) return;
  pendingBinding = { owner, generation };
}

function nativeGap(owner, generation, expected, received, reason) {
  const reporter = reporterFor(owner, generation);
  if (!reporter) return;
  reporter.emit('native_gap', {
    expected: safeCount(expected),
    received: safeCount(received),
    code: REASON_CODES[reason] ?? 0,
    damaged: reason === 'decode',
  });
}

function nativeStop(owner, generation, last, seen, damaged) {
  const reporter = reporterFor(owner, generation);
  if (!reporter) return;
  reporter.emit('native_stop', {
    last: safeCount(last),
    seen: safeCount(seen),
    damaged: damaged === true,
  });
}

/* -------------------------------------------------------------------------- */
/* WebSocket observer                                                          */
/* -------------------------------------------------------------------------- */

/** The take a socket belongs to, frozen when the socket is constructed. */
const socketTokens = new WeakMap();

/**
 * A WebSocket subclass installed over the constructor the page started with.
 * Only a socket of a bound take is observed and tagged; every other socket —
 * the settings page's, a test's, a later browser take's — is constructed with
 * exactly the arguments it was given and never touched again.
 */
function makeObservedWebSocket(Native) {
  return class DiagnosticsWebSocket extends Native {
    constructor(url, protocols, options) {
      const token = activeToken;
      const wanted = token && token.genuine && token.reporter ? token : null;
      // Only this page's own Live socket of a bound take is observed: a foreign
      // host, another scheme or an unrelated same-origin route is constructed
      // with exactly the arguments it was given and never counted or tagged.
      const observed = wanted !== null && isOwnLiveTarget(url) ? wanted : null;
      const target = observed === null ? url : tagUrl(url, observed.owner, observed.generation);
      // The overloads the platform accepts are preserved exactly.
      if (arguments.length >= 3) super(target, protocols, options);
      else if (arguments.length === 2) super(target, protocols);
      else super(target);
      if (!observed) return;
      const reporter = observed.reporter;
      socketTokens.set(this, observed);
      reporter.attachSocket(this);
      reporter.emit('socket_open');
      const socket = this;
      // Listeners, not handlers: whatever the core assigns to `onmessage` and
      // friends stays exactly the callback it was, and keeps `this` as before.
      socket.addEventListener('message', (event) => {
        if (socketTokens.get(socket) !== observed) return;
        withDispatch(observed, () => {
          // Only `type` is read, and only a classified message moves a counter.
          const kind = receiveKind(event);
          if (kind) reporter.received(kind);
        });
      });
      socket.addEventListener('error', () => {
        if (socketTokens.get(socket) !== observed) return;
        withDispatch(observed, () => reporter.emit('socket_error'));
      });
      socket.addEventListener('close', (event) => {
        if (socketTokens.get(socket) !== observed) return;
        withDispatch(observed, () => {
          reporter.attachSocket(socket);
          reporter.emit('socket_close', { code: closeCode(event) });
        });
      });
    }

    send(data) {
      const token = socketTokens.get(this);
      const result = super.send(data);
      if (token && token.reporter && !token.reporter.closed) {
        const kind = sendKind(data);
        if (kind === 'audio') token.reporter.sentAudio(audioBytes(data));
        else if (kind === 'start' || kind === 'end') token.reporter.sentControl();
      }
      return result;
    }
  };
}

/** The receive class of a Live message; only its `type` is ever read. */
function receiveKind(event) {
  const data = event?.data;
  if (typeof data !== 'string') return null;
  let message;
  try {
    message = JSON.parse(data);
  } catch {
    return null;
  }
  const type = message && typeof message === 'object' ? message.type : null;
  if (type === 'ready' || type === 'interim' || type === 'final' || type === 'done') return type;
  if (type === 'error') return 'error';
  return null;
}

function closeCode(event) {
  const code = event && typeof event.code === 'number' ? event.code : 0;
  // Only a real u16 close code travels; anything else is omitted rather than
  // clamped or replaced, so an unknown code arrives as no code at all.
  if (!Number.isInteger(code) || code <= 0 || code > 0xffff) return null;
  return code;
}

/**
 * True for a string target that resolves to this page's own Live route. The
 * socket scheme names its counterpart origin — `ws` answers to `http`, `wss` to
 * `https` — so a socket of any other scheme, host or path stays untouched.
 */
function isOwnLiveTarget(url) {
  if (typeof url !== 'string') return false;
  const origin = originOf();
  const location = globalThis.location;
  if (origin === null || !location || typeof location.protocol !== 'string') return false;
  let resolved;
  try {
    resolved = new URL(url, origin);
  } catch {
    return false;
  }
  if (resolved.pathname !== LIVE_PATH) return false;
  const expected = location.protocol === 'https:' ? 'wss:' : 'ws:';
  if (resolved.protocol !== expected) return false;
  return `${expected === 'wss:' ? 'https:' : 'http:'}//${resolved.host}` === origin;
}

/**
 * The take whose socket is dispatching right now. A core callback raised from
 * a socket event — an error frame answered through `onmessage`, a transport
 * `onerror` — arrives while this is set, so an error of a take that is already
 * over is never counted against the take that replaced it.
 *
 * It has to stay set for the whole synchronous dispatch, because the core's own
 * `on*` handler runs after this decorator's listener. Anything that is *not*
 * that handler — a page callback, or a recorder operation a page callback
 * performs — runs under `maskDispatch`, which hides the inherited context and
 * puts it back afterwards: a new or unrelated take raising its own error from
 * inside such a call is attributed to itself, and the dispatching socket keeps
 * its context for the handler that follows.
 */
let dispatchToken = null;

function withDispatch(token, run) {
  dispatchToken = token;
  try {
    run();
  } finally {
    queueMicrotask(() => {
      if (dispatchToken === token) dispatchToken = null;
    });
  }
}

/** Hides an inherited dispatch context while page-owned code runs. */
function maskDispatch(run) {
  const saved = dispatchToken;
  dispatchToken = null;
  try {
    return run();
  } finally {
    dispatchToken = saved;
  }
}

/* -------------------------------------------------------------------------- */
/* fetch observer                                                              */
/* -------------------------------------------------------------------------- */

/**
 * The page's fetch, decorated only for the two routes of a bound take that
 * live on this same origin. Everything else — the catalog, the settings read,
 * the fixture route and the reporter's own POST — is passed through untouched,
 * and the original promise is always the one the caller gets back.
 */
function observedFetch(input, init) {
  if (nativeFetch === null) return Promise.reject(new Error('fetch is unavailable'));
  const token = activeToken;
  if (!token || !token.genuine || !token.reporter) return nativeFetch.call(globalThis, input, init);
  const raw = requestTarget(input);
  if (raw === null) return nativeFetch.call(globalThis, input, init);
  const resolved = absoluteUrl(raw);
  if (resolved === null || !isOwnPath(resolved)) return nativeFetch.call(globalThis, input, init);
  const path = resolved.pathname;
  if (path !== LIVE_URL && path !== TRANSCRIBE_URL) return nativeFetch.call(globalThis, input, init);

  const reporter = token.reporter;
  const tagged = tagUrl(resolved.href, token.owner, token.generation);
  const target = typeof input === 'string' || (typeof URL === 'function' && input instanceof URL)
    ? tagged
    : rebuildRequest(input, tagged);
  if (path === TRANSCRIBE_URL) reporter.emit('batch_begin');
  const promise = nativeFetch.call(globalThis, target, init);
  if (path !== TRANSCRIBE_URL) return promise;
  // The answer itself is never read: only that the request is over. The
  // observer is a separate, handled side chain, so the caller gets the exact
  // promise the captured fetch returned — its own response value and its own
  // exceptions — and this chain swallows its own settlement.
  promise.then(
    () => reporter.emit('batch_end'),
    () => reporter.emit('batch_error'),
  ).catch(() => {});
  return promise;
}

/** The URL of any fetch input, without reading a body or a header. */
function requestTarget(input) {
  if (typeof input === 'string') return input;
  if (typeof URL === 'function' && input instanceof URL) return input.href;
  if (input && typeof input.url === 'string') return input.url;
  return null;
}

/** A Request with the tagged URL; its body, method, headers and signal stay. */
function rebuildRequest(request, url) {
  try {
    return new Request(url, request);
  } catch {
    return request;
  }
}

/* -------------------------------------------------------------------------- */
/* install                                                                     */
/* -------------------------------------------------------------------------- */

function install() {
  const scope = globalThis;
  try {
    scope[MARKER_NAME] = { setContext, nativeGap, nativeStop };
  } catch { /* a frozen global only means no take can be bound */ }
  if (NativeWebSocket) {
    try {
      const observed = makeObservedWebSocket(NativeWebSocket);
      Object.defineProperty(observed, 'name', { value: NativeWebSocket.name || 'WebSocket', configurable: true });
      scope.WebSocket = observed;
    } catch { /* an unwrappable constructor is left exactly as it was */ }
  }
  if (nativeFetch) {
    try {
      scope.fetch = observedFetch;
    } catch { /* a frozen fetch is left exactly as it was */ }
  }
}

install();

/* -------------------------------------------------------------------------- */
/* createRecorder                                                              */
/* -------------------------------------------------------------------------- */

/**
 * The same surface as the core, decorated. `handlers` is forwarded unchanged
 * except for `onState` and `onError`, which are observed and then called with
 * exactly the arguments they always received.
 */
export function createRecorder(handlers = {}) {
  const source = handlers && typeof handlers === 'object' ? handlers : {};
  const originalState = typeof source.onState === 'function' ? source.onState : null;
  const originalError = typeof source.onError === 'function' ? source.onError : null;

  /** The take of this recorder; replaced only by a genuine new start. */
  let token = null;
  /** The core's state, mirrored from its own callbacks. */
  let mirroredState = 'idle';
  let disposed = false;
  /**
   * The error report the core delivered last and the take it belongs to. It is
   * read once more when a start rejects, because that rejection is what carries
   * the raw diagnostic: the report is completed only when the rejection names
   * the very descriptor this report was recorded from.
   */
  let lastFailure = null;

  function reporterOf(current) {
    return current && current.genuine ? current.reporter : null;
  }

  function mirror(next) {
    // Once the recorder gave up its take, no later state of the core may touch
    // a reporter again: the disposed state is the last one reported.
    if (disposed) return;
    const current = token;
    mirroredState = next;
    // A state of a token that is no longer the current one never touches the
    // take that replaced it: the idle of a settling stop is not a new start.
    if (current === null || current !== token || !current.genuine || !current.reporter) return;
    if (next === 'arming') current.reporter.emit('recorder_start');
    else if (next === 'stopping') current.reporter.emit('recorder_stop');
    else if (next === 'idle') current.reporter.emit('recorder_idle');
    current.reporter.setState(next);
  }

  const core = createCoreRecorder({
    ...source,
    onState(next) {
      try {
        mirror(next);
      } catch { /* observation never breaks a dictation */ }
      // The page's own handler runs with no inherited socket context: work it
      // starts belongs to itself, not to the event that called it.
      if (originalState) maskDispatch(() => originalState(next));
    },
    onError(...args) {
      const [name, message, ui, diagnostic] = args;
      try {
        // A core error raised from a socket callback belongs to the take that
        // socket dispatched for: an error frame or a transport error of a take
        // that is already over is never counted or recorded against a later
        // take. Only a raw diagnostic the core really passed travels with it.
        const current = dispatchToken ?? token;
        if (current !== null && current.genuine && current.reporter) {
          current.reporter.error();
          const entry = errorEntry(name, message, ui, diagnostic);
          if (entry === null) {
            lastFailure = null;
            // The core delivered something that is not the report it promises,
            // so this take's content is incomplete: it is accounted for as
            // lost instead of being recorded in a shape of the observer's own.
            current.reporter.loseContent('content_invalid');
          } else {
            const index = current.reporter.addError(entry);
            // Remembered for the rejection of a failed start, which is the one
            // place the core hands over the original raw text of this failure.
            lastFailure = index < 0 ? null : { token: current, ui, index };
          }
        }
      } catch { /* observation never breaks a dictation */ }
      // The original handler keeps its receiver, its exact arguments and its
      // own exceptions, and runs outside the dispatching socket's context.
      if (originalError) maskDispatch(() => originalError(...args));
    },
  });

  /**
   * Settles the take: the terminal outcome is recorded once, outside the
   * droppable events, and the token is only cleared while it is still current.
   */
  function settleTake(current, outcome) {
    if (current === null || !current.genuine || current.settled) return;
    current.settled = true;
    try {
      // The terminal report is handed over now rather than on the next tick,
      // and its bounded retry loop survives the end of the take.
      current.reporter.terminal(outcome);
      current.reporter.flush();
      current.reporter.finish();
    } catch { /* the capture is unaffected */ }
    if (activeToken === current) activeToken = null;
  }

  /**
   * A start that fails reports its error first and rejects with the same
   * descriptor afterwards, carrying the raw text of the original exception on
   * it. The report is completed from that rejection — and only when it names
   * the very descriptor this take recorded — so no diagnostic is ever invented
   * and the caller's rejection itself stays exactly the core's own.
   */
  function completeFailure(current, cause) {
    const failure = lastFailure;
    lastFailure = null;
    if (failure === null || failure.token !== current || !current.reporter) return;
    if (!cause || cause.ui !== failure.ui) return;
    const diagnostic = typeof cause.diagnosticMessage === 'string' ? cause.diagnosticMessage : '';
    if (diagnostic === '') return;
    current.reporter.attachDiagnostic(failure.index, diagnostic);
  }

  function implStart(options = {}) {
    const starting = !disposed && mirroredState === 'idle';
    let current = token;
    if (starting) {
      try {
        current = beginTake(options);
        token = current;
      } catch {
        current = null;
      }
    }
    const promise = core.start(options);
    if (starting && current !== null && current.genuine) {
      promise.then(
        () => {},
        (cause) => {
          try {
            completeFailure(current, cause);
          } catch { /* observation never breaks a dictation */ }
          settleTake(current, 'error');
        },
      );
    }
    return promise;
  }

  function implStop() {
    const current = token;
    const genuine = current !== null && current.genuine && !current.stopObserved
      && mirroredState !== 'idle' && mirroredState !== 'disposed';
    if (genuine) current.stopObserved = true;
    const promise = core.stop();
    if (genuine) {
      promise.then(
        (result) => {
          try {
            // The exact object the core resolved with is recorded before the
            // terminal outcome is settled, so the terminal report carries the
            // resolution of the very take whose outcome it announces.
            if (current.reporter) current.reporter.setResult(result);
          } catch { /* observation never breaks a dictation */ }
          const failed = Boolean(result && result.errors && Object.keys(result.errors).length > 0);
          settleTake(current, failed ? 'error' : 'success');
        },
        () => settleTake(current, 'error'),
      );
    }
    return promise;
  }

  function implFeed(chunk) {
    try {
      const reporter = reporterOf(token);
      if (reporter) {
        if (chunk instanceof Int16Array && chunk.length > 0) {
          if (!disposed && (mirroredState === 'recording' || mirroredState === 'stopping')) reporter.handed(chunk);
          else reporter.ignored();
        } else reporter.ignored();
      }
    } catch { /* counting never changes what the core accepts */ }
    core.acceptPcmChunk(chunk);
  }

  function implDispose() {
    const current = token;
    const reporter = reporterOf(current);
    // A take that already settled keeps the outcome it settled with: disposing
    // after that drops the recorder, not the result it produced.
    const settling = current !== null && current.genuine && !current.settled;
    if (settling) {
      current.settled = true;
      try {
        reporter.emit('recorder_dispose');
        reporter.terminal('cancelled');
        reporter.setState('disposed');
      } catch { /* the take is dropped either way */ }
    }
    if (activeToken === current) activeToken = null;
    // The core settles its own state to idle here; the guard in `mirror` keeps
    // that from overwriting the disposed state of the take being dropped.
    disposed = true;
    mirroredState = 'disposed';
    core.dispose();
    if (settling) {
      // Best effort: the cancelled take's terminal report goes out before the
      // reporter stops scheduling, and its retries stay bounded by its grace.
      try {
        reporter.flush();
      } catch { /* the take is dropped either way */ }
      reporter.finish();
    }
  }

  /*
   * The page's own calls run with no inherited socket context: a take started,
   * fed, stopped or dropped here — even from inside a late callback of an older
   * socket — is attributed to itself and never to the event that led here.
   */
  function start(options = {}) {
    return maskDispatch(() => implStart(options));
  }

  function stop() {
    return maskDispatch(() => implStop());
  }

  function acceptPcmChunk(chunk) {
    return maskDispatch(() => implFeed(chunk));
  }

  function dispose() {
    return maskDispatch(() => implDispose());
  }

  return { start, stop, acceptPcmChunk, dispose };
}
