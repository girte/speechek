/**
 * public/i18n.js — one translation contract shared by every Speechek page.
 *
 * One catalog (`/messages.json`, embedded) and one runtime language, pushed by
 * the shell as validated `{language, revision}` snapshots. Every page imports
 * this module; no page keeps its own copy of translated prose.
 *
 * Public contract:
 *   - `t(key, args)`                 — catalog text for this page's key.
 *   - `formatMessage(ui)`            — text for a `{key, args}` descriptor.
 *   - `formatError(error)`           — text for an error from any boundary.
 *   - `keyCountText(count)`          — pluralized key count in this language.
 *   - `initI18n({ onSettings })`     — boot: listener first, then the catalog
 *                                      and `/api/settings` in parallel.
 *   - `acceptLanguageSnapshot(view)` — feed a settings view or event payload.
 *   - `refreshLanguageSnapshot()`    — bounded recovery read (window focus).
 *   - `onLanguageChange(cb)`         — observe applied snapshots, post-init.
 *   - `applyTranslations(root)`      — translate static `data-i18n*` markers.
 *
 * Boot outcomes `initI18n` reports (it resolves, never rejects):
 *   - `catalogReady === false`: nothing can be translated. The bilingual
 *     notice is shown, page actions are made inert, and the overlay must not
 *     call `overlay_ready`; a restart is the only recovery.
 *   - `catalogReady && !snapshotReady`: English stands in and the
 *     `UiLanguageReadFailed` notice is shown. Overlay and lab may run; settings
 *     actions wait for a valid `settings_open` view (pass it to
 *     `acceptLanguageSnapshot`), which restores `snapshotReady`.
 *
 * Rules that outlive this file:
 *   - Substitution is text only (single pass over `{name}` placeholders);
 *     substituted values are never rescanned and no HTML is ever built. No
 *     secrets may appear in `args`: they would reach the DOM as text.
 *   - Descriptors (`ui`) are the unit pages keep and re-render on a language
 *     change; a translated string is never the only copy of a message.
 *   - Unknown catalog keys are developer errors: `t()` keeps the key visible
 *     and logs to the console; `formatMessage`/`formatError` show translated
 *     `InternalError` plus the safe code/detail instead of the raw key.
 *   - Importing this module has no side effects and touches no DOM; listeners
 *     and requests start in `initI18n`.
 */

/* -------------------------------------------------------------------------- */
/* Constants                                                                   */
/* -------------------------------------------------------------------------- */

const CATALOG_URL = '/messages.json';
const SETTINGS_URL = '/api/settings';
const SETTINGS_CHANGED_EVENT = 'speechek:settings-changed';

/** Only these interface languages exist; the selector shows self-names. */
const LANGUAGES = new Set(['en', 'ru']);
/** Both boot requests are bounded so a hung server cannot freeze a page. */
const FETCH_TIMEOUT_MS = 2000;
/** Nested UiMessage rendering is bounded; real payloads are one or two deep. */
const MAX_NESTING = 16;

/** Catalog keys this module itself renders. */
const INTERNAL_KEY = 'InternalError';
const READ_FAILED_KEY = 'UiLanguageReadFailed';
const COUNT_KEYS = Object.freeze({
  one: 'KeysCountOne',
  few: 'KeysCountFew',
  many: 'KeysCountMany',
  other: 'KeysCountOther',
});

/** Last-resort text if the catalog itself is broken (a build error). */
const INTERNAL_FALLBACK = Object.freeze({ en: 'Internal error.', ru: 'Внутренняя ошибка.' });

/** Catalog identifiers: PascalCase ASCII, identical to the Rust `MessageId`. */
const MESSAGE_KEY_RE = /^[A-Za-z][A-Za-z0-9]*$/;
/** Named single-pass placeholders, e.g. `{count}` or `{rate}`. */
const PLACEHOLDER_RE = /\{([A-Za-z][A-Za-z0-9_]*)\}/g;

const PENDING_ATTR = 'data-i18n-pending';
const DISABLED_ATTR = 'data-i18n-disabled';
const NOTICE_ID = 'ui-language-notice';
const BARRIER_STYLE_ID = 'i18n-barrier-style';

/**
 * The catalog cannot fail AND offer a key for that failure, so this notice is
 * bilingual by construction and never goes through `t()`.
 */
const CATALOG_FAILURE_NOTICE = 'Could not load the interface. Restart Speechek. / ' +
  'Не удалось загрузить интерфейс. Перезапустите Speechek.';

/** Static annotation markers mapped to the attributes they translate. */
const ATTRIBUTE_KEYS = Object.freeze([
  ['data-i18n-aria-label', 'aria-label'],
  ['data-i18n-title', 'title'],
  ['data-i18n-placeholder', 'placeholder'],
]);

/* -------------------------------------------------------------------------- */
/* State                                                                       */
/* -------------------------------------------------------------------------- */

/**
 * `appliedRevision` is the newest runtime revision whose language is in effect
 * (-1: none yet); `buffered` holds the newest validated snapshot that arrived
 * before the catalog did. `snapshotReady` is derived, never stored.
 */
const state = {
  language: 'en',
  appliedRevision: -1,
  buffered: null,
  catalog: null,
  catalogReady: false,
  latestSettings: null,
  settingsRevision: -1,
  dispatchedSettingsRevision: -1,
  onSettings: null,
  refreshing: null,
};

/** Callbacks observing applied snapshots; see `onLanguageChange`. */
const languageListeners = new Set();
/** Diagnostics print once per distinct message: pages call `t()` in loops. */
const reportedDiagnostics = new Set();
/** `initI18n` runs once per page; repeated calls share this promise. */
let initPromise = null;
/** Until init settles, delivery waits so no page renders mid-boot. */
let initSettled = false;
let pendingLanguageFlush = false;
/** One lazy `Intl.PluralRules` per language. */
const pluralRules = new Map();

/* -------------------------------------------------------------------------- */
/* Diagnostics                                                                 */
/* -------------------------------------------------------------------------- */

/** Developer-facing diagnostics; never shown to users, never localized. */
function diagnostic(message) {
  if (typeof console === 'undefined' || typeof console.error !== 'function') return;
  if (reportedDiagnostics.has(message)) return;
  if (reportedDiagnostics.size >= 200) reportedDiagnostics.clear();
  reportedDiagnostics.add(message);
  console.error(`speechek i18n: ${message}`);
}

function describeError(error) {
  if (error instanceof Error && typeof error.message === 'string' && error.message) {
    return error.message;
  }
  try {
    return String(error);
  } catch {
    return 'unknown error';
  }
}

/* -------------------------------------------------------------------------- */
/* Catalog                                                                     */
/* -------------------------------------------------------------------------- */

function isRecord(value) {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isUiMessageLike(value) {
  return isRecord(value) && typeof value.key === 'string' && value.key.length > 0;
}

function templateFor(key, language) {
  const entry = state.catalog ? state.catalog[key] : undefined;
  if (!entry) return null;
  return language === 'ru' ? entry.ru : entry.en;
}

function scanPlaceholders(template) {
  const names = new Set();
  for (const match of template.matchAll(PLACEHOLDER_RE)) names.add(match[1]);
  return names;
}

/**
 * Keeps only well-formed entries: the page must keep working when one entry is
 * broken, and a wholly broken catalog is reported as a failure instead.
 */
function normalizeCatalog(raw) {
  if (!isRecord(raw)) {
    diagnostic('message catalog must be a JSON object');
    return null;
  }
  const catalog = Object.create(null);
  const problems = [];
  for (const [key, entry] of Object.entries(raw)) {
    if (!MESSAGE_KEY_RE.test(key)) {
      problems.push(`${key}: invalid key`);
      continue;
    }
    if (!isRecord(entry) || typeof entry.en !== 'string' || typeof entry.ru !== 'string' ||
        entry.en.length === 0 || entry.ru.length === 0) {
      problems.push(`${key}: needs non-empty en and ru strings`);
      continue;
    }
    const en = scanPlaceholders(entry.en);
    const ru = scanPlaceholders(entry.ru);
    let mismatch = en.size !== ru.size;
    if (!mismatch) {
      for (const name of en) {
        if (!ru.has(name)) {
          mismatch = true;
          break;
        }
      }
    }
    if (mismatch) {
      problems.push(`${key}: placeholder mismatch between en and ru`);
      continue;
    }
    catalog[key] = Object.freeze({ en: entry.en, ru: entry.ru });
  }
  if (problems.length > 0) {
    diagnostic(`message catalog rejected ${problems.length} entries (${problems.slice(0, 5).join('; ')})`);
  }
  if (Object.keys(catalog).length === 0) {
    diagnostic('message catalog has no usable entries');
    return null;
  }
  return catalog;
}

/* -------------------------------------------------------------------------- */
/* Rendering                                                                   */
/* -------------------------------------------------------------------------- */

function safeString(value) {
  try {
    return String(value);
  } catch {
    return '';
  }
}

function firstString(...values) {
  for (const value of values) {
    if (typeof value === 'string' && value.length > 0) return value;
  }
  return null;
}

function pluralRulesFor(language) {
  let rules = pluralRules.get(language);
  if (rules) return rules;
  if (typeof Intl === 'undefined' || typeof Intl.PluralRules !== 'function') return null;
  rules = new Intl.PluralRules(language);
  pluralRules.set(language, rules);
  return rules;
}

/** Translated fallback plus the safe machine code and raw detail when known. */
function formatInternal({ code = null, detail = null } = {}) {
  let text = templateFor(INTERNAL_KEY, state.language) ??
    INTERNAL_FALLBACK[state.language] ?? INTERNAL_FALLBACK.en;
  if (typeof code === 'string' && code.length > 0 && !text.includes(code)) text += ` (${code})`;
  if (typeof detail === 'string') {
    const trimmed = detail.trim();
    if (trimmed.length > 0 && !text.includes(trimmed)) text += ` — ${trimmed}`;
  }
  return text;
}

function stringifyArg(value, language, key, name, depth) {
  if (typeof value === 'string') return value;
  if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  if (typeof value === 'boolean') return value ? 'true' : 'false';
  if (isUiMessageLike(value)) {
    if (depth >= MAX_NESTING) {
      diagnostic(`nested UiMessage is too deep for ${key}`);
      return value.key;
    }
    return renderUiMessage(value, language, depth + 1);
  }
  diagnostic(`argument ${name} of ${key} is neither a string, a number nor a UiMessage`);
  return safeString(value);
}

/**
 * One pass over the template: substituted values are never rescanned, so an OS
 * or Google string that happens to contain `{...}` stays literal text.
 */
function renderTemplate(template, language, args, key, depth) {
  if (!template.includes('{')) return template;
  const missing = [];
  let text;
  try {
    text = template.replace(PLACEHOLDER_RE, (whole, name) => {
      const value = args[name];
      if (value === undefined || value === null) {
        if (!missing.includes(name)) missing.push(name);
        return whole;
      }
      return stringifyArg(value, language, key, name, depth);
    });
  } catch (error) {
    diagnostic(`rendering ${key} failed: ${describeError(error)}`);
    return template;
  }
  if (missing.length > 0) diagnostic(`missing argument(s) ${missing.join(', ')} for ${key}`);
  return text;
}

/** Resolves a descriptor; an unknown key becomes InternalError, never the key. */
function renderUiMessage(ui, language, depth) {
  const template = templateFor(ui.key, language);
  if (template === null) {
    diagnostic(`unknown message key ${ui.key}`);
    return formatInternal({ code: firstString(ui.code), detail: firstString(ui.detail, ui.message) });
  }
  return renderTemplate(template, language, isRecord(ui.args) ? ui.args : {}, ui.key, depth);
}

/** Descriptor display with safe code/detail attached only when the key is unknown. */
function renderDisplayDescriptor(ui, context, fallbackDetail) {
  const template = templateFor(ui.key, state.language);
  if (template === null) {
    diagnostic(`unknown message key ${ui.key}`);
    return formatInternal({
      code: firstString(context?.code, ui.code),
      detail: firstString(fallbackDetail, ui.detail, ui.message, context?.message),
    });
  }
  return renderTemplate(template, state.language, isRecord(ui.args) ? ui.args : {}, ui.key, 0);
}

/* -------------------------------------------------------------------------- */
/* Public translation API                                                      */
/* -------------------------------------------------------------------------- */

/**
 * The page's translation call. The key must exist: an unknown key is a wiring
 * bug, so this keeps it visible and logs it. `args` are plain values — numbers,
 * safe strings, or nested `{key, args}` descriptors.
 */
export function t(key, args = {}) {
  if (typeof key !== 'string' || key.length === 0) {
    diagnostic('t() was called without a message key');
    return '';
  }
  const template = templateFor(key, state.language);
  if (template === null) {
    diagnostic(state.catalogReady
      ? `unknown message key ${key}`
      : `message key ${key} used before the catalog was ready`);
    return key;
  }
  if (!isRecord(args)) {
    diagnostic(`t(${key}) expects an args object`);
    args = {};
  }
  return renderTemplate(template, state.language, args, key, 0);
}

/**
 * Renders a semantic descriptor (`{key, args}`). Descriptors are the copy
 * pages keep and re-render; an unknown key never leaks — the user sees
 * translated `InternalError` with the safe code/detail when the payload
 * carries one.
 */
export function formatMessage(ui) {
  if (!isUiMessageLike(ui)) {
    diagnostic('formatMessage() expects an object with a string key');
    return formatInternal({});
  }
  return renderDisplayDescriptor(ui, ui, '');
}

/**
 * Boundary renderer for errors from any producer: a Rust `UiError`/`ApiError`
 * (`{code, message, ui}`), a native rejection, a recorder `Error` carrying
 * `.ui`, a bare `{key, args}`, or the page's own raw fallback. The descriptor
 * wins; a safe raw string remains the last resort.
 */
export function formatError(error, fallbackDetail = '') {
  const ui = isUiMessageLike(error?.ui) ? error.ui : isUiMessageLike(error) ? error : null;
  if (ui) return renderDisplayDescriptor(ui, error, fallbackDetail);
  if (typeof error === 'string' && error.trim().length > 0) return error;
  const raw = firstString(error?.message, fallbackDetail);
  if (raw) return raw;
  return formatInternal({});
}

/**
 * Pluralized key count: `Intl.PluralRules` per language (en one/other, ru
 * one/few/many/other) mapped onto the `KeysCount*` keys.
 */
export function keyCountText(count) {
  const value = typeof count === 'number' ? count : Number(count);
  let category = 'other';
  if (Number.isFinite(value)) {
    const rules = pluralRulesFor(state.language);
    try {
      category = rules ? rules.select(value) : (value === 1 ? 'one' : 'other');
    } catch (error) {
      diagnostic(`plural selection failed: ${describeError(error)}`);
      category = 'other';
    }
  } else {
    diagnostic('keyCountText() expects a finite count');
  }
  const key = COUNT_KEYS[category] ?? COUNT_KEYS.other;
  return t(key, { count: Number.isFinite(value) ? value : count });
}

/**
 * Translates static annotations: `data-i18n` on text leaves, and
 * `data-i18n-aria-label` / `data-i18n-title` / `data-i18n-placeholder` on
 * attributes. A parent holding elements of its own is never replaced — ids,
 * links, icons and values stay untouched. Runs again on every language change.
 */
export function applyTranslations(root) {
  if (root === undefined) {
    if (typeof document === 'undefined') return;
    root = document;
  }
  if (!root || typeof root.querySelectorAll !== 'function') {
    diagnostic('applyTranslations() expects a DOM root');
    return;
  }
  if (!state.catalogReady) {
    diagnostic('applyTranslations() ran before the catalog was ready');
    return;
  }
  try {
    for (const element of root.querySelectorAll('[data-i18n]')) {
      const key = element.getAttribute('data-i18n');
      if (!key) continue;
      if (element.children.length > 0) {
        diagnostic(`[data-i18n="${key}"] must sit on a text leaf`);
        continue;
      }
      const text = t(key);
      if (element.textContent !== text) element.textContent = text;
    }
    for (const [marker, attribute] of ATTRIBUTE_KEYS) {
      for (const element of root.querySelectorAll(`[${marker}]`)) {
        const key = element.getAttribute(marker);
        if (!key) continue;
        const text = t(key);
        if (element.getAttribute(attribute) !== text) element.setAttribute(attribute, text);
      }
    }
  } catch (error) {
    diagnostic(`applyTranslations() failed: ${describeError(error)}`);
  }
}

/* -------------------------------------------------------------------------- */
/* DOM state: barrier, notice, document language                               */
/* -------------------------------------------------------------------------- */

function domDocument() {
  return typeof document === 'undefined' ? null : document;
}

/** The body hides until the first coherent localization; see `initI18n`. */
function setPending(pending) {
  const doc = domDocument();
  if (!doc || !doc.body) return;
  try {
    if (pending) doc.body.setAttribute(PENDING_ATTR, '');
    else doc.body.removeAttribute(PENDING_ATTR);
  } catch (error) {
    diagnostic(`pending barrier failed: ${describeError(error)}`);
  }
}

/** The barrier and freeze rules travel with the module, not with each page. */
function ensureBarrierStyle() {
  const doc = domDocument();
  if (!doc || !doc.head || doc.getElementById(BARRIER_STYLE_ID)) return;
  try {
    const style = doc.createElement('style');
    style.id = BARRIER_STYLE_ID;
    style.textContent = `body[${PENDING_ATTR}]{visibility:hidden}` +
      `body[${DISABLED_ATTR}]{pointer-events:none}`;
    doc.head.appendChild(style);
  } catch (error) {
    diagnostic(`barrier style failed: ${describeError(error)}`);
  }
}

function showNotice(text) {
  const doc = domDocument();
  if (!doc || !doc.body) return;
  try {
    let notice = doc.getElementById(NOTICE_ID);
    if (!notice) {
      notice = doc.createElement('div');
      notice.id = NOTICE_ID;
      notice.className = 'ui-language-notice';
      notice.setAttribute('role', 'status');
      doc.body.prepend(notice);
    }
    if (notice.textContent !== text) notice.textContent = text;
  } catch (error) {
    diagnostic(`notice failed: ${describeError(error)}`);
  }
}

function clearNotice() {
  const doc = domDocument();
  if (!doc) return;
  try {
    const notice = doc.getElementById(NOTICE_ID);
    if (notice) notice.remove();
  } catch (error) {
    diagnostic(`notice cleanup failed: ${describeError(error)}`);
  }
}

/**
 * Catalog failure leaves nothing to translate with: the page is revealed with
 * the bilingual notice and its actions are made inert instead of half-alive.
 */
function freezeActions() {
  const doc = domDocument();
  if (!doc || !doc.body) return;
  try {
    doc.body.setAttribute(DISABLED_ATTR, 'catalog');
    for (const child of doc.body.children) {
      if (child.id === NOTICE_ID) continue;
      child.inert = true;
      child.hidden = true;
    }
  } catch (error) {
    diagnostic(`freezing actions failed: ${describeError(error)}`);
  }
}

function syncDocumentLanguage() {
  const doc = domDocument();
  if (!doc || !doc.documentElement) return;
  if (doc.documentElement.lang !== state.language) doc.documentElement.lang = state.language;
}

/* -------------------------------------------------------------------------- */
/* Snapshot intake                                                             */
/* -------------------------------------------------------------------------- */

/**
 * Reads the runtime snapshot out of a settings payload: a `SettingsView`
 * (`{settings: {language}, runtimeRevision}`), a flat `/api/settings` answer,
 * or an event payload (`{language, revision}`). A view's own `revision` is the
 * draft revision and must never stand in for the runtime one.
 */
function snapshotOf(view) {
  if (!isRecord(view)) return null;
  if (isRecord(view.settings)) {
    return { language: view.settings.language, revision: view.runtimeRevision };
  }
  return { language: view.language, revision: view.revision };
}

/**
 * Only `en`/`ru` and nonnegative safe-integer revisions are accepted. A bad
 * payload is ignored entirely: it must never move the last-good language.
 */
function validateSnapshot(view) {
  const snapshot = snapshotOf(view);
  if (!snapshot) {
    diagnostic('settings snapshot must be an object');
    return null;
  }
  if (typeof snapshot.language !== 'string' || !LANGUAGES.has(snapshot.language)) {
    diagnostic(`settings snapshot has an unsupported language: ${safeString(snapshot.language)}`);
    return null;
  }
  if (!Number.isSafeInteger(snapshot.revision) || snapshot.revision < 0) {
    diagnostic(`settings snapshot has an invalid revision: ${safeString(snapshot.revision)}`);
    return null;
  }
  return snapshot;
}

/**
 * Revision ordering is strict: an older snapshot is dropped, the current one
 * is a no-op, and a newer one is kept — buffered until the catalog is ready,
 * applied otherwise. Returns true when the snapshot is valid and at least as
 * new as anything seen, which is what "this view may drive the UI" means.
 */
function acceptSnapshot(snapshot) {
  const known = state.buffered
    ? Math.max(state.buffered.revision, state.appliedRevision)
    : state.appliedRevision;
  if (snapshot.revision < known) return false;
  if (snapshot.revision === known) return true;
  if (!state.catalogReady) {
    state.buffered = snapshot;
    return true;
  }
  applySnapshot(snapshot);
  return true;
}

function applySnapshot(snapshot) {
  state.buffered = null;
  state.language = snapshot.language;
  state.appliedRevision = snapshot.revision;
  applyTranslations();
  syncDocumentLanguage();
  clearNotice();
  dispatchLanguage();
}

/**
 * Feeds the module a settings snapshot: `/api/settings`, a SettingsView
 * (`{settings: {language}, runtimeRevision}`), a `speechek:settings-changed`
 * payload, or the page's own `{language, revision}`. Call it with every
 * `settings_open` view; a valid one restores a fallback language state.
 */
export function acceptLanguageSnapshot(view) {
  const snapshot = validateSnapshot(view);
  if (!snapshot) return false;
  return acceptSnapshot(snapshot);
}

function flushLanguage() {
  const language = state.language;
  const revision = state.appliedRevision;
  for (const listener of [...languageListeners]) {
    try {
      listener(language, revision);
    } catch (error) {
      diagnostic(`language listener failed: ${describeError(error)}`);
    }
  }
}

function dispatchLanguage() {
  if (!initSettled) {
    pendingLanguageFlush = true;
    return;
  }
  flushLanguage();
}

/**
 * Registers a callback that runs after each applied snapshot — never during
 * import and never before `initI18n` resolves (a change applied during boot is
 * delivered as one flush of the final state). Re-render retained descriptors
 * from here; the callback must not start IPC, sessions or saves. Returns an
 * unsubscribe function.
 */
export function onLanguageChange(callback) {
  if (typeof callback !== 'function') {
    diagnostic('onLanguageChange() expects a function');
    return () => {};
  }
  languageListeners.add(callback);
  return () => {
    languageListeners.delete(callback);
  };
}

/* -------------------------------------------------------------------------- */
/* Settings payloads                                                           */
/* -------------------------------------------------------------------------- */

function rememberSettings(payload, revision) {
  if (revision <= state.settingsRevision) return;
  state.settingsRevision = revision;
  state.latestSettings = payload;
}

/** Hands the newest accepted settings payload to `onSettings`, once per revision. */
function dispatchSettingsIfReady() {
  if (!state.catalogReady || !initSettled) return;
  if (typeof state.onSettings !== 'function') return;
  if (state.latestSettings === null) return;
  if (state.settingsRevision <= state.dispatchedSettingsRevision) return;
  state.dispatchedSettingsRevision = state.settingsRevision;
  try {
    state.onSettings(state.latestSettings);
  } catch (error) {
    diagnostic(`onSettings callback failed: ${describeError(error)}`);
  }
}

function handleSettingsPayload(payload) {
  const snapshot = validateSnapshot(payload);
  if (!snapshot) return;
  if (!acceptSnapshot(snapshot)) return;
  rememberSettings(payload, snapshot.revision);
  dispatchSettingsIfReady();
}

/* -------------------------------------------------------------------------- */
/* Fetching                                                                    */
/* -------------------------------------------------------------------------- */

/** Bounded, no-store read. Relative paths resolve against the page origin. */
async function fetchBounded(path) {
  const base = typeof location === 'undefined' || !location || !location.href ? null : location.href;
  let url = path;
  if (base) {
    try {
      url = new URL(path, base).href;
    } catch {
      url = path;
    }
  }
  const options = { cache: 'no-store' };
  if (typeof AbortSignal !== 'undefined' && typeof AbortSignal.timeout === 'function') {
    options.signal = AbortSignal.timeout(FETCH_TIMEOUT_MS);
  }
  const response = await fetch(url, options);
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response;
}

async function fetchJson(path) {
  return (await fetchBounded(path)).json();
}

function adoptCatalog(catalog) {
  state.catalog = catalog;
  state.catalogReady = true;
  if (!catalog[INTERNAL_KEY]) diagnostic(`message catalog is missing ${INTERNAL_KEY}`);
  const buffered = state.buffered;
  state.buffered = null;
  if (buffered) {
    applySnapshot(buffered);
  } else {
    applyTranslations();
    syncDocumentLanguage();
  }
  dispatchSettingsIfReady();
}

async function loadCatalog() {
  try {
    const catalog = normalizeCatalog(await fetchJson(CATALOG_URL));
    if (catalog) adoptCatalog(catalog);
  } catch (error) {
    diagnostic(`message catalog read failed: ${describeError(error)}`);
  }
}

async function loadInitialSettings() {
  try {
    handleSettingsPayload(await fetchJson(SETTINGS_URL));
  } catch (error) {
    diagnostic(`initial settings read failed: ${describeError(error)}`);
  }
}

/**
 * Recovery probe for pages that can miss the shell push (a browser tab, or an
 * activation): one bounded no-store read with the same revision filtering; a
 * failure keeps the last-good language. Settings and both lab pages call this
 * on window focus; the overlay calls it fire-and-forget when it returns to
 * idle. Resolves true when a read succeeded.
 */
export function refreshLanguageSnapshot() {
  if (state.refreshing) return state.refreshing;
  const request = (async () => {
    try {
      handleSettingsPayload(await fetchJson(SETTINGS_URL));
      return true;
    } catch {
      // Expected whenever a page outlives the server; last-good stands.
      return false;
    } finally {
      state.refreshing = null;
    }
  })();
  state.refreshing = request;
  return request;
}

/* -------------------------------------------------------------------------- */
/* Boot                                                                        */
/* -------------------------------------------------------------------------- */

/**
 * The shell pushes every committed settings save as one event. Registered (and
 * awaited) before any HTTP, so a committed change cannot slide past the boot
 * read. Plain browsers have no such push; activation refresh covers them.
 */
async function registerSettingsListener() {
  if (typeof globalThis.addEventListener === 'function') {
    try {
      globalThis.addEventListener(SETTINGS_CHANGED_EVENT, (event) => {
        handleSettingsPayload(event?.detail);
      });
    } catch (error) {
      diagnostic(`settings-changed DOM bridge was not accepted: ${describeError(error)}`);
    }
  }
  const tauri = globalThis.__TAURI__;
  if (typeof tauri?.event?.listen !== 'function') return;
  try {
    await tauri.event.listen(SETTINGS_CHANGED_EVENT, (event) => {
      handleSettingsPayload(event?.payload);
    });
  } catch (error) {
    diagnostic(`settings-changed listener was not accepted: ${describeError(error)}`);
  }
}

/**
 * Boots the shared i18n state and resolves `{catalogReady, snapshotReady,
 * language, revision}` in every failure case — it never rejects:
 *
 *   - the `speechek:settings-changed` listener comes first, before any HTTP;
 *   - the catalog and `/api/settings` are fetched in parallel, each bounded;
 *   - the newest validated snapshot is buffered until the catalog is ready, so
 *     a pushed change is never lost to the boot read;
 *   - `body[data-i18n-pending]` keeps the page hidden until the first coherent
 *     localization; catalog failure instead reveals the bilingual notice with
 *     the page's actions made inert (do not call `overlay_ready` then);
 *   - `snapshotReady === false` with `catalogReady === true` means English
 *     stands in and the read-failure notice is shown.
 *
 * `onSettings(view)` receives the newest accepted settings payload once its
 * revision is newer than the last one delivered — overlay uses it for the
 * hotkey. It is never called before the catalog is ready, and never with a
 * stale revision.
 */
export function initI18n(options = {}) {
  if (initPromise) return initPromise;
  initPromise = performInit(isRecord(options) ? options : {});
  return initPromise;
}

async function performInit(options) {
  state.onSettings = typeof options.onSettings === 'function' ? options.onSettings : null;
  ensureBarrierStyle();
  setPending(true);
  await registerSettingsListener();
  const catalogTask = loadCatalog();
  const settingsTask = loadInitialSettings();
  await catalogTask;
  await settingsTask;
  finalizeInit();
  return {
    catalogReady: state.catalogReady,
    snapshotReady: state.catalogReady && state.appliedRevision >= 0,
    language: state.language,
    revision: state.appliedRevision,
  };
}

function finalizeInit() {
  if (!state.catalogReady) {
    showNotice(CATALOG_FAILURE_NOTICE);
    freezeActions();
  } else if (state.appliedRevision < 0) {
    showNotice(t(READ_FAILED_KEY));
  }
  setPending(false);
  initSettled = true;
  if (pendingLanguageFlush) {
    pendingLanguageFlush = false;
    flushLanguage();
  }
  dispatchSettingsIfReady();
}
