/**
 * scripts/localization-sources.test.mjs
 *
 * Regression tests for the static localization guard. All fixtures are
 * synthetic sources defined here; nothing reads the repository's real pages or
 * copy, so the tests stay green while the pages are rewritten.
 *
 * Run: node --test scripts/localization-sources.test.mjs
 * (the release wrapper may also run it with Bun 1.4.2's node:test support)
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import { inspectSources } from './localization-sources.mjs';

/** Small catalog fixture; keys mirror the real PascalCase convention. */
const CATALOG = Object.freeze({
  Alpha: { en: 'Alpha', ru: 'Альфа' },
  Beta: { en: 'Beta', ru: 'Бета' },
  Gamma: { en: 'Gamma', ru: 'Гамма' },
  Delta: { en: 'Delta', ru: 'Дельта' },
  CountOne: { en: '{count} item', ru: '{count} элемент' },
  WithDetail: { en: 'Failed: {detail}', ru: 'Ошибка: {detail}' },
  WithLine: { en: 'Line {line}', ru: 'Строка {line}' },
  WithCount: { en: '{count} items', ru: '{count} элемент' },
})

function inspect(files, catalog = CATALOG) {
  return inspectSources(catalog, files);
}

function codes(findings) {
  return findings.map((item) => `${item.code}@${item.path}:${item.line}`);
}

/* -------------------------------------------------------------------------- */
/* Key references: HTML                                                        */
/* -------------------------------------------------------------------------- */

test('HTML markers: unknown key is an error with a deterministic line', () => {
  const html = [
    '<html><body>',
    '  <h1 data-i18n="Alpha">Alpha</h1>',
    '  <p data-i18n="MissingKey">x</p>',
    '  <button data-i18n-aria-label="AlsoMissing" aria-label="x">x</button>',
    '</body></html>',
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'page.html', text: html }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.html:3', 'unknown-key@page.html:4']);
  assert.deepEqual(warnings, []);
});

test('HTML markers: empty and invalid-shaped keys are errors', () => {
  const html = '<div data-i18n="">a</div><div data-i18n="lowerCase">b</div><div data-i18n="Has Space">c</div>';
  const { errors } = inspect([{ path: 'page.html', text: html }]);
  assert.deepEqual(errors.map((item) => item.code), ['empty-i18n-ref', 'invalid-i18n-ref', 'invalid-i18n-ref']);
});

test('HTML markers: known keys produce no findings', () => {
  const html = '<h1 data-i18n="Alpha">Alpha</h1><button data-i18n-title="Beta" title="Beta">x</button><input data-i18n-placeholder="Gamma" placeholder="Gamma">';
  const { errors, warnings } = inspect([{ path: 'page.html', text: html }]);
  assert.deepEqual(errors, []);
  assert.deepEqual(warnings, []);
});

test('HTML: unmarked authored text and attributes warn, marked fallbacks do not', () => {
  const html = [
    '<p data-i18n="Alpha">Alpha</p>',
    '<p>Hardcoded sentence</p>',
    '<input placeholder="Search everything">',
    '<input placeholder="F2">',
    '<input placeholder="4173">',
    '<span aria-label="Recording duration" data-i18n-aria-label="Beta">00:00</span>',
    '<button aria-label="Help: mode" data-i18n-aria-label="Beta">?</button>',
    '<span class="model-name">gemini-3.5-transcribe-live</span>',
    '<span aria-hidden="true">◉</span>',
    '<span>SPEECHEK</span>',
    '<option value="en">English</option>',
    '<option value="ru">Русский</option>',
    '<span>Speechek — Dev</span>',
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'page.html', text: html }]);
  assert.deepEqual(errors, []);
  assert.deepEqual(
    warnings.map((item) => `${item.code}:${item.line}`),
    ['hardcoded-html-text:2', 'hardcoded-html-attribute:3'],
  );
});

test('HTML: a marked text leaf covers only its own text, not marked parents with element children', () => {
  const html = '<div data-i18n="Alpha"><span>Raw child</span></div>';
  const { warnings } = inspect([{ path: 'page.html', text: html }]);
  assert.deepEqual(warnings.map((item) => item.code), ['hardcoded-html-text']);
});

/* -------------------------------------------------------------------------- */
/* Key references: JavaScript literals, ternaries, maps, descriptors           */
/* -------------------------------------------------------------------------- */

test('JS: literal helper calls — unknown keys error, known keys pass', () => {
  const js = [
    "import { t } from './i18n.js';",
    "const ok = t('Alpha');",
    "const bad = t('NotInCatalog');",
    "const empty = t('');",
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.js:3', 'empty-i18n-ref@page.js:4']);
});

test('JS: locally declared helpers (msg/ui/uiMsg) are checked like imported t', () => {
  const js = [
    'function msg(key, args = {}) { return { key, args }; }',
    "const ui = (key, args) => (args ? { key, args } : { key });",
    "const a = msg('Alpha');",
    "const b = ui('MissingOne');",
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.js:4']);
});

test('JS: shadowed helper names are not treated as translation calls', () => {
  const js = [
    "import { t } from './i18n.js';",
    "const msg = helpers.msg;",
    "msg('NotAKeyButData');",
    // t is re-declared as a non-callable unique const: the import binding wins
    // only when it is unique, so this file must produce no error for either.
    "const value = t('Alpha');",
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(errors, []);
});

test('JS: ternary and logical key branches are all checked', () => {
  const js = [
    "import { t } from './i18n.js';",
    "const a = t(flag ? 'Alpha' : 'MissingBranch');",
    "const b = t(other ?? 'Beta');",
    "const c = t(dynamicValue);",
    "const d = t(flag ? dynamicLeft : dynamicRight);",
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.js:2']);
});

test('JS: _KEYS constant maps (including Object.freeze) and inline maps with dynamic index', () => {
  const js = [
    'const MODE_KEYS = { live: \'Alpha\', smart: \'MissingInMap\' };',
    'const COUNT_KEYS = Object.freeze({ one: \'Gamma\', other: \'Beta\' });',
    "import { t } from './i18n.js';",
    'const a = t(MODE_KEYS[name]);',
    'const b = t(COUNT_KEYS[category] ?? COUNT_KEYS.one ?? { one: \'Gamma\' }[category]);',
    'const c = t({ x: \'MissingInline\' }[dynamicIndex]);',
    "const d = t(MODE_KEYS.live, { count: 1 });",
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), [
    'unknown-key@page.js:1',
    'unknown-key@page.js:6',
  ]);
  assert.deepEqual(codes(warnings), ['extra-arg@page.js:7']);
});

test('JS: _KEY constants with non-key data values are ignored, PascalCase values are checked', () => {
  const js = [
    "const INTERNAL_KEY = 'Alpha';",
    "const READ_FAILED_KEY = 'MissingConstant';",
    "const EVENT_HOTKEY_KEY = 'speechek:settings-hotkey-key';",
    "const DEFAULT_HOTKEY = 'F2';",
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.js:2']);
});

test('JS: descriptor literals with a key property are checked anywhere', () => {
  const js = [
    'const MIC_DENIED = { key: \'Alpha\' };',
    "finishError(active, { key: 'NotInCatalog' }, MIC_DENIED);",
    'const other = { key: dynamicKey };',
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.js:2']);
});

test('JS: identifier-resolved const keys are checked at the use site', () => {
  const js = [
    "import { t } from './i18n.js';",
    "const INTERNAL_KEY = 'Alpha';",
    "const OTHER_KEY = 'MissingConst';",
    'const a = t(INTERNAL_KEY);',
    'const b = t(OTHER_KEY);',
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.js:3', 'unknown-key@page.js:5']);
});

test('JS: dynamic argument keys are tolerated without findings', () => {
  const js = [
    "import { t } from './i18n.js';",
    'const a = t(key);',
    'const b = t(`Interp${suffix}`);',
    'const c = t(getKey());',
    'const d = t(MODE_KEYS.live);',
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(errors, []);
  assert.deepEqual(warnings, []);
});

/* -------------------------------------------------------------------------- */
/* Placeholder arguments                                                       */
/* -------------------------------------------------------------------------- */

test('args: missing placeholder argument is an error, extra argument is a warning', () => {
  const js = [
    "import { t } from './i18n.js';",
    "const ok = t('CountOne', { count: 3 });",
    "const missing = t('WithLine', { count: 3 });",
    "const absent = t('WithDetail');",
    "const extra = t('Alpha', { count: 3 });",
    "const dynamic = t('WithLine', args);",
    "const spread = t('WithLine', { ...args });",
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['missing-arg@page.js:3', 'missing-arg@page.js:4']);
  assert.deepEqual(codes(warnings), ['extra-arg@page.js:3', 'extra-arg@page.js:5']);
});

test('args: a dynamic key skips the args check entirely', () => {
  const js = [
    "import { t } from './i18n.js';",
    'const dynamic = t(key, { something: 1 });',
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(errors, []);
  assert.deepEqual(warnings, []);
});

/* -------------------------------------------------------------------------- */
/* Hardcoded JavaScript text                                                   */
/* -------------------------------------------------------------------------- */

test('JS: literal assignments into UI sinks warn; data-only and dynamic values do not', () => {
  const js = [
    "el.textContent = 'Hardcoded label';",
    "el.innerText = 'Also hardcoded';",
    "el.title = theme === 'dark' ? 'Dark mode' : 'Light mode';",
    "el.placeholder = 'Search';",
    "el.setAttribute('aria-label', 'Close dialog');",
    "el.setAttribute('aria-expanded', on ? 'true' : 'false');",
    "alert('Saved');",
    "el.textContent = '';",
    "el.textContent = '00:00';",
    "el.textContent = String(count);",
    "el.textContent = label;",
    "el.textContent = `Interpolated ${value}`;",
    "el.textContent = `${String(minutes)}:${String(seconds)}`;",
    "console.error('diagnostic only');",
    "el.textContent = 'Speechek';",
  ].join('\n');
  const { warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(
    warnings.map((item) => `${item.code}:${item.line}`),
    [
      'hardcoded-js-text:1',
      'hardcoded-js-text:2',
      'hardcoded-js-text:3',
      'hardcoded-js-text:4',
      'hardcoded-js-text:5',
      'hardcoded-js-text:7',
      'hardcoded-js-text:12',
    ],
  );
});

test('JS: composed static strings warn at the authored fragment line', () => {
  const js = [
    "el.title = 'Speechek — ' + name;",
    "el.textContent = prefix + ' items';",
    "el.placeholder = prefix + ' / ' + suffix;",
  ].join('\n');
  const { warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(warnings), ['hardcoded-js-text@page.js:1', 'hardcoded-js-text@page.js:2']);
});

/* -------------------------------------------------------------------------- */
/* Rust advisory                                                               */
/* -------------------------------------------------------------------------- */

test('Rust: literal native UI text warns; dynamic catalog calls and data ids do not', () => {
  const rust = [
    'window.set_title("Hardcoded native");',
    'items.settings.set_text(localized(language, MessageId::TraySettings));',
    '.title(crate::localized(app, crate::i18n::MessageId::OverlayTitle))',
    'TrayIconBuilder::with_id("speechek")',
    'label.set_text("Settings")',
    'MenuItem::with_id(app, "menu-id", localized(language, MessageId::TrayExit), true, None)',
    'MenuItem::with_id(app, "menu-id", "Hardcoded item", true, None)',
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'src/lib.rs', text: rust }]);
  assert.deepEqual(errors, []);
  assert.deepEqual(codes(warnings), [
    'hardcoded-rust-text@src/lib.rs:1',
    'hardcoded-rust-text@src/lib.rs:5',
    'hardcoded-rust-text@src/lib.rs:7',
  ]);
});

test('Rust: raw strings and comments are handled; logging and dynamic calls are untouched', () => {
  const rust = [
    'let copy = r#"Speechek — Settings"#;',
    '// set_title("Commented out")',
    '/* set_text("Block commented") */',
    'eprintln!("cannot retitle the window");',
    'tray.set_text(copy);',
    'tray.set_title(r#"Hardcoded raw"#);',
    'menu.set_text("Hardcoded plain");',
  ].join('\n');
  const { warnings } = inspect([{ path: 'src/lib.rs', text: rust }]);
  assert.deepEqual(codes(warnings), ['hardcoded-rust-text@src/lib.rs:6', 'hardcoded-rust-text@src/lib.rs:7']);
});

/* -------------------------------------------------------------------------- */
/* Ignore convention                                                           */
/* -------------------------------------------------------------------------- */

test('ignore: HTML comment suppresses the next node warnings only when a reason is present', () => {
  const html = [
    '<!-- i18n-ignore-next: preboot English fallback -->',
    '<p>Ignored fallback</p>',
    '<p>Still warned</p>',
    '<!-- i18n-ignore-next: -->',
    '<p>No reason, still warned</p>',
  ].join('\n');
  const { warnings } = inspect([{ path: 'page.html', text: html }]);
  assert.deepEqual(codes(warnings), [
    'hardcoded-html-text@page.html:3',
    'hardcoded-html-text@page.html:5',
  ]);
});

test('ignore: warnings are suppressed, key errors never are', () => {
  const html = '<!-- i18n-ignore-next: reason --><p data-i18n="MissingKey">text</p>';
  assert.deepEqual(codes(inspect([{ path: 'page.html', text: html }]).errors), ['unknown-key@page.html:1']);

  const js = [
    "import { t } from './i18n.js';",
    '// i18n-ignore-next: reason',
    "el.textContent = 'Ignored label';",
    "t('MissingKey');",
  ].join('\n');
  const result = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(result.warnings), []);
  assert.deepEqual(codes(result.errors), ['unknown-key@page.js:4']);
});

test('ignore: an inline comment silences only the node that follows it', () => {
  const html = '<!-- i18n-ignore-next: reason --><p>Ignored</p><p>Warned</p>';
  const { warnings } = inspect([{ path: 'page.html', text: html }]);
  assert.deepEqual(warnings.map((item) => item.message), ['text is not translated: "Warned"']);
});

test('ignore: a JS comment silences only the exact next statement', () => {
  const js = [
    "import { t } from './i18n.js';",
    '// i18n-ignore-next: reason',
    "el.textContent = 'Ignored'; el.textContent = 'Warned';",
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(errors, []);
  assert.deepEqual(warnings.map((item) => item.message), ['UI text assigned to .textContent is not translated: "Warned"']);
});

test('all-caps UI copy warns while the brand stays excluded', () => {
  const { warnings } = inspect([
    { path: 'page.js', text: "el.textContent = 'SAVE';\nel.textContent = 'CONTINUE';\nel.textContent = 'SPEECHEK';" },
    { path: 'page.html', text: '<button>SAVE</button><span>SPEECHEK</span>' },
  ]);
  assert.deepEqual(warnings.map((item) => `${item.code}:${item.path}:${item.line}`), [
    'hardcoded-html-text:page.html:1',
    'hardcoded-js-text:page.js:1',
    'hardcoded-js-text:page.js:2',
  ]);
});

test('JS: every branch of a conditional key map is checked, directly and through an alias', () => {
  const js = [
    "import { t } from './i18n.js';",
    "const MODE_MAP = live ? { state: 'MissingFirstBranch' } : { state: 'Alpha' };",
    'const alias = MODE_MAP;',
    'const a = t(MODE_MAP[kind]);',
    'const b = t(alias[kind]);',
    "const c = t((live ? { state: 'MissingInlineFirst' } : { state: 'Gamma' })[kind]);",
    "const d = t((live ? { state: 'Alpha' } : { state: 'MissingSecondBranch' })[kind]);",
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), [
    'unknown-key@page.js:2',
    'unknown-key@page.js:6',
    'unknown-key@page.js:7',
  ]);
});

test('JS: unique const literals assigned to UI sinks warn, dynamic user values stay silent', () => {
  const js = [
    "const LABEL = 'Please wait';",
    'const USER = input.value;',
    "const TIMER = '00:00';",
    "const BRAND = 'Speechek';",
    'const alias = LABEL;',
    'node.textContent = LABEL;',
    'node.placeholder = alias;',
    'node.textContent = USER;',
    'node.textContent = TIMER;',
    'node.textContent = BRAND;',
  ].join('\n');
  const { warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(warnings.map((item) => item.message), [
    'UI text assigned to .placeholder is not translated: "Please wait"',
    'UI text assigned to .textContent is not translated: "Please wait"',
  ]);
});

/* -------------------------------------------------------------------------- */
/* Robustness, ordering and catalog-free mode                                  */
/* -------------------------------------------------------------------------- */

test('a JS parse failure is blocking and never throws', () => {
  const { errors, warnings } = inspect([{ path: 'broken.js', text: 'const = ;' }]);
  assert.deepEqual(errors.map((item) => item.code), ['js-parse-error']);
  assert.deepEqual(warnings, []);
});

test('unknown extensions are ignored; .mjs/.cjs/.htm are inspected', () => {
  const files = [
    { path: 'notes.txt', text: 'Hardcoded text' },
    { path: 'page.htm', text: '<p>Hardcoded text</p>' },
    { path: 'mod.mjs', text: "el.textContent = 'Hardcoded label';" },
    { path: 'mod.cjs', text: "el.textContent = 'Hardcoded label';" },
  ];
  const { warnings } = inspect(files);
  assert.deepEqual(warnings.map((item) => item.path), ['mod.cjs', 'mod.mjs', 'page.htm']);
});

test('findings are sorted deterministically and de-duplicated across repeated runs', () => {
  const files = [
    { path: 'z.js', text: "import { t } from './i18n.js';\nt('MissingZ'); t('MissingZ');" },
    { path: 'a.html', text: '<p data-i18n="MissingA">x</p>' },
  ];
  const first = inspect(files);
  const second = inspect(files);
  assert.deepEqual(first, second);
  assert.deepEqual(codes(first.errors), ['unknown-key@a.html:1', 'unknown-key@z.js:2']);
});

test('inspectSources rejects a non-object catalog', () => {
  assert.throws(() => inspectSources(null, []), TypeError);
  assert.throws(() => inspectSources([], []), TypeError);
});

test('HTML markers cannot supply placeholders, so a parameterized key is a missing-arg error', () => {
  const html = [
    '<p data-i18n="WithCount">x</p>',
    '<button data-i18n-title="WithCount">y</button>',
    '<input data-i18n-placeholder="WithLine">',
    '<p data-i18n="Alpha">Alpha</p>',
  ].join('\n');
  const { errors } = inspect([{ path: 'page.html', text: html }]);
  assert.deepEqual(codes(errors), [
    'missing-arg@page.html:1',
    'missing-arg@page.html:2',
    'missing-arg@page.html:3',
  ]);
});

test('JS: a nested local t does not disable the module-level imported t', () => {
  const js = [
    "import { t } from './i18n.js';",
    "t('MissingAtRoot');",
    'function helper() {',
    '  const t = config.translate;',
    "  return t('external-data');",
    '}',
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.js:2']);
});

test('JS: a shadowing parameter is not treated as the imported translator', () => {
  const js = [
    "import { t } from './i18n.js';",
    'function consume(t) {',
    "  t('external-data');",
    '}',
    'function log(alert) {',
    "  alert('external dialog');",
    '}',
    "t('MissingWhileShadowed');",
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.js:8']);
  assert.deepEqual(warnings, []);
});

test('JS: a shadowed const key resolves to the nearest binding', () => {
  const js = [
    "import { t } from './i18n.js';",
    "const KEY = 'Alpha';",
    'function inner() {',
    "  const KEY = 'MissingInner';",
    '  return t(KEY);',
    '}',
    'function outer() {',
    "  const KEY = 'Beta';",
    '  return t(KEY);',
    '}',
    'const ok = t(KEY);',
  ].join('\n');
  const { errors } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(codes(errors), ['unknown-key@page.js:5']);
});

test('missing argument detection ignores foreign placeholders and nested descriptors', () => {
  const js = [
    "import { t } from './i18n.js';",
    "const ui = (key, args) => ({ key, args });",
    // Nested descriptor values are not placeholder names of this key.
    "const a = t('WithDetail', { detail: ui('Alpha') });",
  ].join('\n');
  const { errors, warnings } = inspect([{ path: 'page.js', text: js }]);
  assert.deepEqual(errors, []);
  assert.deepEqual(warnings, []);
});
