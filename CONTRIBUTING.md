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

The debug build is **portable**: double-click `src-tauri\target\debug\speechek.exe` to run it — no installer, shortcuts, uninstall entry or registry write. The Development profile keeps its `settings.json` and `secrets.bin` beside that EXE (inside `src-tauri\target\debug`), separate from the installed production app, which keeps them under `%APPDATA%\Speechek`; the two profiles never read each other's files.

The three build profiles never share files, hotkeys or ports:

| Profile | Settings and keys | Default hotkey | First port | Interface language on first start |
| --- | --- | --- | --- | --- |
| Production (installed) | `%APPDATA%\Speechek` | `F2` | `4173` | Installer choice (`1033` English / `1049` Russian), else Windows UI language |
| Development (debug EXE) | Beside the EXE | `Ctrl+Shift+F9` | `4174` | Ignores installer registry entries; Windows UI language |
| Test | `%APPDATA%\Speechek-Test` or an absolute `SPEECHEK_CONFIG_PATH` | `Ctrl+Shift+F10` | `4175` | Reads only its own isolated installer entry, never Production's |

A saved `language` in the profile always wins; changing it in the app never writes the installer registry value. The uninstaller removes only the Production profile, and the installer closes only a copy running from the install folder.

For a release build, link the C runtime statically for that command only:

```bat
set "RUSTFLAGS=-C target-feature=+crt-static"
cargo build --manifest-path src-tauri\Cargo.toml --locked --release --no-default-features
set RUSTFLAGS=
```

Official releases are produced by the scripts in `scripts/` — see [docs/releasing.md](docs/releasing.md). Do not hand-assemble a release or create extra target directories to work around a locked executable.

`tauri build` is **not** equivalent to the commands above: it enables custom-protocol, while the embedded WebView loads `devUrl` (default `http://127.0.0.1:4173`, the actual runtime port otherwise) served by the app itself.

## Check and test

```bat
cargo check --manifest-path src-tauri\Cargo.toml --locked --no-default-features
cargo test --manifest-path src-tauri\Cargo.toml --locked --no-default-features --features test-provider
```

The `test-provider` feature and the `SPEECHEK_TEST_PROVIDER_HTTP` / `SPEECHEK_TEST_PROVIDER_WSS` overrides exist **only in debug builds**. Use a temporary or absolute `%APPDATA%` path and fake keys. **Never** run automated checks with real keys or real speech, and never commit `settings.json`, `secrets.bin` or `keys.txt`.

These checks must stay **GUI-free**: do not launch `cargo run`, a debug or release executable, or any visible-window scenario from an automated run. The visible-window smoke (overlay, tray, microphone, paste) is performed by hand by the owner. Portable previews handed to the owner are built with `scripts/prepare-preview.ps1` — see [docs/releasing.md](docs/releasing.md).

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
- `src-tauri/src/backend.rs`, `provider.rs`, `live.rs` — the embedded server and Gemini calls.

## Invariants to preserve

1. **Escape cancels** a dictation before the text is handed to paste: it hides the overlay, stops capture and prevents late responses from pasting or changing the clipboard. The Finalizing → Pasting transition is synchronised with cancellation under one session lock; once the Windows Ctrl+V has been sent it cannot be recalled.
2. **Stop on the hotkey is not cancel**: stop finishes recording and keeps the "processing" overlay until the result is ready; cancel hides the overlay and discards the result. Late frames, errors and responses from a previous generation must not affect a new session.
3. **Keys live only in `secrets.bin`**, protected with DPAPI for the current user. HTTP exposes only ordinary settings; keys are sent to the settings window only via the explicit "show keys" command. Never use real keys or speech in automated checks and never commit them.
4. **General settings apply immediately** (mode, mute, microphone, text hotkey); the local port is saved immediately and applied after a restart. API keys are applied only with the "apply keys" button. A started dictation keeps the snapshot captured at its start.
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
