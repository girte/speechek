// Tests for scripts/localization-report.mjs.
//
// Pure-function tests run without Git. Git-backed tests build a throwaway
// repository inside a unique temporary directory, point Git at an empty
// fixture-local config, and remove the whole directory afterwards.

import { test as runTest } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { compareCatalogs, formatReport, readBaseline } from './localization-report.mjs';

// Bun's test runner defaults to a 5s per-test timeout, too short for throwaway
// Git repositories on Windows; Node's runner has no per-test default.
const TEST_TIMEOUT_MS = 120_000;
const test = (name, fn) => runTest(name, { timeout: TEST_TIMEOUT_MS }, fn);

const FIXTURE_PREFIX = 'speechek-localization-report-';
const CATALOG_FILE = path.join('public', 'messages.json');
const COMMIT_SHA = /^[0-9a-f]{40}$/;

const GIT_CONFIG_ARGS = [
  '-c', 'user.name=Localization Report Tests',
  '-c', 'user.email=tests@example.invalid',
  '-c', 'commit.gpgsign=false',
  '-c', 'tag.gpgSign=false',
  '-c', 'core.autocrlf=false',
];

function runGit(repo, args) {
  const result = spawnSync('git', [...GIT_CONFIG_ARGS, ...args], {
    cwd: repo,
    encoding: 'utf8',
    shell: false,
    windowsHide: true,
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(`git ${args.join(' ')} failed (${result.status}): ${String(result.stderr || '').trim()}`);
  }
  return result.stdout;
}

function withFixture(fn) {
  const root = mkdtempSync(path.join(tmpdir(), FIXTURE_PREFIX));
  const repo = path.join(root, 'repo');
  mkdirSync(repo);

  const globalConfig = path.join(root, 'isolated.gitconfig');
  writeFileSync(globalConfig, '', 'utf8');

  const overrides = {
    GIT_CONFIG_NOSYSTEM: '1',
    GIT_CONFIG_GLOBAL: globalConfig,
    GIT_TERMINAL_PROMPT: '0',
    GIT_OPTIONAL_LOCKS: '0',
  };
  const removals = ['GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_OBJECT_DIRECTORY', 'GIT_COMMON_DIR'];
  const restore = new Map();
  for (const [key, value] of Object.entries(overrides)) {
    restore.set(key, Object.prototype.hasOwnProperty.call(process.env, key) ? process.env[key] : null);
    process.env[key] = value;
  }
  for (const key of removals) {
    if (Object.prototype.hasOwnProperty.call(process.env, key)) {
      restore.set(key, process.env[key]);
      delete process.env[key];
    }
  }

  let failure = null;
  try {
    runGit(repo, ['init', '--quiet', '-b', 'main']);
    fn({ root, repo });
  } catch (error) {
    failure = error;
  } finally {
    for (const [key, value] of restore) {
      if (value === null) {
        delete process.env[key];
      } else {
        process.env[key] = value;
      }
    }
    try {
      rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 25 });
    } catch (error) {
      failure ??= error;
    }
    if (existsSync(root)) {
      const error = new Error(`temporary fixture ${root} was not removed`);
      failure ??= error;
    }
  }

  if (failure) {
    throw failure;
  }
}

function captureThrow(fn) {
  try {
    fn();
  } catch (error) {
    return error;
  }
  throw new assert.AssertionError({ message: 'expected the call to throw' });
}


function catalogPath(repo) {
  return path.join(repo, CATALOG_FILE);
}

function writeCatalog(repo, catalog, { pretty = true } = {}) {
  mkdirSync(path.join(repo, 'public'), { recursive: true });
  writeFileSync(catalogPath(repo), pretty ? `${JSON.stringify(catalog, null, 2)}\n` : JSON.stringify(catalog), 'utf8');
}

function writeRawCatalog(repo, text) {
  mkdirSync(path.join(repo, 'public'), { recursive: true });
  writeFileSync(catalogPath(repo), text, 'utf8');
}

function writeFile(repo, relativePath, text) {
  const target = path.join(repo, relativePath);
  mkdirSync(path.dirname(target), { recursive: true });
  writeFileSync(target, text, 'utf8');
}

function readWorkingCatalog(repo) {
  return JSON.parse(readFileSync(catalogPath(repo), 'utf8'));
}

function commit(repo, message) {
  runGit(repo, ['add', '--all']);
  runGit(repo, ['commit', '--quiet', '--no-verify', '--message', message]);
  return runGit(repo, ['rev-parse', 'HEAD']).trim();
}

function tag(repo, name, { annotated = false } = {}) {
  if (annotated) {
    runGit(repo, ['tag', '-a', name, '-m', `release ${name}`]);
  } else {
    runGit(repo, ['tag', name]);
  }
}

function headSha(repo) {
  return runGit(repo, ['rev-parse', 'HEAD']).trim();
}

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

test('a stale single-language edit is reported per language and flagged for review', () => {
  const before = {
    SettingsTitle: { en: 'Settings', ru: 'Настройки' },
    LabTitle: { en: 'Lab', ru: 'Лаборатория' },
    TrayExit: { en: 'Exit', ru: 'Выход' },
  };
  const after = {
    SettingsTitle: { en: 'Settings', ru: 'Настройки-НОВЫЕ' },
    LabTitle: { en: 'Lab', ru: 'Лаборатория' },
    TrayExit: { en: 'Exit', ru: 'Выход' },
  };
  const beforeSnapshot = JSON.stringify(before);
  const changes = compareCatalogs(before, after);

  assert.deepEqual(changes, [
    {
      key: 'SettingsTitle',
      status: 'changed',
      changedLanguages: ['ru'],
      unchangedLanguages: ['en'],
      before: before.SettingsTitle,
      after: after.SettingsTitle,
    },
  ]);
  assert.equal(JSON.stringify(before), beforeSnapshot, 'inputs must not be mutated');

  const rendered = formatReport(changes, 'v1.0.0');
  assert.ok(rendered.includes('SettingsTitle'));
  assert.ok(rendered.includes('v1.0.0'));
  assert.ok(rendered.includes('Настройки-НОВЫЕ'), 'new value must be visible');
  assert.ok(rendered.includes('"Настройки"'), 'previous value must be visible');
  assert.ok(rendered.includes('"Settings"'), 'unchanged language value must be visible for review');
  assert.ok(rendered.includes('REVIEW'), 'subset-only edit must be a REVIEW warning');
  assert.ok(!rendered.includes('LabTitle'), 'unchanged keys must be omitted');
  assert.ok(!rendered.includes('TrayExit'), 'unchanged keys must be omitted');
});

test('an all-language edit is reported without a review warning', () => {
  const before = { LabTitle: { en: 'Lab', ru: 'Лаборатория' } };
  const after = { LabTitle: { en: 'Laboratory', ru: 'Лаборатория-2' } };
  const changes = compareCatalogs(before, after);

  assert.deepEqual(changes, [
    {
      key: 'LabTitle',
      status: 'changed',
      changedLanguages: ['en', 'ru'],
      unchangedLanguages: [],
      before: before.LabTitle,
      after: after.LabTitle,
    },
  ]);
  const rendered = formatReport(changes, 'v1.0.0');
  assert.ok(rendered.includes('Lab'));
  assert.ok(rendered.includes('Лаборатория-2'), 'new value must be visible');
  assert.ok(!rendered.includes('REVIEW'), 'a full-language edit is not a subset-only edit');
});

test('added and removed keys are reported with sorted keys and null sides', () => {
  const before = { Alpha: { en: 'a', ru: 'а' }, Zeta: { en: 'z', ru: 'з' } };
  const after = { Alpha: { en: 'a', ru: 'а' }, Beta: { en: 'b', ru: 'б' } };
  const changes = compareCatalogs(before, after);

  assert.deepEqual(changes.map((change) => [change.key, change.status]), [['Beta', 'added'], ['Zeta', 'removed']]);
  assert.deepEqual(changes[0], {
    key: 'Beta',
    status: 'added',
    changedLanguages: ['en', 'ru'],
    unchangedLanguages: [],
    before: null,
    after: after.Beta,
  });
  assert.deepEqual(changes[1], {
    key: 'Zeta',
    status: 'removed',
    changedLanguages: ['en', 'ru'],
    unchangedLanguages: [],
    before: before.Zeta,
    after: null,
  });

  const rendered = formatReport(changes, 'v1.0.0');
  assert.ok(rendered.includes('[added] Beta'));
  assert.ok(rendered.includes('[removed] Zeta'));
  assert.ok(rendered.includes('"b"'));
  assert.ok(rendered.includes('"з"'));
  assert.ok(!rendered.includes('REVIEW'));
});

test('catalog order and value formatting changes are invisible', () => {
  const before = { SettingsTitle: { en: 'Settings', ru: 'Настройки' }, LabTitle: { en: 'Lab', ru: 'Лаборатория' } };
  const after = { LabTitle: { en: 'Lab', ru: 'Лаборатория' }, SettingsTitle: { en: 'Settings', ru: 'Настройки' } };

  assert.deepEqual(compareCatalogs(before, after), []);
  assert.deepEqual(compareCatalogs(JSON.parse(JSON.stringify(before)), JSON.parse(JSON.stringify(after))), []);

  const rendered = formatReport([], 'v1.0.0');
  assert.ok(rendered.includes('v1.0.0'));
  assert.ok(!rendered.includes('SettingsTitle'));
  assert.ok(!rendered.includes('REVIEW'));
  assert.ok(formatReport([], undefined).includes('HEAD'));
});

test('languages come from the union of entry fields, not a fixed pair', () => {
  const threeLanguagesBefore = { Greeting: { de: 'Hallo', en: 'Hello', ru: 'Привет' } };
  const threeLanguagesAfter = { Greeting: { de: 'Hallo!', en: 'Hello', ru: 'Привет' } };
  assert.deepEqual(compareCatalogs(threeLanguagesBefore, threeLanguagesAfter), [
    {
      key: 'Greeting',
      status: 'changed',
      changedLanguages: ['de'],
      unchangedLanguages: ['en', 'ru'],
      before: threeLanguagesBefore.Greeting,
      after: threeLanguagesAfter.Greeting,
    },
  ]);

  const grownBefore = { Greeting: { en: 'Hello' } };
  const grownAfter = { Greeting: { en: 'Hello', ru: 'Привет' } };
  assert.deepEqual(compareCatalogs(grownBefore, grownAfter), [
    {
      key: 'Greeting',
      status: 'changed',
      changedLanguages: ['ru'],
      unchangedLanguages: ['en'],
      before: grownBefore.Greeting,
      after: grownAfter.Greeting,
    },
  ]);
});

test('report ordering is deterministic regardless of input ordering', () => {
  const before = {};
  const after = {
    zeta: { en: 'z' },
    Alpha: { en: 'a' },
    omega: { en: 'o' },
  };
  const forward = compareCatalogs(before, after);
  const reversed = compareCatalogs(before, {
    omega: { en: 'o' },
    Alpha: { en: 'a' },
    zeta: { en: 'z' },
  });

  assert.deepEqual(forward.map((change) => change.key), ['Alpha', 'omega', 'zeta']);
  const first = formatReport(forward, 'v2.0.0');
  assert.equal(first, formatReport(reversed, 'v2.0.0'));
  assert.equal(first, formatReport([...forward].reverse(), 'v2.0.0'));
  assert.ok(first.indexOf('Alpha') < first.indexOf('omega'));
  assert.ok(first.indexOf('omega') < first.indexOf('zeta'));
});

// ---------------------------------------------------------------------------
// readBaseline against a throwaway repository
// ---------------------------------------------------------------------------

test('default baseline is the latest reachable release tag with later commits included as working changes', () => {
  withFixture(({ repo }) => {
    writeCatalog(repo, { SettingsTitle: { en: 'one', ru: 'один' } });
    commit(repo, 'one');
    tag(repo, 'v1.0.0', { annotated: true });

    writeCatalog(repo, { SettingsTitle: { en: 'two', ru: 'два' } });
    commit(repo, 'two');
    tag(repo, 'v1.2.0');

    writeCatalog(repo, { SettingsTitle: { en: 'three', ru: 'три' } });
    commit(repo, 'three');

    writeCatalog(repo, { SettingsTitle: { en: 'working', ru: 'рабочий' } });

    const count = Number(runGit(repo, ['rev-list', '--count', 'v1.2.0..HEAD']).trim());
    assert.ok(count >= 1, `expected commits after the release tag, got ${count}`);

    const baseline = readBaseline(repo);
    assert.equal(baseline.ref, 'v1.2.0');
    assert.deepEqual(baseline.catalog, { SettingsTitle: { en: 'two', ru: 'два' } });

    const changes = compareCatalogs(baseline.catalog, readWorkingCatalog(repo));
    assert.deepEqual(changes.map((change) => [change.key, change.status]), [['SettingsTitle', 'changed']]);

    assert.deepEqual(
      runGit(repo, ['status', '--porcelain']).split('\n').filter((line) => line.length > 0),
      [' M public/messages.json'],
      'readBaseline must not add or remove repository changes',
    );
    assert.equal(runGit(repo, ['config', '--get', 'user.name']).trim(), 'Localization Report Tests');
    assert.ok(repo.startsWith(tmpdir()));
    assert.ok(existsSync(path.join(repo, '.git')));
  });
});

test('default baseline ignores unreachable and non-release tags', () => {
  withFixture(({ repo, root }) => {
    writeCatalog(repo, { SettingsTitle: { en: 'released', ru: 'релиз' } });
    commit(repo, 'released');
    tag(repo, 'v0.5.0');

    writeCatalog(repo, { SettingsTitle: { en: 'on side branch', ru: 'на ветке' } });
    runGit(repo, ['checkout', '--quiet', '-b', 'side']);
    commit(repo, 'side work');
    tag(repo, 'v9.9.9');
    runGit(repo, ['checkout', '--quiet', 'main']);

    writeCatalog(repo, { SettingsTitle: { en: 'head', ru: 'голова' } });
    commit(repo, 'head work');
    tag(repo, 'v9.9.9-preview');

    assert.ok(existsSync(path.join(root, 'isolated.gitconfig')));
    assert.equal(process.env.GIT_CONFIG_GLOBAL, path.join(root, 'isolated.gitconfig'));

    const baseline = readBaseline(repo);
    assert.equal(baseline.ref, 'v0.5.0');
    assert.deepEqual(baseline.catalog, { SettingsTitle: { en: 'released', ru: 'релиз' } });

    const reachable = runGit(repo, ['tag', '--merged=HEAD', '--list']).split('\n').map((line) => line.trim()).filter(Boolean);
    assert.deepEqual(reachable, ['v0.5.0', 'v9.9.9-preview']);
  });
});

test('default baseline falls back to HEAD when no release tag matches', () => {
  withFixture(({ repo }) => {
    writeCatalog(repo, { SettingsTitle: { en: 'first', ru: 'первый' } });
    commit(repo, 'first');
    tag(repo, '0.1.0');
    tag(repo, 'vNext');
    tag(repo, 'release-1.0');

    runGit(repo, ['checkout', '--quiet', '-b', 'side']);
    writeCatalog(repo, { SettingsTitle: { en: 'side', ru: 'бок' } });
    commit(repo, 'side');
    tag(repo, 'v5.0.0');
    runGit(repo, ['checkout', '--quiet', 'main']);

    writeCatalog(repo, { SettingsTitle: { en: 'head', ru: 'голова' } });
    commit(repo, 'head');
    tag(repo, 'v2.0.0-rc1');

    const baseline = readBaseline(repo);
    assert.equal(baseline.ref, 'HEAD');
    assert.deepEqual(baseline.catalog, { SettingsTitle: { en: 'head', ru: 'голова' } });

    const explicitHead = readBaseline(repo, 'HEAD');
    assert.deepEqual(explicitHead.catalog, baseline.catalog);
    assert.match(explicitHead.ref, COMMIT_SHA);
    assert.equal(explicitHead.ref, headSha(repo));
  });
});

test('explicit baseRef is resolved to a commit SHA', () => {
  withFixture(({ repo }) => {
    writeCatalog(repo, { SettingsTitle: { en: 'tagged', ru: 'тег' } });
    const taggedSha = commit(repo, 'tagged');
    tag(repo, 'v1.0.0');

    writeCatalog(repo, { SettingsTitle: { en: 'head', ru: 'голова' } });
    commit(repo, 'head');
    writeCatalog(repo, { SettingsTitle: { en: 'working', ru: 'рабочий' } });

    const byTag = readBaseline(repo, 'v1.0.0');
    assert.equal(byTag.ref, taggedSha);
    assert.match(byTag.ref, COMMIT_SHA);
    assert.deepEqual(byTag.catalog, { SettingsTitle: { en: 'tagged', ru: 'тег' } });
    const padded = readBaseline(repo, '  v1.0.0  ');
    assert.equal(padded.ref, taggedSha);
    assert.deepEqual(padded.catalog, byTag.catalog);

    const bySha = readBaseline(repo, taggedSha);
    assert.equal(bySha.ref, taggedSha);
    assert.deepEqual(bySha.catalog, byTag.catalog);

    const byHead = readBaseline(repo, 'HEAD');
    assert.equal(byHead.ref, headSha(repo));
    assert.deepEqual(byHead.catalog, { SettingsTitle: { en: 'head', ru: 'голова' } });

    assert.deepEqual(
      runGit(repo, ['status', '--porcelain']).split('\n').filter((line) => line.length > 0),
      [' M public/messages.json'],
    );
  });
});

test('invalid baseRef values fail instead of silently substituting a baseline', () => {
  withFixture(({ repo }) => {
    writeCatalog(repo, { SettingsTitle: { en: 'head', ru: 'голова' } });
    commit(repo, 'head');
    tag(repo, 'v1.0.0');

    for (const badRef of ['no-such-ref', '--all', 'v1.0.0..HEAD', 'HEAD~99', '   ', '']) {
      const error = captureThrow(() => readBaseline(repo, badRef));
      assert.ok(error instanceof Error, `expected an Error for ${JSON.stringify(badRef)}`);
      if (badRef.trim() !== '') {
        assert.ok(error.message.includes(badRef), `error should name ${JSON.stringify(badRef)}: ${error.message}`);
      }
    }

    const valid = readBaseline(repo, 'v1.0.0');
    assert.match(valid.ref, COMMIT_SHA);
    assert.deepEqual(valid.catalog, { SettingsTitle: { en: 'head', ru: 'голова' } });
  });
});

test('a catalog absent at a historical commit is an empty catalog, not a failure', () => {
  withFixture(({ repo }) => {
    writeFile(repo, 'README.md', '# fixture\n');
    commit(repo, 'no catalog yet');
    tag(repo, 'v1.0.0');

    writeCatalog(repo, { SettingsTitle: { en: 'introduced', ru: 'введён' } });
    commit(repo, 'introduce catalog');

    const baseline = readBaseline(repo);
    assert.equal(baseline.ref, 'v1.0.0');
    assert.deepEqual(baseline.catalog, {});

    const explicit = readBaseline(repo, 'v1.0.0');
    assert.deepEqual(explicit.catalog, {});

    const changes = compareCatalogs(baseline.catalog, readWorkingCatalog(repo));
    assert.deepEqual(changes.map((change) => [change.key, change.status]), [['SettingsTitle', 'added']]);
  });
});

test('malformed or unexpected historical catalogs fail', () => {
  withFixture(({ repo }) => {
    writeRawCatalog(repo, '{ "Greeting": ');
    commit(repo, 'malformed catalog');
    tag(repo, 'v1.0.0');

    assert.throws(() => readBaseline(repo), /Malformed|messages\.json/);
    assert.throws(() => readBaseline(repo, 'v1.0.0'), /Malformed|messages\.json/);

    writeCatalog(repo, { SettingsTitle: { en: 'fixed', ru: 'исправлен' } });
    commit(repo, 'fix catalog');

    const fixed = readBaseline(repo, 'HEAD');
    assert.deepEqual(fixed.catalog, { SettingsTitle: { en: 'fixed', ru: 'исправлен' } });

    writeRawCatalog(repo, '[1, 2, 3]');
    commit(repo, 'unexpected shape');
    assert.throws(() => readBaseline(repo, 'HEAD'), /Malformed|messages\.json/);
  });
});

test('formatting-only working-tree edits produce no changes end to end', () => {
  withFixture(({ repo }) => {
    writeCatalog(repo, { SettingsTitle: { en: 'Settings', ru: 'Настройки' }, LabTitle: { en: 'Lab', ru: 'Лаборатория' } });
    commit(repo, 'pretty catalog');
    tag(repo, 'v1.0.0');

    writeFileSync(catalogPath(repo), JSON.stringify({ LabTitle: { ru: 'Лаборатория', en: 'Lab' }, SettingsTitle: { ru: 'Настройки', en: 'Settings' } }), 'utf8');

    const baseline = readBaseline(repo);
    assert.equal(baseline.ref, 'v1.0.0');
    assert.deepEqual(compareCatalogs(baseline.catalog, readWorkingCatalog(repo)), []);
  });
});

test('temporary fixtures are isolated and removed even when the body throws', () => {
  let captured = null;
  const error = captureThrow(() =>
    withFixture(({ root }) => {
      captured = root;
      assert.ok(root.startsWith(path.join(tmpdir(), FIXTURE_PREFIX)));
      throw new Error('fixture body boom');
    }),
  );
  assert.match(error.message, /fixture body boom/);
  assert.equal(existsSync(captured), false, 'fixture directory must be removed');

  let second = null;
  withFixture(({ root }) => {
    second = root;
  });
  assert.equal(existsSync(second), false, 'fixture directory must be removed on success too');
});
