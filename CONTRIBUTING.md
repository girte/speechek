# Contributing to Speechek

Thanks for wanting to help. This guide covers the toolchain, building, testing and how to send a change.

## Requirements

- **Windows 11 x64** with **Visual Studio 2022 BuildTools** (the MSVC toolchain).
- **Rust 1.97.1** — install with [rustup](https://rustup.rs/) and use the `x86_64-pc-windows-msvc` target. The version is pinned in `rust-toolchain.toml`.
- **Bun 1.4.2** — only needed for the Tauri CLI. It is a build tool, not a runtime requirement.
- **Microsoft WebView2 Runtime** for running a build.

## Build

Run from a **Developer Command Prompt** for BuildTools (or any shell where the MSVC environment is active):

```bat
cargo build --manifest-path src-tauri\Cargo.toml --locked --no-default-features
```

The debug executable appears at `src-tauri\target\debug\speechek.exe`.

The debug build is **portable**: double-click `src-tauri\target\debug\speechek.exe` to run it — no installer, shortcuts, uninstall entry or registry write. The Development profile keeps its `settings.json` and `secrets.bin` beside that EXE (inside `src-tauri\target\debug`), separate from the installed production app, which keeps them under `%APPDATA%\Speechek`; the two profiles never read each other's files.

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

## Submitting a change

- Open an issue using one of the templates under `.github/ISSUE_TEMPLATE/` first when the change is not trivial.
- Keep changes focused; explain what changed and how you verified it (the command you ran and what you observed).
- Do not include keys, `secrets.bin`, `settings.json`, audio recordings or personal data anywhere in an issue, pull request or commit.
- Do not attach build output (`src-tauri/target/`, `node_modules/`) or generate new release files by hand.

## License

By contributing, you agree that your contributions are licensed under the project's [MIT License](LICENSE). Adapted third-party code keeps its original copyright and license — see `third_party/Handy.LICENSE`.
