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
 * can survive from one fixture into the next.
 */
function makeFixtureTree() {
  const root = mkdtempSync(path.join(tmpdir(), FIXTURE_PREFIX));
  writeFileSync(path.join(root, 'dev-recorder.js'), DEV_SOURCE);
  writeFileSync(path.join(root, 'recorder-core.js'), CORE_SOURCE);
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

  const root = makeFixtureTree();
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
      // Privacy: no text, URL, key, argument or payload field may travel.
      const wire = harness.reports.map((entry) => entry.body).join('\n');
      expect(wire).not.toContain('SECRET_TRANSCRIPT');
      expect(wire).not.toContain('ws://');
      expect(wire).not.toContain('/api/live');
      expect(wire).not.toContain('127.0.0.1');
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
      expect(harness.reports.map((entry) => entry.body).join('\n')).not.toContain('SECRET_SERVICE_DETAIL');
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
