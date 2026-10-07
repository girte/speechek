# Speechek user guide

[English](user-guide.md) | [Русский](user-guide.ru.md) · Back to [README](../README.md)

This guide covers installation, your Gemini API key, the settings window, the settings files, privacy, updating, uninstalling, autostart and diagnostics.

## Requirements

- **Windows 11 x64** (the installer also runs on ARM64 through Windows emulation only for non-target use; the supported platform is x64).
- **Microsoft WebView2 Runtime.** The installer downloads the bootstrapper if it is missing, so the first download needs internet access.
- **Internet access** for recognition, and a Gemini region where the Google AI API is available.
- A **microphone** visible to Windows.

## Install and first run

Run the `*-setup.exe` downloaded from [Releases](https://github.com/girte/speechek/releases). The installer:

- installs into your user profile (`%LOCALAPPDATA%\Speechek`), without administrator rights;
- offers a **desktop shortcut** and **start at sign-in** on a first install, both unchecked;
- has a **Run Speechek** box on the final page, checked by default;
- is not code-signed, so SmartScreen or Smart App Control may warn or block.

On the first launch Speechek creates `%APPDATA%\Speechek\settings.json` with an annotated template and **keeps running**: the process does not exit and no separate text file for keys is created. The settings window opens on the **API keys / API-ключи** section — enter at least one Gemini key, one per line, and click **Apply keys / Применить ключи**. No restart is required. Do not keep keys in `settings.json` or in Git.

There is no main window: the icon lives in the system tray. The app needs Microsoft WebView2 Runtime, microphone access and a free local port (default `127.0.0.1:4173`, changeable in General settings and applied after a restart). If the saved port is busy at startup, the app temporarily listens on a free port and reports it in the window. Keys are encrypted for the current Windows user; another account cannot read them.

## Getting a Gemini API key

1. Sign in with a Google account and open the [Gemini keys page in AI Studio](https://aistudio.google.com/apikey).
2. Accept the terms. For new users AI Studio creates a default project and key; if you already have Google Cloud projects, import the one you want (Dashboard → Projects → "Import projects") and create a key in it with "Create API key". New AI Studio keys are issued as authorization (auth) keys.
3. Billing is not required: the Free Tier is available to an active project without a linked payment method and gives free input and output for the available models. Limits depend on the model and are counted **per project, not per key**; current values are on the [rate limits page](https://ai.google.dev/gemini-api/docs/rate-limits), available models and prices on the [pricing page](https://ai.google.dev/gemini-api/docs/pricing). The listed limits are not guaranteed and can change.
4. Copy the key (you can copy several), paste them one per line in the settings window under **API keys / API-ключи**, and click **Apply keys / Применить ключи**. **Check keys / Проверить ключи** confirms only API access (`models.list`), not quota, model choice or recognition quality.

The Gemini API Free Tier and AI Studio are not available in every country — check the [available regions list](https://ai.google.dev/gemini-api/docs/available-regions). On the free tier Google may use the content you send to improve its products — see the [terms](https://ai.google.dev/gemini-api/terms) and the privacy section below.

## Quick start

1. Put the cursor in the text field you want.
2. Press **F2** (or your configured hotkey), speak, and press it again.
3. While recording, the overlay pill occupies 224×48 CSS pixels: indicator, status and timer separated by equal 12-pixel gaps. After you stop, a "Processing / Обработка" label and a spinning indicator remain; on an error the pill widens to show the reason. When the result is ready the pill hides before the text is pasted. It does not take focus: the text is pasted into the field active at paste time and stays in the clipboard.
4. **Escape** during recording or while waiting for the transcript cancels the current dictation: the pill hides, the microphone is released, the result is not pasted and the clipboard is not changed. If Gemini processing has already started, the sent request cannot be recalled. Outside dictation, Escape is an ordinary key of the active application.

If the app cannot confirm the automatic paste, it shows "Copied — paste manually / Скопировано — вставьте вручную" for a few seconds. The text is already in the clipboard: press **Ctrl+V** in the target field. If you copied something else during the operation, Speechek will not overwrite the newer copy. Once the text handover has begun, Windows cannot recall the Ctrl+V already sent; press Escape during recording or while waiting for the transcript instead.

Until at least one key is saved, dictation does not start: pressing the hotkey opens the settings window on **API keys / API-ключи** instead of recording.

## Tray and settings window

Right-clicking the tray icon opens a native menu with two items — **Settings / Настройка** and **Exit / Выход**, in that order; the click itself does not open the window. **Settings / Настройка** shows the single settings window and gives it focus **at any time, including during dictation**: a recording in progress is not cancelled or stopped and continues as usual; closing the window during dictation does not affect the session either. The hotkey starts and stops recording even with settings open, except while the cursor is in the hotkey field itself, where you can enter the active combination without starting a recording. The paste target is chosen only when the text is ready: if settings are active at that moment, Speechek does not paste into its own fields and keeps the text in the clipboard instead.

- General settings — mode, mute during recording, microphone and hotkey — apply immediately, **no restart needed**; the local port is saved immediately but applied after a restart. API keys are applied with the separate **Apply keys / Применить ключи** button. Recording parameters and the key list changed during dictation take effect from the next recording: the current session finishes with the snapshot captured at its start. Interface language is separate from that snapshot and changes immediately, including during recording.
- Closing the settings or lab window does not exit the app. If the key draft contains a genuinely changed or not-yet-submitted list, closing the window (and the **Exit / Выход** item) asks **Apply keys / Применить ключи**, **Do not apply / Не применять** or **Return / Вернуться**; **Do not apply / Не применять** destroys only the key draft and does not roll back already-applied general settings. Viewing and hiding the saved list without edits does not count as a change, so the question does not appear on close.
- The tray **Exit / Выход** item exits the app: the microphone, global hotkeys and port are released. An unapplied key draft and an unfinished apply are not lost silently.
- While recording, the hotkey cannot be changed: the combination field is disabled and submitting a new value is rejected with a clear hint until dictation finishes or is cancelled. Other recording settings — mode, mute and key operations — are available during recording but take effect from the next dictation and do not change the current one. The interface-language selector remains available and changes UI text immediately.
- The "?" hints next to settings open on hover or keyboard focus; a click does not pin the card, and moving the pointer away, losing focus, pressing Escape or switching sections hides it. The key-list field itself has no extra hint: the "apply keys" rule stays in the **API keys / API-ключи** section. The expanded microphone list uses the app colours if WebView2 supports a stylable `<select>`; other versions keep the system list. The settings and lab scrollbars stay visible and use the app palette.
- The lab always compares all three modes on one recording; there is no separate comparison toggle, and its button is unavailable while dictation is running. A lab recording uses the same native capture as dictation, one microphone at a time; closing the lab window stops its recording instead of leaving the microphone to a hidden page.

### Interface language

The **Language / Язык интерфейса** selector is in the settings header, above both **General settings / Общие настройки** and **API keys / API-ключи**. Its two choices always read **English** and **Русский**; there is no automatic third option.

A successful choice is saved and immediately updates settings, the built-in lab, overlay, tray menu, window titles, hints and messages, including already displayed statuses and errors. It needs no restart and does not stop recording, reset its timer, lose an API-key draft or translate speech or transcripts. Recognition mode, microphone and key list for an active recording stay pinned to that session. The choice does not change the NSIS installer or uninstaller language. A lab opened separately in an ordinary browser refreshes the language when its window regains focus; it does not receive the instant native-window settings event.

Initial language is resolved in this order:

1. The app's saved `language` choice (`en` or `ru`).
2. A supported installer choice: English (`1033`) or Russian (`1049`). A missing, unreadable, malformed or unsupported installer value is treated as absent.
3. The Windows **UI** language, not the region or keyboard layout: a Russian primary language selects Russian, and every other language selects English.

Development ignores installer registry entries and, without a saved app choice, uses Windows UI language. Test reads only its isolated installer entry, never Production's; it also keeps an independent saved choice. Changing the app language does not write the installer registry value.

Switching language does not apply, clear or replace the key draft. Existing hotkey and port commits are unchanged: if you leave either field to reach the selector, its usual blur commit still runs. A translation refresh itself does not submit fields or overwrite their unsubmitted text.

### General settings

- **Recognition mode / Режим распознавания**: **Live Smart** — streaming recognition during speech, the result arrives after you stop; **Smart** — recognition after recording with filler-word removal and formatting by meaning; **Verbatim / Дословно** — recognition after recording without editing. Switching applies immediately to the settings used for the next recording.
- **Hotkey / Горячая клавиша**: a plain text field with the current applied value. Enter `F2` or a combination such as `Ctrl+Shift+Space` and press Enter or move focus out of the field — the finished value applies immediately, there is no separate button; typing character by character only edits the field. Single letters and digits are not accepted, a modifier is required (Ctrl, Alt, Shift, Super); single function keys F1–F11 and F13–F24 are allowed, `F12` is reserved by the app and `Fn` is not reported to applications by Windows. An empty string, `Fn`, `F12` and a combination of modifiers only are rejected with an error next to the field, and the previous working combination keeps working. If the combination is taken by another program, the value is not applied and the previous key stays active. By default the hotkey is `F2`. **Escape without modifiers** is rejected: Escape is reserved for cancelling dictation.
- **Mute sound while recording / Отключать звук во время записи** — when enabled, Speechek mutes the overall Windows sound on the default playback device (the same control as the mute key) only for the actual microphone recording and restores the previous state after stopping or cancelling; outside recording the sound is untouched. If you changed the sound yourself during recording, Speechek keeps your decision. A failed mute or restore does not interrupt dictation: the pill shows a localized warning. Off by default; the change applies from the next dictation.
- **Start Speechek when signing in to Windows / Запускать Speechek при входе в Windows** — the same current-user autostart registration the installer can create: the switch is on when Speechek's own entry points to this executable and Windows has not disabled it, off when the entry is absent or disabled, and mixed/disabled while the registration cannot be read. Turning it on here re-enables an autostart that was disabled in Windows, so there is no need to open **Settings → Apps → Startup**; signing in may take slightly longer. The installer and this switch write the same registry value.
- **Microphone / Микрофон** — a dropdown of recording devices: **System default device / Системное устройство по умолчанию** plus connected devices with their identifiers; the list refreshes when the window opens and is activated. The choice is saved immediately and applies from the next recording — for both normal dictation and the built-in lab; a recording in progress finishes on the previous device. If the selected device is disconnected, recording uses the system microphone and the pill and window warn about it; the saved choice is not replaced by itself — select the device again when it reappears. The lab opened in an ordinary browser without the Speechek window uses the browser microphone. This selects the **input** device; it is unrelated to mute-during-recording, which targets the playback device.
- **Local port / Локальный порт** — the port of the built-in server, default `4173`. Typing digits only edits the field; Enter or leaving the field saves a whole number in the range 1–65535, and it applies after an app restart. If the saved port is busy at startup, the app temporarily listens on a free port and reports it in the settings window without overwriting the saved value.

### API keys

- Values are hidden: the summary shows only the number of saved keys. **Show keys / Показать ключи** is the only action that displays the list; switching sections, applying keys or closing the window hides it again. Opening, viewing and hiding the list does not change the draft: the close question appears only if the normalised list genuinely differs from the saved one, or the entered text has not reached the app yet.
- The single field opens with an explicit **Show keys / Показать ключи** and is editable. **Clear list / Очистить список** is a confirmed action on the draft: an empty field becomes a clear only after **Apply keys / Применить ключи**; with no saved key, dictation is disabled.
- **Apply keys / Применить ключи** is the only action that writes the entered (or empty) list to the DPAPI store. General settings, including interface language, and the hotkey apply independently and never read or replace the key draft. If the container is unavailable, the **Show keys / Показать ключи** button is labelled **Enter keys / Ввести ключи** and opens an empty editor without revealing unknown saved values; replacing them requires separate confirmation.
- **Check keys / Проверить ключи** checks access to the Gemini API (`models.list`) for the current draft, one key at a time. Success confirms only access, not model choice, recognition quality or quota. The check saves nothing, does not consume the key queue and is not needed to apply.
- One key per line; empty lines and duplicates are dropped, a space inside a key is an error for that line with its number.

## Settings file

`%APPDATA%\Speechek\settings.json` is ordinary JSON you can open in any editor. On first launch it is written from the annotated template `config/settings.example.json` (embedded into the EXE), with `language` set to the detected initial language rather than blindly copying the template's `en` placeholder. Only `//` comments on a **separate line** are allowed; trailing comments and extra commas are not. The `hotkey` and `mode` fields are required and must appear exactly once: a missing or duplicated field makes the file invalid instead of substituting a default. `mute_during_recording`, `port` and `input_device` may be omitted — then `false`, `4173` and the system input device apply. `language` may also be omitted: the initial-language rules above supply its default. Older files are read without being rewritten at startup; missing optional fields, including the resolved `language`, are inserted on the next successful settings write. A no-op apply that performs no write does not migrate the file. Writes update only managed values and preserve unrelated properties, comments, BOM and line endings. An explicit `null`, number, unsupported string or duplicate `language` is invalid, not a request for automatic detection.

| Field | Meaning |
| --- | --- |
| `hotkey` | Global record/stop key, default `F2`. Combinations such as `Ctrl+Shift+F9` are possible; before the key you may use `Ctrl`, `Alt`, `Shift`, `Super`. Single letters and digits, `Fn`, `F12`, modifier-only combinations and **Escape without modifiers** are rejected: Escape is reserved for cancelling dictation. |
| `mode` | `live` — Live Smart (streaming recognition during speech); `smart` — process the finished recording: remove filler words and format by meaning; `verbatim` — verbatim transcription without editing. |
| `mute_during_recording` | `true` — mute the overall Windows sound on the default playback device for the actual recording and restore the previous state after stopping or cancelling; `false` — leave the sound alone. Optional: absence means `false`. |
| `port` | Local port of the built-in server, default `4173`; an integer from 1 to 65535. A new value applies on the next app start; a busy saved port does not block startup — the app temporarily listens on a free port. Optional: absence means `4173`. |
| `input_device` | Identifier of the selected microphone as Windows reports it; `null` or a missing field means the default system input device. The choice is set from the **Microphone / Микрофон** list in **General settings / Общие настройки**, saved immediately, takes effect from the next recording and is not replaced by itself if the device is unavailable. |
| `language` | Interface language: `"en"` for English or `"ru"` for Russian. Optional: absence uses the initial-language precedence above without a startup rewrite. Set it with **Language / Язык интерфейса** for immediate application and saving; it does not translate dictated text or change the installer/uninstaller language. |

There are no keys or paths to them in the file: the list is encrypted separately in `secrets.bin` next to `settings.json`.

Startup errors fall into two kinds. An invalid `settings.json` — including a port outside the range 1–65535 — stops the app with a native message: the file is not overwritten and no other port is substituted. A busy saved port no longer stops startup: the app temporarily listens on a free port and reports it in the settings window while the saved value stays. Missing keys, a corrupted `secrets.bin` or a hotkey taken at startup are not fatal: the app starts, dictation stays unavailable, and the settings window opens on the section where this is fixed — **API keys / API-ключи** for a key problem, **General settings / Общие настройки** for a hotkey-only problem; if both are broken, **API keys / API-ключи** takes priority.

### Partial apply

Each of the two files is replaced atomically on its own, but the pair `settings.json` + `secrets.bin` is not a single crash-safe transaction: they are written sequentially and a failure between them is possible. If a previous apply could not be fully rolled back, the window shows a localized warning about partially changed files while the app continues using the previous settings. It is not cleared by closing or reopening the window; the next successful apply — of general settings, the hotkey or keys — forcibly rewrites and re-reads all potentially affected files, and only then is the warning cleared. Until a successful apply, the app runs on the previous settings.

## How a Gemini key is chosen

Keys are stored in `secrets.bin` next to `settings.json` and read at startup. The list is normalised when keys are applied: empty lines and duplicate keys are dropped, keeping the first occurrence.

For each **accepted** request the next key is taken in a circle: first, second, third, first again. A Live stream uses the chosen key until the connection closes. A batch request uses the same key for uploading the audio, recognising it and deleting the temporary file. If the applied list is unchanged, the queue continues from the previous position; after the list changes, counting starts from the first key, and a dictation already in progress finishes on the previous set. Keys of one project share its limits: rotation does not increase the total quota and does not reset exhausted limits.

A failed request that is rejected before reaching Gemini (for example, a bad WAV) does not advance the queue. If Gemini rejects the already-chosen key, the attempt counts as spent: there is no automatic switch to another key and no retry. In the lab, comparing three modes fires one Live and two batch requests; which of the simultaneous requests gets the next key depends on their arrival order.

## Privacy and limitations

- Keys are encrypted with Windows DPAPI **for the current user** (`CryptProtectData`, without machine scope, without an extra password or environment variables) and stored in `secrets.bin` next to `settings.json`. The file is created with a protected access descriptor: full access only for SYSTEM and the current user. This protects the profile, not the EXE: any code running under the same account can decrypt the file with the same Windows facilities.
- There is no plaintext key on disk and no fallback path: a foreign, truncated or undecryptable container counts as an error, not as an "empty list". `/api/settings` returns only `hotkey`, `mode`, `language` and the runtime `revision` — no keys, no count, no paths. Keys are sent to the settings window only via the explicit **Show keys / Показать ключи** button; app error messages strip key values. The app does not promise to erase every copy of a key from WebView processes, network buffers and system logs.
- Audio is not saved by the app as a file. A batch recording is temporarily uploaded to the Gemini Files API and deleted after the request; for Interactions, `store:false` is used. The maximum recording length is 10 minutes.
- The **Google Free-tier** terms may allow the speech and responses you send to be used to improve the services, including human review. Do not dictate confidential information without weighing these terms.
- Batch text is pasted only after the result is confirmed; the intermediate Live text is never pasted. When the app cannot prove that the target window read the clipboard, it leaves the text for manual insertion.
- The paste mechanism is adapted from Handy under MIT; the full text is `third_party/Handy.LICENSE`, and in the running app `/licenses/Handy.LICENSE`.

## Autostart and taskbar

- **Start at sign-in** is offered once during a first install (a checked-by-default **Run Speechek** box is separate). Change it later with the **Start Speechek when signing in to Windows / Запускать Speechek при входе в Windows** switch in **General settings / Общие настройки**: it writes the same per-user `Run` entry, so there is no manual Windows Settings step, and if autostart was disabled in Windows, turning the switch on re-enables it. When the registration cannot be read (a foreign or damaged value owns the name), the switch is mixed and disabled and says so instead of reporting a state Windows did not confirm. The installer does not re-create or re-enable the entry during an in-place upgrade, and it does not override a manual removal or a change made in Windows.
- **Taskbar pinning** is a user action, not an installer action. Find Speechek in the Start menu, open its context menu (**More options**), and choose **Pin to taskbar** if your Windows policy allows it. The installer does not pin anything through Shell scripts.

## Update

Speechek has no built-in auto-updater and makes no hidden self-update network calls. To update, download the newer `*-setup.exe` from [Releases](https://github.com/girte/speechek/releases) and run it: the installer detects the installed version and performs an in-place upgrade, keeping your settings and keys. Only a program currently running from the installed folder can be closed by the installer; a standalone portable EXE outside the install folder is not terminated — close it yourself before running the installed version. A newer installer can also be run for repair if a version check or the uninstall entry looks damaged.

## Uninstall

Use **Settings → Apps → Installed apps → Speechek → Uninstall**, or the `uninstall.exe` next to the installed executable. The uninstaller warns that Speechek will be closed before files are removed. By default your settings and API keys are kept; a separate unchecked **Delete settings and API keys** box removes the real `%APPDATA%\Speechek` profile and the WebView2 data for the app. The uninstaller never touches Speechek-Dev or Speechek-Test data, and it does not recursively delete arbitrary folders.

## Diagnostics

If the tray icon did not appear, check the native message at startup: the most common causes are an invalid `settings.json` or a port outside the range 1–65535. If F2 does not start recording, there is probably no saved key or the startup combination was taken: the app opens the settings window on the relevant section, and the key can be reassigned in **General settings / Общие настройки** without a restart. If the overlay does not appear, check the Windows microphone permission. If the text was not pasted, check the active field and the clipboard; failure to paste into an elevated application is not counted as a successful paste.

## Building from source

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the toolchain, build and test commands.

The owner preview runs GUI-free localization control before the final Development build; the standalone command is `scripts/check-localization.ps1`. Bun is a development tool for these checks, not an app runtime dependency. Native UI and text smoke remain exclusively owner-run.
