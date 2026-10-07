// The PowerShell entrypoint runs the existing Rust catalog validator first.
// This component checks source references and reports Git changes; it does not
// maintain a second catalog schema or translation-approval state.
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, extname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { inspectSources } from './localization-sources.mjs';
import { compareCatalogs, readBaseline, formatReport } from './localization-report.mjs';

function readSources(root, directory, extensions) {
  const files = [];
  for (const entry of readdirSync(join(root, directory), { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    const path = `${directory}/${entry.name}`;
    if (entry.isDirectory()) files.push(...readSources(root, path, extensions));
    else if (entry.isFile() && extensions.has(extname(entry.name))) {
      files.push({ path, text: readFileSync(join(root, path), 'utf8') });
    }
  }
  return files;
}

export function runChecks(repoRoot, baseRef) {
  const catalog = JSON.parse(readFileSync(join(repoRoot, 'public/messages.json'), 'utf8'));
  const files = [
    ...readSources(repoRoot, 'public', new Set(['.js', '.html'])),
    ...readSources(repoRoot, 'src-tauri/src', new Set(['.rs'])),
  ];
  const findings = inspectSources(catalog, files);
  const baseline = readBaseline(repoRoot, baseRef);
  return { ...findings, changes: compareCatalogs(baseline.catalog, catalog), ref: baseline.ref, files: files.length };
}

function main() {
  let root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  let baseRef;
  const args = process.argv.slice(2);
  for (let index = 0; index < args.length; index++) {
    const option = args[index];
    if (option === '--help') {
      console.log('Source component: bun scripts/check-localization.mjs [--base <ref>] [--repo-root <path>]');
      console.log('Full catalog + sources + report: powershell.exe -NoProfile -File scripts/check-localization.ps1 [-BaseRef <ref>]');
      return;
    }
    if (option !== '--base' && option !== '--repo-root') throw new Error(`Unknown option: ${option}`);
    const value = args[++index];
    if (!value) throw new Error(`${option} requires a value`);
    if (option === '--base') baseRef = value;
    else root = resolve(value);
  }
  const result = runChecks(root, baseRef);
  for (const finding of result.errors) {
    console.error(`FAIL [${finding.code}] ${finding.path}:${finding.line}: ${finding.message}`);
  }
  for (const finding of result.warnings) {
    console.log(`WARN [${finding.code}] ${finding.path}:${finding.line}: ${finding.message}`);
  }
  console.log(formatReport(result.changes, result.ref));
  if (result.errors.length) {
    console.error(`BLOCK [localization-sources] ${result.errors.length} error(s); ${result.warnings.length} advisory warning(s).`);
    process.exitCode = 2;
  } else {
    console.log(`OK [localization-sources] ${result.files} source files; ${result.warnings.length} advisory warning(s).`);
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  try { main(); }
  catch (error) {
    console.error(`FAIL [localization] ${error.message}`);
    process.exitCode = 1;
  }
}
