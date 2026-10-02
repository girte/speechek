# Speechek

![Speechek icon](design/approved-icon/icon-128.png)

**English** | [Русский](README.ru.md)

Speechek is a free voice-input app for Windows 11 x64 that turns your speech into text using your own Gemini API keys. Press a hotkey, dictate, press it again — the confirmed text is pasted into the field that is active at paste time. A single `speechek.exe` runs from the system tray; there is no separate Bun/Node backend and no local model to download.

## Why Speechek

1. **Free dictation, paid for by your own Gemini Free Tier.** The app itself is free with no subscription. The Gemini models it uses transcribe within the Google AI Free Tier, subject to Google's quotas, model availability and terms.
2. **Several keys add headroom — only when they belong to projects with independent quotas.** Speechek rotates requests across the keys you add so no single project takes all the load, and you do not switch keys by hand before each dictation. Keys from the same project share that project's limits, so adding them does not increase the total quota. Availability is decided by Google, and Speechek does not automatically retry a failed request on another key.
3. **Ready-to-use text with Gemini, without local models.** Smart modes remove filler words and format speech by meaning, so the result is easier to use in messages and documents. No local model download and no powerful GPU.
4. **Three modes — no need to force every task into one way of dictating.** **Live Smart** transcribes the stream while you speak (only the final result is pasted), **Smart** processes the finished recording and cleans it up, and **Дословно** keeps your original wording without Smart editing.

## Download and install

Get the latest installer from **[github.com/girte/speechek/releases/latest](https://github.com/girte/speechek/releases/latest)**.

- Platform: **Windows 11 x64**. The **installer** UI is available in **English and Russian**; the app UI is currently Russian.
- The installer is **not code-signed**. Windows SmartScreen or Smart App Control may warn you, or a restrictive policy may block the app from starting. There is no paid code-signing certificate yet.

## Quick start

1. Download the NSIS `*-setup.exe` asset from the releases page and run it. It installs for the current user — no administrator rights are required.
2. During a first install the setup offers two optional boxes, both unchecked: a desktop shortcut and starting Speechek when you sign in to Windows. The **Run Speechek** box on the final page is checked by default.
3. Speechek starts in the system tray (there is no main window). Right-click the tray icon, choose **Настройка**, then add one or more Gemini API keys under **API-ключи** and click **Применить ключи**. Without a saved key, pressing the hotkey opens the settings window instead of recording.
4. Put the cursor in the field you want, press **F2** to start, speak, and press **F2** again. **Escape** cancels the current dictation before the text is handed to the paste step. If the app cannot confirm the automatic paste, it shows "Скопировано — вставьте вручную"; the text is already in your clipboard, so press **Ctrl+V**.
5. To change modes, hotkey, microphone, sound behaviour or start-at-sign-in later, open the same settings window from the tray. General settings apply immediately; the local port is applied after a restart. The start-at-sign-in switch writes the same per-user entry as the installer, so no Windows Settings step is needed.

### Important caveats

- Recognition needs **internet access**, a **microphone**, a Gemini region where the Google AI API is available, and Microsoft **WebView2 Runtime**. The installer downloads the WebView2 bootstrapper if it is missing.
- Your **speech is sent to Google** for recognition. On the **Free Tier**, Google may use the content you send to improve its products, including human review. Do not dictate confidential information without weighing Google's terms.
- Google quotas, model availability and Free Tier conditions can change; requests on a paid project may be billed.

## Privacy at a glance

Speechek is cloud-based and needs internet access. Your speech is sent to Google for recognition. Gemini API keys are encrypted with Windows DPAPI for the current user in `secrets.bin` next to your settings file — not stored next to the executable and not in plaintext. Recordings are not saved as files. The full notes, including what the app can and cannot guarantee, are in the [user guide](docs/user-guide.md).

## Documentation

- [User guide (English)](docs/user-guide.md) · [Руководство (Russian)](docs/user-guide.ru.md) — API keys, settings, files, update, uninstall, autostart, diagnostics.
- [Contributing](CONTRIBUTING.md) — build and test from source.
- [Releasing](docs/releasing.md) — how official builds are produced and published.
- [Changelog](CHANGELOG.md) · [Releases](https://github.com/girte/speechek/releases)

## License and attribution

Speechek's own code is released under the [MIT License](LICENSE). The clipboard-paste mechanism is adapted from [Handy](https://github.com/cjpais/Handy) (MIT); its full license text is kept at [`third_party/Handy.LICENSE`](third_party/Handy.LICENSE) and served inside the app at `/licenses/Handy.LICENSE`. Third-party components bundled into the application and installer are listed in the notices that ship with each release; Handy's copyright and license are kept intact.
