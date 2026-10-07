# Releasing Speechek

[README](../README.md) · [Contributing](../CONTRIBUTING.md) · [User guide](user-guide.md)

This document describes how official Speechek builds are produced, verified and published. Release artifacts are built by the on-demand candidate workflow and only that verified candidate is ever published; a release is never assembled by hand from a locally built executable. Publishing additionally requires the owner's manual smoke of the portable Development preview for the same `main` commit — see *Owner preview before publishing*.

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
- Publishing is gated by the owner's manual smoke of the Development preview for the exact clean `main` commit; `scripts/publish-tag.ps1` refuses to dispatch or tag without `-OwnerApproved` and the matching preview manifest (see *Owner preview before publishing*).
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

`.github/workflows/candidate.yml` runs only when dispatched manually (`workflow_dispatch`) from `main`, on `windows-2022` (x64) with read-only repository permissions. This repository has no automatic CI: no branch or pull-request event starts any job. The dispatch requires a `commit` input that must be the full SHA of the dispatched `main` commit (`github.sha`), plus a `requestId` input (32 lowercase hex) that names the run `release-<requestId>`; any other value, or a dispatch from another branch, fails the run before anything is built.

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

## Owner preview before publishing

Every release starts with a portable Development preview that the **owner** runs by hand; the agent never opens a visible window. The authoritative order is `prepare-preview → owner feedback → publish`:

1. **prepare-preview** — `scripts/prepare-preview.ps1`, from a clean committed `main`, runs the GUI-free checks and unit tests (`cargo check`, localization source/report checks with checker regressions, `cargo test --features test-provider`) and then builds the Development flavor **last**, so nothing else overwrites the canonical `src-tauri/target/debug/speechek.exe`. Localization reuses the catalog validator from the existing Cargo check and uses locked Bun development parsers for source checks; see [Localization](../CONTRIBUTING.md#localization). It writes the ignored `src-tauri/target/debug/preview-manifest.json` (`schemaVersion` 1, `commit`, `version`, `flavor` `development`, `exe.file` `speechek.exe`, `exe.sha256`) and never starts the app, an installer, a shortcut, a Run entry or a registry write.
2. **owner feedback** — the owner double-clicks `src-tauri/target/debug/speechek.exe`: no installation, no shortcuts and no uninstall entry are created. The build runs as **Speechek Dev** (default hotkey `Ctrl+Shift+F9`, first port `4174`) and keeps its `settings.json` and `secrets.bin` **beside that EXE**, separate from the installed production app, which keeps them under `%APPDATA%\Speechek`. The owner enters their own Dev keys and replies either that it works or with the concrete problems.
3. **publish** — `scripts/publish-tag.ps1` accepts the release only when it is given `-OwnerApproved` and the canonical preview manifest that matches this clean `main` commit and the current debug EXE, so a bug report restarts the cycle: fix, rebuild the preview, hand it off again, and the previous approval is void. See *Publishing a tag* below.

The preview manifest describes the preview EXE only; it carries no user name, host path or environment value, and it is not the production `release-manifest.json` of a candidate run.

## Publishing a tag

`scripts/publish-tag.ps1` is the single entry point for a release, and it runs only **after** the owner preview above has been confirmed. From a clean `main` checkout, with the canonical preview manifest present and `-OwnerApproved` given, it dispatches exactly one candidate run, waits for it and only then tags:

```powershell
$previewManifest = (Resolve-Path 'src-tauri/target/debug/preview-manifest.json').Path
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/publish-tag.ps1 `
    -Version 0.2.1 -PreviewManifest $previewManifest -OwnerApproved
```

The script:

1. requires the owner-approval gate to hold **before the first GitHub API call**: `-OwnerApproved` is given, `-PreviewManifest` is the absolute canonical `src-tauri/target/debug/preview-manifest.json`, that manifest has `schemaVersion` 1, `flavor` `development` and `exe.file` `speechek.exe`, its `version` and `commit` equal `-Version` and the current clean `main` HEAD, and the SHA-256 of the current `src-tauri/target/debug/speechek.exe` still equals the manifest `exe.sha256`, so a rebuilt or replaced preview invalidates the approval; nothing is dispatched and no tag is created when any of this fails;
2. dispatches `.github/workflows/candidate.yml` once with the `commit` input set to that `main` HEAD and a fresh random `requestId` (the GitHub workflow-dispatch API). The API may answer with the documented HTTP 200 body carrying a `workflow_run_id`, or with HTTP 204 and no body at all; the returned id is used when it is present and well formed, and otherwise the script locates exactly the run named `release-<requestId>` through the workflow-runs API (filtered to `workflow_dispatch` on `main` at this commit), waiting a bounded time for it to appear. Only a run whose name is exactly `release-<requestId>` is accepted, a missing or ambiguous match fails the script, and no run is ever selected by recency;
3. accepts only a successful run that belongs to this repository, uses the candidate workflow, is a `workflow_dispatch` event on `main` with a `head_sha` equal to the published commit and the expected `release-<requestId>` name, and whose unexpired `windows-x64-<commit>` artifact carries a production manifest whose installer bytes match both the manifest and `SHA256SUMS.txt`;
4. creates the annotated tag `vX.Y.Z`, whose message records `candidate-run:<id> installer-sha256:<hex>`, and pushes only that tag.

The tag pipeline accepts only that recorded candidate, so the published file is exactly the one the candidate build produced and is never rebuilt. Installing the candidate by hand is not a release gate: the owner gate is the confirmed Development preview for this source commit, and `-OwnerApproved` is passed only after that explicit positive answer. A preview bug report restarts the cycle (fix, rebuild the preview, re-confirm) and voids the previous approval. A dispatch response without a run id (the observed HTTP 204 empty body) is not a failure: the same single invocation resolves its own run through the unique `requestId` and never falls back to a previous or "latest" run. If the candidate run fails, or its artifact has expired before the tag is pushed, resolve the problem and dispatch a new candidate; a tag is never created for a file that was not verified.

## Release workflow

`.github/workflows/release.yml` runs on a push of a single `v[0-9]*` tag and can also be started manually to recover an existing tag (see below); it never runs on pull requests. It reads the **remote** tag through the GitHub Git Refs/Tags API, never through a local checkout ref — the ref must be an annotated `tag` object whose target is a commit and whose message records the candidate, because a checkout may show the tag peeled to its commit. It validates that the tag is `vX.Y.Z`, checks out exactly the resolved tag commit, and verifies that this commit is on `main`, that all version manifests agree with the tag and that the CHANGELOG has a dated section for the version. Through the GitHub API it then confirms that the candidate run named in the annotated tag belongs to the current public repository, uses the candidate workflow, was a `workflow_dispatch` event on `main`, concluded `success`, has a `head_sha` equal to that commit, and still has the `windows-x64-<SHA>` artifact, unexpired. A lightweight tag, a tag whose peeled commit differs from the recorded candidate, or a tag without exactly one candidate record fails the run closed.

The publish job receives `contents:write` and `actions:read` only after those checks pass. It builds release notes from the matching CHANGELOG section, adds the fixed footer (Windows 11 x64, install, manual-download update, unsigned, accepted hang risks, SHA-256), attaches the exact same setup executable and checksum, and publishes the release as latest within the tag workflow. Downloaded scripts are never executed — only bytes and documents are validated. If any check fails, the release is not rebuilt from scratch and no substitute file is uploaded.

### Recovering a failed release run

The tag is pushed before the release workflow starts, so a failure of that workflow after the push (for example a bug in its own validation) does not lose the verified candidate. Re-run the same pipeline for the existing tag — without creating a tag, a candidate or a build:

```powershell
gh workflow run release.yml --repo girte/speechek --ref main -f tag=v0.2.0
```

The dispatch re-reads the annotated tag through the API, re-validates the candidate run and artifact recorded in it, and publishes that identical verified file as the latest release. Nothing is rebuilt and nothing downloaded from the run is executed. If the candidate artifact has expired, the workflow fails and a new candidate must be built and verified; a published tag is never moved and a released file is never replaced.

## Immutable releases and updates

GitHub immutable releases are enabled before the first public publication, so release tags and assets cannot be changed afterwards. Speechek ships no auto-updater plugin and makes no hidden self-update network calls; users update by downloading a newer installer from the releases page, which performs an in-place upgrade while keeping settings and keys.
