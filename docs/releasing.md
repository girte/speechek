# Releasing Speechek

[README](../README.md) · [Contributing](../CONTRIBUTING.md) · [User guide](user-guide.md)

This document describes how official Speechek builds are produced, verified and published. Release artifacts are built by the on-demand candidate workflow and only that verified candidate is ever published; a release is never assembled by hand from a locally built executable.

## Pinned toolchain

The exact versions are pinned so a build is reproducible:

| Tool | Version | Pinned in |
| --- | --- | --- |
| Rust | 1.97.1 | `rust-toolchain.toml` |
| Target | `x86_64-pc-windows-msvc` | `rust-toolchain.toml`, build scripts |
| Bun | 1.4.2 | `package.json` lockfile / CI |
| Tauri CLI | 2.12.0 | `package.json` devDependencies |
| Tauri runtime / build | 2.11.5 / 2.6.3 | `src-tauri/Cargo.toml` |

The application version is authoritative in `src-tauri/tauri.conf.json`. `Cargo.toml`, `package.json` and the `Cargo.lock` workspace entry mirror it; the check script compares all of them plus the tag instead of trusting three independent editors.

## Version and tag rules

- Versions are `X.Y.Z`. Tags are annotated `vX.Y.Z` and must match the manifest version exactly.
- The first public release is `0.2.0`. The earlier local build `0.1.1` was never a public GitHub release.
- A published version and its installer file are immutable: to fix something, publish a new version. There is no downgrade support and no reuse of a released number or file.

## Local release build

Run from VS 2022 BuildTools or let the helper activate the x64 environment through `vswhere` + `VsDevCmd`:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build-release.ps1
```

`scripts/build-release.ps1`:

1. checks the pinned tools and versions and that the source tree is clean;
2. builds `src-tauri/target/release/speechek.exe` with `--locked --release --no-default-features` and `-C target-feature=+crt-static` applied only to the build child process;
3. confirms the result is an x64 PE whose import table has no dynamic VCRUNTIME/MSVCP/UCRT dependency;
4. generates the NSIS payload include (expected EXE SHA-256, version, license/notice paths) into the ignored `src-tauri/target/release/` area;
5. runs `tauri bundle --bundles nsis --no-binary-patching --no-sign` and produces `src-tauri/target/release/bundle/nsis/Speechek_<version>_x64-setup.exe`.

The helper does not accept arbitrary profile/provider/target-dir flags. If `speechek.exe` is locked by a running app, close Speechek and retry; do not introduce a second target directory to bypass the lock.

Third-party license texts are collected with `scripts/collect-notices.ps1` into `third_party/THIRD-PARTY-NOTICES.txt` from the exact locked release graph, and the notice file ships inside the installer and the application.

## Pre-flight checks

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/check-release.ps1
```

The check verifies the three version mirrors and the tag, the locked dependency graph and notices, the public file manifest, that production builds do not contain the `test-provider` feature or a custom-protocol WebView, the x64 PE and import table, the working capabilities, the absence of private paths or secret patterns in the Git index, and a clean tracked tree.

## Candidate workflow

`.github/workflows/candidate.yml` runs only when dispatched manually (`workflow_dispatch`) from `main`, on `windows-2022` (x64) with read-only repository permissions. This repository has no automatic CI: no branch or pull-request event starts any job. The dispatch requires a `commit` input that must be the full SHA of the dispatched `main` commit (`github.sha`); any other value, or a dispatch from another branch, fails the run before anything is built.

The run executes the source and license checks (`scripts/check-release.ps1`) and then the release helper (`scripts/build-release.ps1`), which produces the static-CRT x64 executable and the NSIS installer. The installer is only built, never executed or installed by the workflow, and no debug, Test-profile or fake-provider build exists there. The run uploads an Actions artifact named `windows-x64-<full commit SHA>` containing the setup executable, `SHA256SUMS.txt` and `release-manifest.json`, with 30-day retention.

Only a successful dispatch on `main` can be the source of a public release. The artifact is bound to the exact commit it was built from, and a pull-request or locally installed build is never accepted for publication.

### Release manifest

`release-manifest.json` is machine-readable and has a fixed schema:

```json
{
  "schemaVersion": 1,
  "commit": "<full git SHA>",
  "version": "0.2.0",
  "flavor": "production",
  "target": "x86_64-pc-windows-msvc",
  "rust": "1.97.1",
  "bun": "1.4.2",
  "tauriCli": "2.12.0",
  "runId": "<GITHUB_RUN_ID or null for a local build>",
  "exe": { "file": "speechek.exe", "sha256": "<lowercase 64-hex>" },
  "installer": { "file": "<actual basename>", "sha256": "<lowercase 64-hex>" }
}
```

It contains no host paths, user names or environment contents. `SHA256SUMS.txt` lists lowercase hashes, two spaces, the installer basename, and a trailing newline.

## Publishing a tag

`scripts/publish-tag.ps1` is the single entry point for a release. From a clean `main` checkout it dispatches exactly one candidate run, waits for it and only then tags:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/publish-tag.ps1 -Version 0.2.0
```

The script:

1. requires the checkout to be on `main` at the version it publishes, the version mirrors to equal `-Version`, and the tracked tree to be clean;
2. dispatches `.github/workflows/candidate.yml` once with the `commit` input set to that `main` HEAD (the GitHub workflow-dispatch API) and waits for the run to conclude;
3. accepts only a successful run that belongs to this repository, uses the candidate workflow, is a `workflow_dispatch` event on `main` with a `head_sha` equal to the published commit, and whose unexpired `windows-x64-<commit>` artifact carries a production manifest whose installer bytes match both the manifest and `SHA256SUMS.txt`;
4. creates the annotated tag `vX.Y.Z`, whose message records `candidate-run:<id> installer-sha256:<hex>`, and pushes only that tag.

The tag pipeline accepts only that recorded candidate, so the published file is exactly the one the candidate build produced and is never rebuilt. No local verification report and no local installer smoke is required: installing a candidate by hand is not a release gate. If the candidate run fails, or its artifact has expired before the tag is pushed, resolve the problem and dispatch a new candidate; a tag is never created for a file that was not verified.

## Release workflow

`.github/workflows/release.yml` runs only on a push of a single `v[0-9]*` tag, never on pull requests. It validates that the tag is `vX.Y.Z`, that all manifests agree, that the CHANGELOG has a section for the version, and that the tag commit is on `main`. Through the GitHub API it then confirms that the candidate run named in the annotated tag belongs to the current public repository, uses the candidate workflow, was a `workflow_dispatch` event on `main`, concluded `success`, has a `head_sha` equal to the peeled tag commit, and still has the `windows-x64-<SHA>` artifact, unexpired.

The publish job receives `contents:write` and `actions:read` only after those checks pass. It builds release notes from the matching CHANGELOG section, adds the fixed footer (Windows 11 x64, install, manual-download update, unsigned, accepted hang risks, SHA-256), attaches the exact same setup executable and checksum, and publishes the release as latest within the tag workflow. Downloaded scripts are never executed — only bytes and documents are validated. If any check fails, the release is not rebuilt from scratch and no substitute file is uploaded.

## Immutable releases and updates

GitHub immutable releases are enabled before the first public publication, so release tags and assets cannot be changed afterwards. Speechek ships no auto-updater plugin and makes no hidden self-update network calls; users update by downloading a newer installer from the releases page, which performs an in-place upgrade while keeping settings and keys.
