/**
 * scripts/localization-sources.mjs
 *
 * GUI-free static guard for the Speechek localization surface. The module
 * never reads files and never starts processes: callers pass the parsed
 * `public/messages.json` catalog plus `{ path, text }` source entries.
 *
 * The Rust build script (`src-tauri/build.rs`) remains the only catalog shape /
 * non-empty / placeholder-parity validator. This module only consumes the
 * parsed catalog's key set and placeholder names, so no catalog schema is
 * duplicated here.
 *
 * API
 * ---
 * `inspectSources(catalog, files) -> { errors, warnings }`
 *   Every finding is `{ code, path, line, message }` with deterministic
 *   (path, line, code, message) ordering and duplicate suppression.
 *
 * Error codes (blocking for the caller)
 *   empty-i18n-ref        a marker or helper call has an empty key
 *   invalid-i18n-ref      a static key is not `^[A-Z][A-Za-z0-9]*$`
 *   unknown-key           a static key is missing from the catalog
 *   missing-arg           a static key never receives a declared placeholder
 *                         (JS helper calls and `data-i18n*` HTML markers)
 *   js-parse-error        a JavaScript file could not be parsed; every key
 *                         check on that file would be skipped, so this blocks
 *
 * Warning codes (advisory, never fail the caller)
 *   extra-arg                 a callsite passes an argument the template never uses
 *   hardcoded-html-text        authored HTML text without a translation marker
 *   hardcoded-html-attribute   aria-label/title/placeholder without its marker
 *   hardcoded-js-text          authored literal assigned to a UI sink
 *   hardcoded-rust-text        authored literal passed to a native UI sink
 *   html-parse-error           parse5 threw (tolerant parser; should not happen)
 *
 * Warning exemptions (lexical and narrow; no file-level or path-level
 * ignores exist in this module):
 *   - empty / whitespace / letterless / single-character values (glyphs, timers)
 *   - `Speechek` and `Speechek — Dev/Test` brand / profile suffix
 *   - `Dev`, `Test`, and the language self-names `English` / `Русский`
 *   - single tokens containing a digit (F2, 4173, gemini-3.5-transcribe)
 *   - `*Error` DOM exception names
 *   - colon/dash code ids (`speechek:settings-hotkey-key`) and camelCase ids
 *   - key combos (`Ctrl+Shift+Space`)
 *   - a `<style>` element's `textContent` (injected CSS, not copy)
 *   All-caps UI copy (`SAVE`, `CONTINUE`) is intentionally NOT exempt; use the
 *   narrow ignore comment for a known brand or data value.
 *
 * Ignore convention (narrow; the only suppression mechanism)
 *   HTML: `<!-- i18n-ignore-next: <reason> -->` ignores warnings for the next
 *   sibling node and its subtree (a deliberate pre-boot English fallback).
 *   JavaScript: `// i18n-ignore-next: <reason>` (or a block comment) ignores
 *   warnings inside the exact next statement, by source offsets, so another
 *   statement on the same line is still reported.
 *   A non-empty reason after the colon is required; a bare marker is not an
 *   ignore. Errors (`empty-i18n-ref`, `invalid-i18n-ref`, `unknown-key`,
 *   `missing-arg`, `js-parse-error`) are NEVER ignored.
 *
 * Extraction rules (conservative: anything dynamic is tolerated, never
 * reported): HTML markers, helper calls `t|msg|ui|uiMsg` with literal /
 * ternary / logical / map-member keys, `_KEY`/`_KEYS` constants (including
 * `Object.freeze` maps) and inline maps selected with a dynamic index (every
 * value a name may hold is kept, so both branches of a conditional map are
 * checked), `key:` descriptor literals, and nearest lexical binding lookup
 * (imports, parameters, block and function declarations). Hardcoded-text
 * warnings follow the nearest `const` initializer too, without executing.
 * The Rust scan is a token scan for literal arguments of
 * `set_text|set_title|title|with_title|with_id`, advisory only (inline test
 * modules are not excluded). `with_id` is checked only from its third
 * argument: the first two are the item id.
 */

import { parse as parseJavaScript } from 'acorn';
import { parse as parseHtml } from 'parse5';

/** Findings with these codes are blocking. */
const ERROR_CODES = Object.freeze([
  'empty-i18n-ref',
  'invalid-i18n-ref',
  'unknown-key',
  'missing-arg',
  'js-parse-error',
]);

/** Findings with these codes are advisory. */
const WARNING_CODES = Object.freeze([
  'extra-arg',
  'hardcoded-html-text',
  'hardcoded-html-attribute',
  'hardcoded-js-text',
  'hardcoded-rust-text',
  'html-parse-error',
]);

/** Ignore-comment shape shared by HTML comments and JavaScript comments. */
const IGNORE_COMMENT_PATTERN = /^\s*i18n-ignore-next\s*:\s*\S/;

/** Catalog identifiers: PascalCase ASCII, identical to the Rust `MessageId`. */
const KEY_PATTERN = /^[A-Z][A-Za-z0-9]*$/;
/** Named single-pass placeholders, e.g. `{count}` (mirrors i18n.js). */
const PLACEHOLDER_PATTERN = /\{([A-Za-z][A-Za-z0-9_]*)\}/g;

/** Page translation helpers (see public/i18n.js, app.js, settings.js, recorder.js). */
const HELPER_NAMES = Object.freeze(['t', 'msg', 'ui', 'uiMsg']);
/** Marker attribute -> the attribute it translates (mirrors i18n.js ATTRIBUTE_KEYS). */
const ATTRIBUTE_MARKERS = Object.freeze([
  ['data-i18n-aria-label', 'aria-label'],
  ['data-i18n-title', 'title'],
  ['data-i18n-placeholder', 'placeholder'],
]);

/** Member names whose literal assignment is visible UI text (JS). */
const JS_TEXT_SINKS = new Set(['textContent', 'innerText', 'title', 'placeholder', 'ariaLabel', 'ariaPlaceholder']);
/** setAttribute names that carry visible or accessibility text (JS). */
const JS_ATTRIBUTE_SINKS = new Set(['aria-label', 'aria-description', 'aria-placeholder', 'aria-roledescription', 'title', 'placeholder', 'alt']);
/** Literal first arguments that end up in front of the user (JS). */
const JS_DIALOG_SINKS = new Set(['alert', 'confirm', 'prompt']);
/** Native sinks whose literal arguments are visible copy (Rust, advisory). */
const RUST_SINK_NAMES = new Set(['set_text', 'set_title', 'title', 'with_title', 'with_id']);

/** Message-key holder naming convention (`INTERNAL_KEY`, `MODE_KEYS`, ...). */
const KEY_CONSTANT_PATTERN = /_(KEY|KEYS)$/;

/**
 * Inspects HTML/JavaScript/Rust sources against the parsed message catalog.
 *
 * @param {object} catalog parsed `public/messages.json` ({ Key: { en, ru } })
 * @param {Array<{path: string, text: string}>} files sources to inspect
 * @returns {{errors: Array<object>, warnings: Array<object>}}
 */
export function inspectSources(catalog, files) {
  if (!catalog || typeof catalog !== 'object' || Array.isArray(catalog)) {
    throw new TypeError('inspectSources: catalog must be the parsed messages.json object');
  }
  const index = buildCatalogIndex(catalog);
  const found = [];
  for (const file of Array.isArray(files) ? files : []) {
    if (!file || typeof file.text !== 'string') continue;
    const path = typeof file.path === 'string' ? file.path : String(file.path ?? '');
    const lower = path.toLowerCase();
    if (lower.endsWith('.html') || lower.endsWith('.htm')) {
      found.push(...inspectHtml(index, path, file.text));
    } else if (lower.endsWith('.js') || lower.endsWith('.mjs') || lower.endsWith('.cjs')) {
      found.push(...inspectJavaScript(index, path, file.text));
    } else if (lower.endsWith('.rs')) {
      found.push(...inspectRust(path, file.text));
    }
  }
  return splitFindings(found);
}

/* -------------------------------------------------------------------------- */
/* Shared helpers                                                              */
/* -------------------------------------------------------------------------- */

function finding(code, path, line, message) {
  const normalized = Number.isInteger(line) && line > 0 ? line : 1;
  return { code, path, line: normalized, message };
}

function splitFindings(found) {
  const seen = new Set();
  const unique = [];
  for (const item of found) {
    const id = `${item.code}\u0000${item.path}\u0000${item.line}\u0000${item.message}`;
    if (seen.has(id)) continue;
    seen.add(id);
    unique.push(item);
  }
  unique.sort((a, b) => (
    compareText(a.path, b.path)
    || a.line - b.line
    || compareText(a.code, b.code)
    || compareText(a.message, b.message)
  ));
  const blocking = new Set(ERROR_CODES);
  return {
    errors: unique.filter((item) => blocking.has(item.code)),
    warnings: unique.filter((item) => !blocking.has(item.code)),
  };
}

function compareText(a, b) {
  if (a === b) return 0;
  return a < b ? -1 : 1;
}

function buildCatalogIndex(catalog) {
  const keys = new Set();
  const placeholders = new Map();
  for (const key of Object.keys(catalog)) {
    keys.add(key);
    const expected = new Set();
    const entry = catalog[key];
    if (entry && typeof entry === 'object') {
      collectPlaceholders(entry.en, expected);
      collectPlaceholders(entry.ru, expected);
    }
    placeholders.set(key, expected);
  }
  return { keys, placeholders };
}

function collectPlaceholders(template, into) {
  if (typeof template !== 'string') return;
  for (const match of template.matchAll(PLACEHOLDER_PATTERN)) into.add(match[1]);
}

function stripBom(text) {
  return text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
}

function quoteSnippet(value) {
  const text = value.length > 80 ? `${value.slice(0, 77)}...` : value;
  return JSON.stringify(text);
}

/**
 * Decides whether a raw string is authored, user-visible copy that would need
 * a catalog entry. Everything excluded here is a documented data category;
 * only words outside those categories qualify.
 */
function isAuthoredUiText(raw) {
  const trimmed = String(raw).trim();
  if (trimmed.length === 0) return false; // empty or whitespace-only
  if (!/[A-Za-z\u0400-\u04FF]/.test(trimmed)) return false; // glyphs, timers, numbers
  if (trimmed.length === 1) return false; // single glyph or letter placeholder
  if (/^speechek(\s*[-\u2013\u2014]?\s*(dev|test))?$/i.test(trimmed)) return false; // brand / suffix
  if (/^(dev|test|english|\u0440\u0443\u0441\u0441\u043a\u0438\u0439)$/i.test(trimmed)) return false; // profile, self-names
  if (!/\s/.test(trimmed) && /[0-9]/.test(trimmed)) return false; // F2, 4173, gemini-3.5-transcribe
  // All-caps copy stays eligible: `SAVE` is user-visible text, not a code id.
  if (/^[A-Za-z][A-Za-z0-9]*Error$/.test(trimmed)) return false; // AbortError, NotAllowedError
  if (/^[a-z][a-z0-9]*(?:[.:_-][a-z0-9]+)+$/.test(trimmed)) return false; // colon/dash code ids
  if (/^[a-z][a-z0-9]*[A-Z][A-Za-z0-9]*$/.test(trimmed)) return false; // camelCase code ids
  if (!/\s/.test(trimmed) && /^[A-Za-z0-9+_-]+$/.test(trimmed) && trimmed.includes('+')) return false; // key combos
  return true;
}

function checkKeyReference(index, rawValue, path, line, out) {
  if (rawValue === '') {
    out.push(finding('empty-i18n-ref', path, line, 'empty message key'));
    return false;
  }
  if (!KEY_PATTERN.test(rawValue)) {
    out.push(finding('invalid-i18n-ref', path, line, `message key ${quoteSnippet(rawValue)} is not PascalCase ASCII`));
    return false;
  }
  if (!index.keys.has(rawValue)) {
    out.push(finding('unknown-key', path, line, `message key ${quoteSnippet(rawValue)} is not in the catalog`));
    return false;
  }
  return true;
}

/* -------------------------------------------------------------------------- */
/* HTML                                                                        */
/* -------------------------------------------------------------------------- */

function inspectHtml(index, path, text) {
  let document;
  try {
    document = parseHtml(stripBom(text), { sourceCodeLocationInfo: true });
  } catch (error) {
    return [finding('html-parse-error', path, 1, `could not parse HTML: ${error.message}`)];
  }
  const out = [];
  const comments = [];
  const content = [];
  collectHtmlNodes(document, comments, content);
  const targets = new Set();
  for (const comment of comments) {
    if (!IGNORE_COMMENT_PATTERN.test(comment.value)) continue;
    const next = content.find((entry) => entry.startLine > comment.startLine
      || (entry.startLine === comment.startLine && entry.startOffset >= comment.endOffset));
    if (next) targets.add(next.node);
  }

  const visit = (node, covered, parentIgnored) => {
    const tag = node.tagName;
    if (tag === 'script' || tag === 'style') return;
    const location = node.sourceCodeLocation;
    const ignored = parentIgnored || targets.has(node);

    if (node.nodeName === '#text') {
      const value = typeof node.value === 'string' ? node.value : '';
      const line = location ? location.startLine : 1;
      if (!covered && !ignored && value.trim() !== '' && isAuthoredUiText(value)) {
        out.push(finding('hardcoded-html-text', path, line, `text is not translated: ${quoteSnippet(value.trim())}`));
      }
      return;
    }

    let nextCovered = covered;
    if (node.attrs && node.attrs.length) {
      const textMarker = node.attrs.find((attr) => attr.name === 'data-i18n');
      if (textMarker) {
        checkHtmlMarker(index, path, node, textMarker, out);
        const leaf = !(node.childNodes || []).some((child) => child.tagName);
        if (leaf && (textMarker.value ?? '').length > 0) nextCovered = true;
      }
      for (const [markerName, attribute] of ATTRIBUTE_MARKERS) {
        const marker = node.attrs.find((attr) => attr.name === markerName);
        if (marker) checkHtmlMarker(index, path, node, marker, out);
        const value = node.attrs.find((attr) => attr.name === attribute);
        if (!value || value.value === undefined || value.value.trim() === '') continue;
        const translated = Boolean(marker) && (marker.value ?? '').length > 0;
        const line = htmlAttributeLine(node, attribute);
        if (!ignored && !translated && isAuthoredUiText(value.value)) {
          out.push(finding(
            'hardcoded-html-attribute',
            path,
            line,
            `attribute ${attribute} is not translated: ${quoteSnippet(value.value)}`,
          ));
        }
      }
    }

    for (const child of node.childNodes || []) visit(child, nextCovered, ignored);
  };

  visit(document, false, false);
  return out;
}

/** Splits ignore comments from the ordered nodes such a comment can silence. */
function collectHtmlNodes(node, comments, content) {
  if (!node) return;
  const location = node.sourceCodeLocation;
  if (node.nodeName === '#comment') {
    if (location) {
      comments.push({
        value: typeof node.data === 'string' ? node.data : '',
        startLine: location.startLine,
        endLine: location.endLine,
        endOffset: location.endOffset,
      });
    }
    return;
  }
  const carriesText = node.nodeName === '#text' && String(node.value ?? '').trim() !== '';
  if (location && (node.tagName || carriesText)) {
    content.push({ node, startLine: location.startLine, startOffset: location.startOffset });
  }
  for (const child of node.childNodes || []) collectHtmlNodes(child, comments, content);
}

function checkHtmlMarker(index, path, node, marker, out) {
  const value = marker.value ?? '';
  const line = htmlAttributeLine(node, marker.name);
  if (!checkKeyReference(index, value, path, line, out)) return;
  const expected = index.placeholders.get(value);
  if (!expected || expected.size === 0) return;
  const missing = [...expected].sort();
  out.push(finding('missing-arg', path, line, `HTML marker ${quoteSnippet(marker.name)} for ${quoteSnippet(value)} can not supply argument(s): ${missing.join(', ')}`));
}

function htmlAttributeLine(node, attribute) {
  const location = node.sourceCodeLocation;
  if (location && location.attrs && location.attrs[attribute]) return location.attrs[attribute].startLine;
  return location ? location.startLine : 1;
}

/* -------------------------------------------------------------------------- */
/* JavaScript                                                                  */
/* -------------------------------------------------------------------------- */

function inspectJavaScript(index, path, text) {
  const source = stripBom(text);
  const comments = [];
  let ast = null;
  let lastError = null;
  for (const sourceType of ['module', 'script']) {
    comments.length = 0;
    try {
      ast = parseJavaScript(source, {
        ecmaVersion: 'latest',
        sourceType,
        locations: true,
        onComment: (_block, value, start, end) => comments.push({ value, start, end }),
      });
      break;
    } catch (error) {
      lastError = error;
    }
  }
  if (!ast) {
    const line = lastError && lastError.loc ? lastError.loc.line : 1;
    return [finding('js-parse-error', path, line, `could not parse JavaScript: ${lastError ? lastError.message : 'unknown error'}`)];
  }

  const table = collectScopes(ast);
  const context = { table };
  const ignoredStatements = buildJsIgnoreStatements(ast, comments);
  const out = [];

  walkAst(ast, (node) => {
    if (node.type === 'CallExpression') visitCall(node, context, index, path, out, ignoredStatements);
    else if (node.type === 'ObjectExpression') visitObjectExpression(node, context, index, path, out);
    else if (node.type === 'VariableDeclarator') visitKeyDeclarator(node, context, index, path, out);
    else if (node.type === 'AssignmentExpression') visitAssignment(node, path, out, ignoredStatements, context);
  });

  return out;
}

/** Warning-only suppression: the exact next statement per ignore comment. */
function buildJsIgnoreStatements(ast, comments) {
  const statements = [];
  walkAst(ast, (node) => {
    if (isStatementNode(node)) statements.push(node);
  });
  statements.sort((a, b) => a.start - b.start);
  const ranges = [];
  for (const comment of comments) {
    if (!IGNORE_COMMENT_PATTERN.test(comment.value)) continue;
    const next = statements.find((statement) => statement.start >= comment.end);
    if (next) ranges.push({ start: next.start, end: next.end });
  }
  return ranges;
}

/** True when a warning node lies inside one of the ignored statement ranges. */
function isIgnoredOffset(ranges, offset) {
  return ranges.some((range) => offset >= range.start && offset < range.end);
}

function isStatementNode(node) {
  return /Statement$/.test(node.type)
    || node.type === 'VariableDeclaration'
    || node.type === 'FunctionDeclaration'
    || node.type === 'ClassDeclaration'
    || node.type === 'ExportNamedDeclaration'
    || node.type === 'ExportDefaultDeclaration'
    || node.type === 'ImportDeclaration';
}

function visitCall(node, context, index, path, out, ignoredStatements) {
  if (isHelperCallee(context.table, node.callee)) {
    visitHelperCall(node, index, path, out, context, ignoredStatements);
    return;
  }
  // setAttribute('aria-label', 'literal') accessibility writes.
  if (node.callee.type === 'MemberExpression' && !node.callee.computed
    && node.callee.property.type === 'Identifier' && node.callee.property.name === 'setAttribute') {
    const name = node.arguments[0];
    if (name && name.type === 'Literal' && typeof name.value === 'string' && JS_ATTRIBUTE_SINKS.has(name.value)) {
      warnStaticText(out, path, node.arguments[1], `setAttribute(${quoteSnippet(name.value)})`, ignoredStatements, context);
    }
    return;
  }
  // alert/confirm/prompt literals (skipped when the name is bound in scope).
  if (node.callee.type === 'Identifier' && JS_DIALOG_SINKS.has(node.callee.name)
    && !bindingFor(context.table, node.callee.name, node.callee.start)) {
    warnStaticText(out, path, node.arguments[0], `${node.callee.name}()`, ignoredStatements, context);
  }
}

function visitHelperCall(node, index, path, out, context, ignoredStatements) {
  const helper = node.callee.name;
  const argument = node.arguments[0];
  const candidates = argument ? resolveStatic(argument, context) : [];
  const callLine = node.loc ? node.loc.start.line : 1;
  const knownKeys = [];
  for (const candidate of candidates) {
    const line = candidate.node.loc ? candidate.node.loc.start.line : callLine;
    const known = checkKeyReference(index, candidate.value, path, line, out);
    if (known && !knownKeys.includes(candidate.value)) knownKeys.push(candidate.value);
  }
  if (knownKeys.length === 0) return;

  const argsNode = node.arguments[1];
  if (!argsNode) {
    for (const key of knownKeys) {
      const expected = index.placeholders.get(key);
      if (!expected || expected.size === 0) continue;
      const missing = [...expected].sort();
      out.push(finding('missing-arg', path, callLine, `call ${helper}(...) for ${quoteSnippet(key)} is missing argument(s): ${missing.join(', ')}`));
    }
    return;
  }
  if (argsNode.type !== 'ObjectExpression') return; // dynamic args: tolerated
  const provided = staticPropertyNames(argsNode);
  if (!provided) return; // spread/computed: tolerated
  const providedSet = new Set(provided);
  for (const key of knownKeys) {
    const expected = index.placeholders.get(key);
    if (!expected) continue;
    const missing = [...expected].filter((name) => !providedSet.has(name)).sort();
    if (missing.length > 0) {
      out.push(finding('missing-arg', path, callLine, `call ${helper}(...) for ${quoteSnippet(key)} is missing argument(s): ${missing.join(', ')}`));
    }
    const extra = [...providedSet].filter((name) => !expected.has(name)).sort();
    if (extra.length > 0 && !isIgnoredOffset(ignoredStatements, node.start)) {
      out.push(finding('extra-arg', path, callLine, `call ${helper}(...) for ${quoteSnippet(key)} passes unused argument(s): ${extra.join(', ')}`));
    }
  }
}

function visitObjectExpression(node, context, index, path, out) {
  for (const property of node.properties) {
    if (property.type !== 'Property' || property.computed) continue;
    if (propertyName(property.key) !== 'key') continue;
    for (const candidate of resolveStatic(property.value, context)) {
      const line = candidate.node.loc ? candidate.node.loc.start.line : 1;
      checkKeyReference(index, candidate.value, path, line, out);
    }
  }
}

function visitKeyDeclarator(node, context, index, path, out) {
  if (node.id.type !== 'Identifier' || !KEY_CONSTANT_PATTERN.test(node.id.name) || !node.init) return;
  const values = [];
  collectKeyStrings(node.init, context, values);
  for (const entry of values) {
    if (!KEY_PATTERN.test(entry.value)) continue; // documented non-key data (event names, marker names)
    const line = entry.node.loc ? entry.node.loc.start.line : 1;
    checkKeyReference(index, entry.value, path, line, out);
  }
}

function visitAssignment(node, path, out, ignoredStatements, context) {
  const left = node.left;
  if (left.type !== 'MemberExpression' || left.computed) return;
  if (left.property.type !== 'Identifier' || !JS_TEXT_SINKS.has(left.property.name)) return;
  if (left.property.name === 'textContent' && isStyleTarget(left.object)) return; // injected CSS, not copy
  warnStaticText(out, path, node.right, `.${left.property.name}`, ignoredStatements, context);
}

/** `<style>` elements carry CSS through textContent; that is markup, not copy. */
function isStyleTarget(object) {
  let target = object;
  while (target && (target.type === 'MemberExpression' || target.type === 'ChainExpression')) {
    target = target.type === 'ChainExpression' ? target.expression : target.object;
  }
  return Boolean(target) && target.type === 'Identifier' && /style$/i.test(target.name);
}

function warnStaticText(out, path, valueNode, sink, ignoredStatements, context) {
  const fragments = [];
  collectStaticStrings(valueNode, fragments, context);
  const authored = fragments.find((fragment) => isAuthoredUiText(fragment.value));
  if (!authored) return;
  const line = authored.node.loc ? authored.node.loc.start.line : 1;
  if (isIgnoredOffset(ignoredStatements, valueNode ? valueNode.start : authored.node.start)
    || isIgnoredOffset(ignoredStatements, authored.node.start)) return;
  out.push(finding('hardcoded-js-text', path, line, `UI text assigned to ${sink} is not translated: ${quoteSnippet(authored.value.trim())}`));
}

/**
 * Every literal / template fragment inside an expression, following unique
 * `const` identifiers to their initializer (no code is executed). Dynamic
 * parts contribute no fragment, so `\`${a} / ${b}\``, `String(count)` and a
 * user string held in a variable are silent while a literal prefix next to a
 * value still reports.
 */
function collectStaticStrings(node, out, context, seen = new Set()) {
  if (!node) return;
  switch (node.type) {
    case 'Literal':
      if (typeof node.value === 'string') out.push({ value: node.value, node });
      return;
    case 'TemplateLiteral':
      for (const quasi of node.quasis) {
        const cooked = quasi.value.cooked;
        out.push({ value: typeof cooked === 'string' ? cooked : '', node: quasi });
      }
      return;
    case 'BinaryExpression':
      if (node.operator === '+') {
        collectStaticStrings(node.left, out, context, seen);
        collectStaticStrings(node.right, out, context, seen);
      }
      return;
    case 'ConditionalExpression':
      collectStaticStrings(node.consequent, out, context, seen);
      collectStaticStrings(node.alternate, out, context, seen);
      return;
    case 'LogicalExpression':
      collectStaticStrings(node.left, out, context, seen);
      collectStaticStrings(node.right, out, context, seen);
      return;
    case 'Identifier': {
      if (!context || seen.has(node.name)) return;
      const init = resolveConstInit(context, node.name, node.start);
      if (!init) return;
      seen.add(node.name);
      collectStaticStrings(init, out, context, seen);
      seen.delete(node.name);
      return;
    }
    default:
  }
}

/**
 * Static key candidates of a helper argument. Dynamic operands yield no
 * candidates (tolerated); every possible static key is checked.
 */
function resolveStatic(node, context, seen = new Set()) {
  if (!node) return [];
  switch (node.type) {
    case 'Literal':
      return typeof node.value === 'string' ? [{ value: node.value, node }] : [];
    case 'TemplateLiteral': {
      if (node.expressions.length > 0) return [];
      const cooked = node.quasis[0].value.cooked;
      return [{ value: typeof cooked === 'string' ? cooked : '', node }];
    }
    case 'ConditionalExpression':
      return [...resolveStatic(node.consequent, context, seen), ...resolveStatic(node.alternate, context, seen)];
    case 'LogicalExpression':
      return [...resolveStatic(node.left, context, seen), ...resolveStatic(node.right, context, seen)];
    case 'Identifier': {
      if (seen.has(node.name)) return [];
      const init = resolveConstInit(context, node.name, node.start);
      if (!init) return [];
      seen.add(node.name);
      const resolved = resolveStatic(init, context, seen);
      seen.delete(node.name);
      return resolved.map((entry) => ({ value: entry.value, node }));
    }
    case 'MemberExpression':
      return resolveMember(node, context, seen);
    default:
      return [];
  }
}

function resolveMember(node, context, seen) {
  const map = resolveStaticMap(node.object, context, seen);
  if (!map) return [];
  const values = [];
  if (node.computed) {
    const property = node.property;
    if (property.type === 'Literal' && (typeof property.value === 'string' || typeof property.value === 'number')) {
      values.push(...(map.get(String(property.value)) ?? []));
    } else {
      for (const entry of map.values()) values.push(...entry);
    }
  } else if (node.property.type === 'Identifier') {
    values.push(...(map.get(node.property.name) ?? []));
  }
  const out = [];
  for (const entry of values) out.push(...resolveStatic(entry, context, seen));
  return out;
}

/**
 * Value nodes of a static map expression keyed by property name. Each name
 * keeps every value it may hold (both branches of a conditional map, aliases),
 * so a later branch never hides an earlier key. `null` means dynamic.
 */
function resolveStaticMap(node, context, seen = new Set()) {
  if (!node) return null;
  switch (node.type) {
    case 'ObjectExpression': {
      const map = new Map();
      for (const property of node.properties) {
        if (property.type !== 'Property' || property.computed) continue; // spreads tolerate unknown extras
        const name = propertyName(property.key);
        if (name === null) continue;
        const values = map.get(name);
        if (values) values.push(property.value);
        else map.set(name, [property.value]);
      }
      return map;
    }
    case 'ArrayExpression': {
      const map = new Map();
      node.elements.forEach((element, index) => {
        if (element) map.set(String(index), [element]);
      });
      return map;
    }
    case 'ConditionalExpression':
    case 'LogicalExpression': {
      const left = resolveStaticMap(node.left ?? node.consequent, context, seen);
      const right = resolveStaticMap(node.right ?? node.alternate, context, seen);
      if (!left || !right) return null;
      const merged = new Map();
      for (const [name, values] of [...left, ...right]) {
        const existing = merged.get(name);
        if (existing) existing.push(...values);
        else merged.set(name, [...values]);
      }
      return merged;
    }
    case 'Identifier': {
      if (seen.has(node.name)) return null;
      const init = resolveConstInit(context, node.name, node.start);
      if (!init) return null;
      seen.add(node.name);
      const map = resolveStaticMap(init, context, seen);
      seen.delete(node.name);
      return map;
    }
    case 'CallExpression':
      if (isObjectFreeze(node) && node.arguments[0]) return resolveStaticMap(node.arguments[0], context, seen);
      return null;
    default:
      return null;
  }
}

function isObjectFreeze(node) {
  return node.callee.type === 'MemberExpression' && !node.callee.computed
    && node.callee.object.type === 'Identifier' && node.callee.object.name === 'Object'
    && node.callee.property.type === 'Identifier' && node.callee.property.name === 'freeze';
}

function collectKeyStrings(node, context, out, seen = new Set()) {
  if (!node) return;
  switch (node.type) {
    case 'Literal':
      if (typeof node.value === 'string') out.push({ value: node.value, node });
      return;
    case 'TemplateLiteral':
      if (node.expressions.length === 0) {
        const cooked = node.quasis[0].value.cooked;
        out.push({ value: typeof cooked === 'string' ? cooked : '', node });
      }
      return;
    case 'ObjectExpression':
      for (const property of node.properties) {
        if (property.type === 'Property') collectKeyStrings(property.value, context, out, seen);
        else if (property.type === 'SpreadElement') collectKeyStrings(property.argument, context, out, seen);
      }
      return;
    case 'ArrayExpression':
      for (const element of node.elements) if (element) collectKeyStrings(element, context, out, seen);
      return;
    case 'ConditionalExpression':
      collectKeyStrings(node.consequent, context, out, seen);
      collectKeyStrings(node.alternate, context, out, seen);
      return;
    case 'LogicalExpression':
      collectKeyStrings(node.left, context, out, seen);
      collectKeyStrings(node.right, context, out, seen);
      return;
    case 'Identifier': {
      if (seen.has(node.name)) return;
      const init = resolveConstInit(context, node.name, node.start);
      if (!init) return;
      seen.add(node.name);
      collectKeyStrings(init, context, out, seen);
      seen.delete(node.name);
      return;
    }
    case 'CallExpression':
      if (isObjectFreeze(node) && node.arguments[0]) collectKeyStrings(node.arguments[0], context, out, seen);
      return;
    default:
  }
}

function staticPropertyNames(objectExpression) {
  const names = [];
  for (const property of objectExpression.properties) {
    if (property.type !== 'Property' || property.computed) return null; // spread/computed: tolerated
    const name = propertyName(property.key);
    if (name === null) return null;
    names.push(name);
  }
  return names;
}

function propertyName(key) {
  if (!key) return null;
  if (key.type === 'Identifier') return key.name;
  if (key.type === 'Literal' && typeof key.value === 'string') return key.value;
  return null;
}

const I18N_IMPORT_PATTERN = /(^|\/)i18n\.js$/;

/** Scopes that can own a binding: program, functions, blocks, loops, catch. */
function isScopeNode(node) {
  return node.type === 'Program' || node.type === 'BlockStatement'
    || node.type === 'FunctionDeclaration' || node.type === 'FunctionExpression'
    || node.type === 'ArrowFunctionExpression' || node.type === 'StaticBlock'
    || node.type === 'CatchClause'
    || node.type === 'ForStatement' || node.type === 'ForInStatement' || node.type === 'ForOfStatement';
}

function isFunctionNode(node) {
  return node.type === 'FunctionDeclaration' || node.type === 'FunctionExpression' || node.type === 'ArrowFunctionExpression';
}

/**
 * Lexical binding table: each scope holds only the names declared directly in
 * it, so a lookup follows the scope chain from the use site and a nested
 * shadowing declaration neither hides nor hijacks an outer one. Patterns bind
 * their identifier names; nothing is evaluated.
 */
function collectScopes(ast) {
  const root = { node: ast, start: ast.start, end: ast.end, parent: null, children: [], bindings: new Map() };
  const nearestFunction = (scope) => {
    let current = scope;
    while (current && current.node.type !== 'Program' && !isFunctionNode(current.node)) current = current.parent;
    return current || root;
  };
  const visit = (node, scope) => {
    if (!node || typeof node !== 'object') return;
    if (Array.isArray(node)) {
      for (const child of node) visit(child, scope);
      return;
    }
    if (typeof node.type !== 'string') return;

    let current = scope;
    if (isScopeNode(node) && node !== scope.node) {
      current = { node, start: node.start, end: node.end, parent: scope, children: [], bindings: new Map() };
      scope.children.push(current);
      if (isFunctionNode(node)) {
        for (const param of node.params) bindPattern(param, current, { kind: 'param', init: null, source: null });
      }
    }

    if (node.type === 'ImportDeclaration' && node.source) {
      const source = typeof node.source.value === 'string' ? node.source.value : '';
      for (const specifier of node.specifiers) {
        if (specifier.local && specifier.local.type === 'Identifier') {
          declareBinding(root, specifier.local.name, { kind: 'import', init: null, source });
        }
      }
    } else if (node.type === 'FunctionDeclaration' && node.id) {
      declareBinding(scope, node.id.name, { kind: 'function', init: null, source: null });
    } else if (node.type === 'VariableDeclaration') {
      const target = node.kind === 'var' ? nearestFunction(current) : current;
      for (const declarator of node.declarations) {
        bindPattern(declarator.id, target, { kind: node.kind, init: declarator.init || null, source: null });
      }
    }

    for (const key of Object.keys(node)) {
      if (key === 'type' || key === 'loc' || key === 'start' || key === 'end' || key === 'range') continue;
      const value = node[key];
      if (value && typeof value === 'object') visit(value, current);
    }
  };
  visit(ast, root);
  return root;
}

function declareBinding(scope, name, binding) {
  const list = scope.bindings.get(name);
  if (list) list.push(binding);
  else scope.bindings.set(name, [binding]);
}

/** Binds every identifier a parameter or declarator pattern introduces. */
function bindPattern(pattern, scope, binding) {
  if (!pattern) return;
  switch (pattern.type) {
    case 'Identifier':
      declareBinding(scope, pattern.name, binding);
      return;
    case 'ObjectPattern':
      for (const property of pattern.properties) {
        if (property.type === 'RestElement') bindPattern(property.argument, scope, binding);
        else if (property.type === 'Property') bindPattern(property.value, scope, binding);
      }
      return;
    case 'ArrayPattern':
      for (const element of pattern.elements) if (element) bindPattern(element, scope, binding);
      return;
    case 'AssignmentPattern':
      bindPattern(pattern.left, scope, binding);
      return;
    case 'RestElement':
      bindPattern(pattern.argument, scope, binding);
      return;
    default:
  }
}

function scopeAt(scope, offset) {
  for (const child of scope.children) {
    if (offset >= child.start && offset < child.end) return scopeAt(child, offset);
  }
  return scope;
}

/**
 * Nearest lexical binding for a use site; duplicate declarations in the same
 * scope are ambiguous and treated as unknown, never as an outer name.
 */
function bindingFor(table, name, offset) {
  let scope = scopeAt(table, offset);
  while (scope) {
    const list = scope.bindings.get(name);
    if (list) return list.length === 1 ? list[0] : null;
    scope = scope.parent;
  }
  return null;
}

/** Initializer of the nearest `const` binding, or `null`. */
function resolveConstInit(context, name, offset) {
  const binding = bindingFor(context.table, name, offset);
  return binding && binding.kind === 'const' ? binding.init : null;
}

/** t/msg/ui/uiMsg bound to the shared module import or to a local function. */
function isHelperCallee(table, callee) {
  if (callee.type !== 'Identifier' || !HELPER_NAMES.includes(callee.name)) return false;
  const binding = bindingFor(table, callee.name, callee.start);
  if (!binding) return false;
  if (binding.kind === 'import') return typeof binding.source === 'string' && I18N_IMPORT_PATTERN.test(binding.source);
  if (binding.kind === 'function') return true;
  if (binding.kind === 'const' || binding.kind === 'let' || binding.kind === 'var') {
    return Boolean(binding.init) && (binding.init.type === 'ArrowFunctionExpression' || binding.init.type === 'FunctionExpression');
  }
  return false;
}

/** Generic pre-order AST walk (acorn trees have no parent pointers). */
function walkAst(node, visit) {
  if (!node || typeof node !== 'object') return;
  if (Array.isArray(node)) {
    for (const child of node) walkAst(child, visit);
    return;
  }
  if (typeof node.type !== 'string') return;
  visit(node);
  for (const key of Object.keys(node)) {
    if (key === 'type' || key === 'loc' || key === 'start' || key === 'end' || key === 'range') continue;
    const value = node[key];
    if (value && typeof value === 'object') walkAst(value, visit);
  }
}

/* -------------------------------------------------------------------------- */
/* Rust (advisory)                                                             */
/* -------------------------------------------------------------------------- */

function inspectRust(path, text) {
  const tokens = tokenizeRust(stripBom(text));
  const out = [];
  for (let position = 0; position + 1 < tokens.length; position += 1) {
    const token = tokens[position];
    if (token.type !== 'ident' || !RUST_SINK_NAMES.has(token.value)) continue;
    const next = tokens[position + 1];
    if (next.type !== 'punct' || next.value !== '(') continue;
    let depth = 1;
    let argument = 0;
    for (let scan = position + 2; scan < tokens.length && depth > 0; scan += 1) {
      const current = tokens[scan];
      if (current.type === 'punct' && current.value === '(') depth += 1;
      else if (current.type === 'punct' && current.value === ')') depth -= 1;
      else if (current.type === 'punct' && current.value === ',' && depth === 1) argument += 1;
      else if (current.type === 'string' && depth === 1
        && isCopyArgument(token.value, argument) && isAuthoredUiText(current.value)) {
        out.push(finding('hardcoded-rust-text', path, current.line, `literal UI text passed to ${token.value}() is not translated: ${quoteSnippet(current.value.trim())}`));
      }
    }
  }
  return out;
}

/**
 * `with_id` carries an identifier first and the visible label later
 * (`MenuItem::with_id(app, "menu-id", "Label", ...)`), so only the third and
 * later arguments are copy; the other sinks take copy first.
 */
function isCopyArgument(sink, argument) {
  return sink === 'with_id' ? argument >= 2 : true;
}

/** String/comment-aware token stream: identifiers, `(`, `)`, and string literals. */
function tokenizeRust(source) {
  const tokens = [];
  let index = 0;
  let line = 1;
  const length = source.length;
  const consume = (count) => {
    const end = Math.min(index + count, length);
    while (index < end) {
      if (source[index] === '\n') line += 1;
      index += 1;
    }
  };
  const pushString = (startLine, value) => tokens.push({ type: 'string', value, line: startLine });
  const readQuoted = () => {
    const startLine = line;
    consume(1); // opening quote
    let value = '';
    while (index < length) {
      const current = source[index];
      if (current === '\\') { if (index + 1 < length) value += source[index + 1]; consume(2); continue; }
      if (current === '"') { consume(1); break; }
      if (current === '\n') break;
      value += current;
      consume(1);
    }
    pushString(startLine, value);
  };
  const readRaw = () => {
    const startLine = line;
    consume(1); // 'r'
    let hashes = 0;
    while (source[index] === '#') { hashes += 1; consume(1); }
    consume(1); // opening quote
    const closing = `"${'#'.repeat(hashes)}`;
    let value = '';
    while (index < length) {
      if (source.startsWith(closing, index)) { consume(closing.length); break; }
      value += source[index];
      consume(1);
    }
    pushString(startLine, value);
  };

  while (index < length) {
    const char = source[index];
    if (char === '/' && source[index + 1] === '/') {
      let end = index;
      while (end < length && source[end] !== '\n') end += 1;
      consume(end - index);
      continue;
    }
    if (char === '/' && source[index + 1] === '*') {
      let depth = 0;
      while (index < length) {
        if (source[index] === '/' && source[index + 1] === '*') { depth += 1; consume(2); }
        else if (source[index] === '*' && source[index + 1] === '/') { depth -= 1; consume(2); if (depth <= 0) break; }
        else consume(1);
      }
      continue;
    }
    if (char === '"') {
      readQuoted();
      continue;
    }
    if (char === 'r' && rawStringStart(source, index)) {
      readRaw();
      continue;
    }
    if (char === 'b' && (source[index + 1] === '"' || rawStringStart(source, index + 1))) {
      consume(1); // 'b'
      if (source[index] === '"') readQuoted();
      else readRaw();
      continue;
    }
    if (char === "'") {
      // Char literal (`'a'`, `'\n'`) or a lifetime (`'static`).
      if (source[index + 1] === '\\') {
        consume(2);
        while (index < length && source[index] !== "'" && source[index] !== '\n') consume(1);
        if (source[index] === "'") consume(1);
      } else if (source[index + 2] === "'" && source[index + 1] !== "'") {
        consume(3);
      } else {
        consume(1);
      }
      continue;
    }
    if (/[A-Za-z_]/.test(char)) {
      const startLine = line;
      let end = index;
      while (end < length && /[A-Za-z0-9_]/.test(source[end])) end += 1;
      tokens.push({ type: 'ident', value: source.slice(index, end), line: startLine });
      consume(end - index);
      continue;
    }
    if (char === '(' || char === ')' || char === ',') {
      tokens.push({ type: 'punct', value: char, line });
      consume(1);
      continue;
    }
    consume(1);
  }
  return tokens;
}

function rawStringStart(source, at) {
  if (source[at] !== 'r') return false;
  let scan = at + 1;
  while (source[scan] === '#') scan += 1;
  return source[scan] === '"';
}
