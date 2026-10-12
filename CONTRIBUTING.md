# Contributing to Speechek

Thanks for wanting to help. This guide covers the toolchain, building, testing and how to send a change.

## Requirements

- **Windows 11 x64** with **Visual Studio 2022 BuildTools** (the MSVC toolchain).
- **Rust 1.97.1** — install with [rustup](https://rustup.rs/) and use the `x86_64-pc-windows-msvc` target. The version is pinned in `rust-toolchain.toml`.
- **Bun 1.4.2** — needed for the Tauri CLI and GUI-free localization checks, not for running the app.
- **Microsoft WebView2 Runtime** for running a build.

## Build

Run from a **Developer Command Prompt** for BuildTools (or any shell where the MSVC environment is active):

```bat
cargo build --manifest-path src-tauri\Cargo.toml --locked --no-default-features
```

The debug executable appears at `src-tauri\target\debug\speechek.exe`.

Development diagnostics (the optional local FLAC/journal/content archive, [issue #6](https://github.com/girte/speechek/issues/6)) are **not** in that command: `dev-diagnostics` is a non-default feature, and a debug build without it does not archive. To build the archive-enabled Development profile, pass the feature explicitly:

```bat
cargo build --manifest-path src-tauri\Cargo.toml --locked --no-default-features --features dev-diagnostics
```

The official portable preview is built exactly that way by `scripts/prepare-preview.ps1`, which also passes `dev-diagnostics` to `check` and `test` and builds Development last. `test-provider` and `dev-diagnostics` are debug-only and never appear in the Production release command or the Test-profile installer command. See [Development diagnostics](#development-diagnostics) below for what the archive contains.

The debug build is **portable**: double-click `src-tauri\target\debug\speechek.exe` to run it — no installer, shortcuts, uninstall entry or registry write. The Development profile keeps its `settings.json` and `secrets.bin` beside that EXE (inside `src-tauri\target\debug`), separate from the installed production app, which keeps them under `%APPDATA%\Speechek`; the two profiles never read each other's files.

The three build profiles never share files or hotkeys. Each runs a local server on `127.0.0.1`, with a free port chosen by Windows at every start:

| Profile | Settings and keys | Default hotkey | Interface language on first start |
| --- | --- | --- | --- |
| Production (installed) | `%APPDATA%\Speechek` | `F2` | Installer choice (`1033` English / `1049` Russian), else Windows UI language |
| Development (debug EXE) | Beside the EXE | `Ctrl+Shift+F9` | Ignores installer registry entries; Windows UI language |
| Test | `%APPDATA%\Speechek-Test` or an absolute `SPEECHEK_CONFIG_PATH` | `Ctrl+Shift+F10` | Reads only its own isolated installer entry, never Production's |

A saved `language` in the profile always wins; changing it in the app never writes the installer registry value. The uninstaller removes only the Production profile, and the installer closes only a copy running from the install folder.

For a release build, link the C runtime statically for that command only:

```bat
set "RUSTFLAGS=-C target-feature=+crt-static"
cargo build --manifest-path src-tauri\Cargo.toml --locked --release --no-default-features
set RUSTFLAGS=
```

Official releases are produced by the scripts in `scripts/` — see [docs/releasing.md](docs/releasing.md). Do not hand-assemble a release or create extra target directories to work around a locked executable.

`tauri build` is **not** equivalent to the commands above: it enables custom-protocol, while the embedded WebView loads `devUrl` served by the app itself. The `devUrl` in `tauri.conf.json` is only a placeholder; at runtime the EXE replaces it with `http://127.0.0.1:<port>/`, using the port Windows assigned for that start.

## Check and test

```bat
cargo check --manifest-path src-tauri\Cargo.toml --locked --no-default-features
cargo test --manifest-path src-tauri\Cargo.toml --locked --no-default-features --features test-provider
```

The `test-provider` feature and the `SPEECHEK_TEST_PROVIDER_HTTP` / `SPEECHEK_TEST_PROVIDER_WSS` overrides exist **only in debug builds**. Use a temporary or absolute `%APPDATA%` path and fake keys. **Never** run automated checks with real keys or real speech, and never commit `settings.json`, `secrets.bin` or `keys.txt`.

These checks must stay **GUI-free**: do not launch `cargo run`, a debug or release executable, or any visible-window scenario from an automated run. The visible-window smoke (overlay, tray, microphone, paste) is performed by hand by the owner. Portable previews handed to the owner are built with `scripts/prepare-preview.ps1` — see [docs/releasing.md](docs/releasing.md).

The archive-enabled debug profile additionally runs:

```bat
cargo check --manifest-path src-tauri\Cargo.toml --locked --no-default-features --features dev-diagnostics
cargo test --manifest-path src-tauri\Cargo.toml --locked --no-default-features --features test-provider,dev-diagnostics
```

`scripts/prepare-preview.ps1` runs these plus the Development notices check. The plain Test command above stays valid and unchanged; the Test runtime never enables the archive.

## Localization

`public/messages.json` is the single catalog for native and web UI. Add new messages in every supported language in the same change as the feature. When an existing message changes meaning, review every translation; do not assume an unchanged translation is still correct. Keep keys for typo/style fixes; use distinct semantic keys for different actions or causes. Speech, transcripts, device names, external diagnostics and legal texts remain data, not translated UI copy.

Run the full GUI-free control from the repository root:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/check-localization.ps1
```

The command activates the pinned Rust/MSVC environment and reuses the build-time catalog validator through `cargo check`: every message must have non-blank EN/RU values and matching valid placeholders. Generated `MessageId` references are checked by Rust. It then installs the locked development dependencies with lifecycle scripts disabled, runs the checker regressions, checks static JS/HTML keys and statically known missing arguments, and warns about possible direct UI text in HTML/JS/Rust. These parsers are development tools only; nothing is added to the application runtime.

The Git report shows only added, removed or changed messages, including their before/after values. Changes in only some languages get an advisory `REVIEW` notice, with the unchanged translations shown for comparison. Confirm that they still match the meaning; no artificial edit or per-string approval record is required. By default the baseline is the highest-version reachable `vX.Y.Z` release tag, or `HEAD` if none exists. This includes committed, staged and unstaged changes since that baseline. To review a smaller change, pass `-BaseRef HEAD` or another existing commit/tag.

Missing translations, invalid placeholders, unknown static keys, missing static arguments and unparseable JavaScript block the control. Literal-text warnings and one-language-only changes require review, not another release approval. The literal scan is heuristic, not proof that every text is translated: dynamic values are not executed or guessed. The lab's dynamic text containers start empty and are filled from retained message descriptors before the interface is shown; do not add permanent `data-i18n` markers to transcript containers, because a language change must leave dictated text untouched.

For intentional non-translatable data or catalog-failure fallbacks, a narrow `// i18n-ignore-next: reason` in JS or `<!-- i18n-ignore-next: reason -->` in HTML suppresses warnings for the next statement/node only. Give a concrete reason; key and argument errors remain blocking. Review new warnings instead of hiding an entire screen.

`scripts/prepare-preview.ps1` runs this control after its existing `cargo check`, without a second catalog validator or duplicate Cargo step. Native UI and text smoke remain exclusively owner-run; the publication/owner-approval process is unchanged.


## Code layout

- `src-tauri/src/lib.rs` — startup, dictation state machine, global hotkeys, tray, WebView commands and exit.
- `src-tauri/src/capture.rs` — native microphone capture; one recording owns the microphone at a time.
- `src-tauri/src/overlay.rs` — the recording overlay window.
- `public/` — overlay, lab and settings pages, embedded into the executable via `include_str!`/`include_bytes!`. Rebuild after changing them.
- `src-tauri/src/settings.rs`, `secrets.rs`, `preferences.rs` — settings document, DPAPI key storage, settings-window behaviour.
- `src-tauri/src/diagnostics.rs` (with `diagnostics/storage.rs`, `diagnostics/flac.rs`) — the debug-only Development archive; compiled only under `all(debug_assertions, feature = "dev-diagnostics")`.
- `src-tauri/src/backend.rs`, `provider.rs`, `live.rs` — the embedded server and Gemini calls.

## Development diagnostics

The `dev-diagnostics` feature adds a debug-only local archive used to investigate recognition problems ([issue #6](https://github.com/girte/speechek/issues/6)). It is deliberately narrow in *where* it exists, but complete in *what* one take keeps, so a take can be diagnosed without the owner retyping the dictated speech or the recognized text:

- **Debug Development only.** The feature is non-default, requires `debug_assertions`, and the writer starts only in the Development profile. Production and the automatically launched Test profile always use a disabled sink: no `logs` directory, no audio, no journals, no content records, and none of the codecs or Development assets are compiled in.
- **Files.** `logs/` beside the Development `settings.json` (canonical: `src-tauri\target\debug\logs\`). Each take gets a directory with `session.json`, rotated `events-*.jsonl` segments, content records and at most one audio basename: `audio.flac` (verified complete), `audio.incomplete.flac` (verified surviving subset with known gaps) or `audio.flac.part` (unverified). Successful, failed and cancelled takes are all archived; a take with no audio still keeps its content.
- **Content.** Beyond the numeric journal, a take stores the full text of what happened as `ContentRecord`s: the runtime and capture context, the live setup and every live message and output, the provider request and response bodies, transport errors, socket closes, recorder results and errors, and the exact paste payload with its outcome. The runtime context also identifies the build that produced the take — its flavor and version, plus the exact commit when the binary has one: `scripts/prepare-preview.ps1` passes the clean `main` HEAD to the child build as `SPEECHEK_BUILD_COMMIT`, `build.rs` accepts only a full 40-character hexadecimal SHA and re-exports it, so the official preview names the precise executable behind a take without the owner having to say which same-version build was running. A hand-built debug binary that was given no identity records `commit` as absent instead of inventing a SHA. Evidence is kept, not merely counted: an opaque WebSocket frame (binary, Ping, Pong or any raw frame) and an HTTP response body that is not valid UTF-8 are redacted against the known keys and then stored losslessly as base64 inside the JSON record, so the bytes can be decoded again during forensics; a malformed plaintext payload is stored as plain text under `format: text`. Plain text is never truncated. Anything that reaches a record goes through the redaction helpers first — known API keys in raw, plain, JSON-escaped or URL-encoded form, and credential-named values — so a key that never reaches the text also never reaches a record. Authorization headers, the archive's own connection endpoint URL and request bodies carrying credentials are never recorded. Recognized text is ordinary content, so a URL that was dictated, or one the provider returns in a reply, is kept like any other words. Retention is bounded, not unlimited: the shared queued-content byte budget is 8 MiB and the report route has its own caps, so an oversize or unsavable packet is dropped, counted and advertised as incomplete evidence rather than silently lost or claimed to be kept.
- **Renderer reports.** The laboratory pages post their own technical report to the single Development-only `/api/dev-diagnostics` route (no Tauri command). `RendererReport` carries an optional `result` object and an `errors` array beside the numeric fields; only the known keys (`results`, `errors`, `errorDetails` and the mode-map keys, including an empty global error) are accepted, and the report is recorded after the backend's pinned redaction. A content report is capped at 1 MiB; the ordinary JS technical report stays at 4096 bytes. A bounded result snapshot is never silently cut down: whenever a value cannot be saved, the error and drop counters advertise the take as incomplete.
- **Audio and content are private.** The FLAC and the archived content are plaintext personal data protected by a user/SYSTEM DACL, not encryption. Never serve them over HTTP and never attach the audio or the content to an issue or a commit — the issue forms say the same — and use synthetic data only in automated checks.
- **Audio.** Lossless FLAC of the raw PCM (16 kHz, mono, 16-bit), encoded and independently verified lossless before it is published under a final name.
- **Evidence, not proof.** The archive holds the audio Speechek sent and the replies it received. That cannot prove how Google's service interpreted the audio, and not every bug leaves a distinguishable trace, so never claim the archive detects every problem.
- **Incomplete takes.** Disk errors, an exhausted byte budget and dropped records are counted and the take is diagnosed as incomplete; nothing is ever presented as a complete take or as a success it did not reach. Never truncate valid text to fit: drop and count instead.
- **Retention.** 10 days, or earlier when the files exceed 100,000,000 bytes total across audio, journals, content records, partials, staging and the reserve for finishing an active take. The writer sweeps at startup, each minute and on take close.
- **Never weaken the gates.** `dev-diagnostics` must stay out of the default features, `tauri.conf.json`, the workflows, `scripts/build-release.ps1` and `scripts/build-test-installer.ps1`; `scripts/check-release.ps1` fails if it appears there. Only `scripts/prepare-preview.ps1` may pass it explicitly. The Production notice file `third_party/THIRD-PARTY-NOTICES.txt` stays codec-free; the debug archive's own `third_party/THIRD-PARTY-NOTICES-DEV.txt` is generated, never hand-written:

  ```powershell
  powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/collect-notices.ps1 -Features dev-diagnostics -OutputPath third_party/THIRD-PARTY-NOTICES-DEV.txt
  ```

  `scripts/prepare-preview.ps1` verifies it with the same collector before building.

## Invariants to preserve

1. **Escape cancels** a dictation before the text is handed to paste: it hides the overlay, stops capture and prevents late responses from pasting or changing the clipboard. The Finalizing → Pasting transition is synchronised with cancellation under one session lock; once the Windows Ctrl+V has been sent it cannot be recalled.
2. **Stop on the hotkey is not cancel**: stop finishes recording and keeps the "processing" overlay until the result is ready; cancel hides the overlay and discards the result. Late frames, errors and responses from a previous generation must not affect a new session.
3. **Keys live only in `secrets.bin`**, protected with DPAPI for the current user. HTTP exposes only ordinary settings; keys are sent to the settings window only via the explicit "show keys" command. Never use real keys or speech in automated checks and never commit them.
4. **General settings apply immediately** (mode, mute, microphone, text hotkey). API keys are applied only with the "apply keys" button. A started dictation keeps the snapshot captured at its start.
5. Keep the `/api/settings`, `/api/transcribe`, `/api/live` and `createRecorder` contracts; provider errors must not expose keys or turn intermediate Live text into final text.
6. When you change hotkeys, file locations, user actions or the build, update the documentation in the same change.

## Issue tracking

GitHub issues, not documents, are the source of truth for what is open, planned and finished. The task lists in the READMEs are saved GitHub queries, so no backlog state is copied into the documentation.

Issues use two independent sets of labels:

- **Type** — what the change is: `bug`, `enhancement` or `documentation`. Types combine: a documentation task that also changes behaviour is `enhancement` + `documentation`.
- **Status** — where the work stands: `status:backlog` (awaiting review or implementation), `status:deferred` (explicitly postponed; do not implement without a new request) or `status:in-progress` (actively being worked on). Both issue forms start with `status:backlog`; it is not a promise of implementation. An open issue carries exactly one status label; closing it removes that label.

Closing an issue:

- Finished → close with reason **completed** and the evidence for the result, plus the release that carries it once such a release exists. A single merged change is not a release, so do not promise users that a fix ships until the version containing it is published. Research is not implementation, and handling an error is not removing its cause — state which one actually happened.
- Dropped → close as **not planned** and write the reason.

Where each kind of statement belongs: `CHANGELOG.md` says what a shipped release contains, the user guides describe how the app behaves today, and anything not built yet stays an issue rather than a promise in the docs.

Evidence in a report or pull request is what you actually ran and observed — the exact command and its result — not what you expect to happen.

If the problem is already reported, add your version, steps and logs to that issue instead of opening a duplicate, and do not fix the same bug again in a separate change: work continues in the existing issue, whose status label shows whether anyone is on it.

## Submitting a change

- Open an issue using one of the templates under `.github/ISSUE_TEMPLATE/` first when the change is not trivial, and follow [Issue tracking](#issue-tracking) for labels, status and closing rules.
- Keep changes focused; explain what changed and how you verified it (the command you ran and what you observed).
- Do not include keys, `secrets.bin`, `settings.json`, audio recordings or personal data anywhere in an issue, pull request or commit.
- Do not attach build output (`src-tauri/target/`, `node_modules/`) or generate new release files by hand.

## License

By contributing, you agree that your contributions are licensed under the project's [MIT License](LICENSE). Adapted third-party code keeps its original copyright and license — see `third_party/Handy.LICENSE`.
