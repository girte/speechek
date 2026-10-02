# Changelog

All notable changes to Speechek are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The text of each GitHub Release is generated from the matching version section below plus a fixed install/unsigned/known-limitations/SHA-256 footer.

## [Unreleased]

## [0.2.1] - 2026-10-02

### Changed

- Development builds are portable: `settings.json` and the DPAPI-protected `secrets.bin` live beside the Development executable instead of in `%APPDATA%`; installed Production and isolated Test profiles retain their existing locations.
- The release process now hands off a committed Development preview for owner-run UI verification before dispatching a production candidate. The interactive smoke harness requires an explicit owner-session switch; GUI-free checks remain available without one.

## [0.2.0] - 2026-10-02

The first public release. Version `0.1.1` was a local build and was never published as a GitHub release.

### Added

- Free Windows 11 x64 voice input: press a global hotkey, dictate, press it again, and the confirmed text is pasted into the field active at paste time.
- Three recognition modes using Gemini — **Live Smart** (`gemini-3.5-transcribe-live`), **Smart** and **Дословно** (`gemini-3.5-transcribe`) — with a built-in lab that compares all three on one recording.
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
