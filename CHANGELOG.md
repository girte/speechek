# Changelog

All notable changes to Speechek are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The text of each GitHub Release is generated from the matching version section below plus a fixed install/unsigned/known-limitations/SHA-256 footer.

## [Unreleased]

### Changed

- Removed the local port setting. Windows assigns a free loopback port at every start; an old `port` line in `settings.json` is ignored and removed on the next settings save. ([#8](https://github.com/girte/speechek/issues/8))

### Fixed

- A busy or reserved local port (for example, `os error 10013` at Windows sign-in) no longer stops startup, because Windows chooses a free port instead of Speechek binding a fixed one. ([#8](https://github.com/girte/speechek/issues/8))
- Installer: uninstalling now removes Windows' "disabled" startup marker together with the autostart entry, and a new installation with **Start Speechek when I sign in to Windows** checked clears a stale one, so the opt-in actually starts Speechek. The uninstaller keeps an autostart entry that points at another copy of Speechek, keeps a `%LOCALAPPDATA%\speechek.exe` link that no longer points at this installation, and removes `uninstall.exe` last, so an incomplete uninstall can still be repeated from **Installed apps**. A failed update now rolls back the uninstall record and autostart command with their quotation marks intact (previously the next installer could refuse the damaged record), and the installation stops if its rollback snapshot cannot be written. Choosing to delete settings and keys now reports a profile folder it cannot read instead of silently leaving it. ([#9](https://github.com/girte/speechek/issues/9))

## [0.4.0] - 2026-10-11

### Added

- **Custom vocabulary**: a list of words and phrases recognition leans toward — names, brands, terms and abbreviations as they should be spelled — set in **Settings → General settings → Custom vocabulary**, right after the local port ([#5](https://github.com/girte/speechek/issues/5)). One term per line; the list is trimmed, with blank lines and exact repeats dropped, and the field shows the saved list. Leaving the field or pressing Ctrl+Enter applies it (plain Enter adds a line); a separate button isn't needed, and the list applies from the next recording, so a dictation already running keeps the list it started with. It works in Live Smart, Smart and Verbatim and in the lab, where it biases recognition instead of replacing text. Up to 1000 terms after normalization — a longer list is refused and the previous vocabulary stays in use — and above 100 terms the window shows Google's recommendation to shorten it. The list lives in `settings.json` as `custom_vocabulary` (never in `secrets.bin`) and is sent to Google with every recording, in all three modes; the debug-only diagnostic archive keeps only the number of terms. Both READMEs and both user guides document the setting.
- **Dictation language**: **Settings → General settings → Dictation language**, right after the recognition mode ([#4](https://github.com/girte/speechek/issues/4)). **Auto-detect** stays the default; choosing one of Gemini's 83 transcription languages sends it as a hint, and a second picker appears for mixed speech, for example Russian with English terms. Language names follow the interface language and are sorted in it. The hint rides in `language_codes` for Smart and Verbatim and in `languageCodes` for Live Smart, so it works in all three modes and in the lab; it applies from the next recording, and Auto-detect sends exactly the requests it did before. The choice is stored as `dictation_languages` in `settings.json` (`[]`, one or two codes); an unknown code, another spelling, a repeat or a third code stops startup with an error naming the property.

### Changed

- Rewrote the English and Russian user guides for everyday users: task-oriented sections, a troubleshooting list by symptom, and the developer-only profile details moved to `CONTRIBUTING.md`. Bug reports can now note the recognition mode, and both issue forms accept English or Russian.
- Made GitHub the source of truth for task status, added live bug/improvement/completed-task links to both READMEs, documented type and status labels in `CONTRIBUTING.md`, and set both issue forms to start in the backlog. No product behaviour changed.
- Updated both user guides to reflect Google's report that the October 7 Transcribe outage was fixed, retaining the Live Smart workaround if the error recurs; Speechek's live API has not been rechecked.
- Clarified the interface-language help in English and Russian: it describes the setting's purpose without listing languages, while retaining the immediate-apply, saved-choice and speech-independence explanation. ([#2](https://github.com/girte/speechek/issues/2))
- Expanded the debug-only Development diagnostic archive ([#6](https://github.com/girte/speechek/issues/6)) so a take is diagnosable without the owner retyping anything: beside the verified lossless FLAC, each take now keeps the full text content — the config and runtime snapshot, every recognition reply (interim, final, error), the provider requests and replies, transport and socket errors, the recorder result and the exact paste payload with its outcome — plus the laboratory report's own `result` and `errors` through the Development-only `/api/dev-diagnostics` route. Non-text evidence is kept whole as well: an opaque WebSocket frame (binary, Ping, Pong, raw) and an HTTP body that is not valid UTF-8 are redacted against the known keys and then stored losslessly as base64 JSON instead of only being counted, a malformed plaintext payload stays plain text, and existing text is never truncated. The runtime snapshot now also names the producing build: the official portable preview records the exact commit it was built from (`scripts/prepare-preview.ps1` passes the clean `main` HEAD, `build.rs` accepts it only as a full hexadecimal SHA), so two same-version executables can be told apart without asking the owner, while a hand-built debug binary without that identity records no commit instead of a guess. Docs updated to match in both READMEs, both user guides and `CONTRIBUTING.md`; the archive stays Development-only (Production and Test unchanged), redacts known keys, authorization headers and its own connection URL before writing, keeps the 10-day/100,000,000-byte policy over all content with the 8 MiB queued-content budget, 1 MiB content-report route cap and 4096-byte ordinary report enforced by dropping, counting and marking a take incomplete rather than presenting dropped or failed content as complete, and it cannot prove how the service interpreted the audio, nor does every bug leave a distinguishable trace.

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
