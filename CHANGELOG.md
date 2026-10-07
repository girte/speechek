# Changelog

All notable changes to Speechek are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The text of each GitHub Release is generated from the matching version section below plus a fixed install/unsigned/known-limitations/SHA-256 footer.

## [Unreleased]

### Changed

- Rewrote the English and Russian user guides for everyday users: task-oriented sections, a troubleshooting list by symptom, and the developer-only profile details moved to `CONTRIBUTING.md`. Bug reports can now note the recognition mode, and both issue forms accept English or Russian.

## [0.3.0] - 2026-10-07

### Added

- Full English/Russian app localization with a **Language / Язык интерфейса** selector above both settings tabs; a successful choice is saved and applied immediately to native windows, the lab, overlay, tray, titles, hints and messages without restarting, stopping recording or losing the API-key draft. Speech and transcripts are not translated, and the NSIS installer/uninstaller language is unchanged. A separately opened browser lab refreshes on activation. ([#1](https://github.com/girte/speechek/issues/1))
- GUI-free localization maintenance control before the owner preview: shared catalog validation, static UI message-key and argument checks, advisory direct-text warnings, and a Git-only changed-message report with review notices for edits in only some languages. Native UI/text smoke and owner approval remain manual; no translation service or per-string approval state is introduced. ([#3](https://github.com/girte/speechek/issues/3))

### Changed

- Settings now store `language` as `en` or `ru`. Initial selection uses the saved app choice, then a supported English/Russian installer choice, then the Windows UI language (Russian primary language selects Russian; all others select English). Missing, unreadable, malformed or unsupported installer values fall back to Windows; Development ignores installer registry values, and Test reads only its isolated entry. Older files without `language` use the detected default without a startup rewrite; the next successful settings write inserts the field, while a no-op apply without a write leaves the file unchanged.

### Fixed

- Smart and Verbatim transcription no longer shows Google's raw HTTP 400 thinking-setting rejection. That failure is now the localized hint to switch to **Live Smart** in settings while the problem stays on Google's side; the request payload is unchanged, so neither mode sends a thinking field, and Live Smart is unaffected.

## [0.2.1] - 2026-10-02

### Changed

- Development builds are portable: `settings.json` and the DPAPI-protected `secrets.bin` live beside the Development executable instead of in `%APPDATA%`; installed Production and isolated Test profiles retain their existing locations.
- The release process now hands off a committed Development preview for owner-run UI verification before dispatching a production candidate. The interactive smoke harness requires an explicit owner-session switch; GUI-free checks remain available without one.

## [0.2.0] - 2026-10-02

The first public release. Version `0.1.1` was a local build and was never published as a GitHub release.

### Added

- Free Windows 11 x64 voice input: press a global hotkey, dictate, press it again, and the confirmed text is pasted into the field active at paste time.
- Three recognition modes using Gemini — **Live Smart** (`gemini-3.5-transcribe-live`), **Smart** and **Verbatim** (shown as **Дословно** in the Russian UI) (`gemini-3.5-transcribe`) — with a built-in lab that compares all three on one recording.
- Settings window reachable from the tray: mode, hotkey, microphone, mute during recording, start-at-sign-in and the local server port.
- Multiple Gemini API keys with automatic round-robin, so requests spread across projects that have independent quotas.
- Windows DPAPI key storage for the current user in `secrets.bin`, with no plaintext keys on disk.
- Primary-user NSIS installer with an English/Russian language selector, per-user installation, and opt-in desktop shortcut and start-at-sign-in options plus a default-checked "Run Speechek" action.
- In-app **start-at-sign-in** switch in General settings that reads and writes the same current-user `Run` entry as the installer, re-enables an autostart disabled in Windows, and shows an unreadable registration as mixed and unavailable instead of a false state.
- Bilingual public documentation: English and Russian README and user guide, product context, contribution and security policies, and this changelog.
- Runtime component notices and Handy attribution served inside the app.

### Changed

- The distributable is now a single NSIS installer produced by CI; the application remains one `speechek.exe` with an embedded server and no separate Bun/Node backend.
- Release builds link the C runtime statically, so the executable runs without an extra VC/UCRT dependency.

### Fixed

- Packaged builds detect and close the previously installed instance through a dedicated exit request instead of interrupting the running user session.
- The installer no longer removes the previous installation before an upgrade, and no longer re-creates or re-enables shortcuts and autostart entries that the user removed or disabled.

### Security

- Added `SECURITY.md` and GitHub Private Vulnerability Reporting for private disclosure.
- Set up an immutable-release policy: published tags and installer files are never replaced; fixes ship as a new version.
- Added dependency-notice collection and license checks to the release pipeline.
