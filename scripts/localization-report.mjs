// Localization catalog diff for public/messages.json.
//
// Baseline content is read from a resolved Git commit; the current catalog is
// the caller's parsed working-tree JSON (working tree includes staged and
// unstaged edits). Catalog validity remains the authority of the Rust build
// (src-tauri/build.rs); this module only reports differences.
//
// Exports:
//   compareCatalogs(before, after) -> Array<{
//     key: string,
//     status: 'added' | 'removed' | 'changed',
//     changedLanguages: string[],   // sorted union fields whose values differ
//     unchangedLanguages: string[], // sorted union fields whose values match
//     before: object | null,
//     after: object | null,
//   }>
//   readBaseline(repoRoot, baseRef?) -> { catalog: object, ref: string }
//   formatReport(changes, ref) -> string

import { spawnSync } from 'node:child_process';

const CATALOG_PATH = 'public/messages.json';
const RELEASE_TAG = /^v\d+\.\d+\.\d+$/;
const COMMIT_SHA = /^[0-9a-f]{40,64}$/i;

function runGit(repoRoot, args, label) {
  const result = spawnSync('git', args, {
    cwd: repoRoot,
    encoding: 'utf8',
    shell: false,
    windowsHide: true,
    env: { ...process.env, GIT_OPTIONAL_LOCKS: '0', GIT_TERMINAL_PROMPT: '0' },
  });
  if (result.error) {
    throw new Error(`git ${label} could not start: ${result.error.message}`);
  }
  if (result.status !== 0) {
    const detail = String(result.stderr || result.stdout || '').trim();
    throw new Error(`git ${label} exited with code ${result.status}${detail ? `: ${detail}` : ''}`);
  }
  return result.stdout;
}

function compareText(left, right) {
  return left < right ? -1 : left > right ? 1 : 0;
}

function sortedUnique(values) {
  return Array.from(new Set(values)).sort(compareText);
}

function isEntry(value) {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function entryLanguages(entry) {
  return isEntry(entry) ? Object.keys(entry) : [];
}

function entryValue(entry, language) {
  return isEntry(entry) ? entry[language] : undefined;
}

/**
 * Compare two parsed catalogs (`{ key: { language: string } }`).
 * Languages come from the union of the entry's fields, so any locale set is
 * supported; `en`/`ru` are not special-cased.
 */
export function compareCatalogs(before, after) {
  const beforeCatalog = isEntry(before) ? before : {};
  const afterCatalog = isEntry(after) ? after : {};
  const keys = new Set([...Object.keys(beforeCatalog), ...Object.keys(afterCatalog)]);
  const changes = [];

  for (const key of keys) {
    const beforeHas = Object.prototype.hasOwnProperty.call(beforeCatalog, key);
    const afterHas = Object.prototype.hasOwnProperty.call(afterCatalog, key);
    const beforeEntry = beforeHas ? beforeCatalog[key] : undefined;
    const afterEntry = afterHas ? afterCatalog[key] : undefined;
    const beforePresent = beforeHas && beforeEntry !== undefined && beforeEntry !== null;
    const afterPresent = afterHas && afterEntry !== undefined && afterEntry !== null;

    const status = !beforePresent && afterPresent ? 'added' : beforePresent && !afterPresent ? 'removed' : 'changed';
    const languages = sortedUnique([...entryLanguages(beforeEntry), ...entryLanguages(afterEntry)]);
    const changedLanguages = [];
    const unchangedLanguages = [];

    if (status === 'changed') {
      for (const language of languages) {
        if (Object.is(entryValue(beforeEntry, language), entryValue(afterEntry, language))) {
          unchangedLanguages.push(language);
        } else {
          changedLanguages.push(language);
        }
      }
      // Every locale value matches (and, for entries without locale fields,
      // the raw values match): nothing to report, so ordering/formatting-only
      // differences stay invisible.
      if (changedLanguages.length === 0 && (languages.length > 0 || Object.is(beforeEntry, afterEntry))) {
        continue;
      }
    } else {
      changedLanguages.push(...languages);
    }

    changes.push({
      key,
      status,
      changedLanguages,
      unchangedLanguages,
      before: beforePresent ? beforeEntry : null,
      after: afterPresent ? afterEntry : null,
    });
  }

  changes.sort((left, right) => compareText(left.key, right.key));
  return changes;
}

function latestReachableReleaseTag(repoRoot) {
  const output = runGit(
    repoRoot,
    ['for-each-ref', '--merged=HEAD', '--sort=-version:refname', '--format=%(refname:short)', 'refs/tags'],
    'for-each-ref refs/tags',
  );
  const tags = output
    .split('\n')
    .map((line) => line.trim())
    .filter((name) => RELEASE_TAG.test(name));
  return tags.length > 0 ? tags[0] : null;
}

function resolveCommit(repoRoot, ref) {
  const label = `rev-parse --verify --end-of-options ${ref}^{commit}`;
  const sha = runGit(repoRoot, ['rev-parse', '--verify', '--end-of-options', `${ref}^{commit}`], label).trim();
  if (!COMMIT_SHA.test(sha)) {
    throw new Error(`Could not resolve ${JSON.stringify(ref)} to a commit SHA (got ${JSON.stringify(sha)})`);
  }
  return sha;
}

/**
 * Read the catalog blob exactly as committed at a baseline.
 *
 * Default baseline: latest reachable release tag matching vX.Y.Z
 * (`^v\d+\.\d+\.\d+$`), which ignores preview/beta tags; when no such
 * tag is reachable, HEAD. An explicit `baseRef` must resolve through
 * `rev-parse --verify --end-of-options baseRef^{commit}`; invalid references and
 * Git errors throw instead of silently substituting another baseline.
 *
 * `ref` is the tag name for the default baseline and the resolved commit SHA
 * for an explicit one. A catalog absent at a valid historical commit is an
 * empty catalog (initial introduction); malformed JSON throws.
 */
export function readBaseline(repoRoot, baseRef) {
  if (typeof repoRoot !== 'string' || repoRoot.length === 0) {
    throw new Error('readBaseline requires a repository root path');
  }

  let ref;
  let sha;
  if (baseRef === undefined || baseRef === null) {
    const tag = latestReachableReleaseTag(repoRoot);
    ref = tag ?? 'HEAD';
    sha = resolveCommit(repoRoot, ref);
  } else {
    const requested = baseRef.trim();
    if (requested.length === 0) {
      throw new Error('baseRef must be a non-empty string when provided');
    }
    sha = resolveCommit(repoRoot, requested);
    ref = sha;
  }

  const listed = runGit(repoRoot, ['ls-tree', sha, '--', CATALOG_PATH], `ls-tree ${sha} -- ${CATALOG_PATH}`).trim();
  if (listed.length === 0) {
    return { catalog: {}, ref };
  }

  const content = runGit(repoRoot, ['show', `${sha}:${CATALOG_PATH}`], `show ${sha}:${CATALOG_PATH}`);
  let catalog;
  try {
    catalog = JSON.parse(content);
  } catch (cause) {
    throw new Error(`Malformed ${CATALOG_PATH} at ${sha}: ${cause.message}`);
  }
  if (!isEntry(catalog)) {
    throw new Error(`Malformed ${CATALOG_PATH} at ${sha}: expected a JSON object of message entries`);
  }
  return { catalog, ref };
}

function displayValue(value) {
  if (value === undefined) {
    return '(missing)';
  }
  const text = JSON.stringify(value);
  return text === undefined ? '(missing)' : text;
}

/**
 * Render a deterministic, human-readable report. Ordered by key; subset-only
 * language edits are flagged as REVIEW warnings (never errors).
 */
export function formatReport(changes, ref) {
  const list = Array.isArray(changes) ? [...changes] : [];
  list.sort((left, right) => compareText(String(left?.key), String(right?.key)));

  const counts = { changed: 0, added: 0, removed: 0 };
  for (const change of list) {
    const status = change?.status;
    if (typeof status === 'string' && Object.prototype.hasOwnProperty.call(counts, status)) {
      counts[status] += 1;
    }
  }

  const lines = ['Localization report', `Baseline: ${ref === undefined || ref === null ? 'HEAD' : String(ref)}`];

  if (list.length === 0) {
    lines.push('No catalog changes.');
    return `${lines.join('\n')}\n`;
  }

  lines.push(`Changed: ${counts.changed}, Added: ${counts.added}, Removed: ${counts.removed}`, '');

  const review = [];
  for (const change of list) {
    const changedLanguages = sortedUnique(change?.changedLanguages ?? []);
    const unchangedLanguages = sortedUnique(change?.unchangedLanguages ?? []);
    lines.push(`[${change?.status}] ${change?.key}`);

    if (changedLanguages.length === 0 && unchangedLanguages.length === 0) {
      lines.push(`  (no language fields; ${displayValue(change?.before)} -> ${displayValue(change?.after)})`);
    }
    for (const language of changedLanguages) {
      const beforeValue = entryValue(change?.before, language);
      const afterValue = entryValue(change?.after, language);
      if (change?.status === 'added') {
        lines.push(`  ${language}: + ${displayValue(afterValue)}`);
      } else if (change?.status === 'removed') {
        lines.push(`  ${language}: - ${displayValue(beforeValue)}`);
      } else {
        lines.push(`  ${language}: ${displayValue(beforeValue)} -> ${displayValue(afterValue)}`);
      }
    }
    for (const language of unchangedLanguages) {
      const value = isEntry(change?.after) ? change.after[language] : entryValue(change?.before, language);
      lines.push(`  ${language}: ${displayValue(value)} (unchanged)`);
    }
    lines.push('');

    if (change?.status === 'changed' && changedLanguages.length > 0 && unchangedLanguages.length > 0) {
      review.push(`  - ${change.key} (changed: ${changedLanguages.join(', ')}; unchanged: ${unchangedLanguages.join(', ')})`);
    }
  }

  if (review.length > 0) {
    lines.push('REVIEW: subset-only language edits (confirm intentional):', ...review, '');
  }

  return `${lines.join('\n')}\n`;
}
