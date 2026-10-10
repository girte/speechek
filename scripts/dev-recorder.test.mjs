#!/usr/bin/env bun
/**
 * scripts/dev-recorder.test.mjs — checks for the Development-only diagnostics
 * decorator in `public/dev-recorder.js`.
 *
 * Two branches share this file:
 *
 *   - The unit branch (no arguments) imports `bun:test` dynamically and runs
 *     fixtures against a fresh temporary module tree per fixture: an unmodified
 *     copy of `dev-recorder.js`, the original `public/recorder.js` served as
 *     `recorder-core.js`, and `public/i18n.js`. Every global the decorator reads
 *     (fetch, WebSocket, location, performance, timers, console and the
 *     diagnostics marker) is installed before the import and restored in
 *     `finally`; only that fixture's root is removed.
 *
 *   - The smoke branch (`--smoke-url <origin> --owner <o> --generation <n>`)
 *     never imports `bun:test`. It runs against a real server: it installs a
 *     fetch adapter resolving relative targets against the origin and adding an
 *     `Origin` header, binds a take, starts an external Live session, waits for
 *     the server's `ready` before feeding any audio, feeds the exact frames the
 *     fixture serves (one frame per chunk, never a second PCM conversion), stops
 *     and waits for the final report to be admitted.
 *
 * Both branches only ever print counts and numeric statuses.
 */

import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

/* -------------------------------------------------------------------------- */
/* sources and temporary module trees                                          */
/* -------------------------------------------------------------------------- */

const REPO_ROOT = path.resolve(import.meta.dir, '..');
const PUBLIC_DIR = path.join(REPO_ROOT, 'public');
const FIXTURE_PREFIX = 'speechek-dev-recorder-';

const DEV_SOURCE = readFileSync(path.join(PUBLIC_DIR, 'dev-recorder.js'), 'utf8');
const CORE_SOURCE = readFileSync(path.join(PUBLIC_DIR, 'recorder.js'), 'utf8');
const I18N_SOURCE = readFileSync(path.join(PUBLIC_DIR, 'i18n.js'), 'utf8');
/** The real catalog, so a fixture renders the same text a page would. */
const CATALOG_SOURCE = readFileSync(path.join(PUBLIC_DIR, 'messages.json'), 'utf8');
const CATALOG = JSON.parse(CATALOG_SOURCE);
const SETTINGS_SOURCE = JSON.stringify({ settings: { language: 'en' }, runtimeRevision: 1 });


/**
 * A unique module tree per fixture: no module cache entry and no global state
 * can survive from one fixture into the next. A fixture may serve the real core
 * under another name and put its own `recorder-core.js` in front of it: that
 * wrapper still runs the untouched recorder, and only the shape it answers a
 * call with is the fixture's own.
 */
function makeFixtureTree(files = null) {
  const root = mkdtempSync(path.join(tmpdir(), FIXTURE_PREFIX));
  writeFileSync(path.join(root, 'dev-recorder.js'), DEV_SOURCE);
  writeFileSync(path.join(root, 'recorder-core.js'), files?.core ?? CORE_SOURCE);
  for (const [name, source] of Object.entries(files?.extra ?? {})) {
    writeFileSync(path.join(root, name), source);
  }
  writeFileSync(path.join(root, 'i18n.js'), I18N_SOURCE);
  return root;
}

const realSetTimeout = globalThis.setTimeout;
const realClearTimeout = globalThis.clearTimeout;
const realSetImmediate = typeof globalThis.setImmediate === 'function'
  ? globalThis.setImmediate
  : (fn) => realSetTimeout(fn, 0);

/* -------------------------------------------------------------------------- */
/* globals harness                                                             */
/* -------------------------------------------------------------------------- */

/** Installs a global and answers the restore step. */
function setGlobal(name, value) {
  const had = Object.prototype.hasOwnProperty.call(globalThis, name);
  const previous = had ? globalThis[name] : undefined;
  try {
    globalThis[name] = value;
  } catch {
    Object.defineProperty(globalThis, name, { value, configurable: true, writable: true });
  }
  return () => {
    try {
      if (had) globalThis[name] = previous;
      else delete globalThis[name];
    } catch { /* a global that refuses to be restored decides nothing */ }
  };
}

/**
 * A WebSocket double: it records what it was constructed with and replays
 * events in the order the platform really dispatches them — every listener and
 * every `on*` handler in one registration-ordered list, so a listener added at
 * construction runs before a handler the core assigns afterwards.
 */
function makeFakeWebSocketClass() {
  return class FakeWebSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    static instances = [];

    constructor(url, protocols, options) {
      this.url = url;
      this.protocols = protocols;
      this.options = options;
      this.readyState = FakeWebSocket.OPEN;
      this.bufferedAmount = 0;
      this.sent = [];
      /** type -> ordered dispatch entries `{ fn, handler }`. */
      this.entries = new Map();
      /** type -> the single entry the `on*` property owns. */
      this.handlers = new Map();
      for (const type of ['open', 'message', 'error', 'close']) {
        Object.defineProperty(this, `on${type}`, {
          get: () => this.handlers.get(type)?.fn ?? null,
          set: (fn) => this.setHandler(type, fn),
          configurable: true,
        });
      }
      FakeWebSocket.instances.push(this);
    }

    /** The `on*` property keeps its slot in the list, like a real handler. */
    setHandler(type, fn) {
      const handler = typeof fn === 'function' ? fn : null;
      let entry = this.handlers.get(type);
      if (entry === undefined) {
        entry = { fn: handler, handler: true };
        this.handlers.set(type, entry);
        this.listOf(type).push(entry);
        return;
      }
      entry.fn = handler;
    }

    listOf(type) {
      const list = this.entries.get(type);
      if (list !== undefined) return list;
      const fresh = [];
      this.entries.set(type, fresh);
      return fresh;
    }

    send(data) {
      this.sent.push(data);
      if (typeof this.onSend === 'function') this.onSend(data);
    }

    close() {
      if (this.readyState === FakeWebSocket.CLOSED) return;
      this.readyState = FakeWebSocket.CLOSED;
      this.emit('close', { code: 1000 });
    }

    addEventListener(type, listener) {
      if (typeof listener !== 'function') return;
      this.listOf(type).push({ fn: listener, handler: false });
    }

    removeEventListener(type, listener) {
      this.entries.set(type, this.listOf(type).filter((entry) => entry.fn !== listener));
    }

    /** Replays an event to every entry of this type, in registration order. */
    emit(type, event = {}) {
      for (const entry of [...this.listOf(type)]) {
        if (typeof entry.fn === 'function') entry.fn.call(this, event);
      }
    }
  };
}

/**
 * One isolated page: a temporary module tree, the globals the decorator reads,
 * a virtual clock, and the record of every fetch, socket and report.
 */
function makeHarness(options = {}) {
  const origin = options.origin ?? 'http://127.0.0.1:4173';

  const root = makeFixtureTree(options.files ?? null);
  const restores = [];

  /* ---------------------------------------------------------------- */
  /* records                                                           */
  /* ---------------------------------------------------------------- */

  const calls = [];
  const warnings = [];
  const complaints = [];
  const reports = [];
  let reportCount = 0;


  /* ---------------------------------------------------------------- */
  /* clock                                                             */
  /* ---------------------------------------------------------------- */

  let clock = 0;
  const timers = new Map();
  let timerSeq = 0;
  let virtualTimers = false;

  const fakePerformance = { now: () => clock };

  /* ---------------------------------------------------------------- */
  /* fetch                                                             */
  /* ---------------------------------------------------------------- */

  const plan = options.report ?? (() => ({ status: 202 }));

  async function pageFetch(input, init) {
    const url = typeof input === 'string'
      ? input
      : input instanceof URL ? input.href : input?.url ?? '';
    const method = init?.method ?? 'GET';
    calls.push({ url, method, body: init?.body ?? null, keepalive: init?.keepalive === true });
    // Every attempt is recorded, with its status once it is known: a retry of
    // the same payload is exactly what a fixture has to be able to compare.
    if (url.includes('/api/dev-diagnostics')) {
      reportCount += 1;
      const attempt = {
        url,
        body: typeof init?.body === 'string' ? init.body : '',
        status: 0,
        keepalive: init?.keepalive === true,
      };
      try {
        attempt.payload = JSON.parse(attempt.body);
      } catch {
        attempt.payload = null;
      }
      reports.push(attempt);
      const answer = plan({ url, body: init?.body ?? null }, reportCount);
      const resolved = typeof answer?.then === 'function' ? await answer : answer;
      if (resolved instanceof Error) throw resolved;
      attempt.status = typeof resolved === 'number' ? resolved : resolved?.status ?? 202;
      return new Response('', { status: attempt.status });
    }
    // The page's own boot reads: a real catalog and a valid settings view, so
    // the recorder renders the text a real page would.
    if (url.endsWith('/messages.json')) {
      return new Response(CATALOG_SOURCE, { status: 200, headers: { 'Content-Type': 'application/json' } });
    }
    if (url.endsWith('/api/settings')) {
      return new Response(SETTINGS_SOURCE, { status: 200, headers: { 'Content-Type': 'application/json' } });
    }
    return new Response('{}', { status: 200 });
  }

  /* ---------------------------------------------------------------- */
  /* install                                                           */
  /* ---------------------------------------------------------------- */

  const FakeWebSocket = options.WebSocketClass ?? makeFakeWebSocketClass();
  restores.push(setGlobal('fetch', pageFetch));
  restores.push(setGlobal('WebSocket', FakeWebSocket));
  restores.push(setGlobal('location', {
    origin,
    protocol: 'http:',
    host: new URL(origin).host,
    href: `${origin}/`,
  }));
  restores.push(setGlobal('performance', fakePerformance));
  restores.push(setGlobal('setTimeout', (fn, ms = 0, ...args) => {
    if (!virtualTimers) return realSetTimeout(fn, ms, ...args);
    timerSeq += 1;
    timers.set(timerSeq, { id: timerSeq, at: clock + Math.max(0, Number(ms) || 0), fn, args });
    return timerSeq;
  }));
  restores.push(setGlobal('clearTimeout', (handle) => {
    if (timers.has(handle)) timers.delete(handle);
    else realClearTimeout(handle);
  }));
  restores.push(setGlobal('__speechekDevDiagnostics', undefined));
  const realWarn = console.warn;
  const realError = console.error;
  console.warn = (...args) => { warnings.push(args.join(' ')); };
  console.error = (...args) => { complaints.push(args.join(' ')); };
  restores.push(() => {
    console.warn = realWarn;
    console.error = realError;
  });


  const moduleUrl = pathToFileURL(path.join(root, 'dev-recorder.js')).href;

  /* ---------------------------------------------------------------- */
  /* helpers                                                           */
  /* ---------------------------------------------------------------- */

  async function flush(times = 8) {
    for (let index = 0; index < times; index += 1) {
      await new Promise((resolve) => realSetImmediate(resolve));
    }
  }

  /** Moves the clock and runs every virtual timer that is due. */
  async function advance(ms) {
    clock += ms;
    for (;;) {
      const due = [...timers.values()].filter((timer) => timer.at <= clock).sort((a, b) => a.at - b.at);
      if (due.length === 0) break;
      const timer = due[0];
      timers.delete(timer.id);
      try {
        timer.fn(...timer.args);
      } catch { /* a fixture callback may throw; the clock goes on */ }
      await flush(4);
    }
    await flush();
  }

  function bind(owner, generation) {
    globalThis.__speechekDevDiagnostics.setContext(owner, generation);
  }

  async function dispose() {
    for (const restore of restores.reverse()) restore();
    timers.clear();
    await flush(2);
    try {
      rmSync(root, { recursive: true, force: true });
    } catch { /* the fixture root is the only thing removed */ }
  }
  return {
    calls,
    reports,
    warnings,
    complaints,
    FakeWebSocket,
    load: () => import(moduleUrl),
    /**
     * Boots the fixture's own copy of `i18n.js` against the real catalog, so the
     * recorder renders the same text a page would and no fixture sees a fake
     * untranslated key.
     */
    async initI18n() {
      const i18n = await import(pathToFileURL(path.join(root, 'i18n.js')).href);
      return i18n.initI18n();
    },
    flush,
    advance,
    bind,
    dispose,
    /** Virtual timers start only once a fixture asks for them. */
    useVirtualTimers() {
      virtualTimers = true;
      clock = 0;
    },
    timerCount: () => timers.size,
    reportCount: () => reportCount,
  };
}

/** Runs one fixture with its own tree and restores every global afterwards. */
async function withHarness(options, run) {
  const harness = makeHarness(options);
  try {
    await run(harness);
  } finally {
    await harness.dispose();
  }
}

/* -------------------------------------------------------------------------- */
/* unit fixtures                                                               */
/* -------------------------------------------------------------------------- */

async function runUnit() {
  // The unit branch is the only one that may import the test runner.
  const { test, expect } = await import('bun:test');
  /**
   * Boots the fixture's real catalog and returns the decorator of its own
   * module tree. The decorator imports the same `i18n.js` instance, so the
   * recorder renders the catalog text from here on.
   */
  async function load(harness) {
    const initialized = await harness.initI18n();
    expect(initialized.catalogReady).toBe(true);
    expect(harness.complaints).toEqual([]);
    return harness.load();
  }

  /** A bound external take that is recording and has a socket. */
  async function boundTake(harness, module, generation = 1, modes = ['live']) {
    harness.useVirtualTimers();
    const recorder = module.createRecorder({});
    harness.bind('dictation', generation);
    const started = recorder.start({ external: true, modes, sessionId: generation });
    await harness.flush();
    await started;
    await harness.flush();
    const socket = harness.FakeWebSocket.instances[harness.FakeWebSocket.instances.length - 1];
    return { recorder, socket, started };
  }

  function message(socket, payload) {
    socket.emit('message', { data: JSON.stringify(payload) });
  }

  /** One external PCM frame of 16 kHz mono; long enough for the core's minimum. */
  function pcm(samples = 1600) {
    const length = Math.max(samples, 1600);
    return new Int16Array(Array.from({ length }, (_value, index) => index % 100));
  }

  /** Hands the recorder `frames` separate frames: one call is one frame. */
  function feed(recorder, frames, samples = 1600) {
    for (let index = 0; index < frames; index += 1) recorder.acceptPcmChunk(pcm(samples));
  }

  test('a bound take transcribes the text it recorded and reports no error', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module);
      // The take really opens the Live socket of this page's origin.
      expect(socket.url).toContain('/api/live');
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      message(socket, { type: 'final', text: 'fixture text' });
      message(socket, { type: 'done' });
      const result = await recorder.stop();
      expect(result.results.live).toBe('fixture text');
      expect(result.errors).toEqual({});
      expect(result.errorDetails).toEqual({});
      // A stop after the take settled is the core's own no-op, not a second
      // transcription: an empty result and no request.
      const settled = await recorder.stop();
      expect(settled.results).toEqual({});
      recorder.dispose();
    });
  });

  test('a take without a binding leaves fetch and sockets untouched', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const recorder = module.createRecorder({});
      // No setContext: this is the ordinary browser take.
      const started = recorder.start({ external: true, modes: ['smart', 'live'] });
      await harness.flush();
      await started;
      const socket = harness.FakeWebSocket.instances.at(-1);
      expect(socket.url).toBe('ws://127.0.0.1:4173/api/live');
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      message(socket, { type: 'done' });
      await recorder.stop();
      await harness.advance(5000);
      const tagged = harness.calls.filter((call) => call.url.includes('diagnostics='));
      expect(tagged).toHaveLength(0);
      expect(harness.reportCount()).toBe(0);
      recorder.dispose();
    });
  });

  test('a bound take tags its socket and reports counters and boundary events', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 7);
      expect(socket.url).toBe('ws://127.0.0.1:4173/api/live?session=7&diagnostics=dictation:7');
      message(socket, { type: 'ready' });
      message(socket, { type: 'interim', text: 'SECRET_TRANSCRIPT' });
      feed(recorder, 3);
      message(socket, { type: 'final', text: 'SECRET_TRANSCRIPT' });
      message(socket, { type: 'done' });
      const result = await recorder.stop();
      expect(result.results.live).toBe('SECRET_TRANSCRIPT');
      await harness.advance(1000);
      expect(harness.reportCount()).toBeGreaterThan(0);
      const report = harness.reports.at(-1).payload;
      expect(report.owner).toBe('dictation');
      expect(report.generation).toBe(7);
      expect(report.handedFrames).toBe(3);
      expect(report.handedSamples).toBe(3 * 1600);
      expect(report.ignoredFrames).toBe(0);
      expect(report.sentFrames).toBe(3);
      expect(report.sentPcmBytes).toBe(3 * 3200);
      expect(report.unsentFrames).toBe(0);
      expect(report.readyCount).toBe(1);
      expect(report.interimCount).toBe(1);
      expect(report.finalCount).toBe(1);
      expect(report.errorCount).toBe(0);
      expect(report.doneCount).toBe(1);
      expect(report.state).toBe('idle');
      expect(report.terminalOutcome).toBe('success');
      const names = report.events.map((entry) => entry.event);
      for (const name of ['recorder_start', 'socket_open', 'socket_ready', 'socket_done', 'recorder_stop', 'recorder_idle']) {
        expect(names).toContain(name);
      }
      // The take's own content now travels: the transcript it produced is in
      // the terminal report complete and unredacted, while no URL, host,
      // argument or carrier field ever travels with it.
      const wire = harness.reports.map((entry) => entry.body).join('\n');
      expect(wire).toContain('SECRET_TRANSCRIPT');
      expect(wire).not.toContain('ws://');
      expect(wire).not.toContain('/api/live');
      expect(wire).not.toContain('127.0.0.1');
      expect(wire).not.toContain('session=');
      const terminal = harness.reports
        .filter((entry) => entry.payload.terminalOutcome !== null).at(-1).payload;
      expect(terminal.result.results.live).toBe('SECRET_TRANSCRIPT');
      expect(terminal.result.errors).toEqual({});
      expect(terminal.result.errorDetails).toEqual({});
      for (const entry of report.events) {
        for (const forbidden of ['message', 'url', 'key', 'text', 'args', 'payload']) {
          expect(Object.prototype.hasOwnProperty.call(entry, forbidden)).toBe(false);
        }
      }
      recorder.dispose();
    });
  });

  test('repeated start returns the core promise and never resets counters', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket, started } = await boundTake(harness, module, 3);
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      // The core answers a repeated start with the promise of the take that is
      // already running: the same object, and no counter is reset.
      const again = recorder.start({ external: true, modes: ['live'] });
      expect(again).toBe(started);
      await harness.flush();
      feed(recorder, 3);
      await harness.advance(1000);
      const report = harness.reports.at(-1).payload;
      expect(report.handedFrames).toBe(5);
      expect(report.reportIndex).toBe(1);
      recorder.dispose();
    });
  });

  test('idle before settlement never clears the take that replaced it', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket, started } = await boundTake(harness, module, 1);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      message(socket, { type: 'final', text: 'first take' });
      message(socket, { type: 'done' });
      const stopping = recorder.stop();
      // The page binds the next take while this one is still winding down: the
      // core answers a no-op start, so the pending binding must stay untouched.
      harness.bind('dictation', 2);
      const noop = recorder.start({ external: true, modes: ['live'] });
      await harness.flush();
      expect(noop).toBe(started);
      await stopping;
      await harness.flush();
      expect(harness.FakeWebSocket.instances).toHaveLength(1);
      // Now the recorder is idle again: the same binding must start take two.
      const second = recorder.start({ external: true, modes: ['live'] });
      await harness.flush();
      await second;
      const sockets = harness.FakeWebSocket.instances;
      expect(sockets).toHaveLength(2);
      expect(sockets[1].url).toContain('diagnostics=dictation:2');
      await harness.advance(1000);
      const terminals = harness.reports.map((entry) => entry.payload.terminalOutcome);
      expect(terminals).toContain('success');
      recorder.dispose();
    });
  });

  test('a failed start of a bound native take settles the take as an error', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      harness.useVirtualTimers();
      // The socket the core opens when the native take arms cannot be built:
      // that failure is the take's own, and it has to reach the archive.
      const Throwing = class extends harness.FakeWebSocket {
        constructor() {
          super('about:blank');
          throw new Error('fixture socket failure');
        }
      };
      const restoreSocket = setGlobal('WebSocket', Throwing);
      try {
        const recorder = module.createRecorder({});
        harness.bind('dictation', 4);
        let failure = null;
        try {
          await recorder.start({ external: true, modes: ['live'], sessionId: 4 });
        } catch (cause) {
          failure = cause;
        }
        expect(failure).toBeInstanceOf(Error);
        expect(failure.diagnosticMessage).toContain('fixture socket failure');
        await harness.advance(1000);
        const report = harness.reports.at(-1).payload;
        expect(report.generation).toBe(4);
        expect(report.terminalOutcome).toBe('error');
        expect(report.errorCount).toBeGreaterThan(0);
        recorder.dispose();
      } finally {
        restoreSocket();
      }
    });
  });

  test('a stop whose errors map is not empty settles as an error', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 5);
      message(socket, { type: 'ready' });
      feed(recorder, 1, 2000);
      message(socket, { type: 'error', message: 'SECRET_SERVICE_DETAIL' });
      const result = await recorder.stop();
      expect(Object.keys(result.errors)).toContain('live');
      // The real catalog is loaded: the report is the translated sentence with
      // its detail substituted, never the raw key and never a placeholder.
      expect(result.errors.live).toBe(
        CATALOG.RecorderLiveServiceError.en.replace('{detail}', 'SECRET_SERVICE_DETAIL'),
      );
      expect(result.errors.live).not.toContain('RecorderLiveServiceError');
      // A take that only had its own server error produces no i18n diagnostic.
      expect(harness.complaints).toEqual([]);
      await harness.advance(1000);
      const report = harness.reports.at(-1).payload;
      expect(report.terminalOutcome).toBe('error');
      expect(report.errorCount).toBe(1);
      // The raw server detail is content now: it travels complete and
      // unredacted inside the descriptor the page keeps for a re-render.
      const wire = harness.reports.map((entry) => entry.body).join('\n');
      expect(wire).toContain('SECRET_SERVICE_DETAIL');
      expect(report.result.errors.live).toBe(
        CATALOG.RecorderLiveServiceError.en.replace('{detail}', 'SECRET_SERVICE_DETAIL'),
      );
      expect(report.result.errorDetails.live).toEqual({
        key: 'RecorderLiveServiceError',
        args: { detail: 'SECRET_SERVICE_DETAIL' },
      });
      const reported = report.errors.find((entry) => entry.mode === 'live');
      expect(reported.ui.args.detail).toBe('SECRET_SERVICE_DETAIL');
      // The report is time-addressable: it carries the elapsed milliseconds of
      // the socket callback that raised it, as a real non-negative number.
      expect(Number.isFinite(reported.atMs)).toBe(true);
      expect(reported.atMs).toBeGreaterThanOrEqual(0);
      // The core handed over no raw diagnostic of its own for this report.
      expect(reported.diagnostic).toBeUndefined();
      recorder.dispose();
    });
  });

  test('dispose cancels only the active take and keeps a settled result', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 9);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      message(socket, { type: 'final', text: 'first take' });
      message(socket, { type: 'done' });
      await recorder.stop();
      await harness.advance(1000);
      const settled = harness.reports.at(-1).payload;
      expect(settled.terminalOutcome).toBe('success');

      harness.bind('dictation', 10);
      const second = recorder.start({ external: true, modes: ['live'] });
      await harness.flush();
      await second;
      recorder.dispose();
      await harness.advance(1000);
      const cancelled = harness.reports.at(-1).payload;
      expect(cancelled.terminalOutcome).toBe('cancelled');
      expect(cancelled.state).toBe('disposed');
      expect(cancelled.events.map((entry) => entry.event)).toContain('recorder_dispose');
      // The settled take of the same recorder keeps its own outcome.
      const outcomes = harness.reports.map((entry) => entry.payload.terminalOutcome);
      expect(outcomes.filter((outcome) => outcome === 'success').length).toBeGreaterThan(0);
      // The reporter must not hold a timer past its grace; the core's own drain
      // window is still open here, so the clock runs past both.
      await harness.advance(30000);
      expect(harness.timerCount()).toBe(0);
    });
  });

  test('handed and ignored frames are counted separately', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 11);
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      recorder.acceptPcmChunk(new Uint8Array(4));
      recorder.acceptPcmChunk(new Int16Array(0));
      await harness.advance(1000);
      const report = harness.reports.at(-1).payload;
      expect(report.handedFrames).toBe(2);
      expect(report.ignoredFrames).toBe(2);
      recorder.dispose();
    });
  });

  test('a report is held until it is acknowledged, then the next index starts', async () => {
    let release = null;
    await withHarness({
      report: () => new Promise((resolve) => { release = resolve; }),
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 12);
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      await harness.advance(1000);
      expect(harness.reportCount()).toBe(1);
      await harness.advance(3000);
      expect(harness.reportCount()).toBe(1);
      release({ status: 202 });
      await harness.flush();
      feed(recorder, 1);
      await harness.advance(1000);
      expect(harness.reportCount()).toBeGreaterThan(1);
      const indices = harness.reports.map((entry) => entry.payload.reportIndex);
      expect(indices[0]).toBe(1);
      expect(indices[indices.length - 1]).toBeGreaterThan(1);
      recorder.dispose();
    });
  });

  test('429 keeps the identical payload and index for the next tick', async () => {
    await withHarness({
      report: (_request, count) => ({ status: count <= 2 ? 429 : 202 }),
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 13);
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      await harness.advance(1000);
      await harness.advance(1000);
      expect(harness.reportCount()).toBe(2);
      expect(harness.reports[1].body).toBe(harness.reports[0].body);
      expect(harness.reports[1].payload.reportIndex).toBe(1);
      expect(harness.reports[0].keepalive).toBe(true);
      await harness.advance(1000);
      expect(harness.reportCount()).toBe(3);
      expect(harness.reports[2].status).toBe(202);
      feed(recorder, 1);
      await harness.advance(1000);
      expect(harness.reports.at(-1).payload.reportIndex).toBe(2);
      recorder.dispose();
    });
  });

  test('stale_report acknowledges the report and moves on', async () => {
    await withHarness({ report: () => ({ status: 200 }) }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 14);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      await harness.advance(1000);
      expect(harness.reports[0].payload.reportIndex).toBe(1);
      feed(recorder, 1);
      await harness.advance(1000);
      expect(harness.reports.at(-1).payload.reportIndex).toBe(2);
      recorder.dispose();
    });
  });

  test('a network failure is retried with the same payload', async () => {
    await withHarness({
      report: (_request, count) => (count === 1 ? new Error('fixture offline') : { status: 202 }),
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 15);
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      await harness.advance(1000);
      await harness.advance(1000);
      expect(harness.reportCount()).toBe(2);
      expect(harness.reports[1].body).toBe(harness.reports[0].body);
      expect(harness.reports[1].payload.reportIndex).toBe(1);
      recorder.dispose();
    });
  });

  test('a full queue after the terminal ends as bounded lost reports', async () => {
    await withHarness({ report: () => ({ status: 429 }) }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 16);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      message(socket, { type: 'final', text: 'bounded' });
      message(socket, { type: 'done' });
      await recorder.stop();
      await harness.advance(10000);
      expect(harness.reportCount()).toBeLessThanOrEqual(5);
      expect(harness.reportCount()).toBeGreaterThan(0);
      // The bounded retry loop is over by now: nothing is still scheduled.
      await harness.advance(30000);
      expect(harness.timerCount()).toBe(0);
    });
  });

  test('a stale session stops scheduling and leaves the audio result alone', async () => {
    await withHarness({ report: () => ({ status: 410 }) }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 17);
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      message(socket, { type: 'final', text: 'kept' });
      message(socket, { type: 'done' });
      const result = await recorder.stop();
      expect(result.results.live).toBe('kept');
      await harness.advance(10000);
      expect(harness.reportCount()).toBe(1);
      // A stopped reporter schedules nothing, now or later.
      await harness.advance(30000);
      expect(harness.timerCount()).toBe(0);
      recorder.dispose();
    });
  });

  test('a refused report stops scheduling with one technical warning', async () => {
    await withHarness({ report: () => ({ status: 413 }) }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 18);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      await harness.advance(5000);
      expect(harness.reportCount()).toBe(1);
      expect(harness.warnings.length).toBe(1);
      expect(harness.warnings[0]).toContain('413');
      recorder.dispose();
    });
  });

  test('every report stays inside the body cap and the event bound', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 19);
      message(socket, { type: 'ready' });
      const marker = globalThis.__speechekDevDiagnostics;
      // Twenty-five boundary events: at most sixteen may travel per report and
      // the overflow has to be accounted for.
      for (let index = 0; index < 25; index += 1) {
        marker.nativeGap('dictation', 19, Number.MAX_SAFE_INTEGER, index, 'sequence');
      }
      await harness.advance(1000);
      expect(harness.reports.length).toBeGreaterThan(0);
      for (const entry of harness.reports) {
        expect(new TextEncoder().encode(entry.body).length).toBeLessThanOrEqual(4096);
        expect(entry.payload.events.length).toBeLessThanOrEqual(16);
      }
      const last = harness.reports.at(-1).payload;
      // Every one of the twenty-five injected events is either delivered in the
      // bounded batch or accounted for as dropped, beside the lifecycle events
      // the take itself produced.
      const delivered = last.events.filter((entry) => entry.event === 'native_gap').length;
      expect(last.events.length).toBeLessThanOrEqual(16);
      expect(delivered + last.eventsDropped).toBe(25);
      expect(last.eventsDropped).toBeGreaterThan(0);
      expect(last.terminalOutcome).toBeNull();
      recorder.dispose();
      await harness.advance(1000);
      const terminal = harness.reports.at(-1).payload;
      expect(terminal.terminalOutcome).toBe('cancelled');
      for (const entry of harness.reports) {
        expect(new TextEncoder().encode(entry.body).length).toBeLessThanOrEqual(4096);
      }
    });
  });

  test('gap and stop hooks only answer for their own bound key', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 21);
      message(socket, { type: 'ready' });
      const marker = globalThis.__speechekDevDiagnostics;
      marker.nativeGap('lab', 21, 5, 4, 'sequence');
      marker.nativeGap('dictation', 22, 5, 4, 'sequence');
      marker.nativeStop('lab', 21, 9, 9, false);
      marker.nativeGap('dictation', 21, 5, 4, 'sequence');
      marker.nativeGap('dictation', 21, 6, 4, 'decode');
      marker.nativeStop('dictation', 21, 9, 9, false);
      marker.nativeGap('dictation', 21, 7, 6, 'not-a-reason');
      await harness.advance(1000);
      const events = harness.reports.flatMap((entry) => entry.payload.events);
      expect(events.filter((entry) => entry.event === 'native_gap')).toHaveLength(3);
      expect(events.filter((entry) => entry.event === 'native_stop')).toHaveLength(1);
      const gaps = events.filter((entry) => entry.event === 'native_gap');
      expect(gaps[0]).toMatchObject({ expected: 5, received: 4, code: 1 });
      expect(gaps[1]).toMatchObject({ expected: 6, received: 4, code: 2, damaged: true });
      expect(gaps[2].code).toBe(0);
      const stops = events.filter((entry) => entry.event === 'native_stop');
      expect(stops[0]).toMatchObject({ last: 9, seen: 9 });
      recorder.dispose();
    });
  });

  test('late callbacks of an old socket never touch a later take', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket: first } = await boundTake(harness, module, 23);
      message(first, { type: 'ready' });
      feed(recorder, 1);
      message(first, { type: 'final', text: 'first take' });
      message(first, { type: 'done' });
      await recorder.stop();
      await harness.flush();

      harness.bind('dictation', 24);
      const second = recorder.start({ external: true, modes: ['live'] });
      await harness.flush();
      await second;
      const sockets = harness.FakeWebSocket.instances;
      expect(sockets).toHaveLength(2);
      const latest = sockets[1];
      message(latest, { type: 'ready' });
      const before = harness.reportCount();
      // A message and a close of the take that is already over.
      message(first, { type: 'final', text: 'late' });
      first.emit('close', { code: 1006 });
      await harness.advance(1000);
      // Only the newest take's reports are inspected: a late event of the take
      // that is already over may still be reported under its own generation.
      const newest = harness.reports.filter((entry) => entry.payload.generation === 24);
      expect(newest.length).toBeGreaterThan(0);
      const report = newest[newest.length - 1].payload;
      expect(report.readyCount).toBe(1);
      expect(report.finalCount).toBe(0);
      expect(report.events.filter((entry) => entry.event === 'socket_close')).toHaveLength(0);
      expect(harness.reportCount()).toBeGreaterThanOrEqual(before);
      recorder.dispose();
    });
  });

  test('the constructor, statics and overloads survive the wrapper', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const NativeSocket = harness.FakeWebSocket;
      expect(globalThis.WebSocket.OPEN).toBe(NativeSocket.OPEN);
      expect(globalThis.WebSocket.CONNECTING).toBe(0);
      expect(globalThis.WebSocket.CLOSING).toBe(2);
      expect(globalThis.WebSocket.CLOSED).toBe(3);

      const plain = new globalThis.WebSocket('ws://127.0.0.1:4173/other');
      expect(plain).toBeInstanceOf(NativeSocket);
      expect(plain.url).toBe('ws://127.0.0.1:4173/other');

      const withProtocols = new globalThis.WebSocket('ws://127.0.0.1:4173/other', ['speechek']);
      expect(withProtocols.protocols).toEqual(['speechek']);

      class Derived extends globalThis.WebSocket {}
      const derived = new Derived('ws://127.0.0.1:4173/other');
      expect(derived).toBeInstanceOf(NativeSocket);

      const recorder = module.createRecorder({});
      const { socket } = await boundTake(harness, module, 25);
      expect(socket).toBeInstanceOf(NativeSocket);
      expect(socket.url).toContain('diagnostics=dictation:25');
      recorder.dispose();
    });
  });

  test('a tag never reaches an unrelated socket or route', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const recorder = module.createRecorder({});
      harness.bind('dictation', 26);
      // A take that never starts leaves the binding pending: an unrelated socket
      // and an unrelated fetch must stay exactly as they were.
      const other = new globalThis.WebSocket('ws://127.0.0.1:4173/api/live');
      expect(other.url).toBe('ws://127.0.0.1:4173/api/live');
      await globalThis.fetch('/messages.json');
      expect(harness.calls.at(-1).url).toBe('/messages.json');
      recorder.dispose();
    });
  });

  test('an unbound recorder never reports and still returns the core result', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      harness.useVirtualTimers();
      const recorder = module.createRecorder({ onError() {} });
      const started = recorder.start({ external: true, modes: ['live'] });
      await harness.flush();
      await started;
      const socket = harness.FakeWebSocket.instances.at(-1);
      message(socket, { type: 'ready' });
      message(socket, { type: 'final', text: 'plain' });
      message(socket, { type: 'done' });
      const result = await recorder.stop();
      expect(result.results.live).toBe('plain');
      await harness.advance(3000);
      expect(harness.reportCount()).toBe(0);
      recorder.dispose();
    });
  });

  test('a bound take only observes its own Live socket', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 31);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      // A foreign host and an unrelated same-origin route, opened while the
      // take runs: neither may be tagged, counted or observed.
      const foreign = new globalThis.WebSocket('wss://other.example/socket', ['other']);
      const unrelated = new globalThis.WebSocket('ws://127.0.0.1:4173/other');
      expect(foreign.url).toBe('wss://other.example/socket');
      expect(unrelated.url).toBe('ws://127.0.0.1:4173/other');
      expect(foreign.protocols).toEqual(['other']);
      foreign.send('{"type":"audio","data":"AAAA"}');
      unrelated.emit('message', { data: JSON.stringify({ type: 'final', text: 'foreign' }) });
      unrelated.emit('close', { code: 1006 });
      await harness.advance(1000);
      const report = harness.reports.at(-1).payload;
      expect(report.generation).toBe(31);
      expect(report.finalCount).toBe(0);
      expect(report.sentFrames).toBe(1);
      expect(report.events.filter((entry) => entry.event === 'socket_close')).toHaveLength(0);
      // The take's own socket is the one that is tagged and observed.
      expect(socket.url).toBe('ws://127.0.0.1:4173/api/live?session=31&diagnostics=dictation:31');
      recorder.dispose();
    });
  });

  test('a browser take clears the pending native binding without reporting', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      harness.useVirtualTimers();
      const restoreNavigator = setGlobal('navigator', {
        mediaDevices: { getUserMedia: async () => { throw new Error('fixture denial'); } },
      });
      try {
        const recorder = module.createRecorder({});
        // The page bound a native key, then started its own microphone: the
        // binding is consumed, and that take is never reported against it.
        harness.bind('lab', 7);
        await recorder.start({ modes: ['live'] }).catch(() => {});
        // The binding was consumed by the browser take: nothing is pending.
        const started = recorder.start({ external: true, modes: ['live'] });
        await harness.flush();
        await started;
        const socket = harness.FakeWebSocket.instances.at(-1);
        expect(socket.url).toBe('ws://127.0.0.1:4173/api/live');
        message(socket, { type: 'ready' });
        feed(recorder, 1);
        message(socket, { type: 'final', text: 'browser' });
        message(socket, { type: 'done' });
        await recorder.stop();
        await harness.advance(3000);
        expect(harness.reportCount()).toBe(0);
        recorder.dispose();
      } finally {
        restoreNavigator();
      }
    });
  });

  test('a terminal report that is never answered expires inside its grace', async () => {
    let release = null;
    await withHarness({
      report: (_request, count) => (count === 1
        ? { status: 202 }
        : new Promise((resolve) => { release = resolve; })),
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 32);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      // One heartbeat is acknowledged, so what stalls below is the terminal one.
      await harness.advance(1000);
      message(socket, { type: 'final', text: 'expiring' });
      message(socket, { type: 'done' });
      await recorder.stop();
      await harness.flush();
      const stalled = harness.reportCount();
      expect(stalled).toBe(2);
      // The endpoint never answers. The reporter's own grace expires first, and
      // the core's Live drain window closes inside the same clock: what the
      // assertions below prove is the reporter's state — no further attempt,
      // no timer of its own, and a late acknowledgement that changes nothing.
      await harness.advance(25000);
      expect(harness.reportCount()).toBe(stalled);
      expect(harness.timerCount()).toBe(0);
      release?.({ status: 202 });
      await harness.flush();
      await harness.advance(5000);
      expect(harness.reportCount()).toBe(stalled);
      expect(harness.timerCount()).toBe(0);
      // The take stays silent when its socket then finishes.
      socket.emit('close', { code: 1000 });
      await harness.flush();
      expect(harness.reportCount()).toBe(stalled);
      recorder.dispose();
    });
  });

  test('a late error of an old socket never counts against a later take', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket: first } = await boundTake(harness, module, 33);
      message(first, { type: 'ready' });
      feed(recorder, 1);
      message(first, { type: 'final', text: 'first take' });
      message(first, { type: 'done' });
      await recorder.stop();
      await harness.flush();

      harness.bind('dictation', 34);
      const second = recorder.start({ external: true, modes: ['live'] });
      await harness.flush();
      await second;
      const sockets = harness.FakeWebSocket.instances;
      expect(sockets).toHaveLength(2);
      message(sockets[1], { type: 'ready' });
      // The socket of the take that is already over fails afterwards: neither
      // its transport error nor an error frame may reach the newest take.
      first.emit('error');
      message(first, { type: 'error', message: 'late failure' });
      await harness.advance(1000);
      const newest = harness.reports.filter((entry) => entry.payload.generation === 34);
      expect(newest.length).toBeGreaterThan(0);
      expect(newest[newest.length - 1].payload.errorCount).toBe(0);
      recorder.dispose();
    });
  });

  test('a real close code travels as it is and an invalid one is omitted', async () => {
    // A real code travels as it is; anything else — zero, an out-of-range or a
    // missing code — is omitted rather than clamped or invented.
    for (const [code, expected] of [[1000, 1000], [1006, 1006], [0, null], [70000, null]]) {
      await withHarness({}, async (harness) => {
        const module = await load(harness);
        const { recorder, socket } = await boundTake(harness, module, 35);
        message(socket, { type: 'ready' });
        feed(recorder, 1);
        socket.emit('close', { code });
        await harness.advance(1000);
        const closes = harness.reports
          .flatMap((entry) => entry.payload.events)
          .filter((entry) => entry.event === 'socket_close');
        expect(closes).toHaveLength(1);
        const reported = Object.prototype.hasOwnProperty.call(closes[0], 'code') ? closes[0].code : null;
        expect(reported).toBe(expected);
        recorder.dispose();
      });
    }
  });

  test('an error a new take raises inside a late old-socket callback stays on the new take', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      harness.useVirtualTimers();
      const calls = [];
      let nextStarted = false;
      /**
       * The page's own handler, exactly as a page writes it: it sees the report
       * of the take that failed, and on the first one it starts the next
       * dictation right here and feeds it before Live answers.
       */
      const recorder = module.createRecorder({
        onError(name, message, ui) {
          calls.push({ name, message, ui, argc: arguments.length });
          if (nextStarted || ui?.key !== 'RecorderLiveConnectionFailed') return;
          nextStarted = true;
          void recorder.start({ external: true, modes: ['live'] });
          for (let index = 0; index < 101; index += 1) recorder.acceptPcmChunk(pcm(1));
        },
      });

      harness.bind('dictation', 41);
      const started = recorder.start({ external: true, modes: ['live'], sessionId: 41 });
      await harness.flush();
      await started;
      const firstSocket = harness.FakeWebSocket.instances.at(-1);
      message(firstSocket, { type: 'ready' });
      feed(recorder, 1);
      message(firstSocket, { type: 'final', text: 'first take' });
      message(firstSocket, { type: 'done' });
      await recorder.stop();
      await harness.flush();

      harness.bind('dictation', 42);
      // The take that is already over fails afterwards. The page answers by
      // starting the next take and overflowing its pre-ready queue, so the new
      // take raises a genuine error of its own from inside that callback.
      firstSocket.emit('error');
      await harness.flush();
      const sockets = harness.FakeWebSocket.instances;
      expect(sockets).toHaveLength(2);
      expect(sockets[1].url).toContain('diagnostics=dictation:42');
      await harness.advance(1000);

      // The new take's own error is counted on the new take...
      const newest = harness.reports.filter((entry) => entry.payload.generation === 42);
      expect(newest.length).toBeGreaterThan(0);
      expect(newest[newest.length - 1].payload.errorCount).toBe(1);
      // ...and the take that is already over keeps its own, empty count.
      for (const entry of harness.reports.filter((row) => row.payload.generation === 41)) {
        expect(entry.payload.errorCount).toBe(0);
      }
      // The original handler saw exactly the three arguments the core passes,
      // with the real catalog text and its descriptor.
      expect(calls[0].argc).toBe(3);
      expect(calls[0].name).toBe('live');
      expect(calls[0].ui.key).toBe('RecorderLiveConnectionFailed');
      expect(calls[0].message).toBe(CATALOG.RecorderLiveConnectionFailed.en);
      expect(calls[1].ui.key).toBe('RecorderLiveServerTimeout');
      expect(calls[1].message).toBe(CATALOG.RecorderLiveServerTimeout.en);
      recorder.dispose();
    });
  });

  test('a long multi-byte result travels whole, past the ordinary cap', async () => {
    // Six bytes per unit in UTF-8, far past the numeric cap: the content of a
    // take is never the part that gets cut.
    const long = 'é😀'.repeat(3000);
    expect(new TextEncoder().encode(long).length).toBe(18000);
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 51);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      message(socket, { type: 'final', text: long });
      message(socket, { type: 'done' });
      const result = await recorder.stop();
      await harness.advance(1000);
      expect(result.results.live).toBe(long);
      // The terminal report is the only one the take sent, and it carries the
      // whole resolution under the wide cap.
      expect(harness.reports).toHaveLength(1);
      const terminal = harness.reports[0];
      const bytes = new TextEncoder().encode(terminal.body).length;
      expect(bytes).toBeGreaterThan(4096);
      expect(bytes).toBeLessThanOrEqual(1048576);
      const parsed = JSON.parse(terminal.body);
      expect(parsed.result.results.live).toBe(long);
      expect(parsed.result.results.live.length).toBe(long.length);
      expect(parsed.result.errors).toEqual({});
      expect(parsed.result.errorDetails).toEqual({});
      expect(parsed.lostReports).toBe(0);
      expect(terminal.payload.result.results.live).toBe(long);
      expect(harness.warnings).toEqual([]);
      await harness.advance(25000);
      expect(harness.reportCount()).toBe(1);
      expect(harness.timerCount()).toBe(0);
      recorder.dispose();
    });
  });

  test('a content report in flight keeps its exact bytes across a 429 retry', async () => {
    await withHarness({
      report: (_request, count) => ({ status: count <= 2 ? 429 : 202 }),
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 59);
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      // The take's own error report is content, and it is recorded before the
      // report that is in flight when the endpoint answers 429.
      message(socket, { type: 'error', message: 'RETRY_CONTENT_DETAIL' });
      await harness.advance(1000);
      await harness.advance(1000);
      expect(harness.reportCount()).toBe(2);
      // The retry carried the identical body and the identical index: the
      // payload is built once and is immutable while it is in flight.
      expect(harness.reports[1].body).toBe(harness.reports[0].body);
      expect(harness.reports[1].payload.reportIndex).toBe(harness.reports[0].payload.reportIndex);
      const parsed = JSON.parse(harness.reports[0].body);
      expect(parsed.terminalOutcome).toBeNull();
      expect(parsed.errors).toHaveLength(1);
      expect(parsed.errors[0].mode).toBe('live');
      expect(parsed.errors[0].ui.args.detail).toBe('RETRY_CONTENT_DETAIL');
      expect(harness.reports[0].body).toContain('RETRY_CONTENT_DETAIL');
      recorder.dispose();
    });
  });

  test('a multi-mode resolution is carried whole with its descriptors and its errors', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 52, ['live', 'smart']);
      message(socket, { type: 'ready' });
      feed(recorder, 2);
      message(socket, { type: 'final', text: 'live text' });
      message(socket, { type: 'done' });
      const result = await recorder.stop();
      // The fixture answers the batch route with an empty object, so that mode
      // really failed: both of its maps belong to the resolution.
      expect(result.results.live).toBe('live text');
      expect(Object.keys(result.errors)).toEqual(['smart']);
      expect(result.errorDetails.smart).toEqual({ key: 'RecorderEmptyText' });
      await harness.advance(1000);

      const terminal = harness.reports
        .filter((entry) => entry.payload.terminalOutcome === 'error').at(-1).payload;
      expect(terminal.result.results.live).toBe('live text');
      expect(terminal.result.errors.smart).toBe(CATALOG.RecorderEmptyText.en);
      expect(terminal.result.errorDetails.smart).toEqual({ key: 'RecorderEmptyText' });
      const reported = terminal.errors.find((entry) => entry.mode === 'smart');
      expect(reported.message).toBe(CATALOG.RecorderEmptyText.en);
      expect(reported.ui).toEqual({ key: 'RecorderEmptyText' });
      // The core handed over no raw diagnostic of its own for this report.
      expect(reported.diagnostic).toBeUndefined();
      recorder.dispose();
    });
  });

  test('a failed start saves its UI error detail and leaves the caller exception as it was', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      harness.useVirtualTimers();
      const original = new Error('fixture socket failure');
      const Throwing = class extends harness.FakeWebSocket {
        constructor() {
          super('about:blank');
          throw original;
        }
      };
      const restoreSocket = setGlobal('WebSocket', Throwing);
      try {
        const calls = [];
        const recorder = module.createRecorder({
          onError(name, message, ui) { calls.push({ name, message, ui, argc: arguments.length }); },
        });
        harness.bind('dictation', 53);
        let failure = null;
        try {
          await recorder.start({ external: true, modes: ['live'], sessionId: 53 });
        } catch (cause) {
          failure = cause;
        }
        // The page catches exactly the core's own failure: the same descriptor
        // object the handler saw, the same raw text and the same cause.
        expect(failure).toBeInstanceOf(Error);
        expect(failure.name).toBe('Error');
        expect(failure.ui).toEqual({ key: 'RecorderStartFailed', args: { detail: 'fixture socket failure' } });
        expect(failure.diagnosticMessage).toBe('fixture socket failure');
        expect(failure.cause).toBe(original);
        expect(calls).toHaveLength(1);
        expect(calls[0].argc).toBe(3);
        expect(calls[0].ui).toBe(failure.ui);
        expect(calls[0].message).toBe(failure.message);

        await harness.advance(1000);
        const terminal = harness.reports
          .filter((entry) => entry.payload.terminalOutcome === 'error').at(-1).payload;
        expect(terminal.errors).toHaveLength(1);
        const reported = terminal.errors[0];
        expect(reported.mode).toBe('');
        expect(reported.message).toBe(failure.message);
        expect(reported.ui).toEqual({ key: 'RecorderStartFailed', args: { detail: 'fixture socket failure' } });
        // The raw diagnostic came from the core's own rejection, not from a
        // guess about what the failure must have been. The report is stamped
        // with the moment its callback was raised, before the rejection.
        expect(reported.diagnostic).toBe('fixture socket failure');
        expect(Number.isFinite(reported.atMs)).toBe(true);
        expect(reported.atMs).toBeGreaterThanOrEqual(0);
        recorder.dispose();
      } finally {
        restoreSocket();
      }
    });
  });

  test('a late error of an old socket lands on no later take and on no content', async () => {
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket: first } = await boundTake(harness, module, 55);
      message(first, { type: 'ready' });
      feed(recorder, 1);
      message(first, { type: 'final', text: 'first take' });
      message(first, { type: 'done' });
      await recorder.stop();
      await harness.flush();

      harness.bind('dictation', 56);
      const second = recorder.start({ external: true, modes: ['live'] });
      await harness.flush();
      await second;
      const latest = harness.FakeWebSocket.instances.at(-1);
      message(latest, { type: 'ready' });
      feed(recorder, 1);
      await harness.advance(1000);
      const open = harness.reports.filter((entry) => entry.payload.generation === 56);
      expect(open.length).toBeGreaterThan(0);
      expect(open.every((entry) => entry.payload.errorCount === 0)).toBe(true);
      expect(open.every((entry) => entry.payload.errors === undefined)).toBe(true);

      // The take that is already over fails afterwards, with a text of its own:
      // neither its transport error nor its error frame may reach the newest
      // take's errors, its result or any body of it.
      first.emit('error');
      message(first, { type: 'error', message: 'LATE_OLD_FAILURE' });
      // The newest take keeps recording after that failure: the report it sends
      // next is still its own, with none of the old socket's error on it.
      feed(recorder, 1);
      await harness.advance(1000);
      const newest = harness.reports.filter((entry) => entry.payload.generation === 56);
      expect(newest.length).toBeGreaterThan(open.length);
      for (const entry of newest) {
        expect(entry.payload.errorCount).toBe(0);
        expect(entry.payload.errors).toBeUndefined();
        expect(entry.body).not.toContain('LATE_OLD_FAILURE');
      }
      // The take that is already over is over: its own reports carry neither
      // the late error nor its text.
      for (const entry of harness.reports.filter((row) => row.payload.generation === 55)) {
        expect(entry.body).not.toContain('LATE_OLD_FAILURE');
      }

      // The newest take is dropped, not stopped: its cancelled terminal carries
      // the failure of the old socket nowhere.
      recorder.dispose();
      await harness.advance(1000);
      const cancelled = harness.reports.filter((entry) => entry.payload.generation === 56).at(-1).payload;
      expect(cancelled.terminalOutcome).toBe('cancelled');
      expect(cancelled.errors).toBeUndefined();
      expect(cancelled.result).toBeUndefined();
      expect(cancelled.errorCount).toBe(0);
    });
  });

  test('content that cannot fit whole is counted as lost, never sent short', async () => {
    const huge = 'x'.repeat(1200000);
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 57);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      message(socket, { type: 'final', text: huge });
      message(socket, { type: 'done' });
      const result = await recorder.stop();
      expect(result.results.live).toBe(huge);
      await harness.advance(2000);

      const terminal = harness.reports
        .filter((entry) => entry.payload.terminalOutcome === 'success').at(-1);
      expect(terminal).toBeDefined();
      // Not one byte of a report was cut: the take advertised the loss through
      // its own counters instead, and the counters stayed inside their cap.
      expect(terminal.payload.result).toBeUndefined();
      expect(terminal.payload.errors).toBeUndefined();
      expect(terminal.payload.lostReports).toBeGreaterThan(0);
      expect(new TextEncoder().encode(terminal.body).length).toBeLessThanOrEqual(4096);
      for (const entry of harness.reports) expect(entry.body).not.toContain('xxxx');
      expect(harness.warnings.some((line) => line.includes('content_overflow'))).toBe(true);
      recorder.dispose();
    });
  });

  test('a terminal report with content that is never answered expires whole', async () => {
    let release = null;
    await withHarness({
      report: (_request, count) => (count === 1
        ? { status: 202 }
        : new Promise((resolve) => { release = resolve; })),
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 58);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      // One heartbeat is acknowledged, so what stalls below is the terminal one.
      await harness.advance(1000);
      message(socket, { type: 'final', text: 'expiring content' });
      message(socket, { type: 'done' });
      await recorder.stop();
      await harness.flush();
      const stalled = harness.reportCount();
      expect(stalled).toBe(2);
      // The attempt that is stalled carries the content complete.
      const attempt = harness.reports.at(-1);
      expect(JSON.parse(attempt.body).result.results.live).toBe('expiring content');

      // The endpoint never answers: the take's own grace ends its reporting,
      // and a late answer, a socket event or a callback changes nothing.
      await harness.advance(25000);
      expect(harness.reportCount()).toBe(stalled);
      expect(harness.timerCount()).toBe(0);
      release?.({ status: 202 });
      await harness.flush();
      await harness.advance(5000);
      expect(harness.reportCount()).toBe(stalled);
      expect(harness.timerCount()).toBe(0);
      // Every body that travelled carried the content whole; none was cut.
      for (const entry of harness.reports) {
        const parsed = JSON.parse(entry.body);
        if (parsed.result) expect(parsed.result.results.live).toBe('expiring content');
      }
      recorder.dispose();
    });
  });

  test('a resolution the core did not promise is advertised as lost, never sent partial', async () => {
    // The real recorder, wrapped: the untouched core does the whole take, and
    // only the object it resolves `stop()` with is the fixture's own — a
    // resolution that names a descriptor which cannot be JSON at all.
    const wrapper = [
      "import { createRecorder as createReal } from './recorder-real.js';",
      'export function createRecorder(handlers = {}) {',
      '  const core = createReal(handlers);',
      '  return {',
      '    start: core.start,',
      '    acceptPcmChunk: core.acceptPcmChunk,',
      '    dispose: core.dispose,',
      '    stop() {',
      '      return core.stop().then((result) => {',
      "        const cyclic = { key: 'RecorderEmptyText' };",
      '        cyclic.args = { detail: cyclic };',
      '        return { ...result, errorDetails: { live: cyclic } };',
      '      });',
      '    },',
      '  };',
      '}',
    ].join('\n');
    await withHarness({
      files: { core: wrapper, extra: { 'recorder-real.js': CORE_SOURCE } },
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 60);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      message(socket, { type: 'final', text: 'kept' });
      message(socket, { type: 'done' });
      const result = await recorder.stop();
      // The caller got exactly the wrapped resolution: it was never replaced,
      // repaired or copied into something else.
      expect(result.results.live).toBe('kept');
      expect(result.errorDetails.live.args.detail).toBe(result.errorDetails.live);
      await harness.advance(1000);

      // That content could not travel, so the take advertised it as lost: no
      // report carried a part of it, and the counters stayed inside their cap.
      const terminal = harness.reports
        .filter((entry) => entry.payload.terminalOutcome !== null).at(-1);
      expect(terminal).toBeDefined();
      expect(terminal.payload.result).toBeUndefined();
      expect(terminal.payload.errors).toBeUndefined();
      expect(terminal.payload.lostReports).toBeGreaterThan(0);
      expect(terminal.payload.terminalOutcome).toBe('success');
      expect(new TextEncoder().encode(terminal.body).length).toBeLessThanOrEqual(4096);
      for (const entry of harness.reports) expect(entry.body).not.toContain('RecorderEmptyText');
      expect(harness.warnings.some((line) => line.includes('content_invalid'))).toBe(true);
      recorder.dispose();
    });
  });

  test('nested descriptor arguments travel at their real depth', async () => {
    // A descriptor nested far deeper than a shallow copy would keep: every
    // level of it has to reach the report. The literal is written into the
    // fixture's own wrapper, so the untouched core still runs the take. The
    // whole arguments tree is compared at once, so what the report carries is
    // exactly the value the core handed over, at whatever depth it had.
    let deep = { innermost: 'deep value' };
    for (let level = 0; level < 12; level += 1) deep = { [`l${level}`]: deep };
    const args = { chain: deep };
    const literal = JSON.stringify({ key: 'RecorderEmptyText', args });
    const wrapper = [
      "import { createRecorder as createReal } from './recorder-real.js';",
      'export function createRecorder(handlers = {}) {',
      '  const core = createReal(handlers);',
      '  return {',
      '    start: core.start,',
      '    acceptPcmChunk: core.acceptPcmChunk,',
      '    dispose: core.dispose,',
      '    stop() {',
      '      return core.stop().then((result) => ({',
      '        ...result,',
      `        errorDetails: { live: ${literal} },`,
      '      }));',
      '    },',
      '  };',
      '}',
    ].join('\n');
    await withHarness({
      files: { core: wrapper, extra: { 'recorder-real.js': CORE_SOURCE } },
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 61);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      message(socket, { type: 'final', text: 'deep' });
      message(socket, { type: 'done' });
      await recorder.stop();
      await harness.advance(1000);
      const terminal = harness.reports
        .filter((entry) => entry.payload.terminalOutcome !== null).at(-1);
      const parsed = JSON.parse(terminal.body);
      expect(parsed.result.errorDetails.live.args).toEqual(args);
      expect(terminal.payload.lostReports).toBe(0);
      expect(harness.warnings).toEqual([]);
      recorder.dispose();
    });
  });

  test('an own __proto__ field travels as it is and pollutes nothing', async () => {
    // A descriptor whose own property is literally named `__proto__`, the way
    // JSON.parse builds one. It has to reach the report as its own field, and
    // the page's objects must be exactly as they were afterwards.
    const descriptor = '{"key":"RecorderEmptyText","args":{"__proto__":{"polluted":true},"detail":"kept"}}';
    const wrapper = [
      "import { createRecorder as createReal } from './recorder-real.js';",
      'export function createRecorder(handlers = {}) {',
      '  const core = createReal(handlers);',
      '  return {',
      '    start: core.start,',
      '    acceptPcmChunk: core.acceptPcmChunk,',
      '    dispose: core.dispose,',
      '    stop() {',
      '      return core.stop().then((result) => ({',
      '        ...result,',
      `        errorDetails: { live: JSON.parse(${JSON.stringify(descriptor)}) },`,
      '      }));',
      '    },',
      '  };',
      '}',
    ].join('\n');
    await withHarness({
      files: { core: wrapper, extra: { 'recorder-real.js': CORE_SOURCE } },
    }, async (harness) => {
      harness.useVirtualTimers();
      const module = await load(harness);
      const seen = [];
      const recorder = module.createRecorder({
        onError(name, message, ui) { seen.push({ receiver: this, argc: arguments.length, ui }); },
      });
      harness.bind('dictation', 62);
      const started = recorder.start({ external: true, modes: ['live'], sessionId: 62 });
      await harness.flush();
      await started;
      const socket = harness.FakeWebSocket.instances.at(-1);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      // A real core failure, so the page's own handler really runs.
      message(socket, { type: 'error', message: 'live failed' });
      message(socket, { type: 'done' });
      await recorder.stop();
      await harness.advance(1000);

      const terminal = harness.reports
        .filter((entry) => entry.payload.terminalOutcome !== null).at(-1);
      const args = terminal.payload.result.errorDetails.live.args;
      expect(Object.prototype.hasOwnProperty.call(args, '__proto__')).toBe(true);
      expect(args.detail).toBe('kept');
      expect(terminal.body).toContain('"__proto__"');
      expect(Object.prototype.polluted).toBeUndefined();
      expect({}.polluted).toBeUndefined();
      expect(Object.getPrototypeOf(terminal.payload.result.errorDetails.live)).toBe(Object.prototype);
      // The page's handler kept its receiver and its exact argument count, and
      // saw the core's own descriptor.
      expect(seen).toHaveLength(1);
      expect(seen[0].receiver).toBeUndefined();
      expect(seen[0].argc).toBe(3);
      expect(seen[0].ui.key).toBe('RecorderLiveServiceError');
      recorder.dispose();
    });
  });

  test('a descriptor value that is not plain JSON is advertised as lost', async () => {
    // A Date has a prototype of its own, so its fields are not the value JSON
    // would carry: the take reports the content lost instead of sending `{}`.
    const wrapper = [
      "import { createRecorder as createReal } from './recorder-real.js';",
      'export function createRecorder(handlers = {}) {',
      '  const core = createReal(handlers);',
      '  return {',
      '    start: core.start,',
      '    acceptPcmChunk: core.acceptPcmChunk,',
      '    dispose: core.dispose,',
      '    stop() {',
      '      return core.stop().then((result) => ({',
      '        ...result,',
      "        errorDetails: { live: { key: 'RecorderEmptyText', args: { when: new Date(0) } } },",
      '      }));',
      '    },',
      '  };',
      '}',
    ].join('\n');
    await withHarness({
      files: { core: wrapper, extra: { 'recorder-real.js': CORE_SOURCE } },
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 63);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      message(socket, { type: 'final', text: 'plain' });
      message(socket, { type: 'done' });
      const result = await recorder.stop();
      expect(result.results.live).toBe('plain');
      expect(result.errorDetails.live.args.when).toBeInstanceOf(Date);
      await harness.advance(1000);
      const terminal = harness.reports
        .filter((entry) => entry.payload.terminalOutcome !== null).at(-1);
      expect(terminal.payload.result).toBeUndefined();
      expect(terminal.payload.errors).toBeUndefined();
      expect(terminal.payload.lostReports).toBeGreaterThan(0);
      expect(terminal.body).not.toContain('RecorderEmptyText');
      expect(harness.warnings.some((line) => line.includes('content_invalid'))).toBe(true);
      recorder.dispose();
    });
  });

  test('a structure too deep to copy is lost, never thrown at the caller', async () => {
    const wrapper = [
      "import { createRecorder as createReal } from './recorder-real.js';",
      'export function createRecorder(handlers = {}) {',
      '  const core = createReal(handlers);',
      '  return {',
      '    start: core.start,',
      '    acceptPcmChunk: core.acceptPcmChunk,',
      '    dispose: core.dispose,',
      '    stop() {',
      '      return core.stop().then((result) => {',
      "        let deep = 'end';",
      '        for (let level = 0; level < 40000; level += 1) deep = { n: deep };',
      '        return {',
      '          ...result,',
      "          errorDetails: { live: { key: 'RecorderEmptyText', args: { chain: deep } } },",
      '        };',
      '      });',
      '    },',
      '  };',
      '}',
    ].join('\n');
    await withHarness({
      files: { core: wrapper, extra: { 'recorder-real.js': CORE_SOURCE } },
    }, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 64);
      message(socket, { type: 'ready' });
      feed(recorder, 1);
      message(socket, { type: 'final', text: 'kept' });
      message(socket, { type: 'done' });
      // The stop promise still resolves with the core's own object: copying it
      // never turns into an exception for the page that awaits it.
      const result = await recorder.stop();
      expect(result.results.live).toBe('kept');
      expect(result.errorDetails.live.args.chain.n.n).toBeDefined();
      await harness.advance(1000);
      const terminal = harness.reports
        .filter((entry) => entry.payload.terminalOutcome === 'success').at(-1);
      expect(terminal).toBeDefined();
      // Either the whole chain was copied and travels inside the wide cap, or
      // it could not be copied and the take advertises the loss: a body that
      // carries a piece of it only is what must never happen.
      const parsed = JSON.parse(terminal.body);
      if (parsed.result === undefined) {
        expect(terminal.payload.lostReports).toBeGreaterThan(0);
      } else {
        expect(parsed.result.results.live).toBe('kept');
        expect(parsed.result.errorDetails.live.args.chain.n.n.n).toBeDefined();
        expect(new TextEncoder().encode(terminal.body).length).toBeLessThanOrEqual(1048576);
      }
      recorder.dispose();
    });
  });

  test('error reports stay time-addressable across reports that accumulate them', async () => {
    // Two failures of the same take, raised at different moments: the live
    // socket fails first, the stop finds a recording too short to transcribe
    // afterwards. Both reach the page, and each keeps the moment its own
    // callback was raised rather than the moment a report happened to carry it.
    await withHarness({}, async (harness) => {
      const module = await load(harness);
      const { recorder, socket } = await boundTake(harness, module, 65);
      message(socket, { type: 'ready' });
      message(socket, { type: 'error', message: 'first failure' });
      // The take keeps running for a while before its stop: the second failure
      // belongs to a later moment, and so does its report.
      await harness.advance(1200);
      const result = await recorder.stop();
      expect(Object.keys(result.errors)).toEqual(['live', '']);
      await harness.advance(1000);

      const terminal = harness.reports
        .filter((entry) => entry.payload.terminalOutcome !== null).at(-1).payload;
      expect(terminal.errors).toHaveLength(2);
      const [first, second] = terminal.errors;
      expect(first.mode).toBe('live');
      expect(second.mode).toBe('');
      for (const entry of terminal.errors) {
        expect(Number.isFinite(entry.atMs)).toBe(true);
        expect(entry.atMs).toBeGreaterThanOrEqual(0);
      }
      // The order of the reports is the order of their moments, and the second
      // one really belongs to the later half of this take.
      expect(first.atMs).toBeLessThan(second.atMs);
      expect(second.atMs).toBeGreaterThanOrEqual(1200);
      // Every report that carried errors carried the same stamps: a take's
      // errors accumulate, so each body's chronology is the leading run of the
      // final one, and no report ever restamps an error it already carried.
      const stamps = terminal.errors.map((row) => row.atMs);
      for (const entry of harness.reports) {
        const parsed = JSON.parse(entry.body);
        if (parsed.errors === undefined) continue;
        expect(stamps.slice(0, parsed.errors.length)).toEqual(parsed.errors.map((row) => row.atMs));
      }
      recorder.dispose();
    });
  });
}

/* -------------------------------------------------------------------------- */
/* smoke branch                                                                */
/* -------------------------------------------------------------------------- */

/** Base64 PCM16LE of one fixture frame, without a second conversion. */
function decodeFrame(data) {
  const binary = atob(data);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return new Int16Array(bytes.buffer, 0, bytes.length >> 1);
}

/**
 * A fixture failure: it is thrown rather than exiting, so the shared cleanup
 * of the smoke branch still runs before the process leaves.
 */
class SmokeFailure extends Error {}

function smokeFail(reason) {
  throw new SmokeFailure(reason);
}

/** True when a report body is the one that carries the terminal outcome. */
function isTerminalBody(body) {
  if (typeof body !== 'string') return false;
  try {
    const parsed = JSON.parse(body);
    return parsed !== null && parsed.terminalOutcome !== null && parsed.terminalOutcome !== undefined;
  } catch {
    return false;
  }
}

async function runSmoke(origin, owner, generation) {
  const root = makeFixtureTree();
  const restores = [];
  const statuses = [];
  /** True once the POST that carried the terminal outcome was admitted. */
  let terminalAck = false;
  const sockets = [];
  let ready = false;
  let readyResolve = null;
  const readySeen = new Promise((resolve) => { readyResolve = resolve; });

  const realFetch = globalThis.fetch;
  const RealWebSocket = globalThis.WebSocket;

  try {
    // The adapter is installed before the decorator is imported, so the
    // decorator captures it as the page's original fetch.
    const adapter = async (input, init) => {
      const target = typeof input === 'string'
        ? new URL(input, origin).href
        : input instanceof URL ? new URL(input.href, origin).href
          : new URL(input.url, origin).href;
      const headers = new Headers(
        init?.headers ?? (input instanceof Request ? input.headers : undefined),
      );
      headers.set('Origin', origin);
      const request = input instanceof Request
        ? { method: input.method, headers, body: input.body, signal: input.signal, duplex: 'half' }
        : { ...(init ?? {}), headers };
      const response = await realFetch(target, request);
      if (target.includes('/api/dev-diagnostics')) {
        statuses.push(response.status);
        // Only the terminal report proves the take was admitted; a periodic
        // report with a null outcome must never satisfy this.
        if (isTerminalBody(init?.body) && (response.status === 202 || response.status === 200)) terminalAck = true;
      }
      return response;
    };
    restores.push(setGlobal('fetch', adapter));

    // A base that hands Bun the options object with the Origin header while
    // preserving the protocols the decorator forwards.
    const Base = class extends RealWebSocket {
      constructor(url, protocols, options) {
        const merged = { headers: { Origin: origin } };
        if (Array.isArray(protocols)) merged.protocols = protocols;
        else if (typeof protocols === 'string') merged.protocols = [protocols];
        else if (protocols && typeof protocols === 'object') Object.assign(merged, protocols);
        if (options && typeof options === 'object') Object.assign(merged, options);
        super(url, merged);
        sockets.push(this);
        this.addEventListener('message', (event) => {
          try {
            if (JSON.parse(event.data).type === 'ready' && !ready) {
              ready = true;
              readyResolve();
            }
          } catch { /* a frame the fixture does not send */ }
        });
      }
    };
    restores.push(setGlobal('WebSocket', Base));
    restores.push(setGlobal('location', {
      origin,
      protocol: 'http:',
      host: new URL(origin).host,
      href: `${origin}/`,
    }));

    // The catalog is booted before the decorator, so the take renders the same
    // text a real page would; the boot goes through the adapter like any page.
    const i18n = await import(pathToFileURL(path.join(root, 'i18n.js')).href);
    // It is best effort: the smoke's acceptance is the admitted report and the
    // journal, not the prose a missing catalog would leave untranslated.
    await i18n.initI18n();

    const module = await import(pathToFileURL(path.join(root, 'dev-recorder.js')).href);
    const marker = globalThis.__speechekDevDiagnostics;
    if (!marker) smokeFail('marker_missing');


    const recorder = module.createRecorder({ onError() {} });
    marker.setContext(owner, generation);
    const started = recorder.start({ external: true, modes: ['live'], sessionId: 1 });
    await started;
    if (sockets.length === 0) smokeFail('no_socket');

    // The server has to answer `ready` before any audio: a core that is still
    // queueing would otherwise overflow its pre-ready cap.
    await Promise.race([
      readySeen,
      new Promise((resolve) => realSetTimeout(resolve, 15000)),
    ]);
    if (!ready) smokeFail('ready_timeout');
    // The fixture may hold its frames until this line: nothing is fed before
    // the server's own `ready`, so the core never overflows its pre-ready cap.
    console.log('SPEECHEK_SMOKE_READY');
    let frames = 0;
    let lastSequence = null;
    let batches = 0;
    for (;;) {
      const response = await realFetch(`${origin}/fixture/frames`, { headers: { Origin: origin } });
      if (!response.ok) smokeFail(`fixture_status_${response.status}`);
      const payload = await response.json();
      for (const frame of payload.frames ?? []) {
        recorder.acceptPcmChunk(decodeFrame(frame.data));
        frames += 1;
        lastSequence = frame.sequence;
      }
      batches += 1;
      if (payload.eof === true) {
        if (Number.isSafeInteger(payload.lastSequence)) lastSequence = payload.lastSequence;
        break;
      }
      if (batches > 10000) smokeFail('fixture_endless');
    }

    marker.nativeStop(owner, generation, lastSequence, lastSequence, false);
    const result = await recorder.stop();
    if (result === undefined) smokeFail('stop_undefined');

    // The terminal report has to be admitted before this child leaves.
    const deadline = Date.now() + 10000;
    while (Date.now() < deadline) {
      if (terminalAck) break;
      await new Promise((resolve) => realSetTimeout(resolve, 50));
    }
    const finalStatus = statuses.length > 0 ? statuses[statuses.length - 1] : 0;
    console.log(`smoke status=${terminalAck ? 'ok' : 'pending'} reports=${statuses.length} last=${finalStatus} frames=${frames} lastSequence=${lastSequence ?? -1}`);
    recorder.dispose();
    // The marker is what the parent waits for: it only appears once the report
    // that carried the terminal outcome was really admitted.
    if (!terminalAck) smokeFail('terminal_report_not_accepted');
    console.log(`SPEECHEK_SMOKE_DONE accepted=1 reports=${statuses.length} frames=${frames} lastSequence=${lastSequence ?? -1}`);
    return 0;
  } catch (cause) {
    const reason = cause instanceof SmokeFailure ? cause.message : `exception_${cause?.name ?? 'unknown'}`;
    console.log(`smoke status=failed reason=${reason}`);
    return 1;
  } finally {
    for (const restore of restores.reverse()) restore();
    try {
      rmSync(root, { recursive: true, force: true });
    } catch { /* the fixture root is the only thing removed */ }
  }
}

/* -------------------------------------------------------------------------- */
/* entry                                                                       */
/* -------------------------------------------------------------------------- */

const argv = process.argv.slice(2);

function argValue(name) {
  const index = argv.indexOf(name);
  return index >= 0 && index + 1 < argv.length ? argv[index + 1] : null;
}

const smokeUrl = argValue('--smoke-url');

if (smokeUrl !== null) {
  // The smoke branch never imports bun:test, and its cleanup runs before the
  // process leaves: the exit code alone tells the parent whether it was clean.
  const code = await runSmoke(smokeUrl, argValue('--owner') ?? 'dictation', Number(argValue('--generation') ?? '1'));
  process.exit(code);
} else {
  await runUnit();
}
