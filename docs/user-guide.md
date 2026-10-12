# Speechek user guide

[English](user-guide.md) | [Русский](user-guide.ru.md) · Back to [README](../README.md)

The [README](../README.md) gets you from download to your first dictation. This guide covers the rest: every setting, how several keys work together, what happens to your recordings, updating, uninstalling and fixing common problems.

**Contents:** [Requirements](#requirements) · [Install](#install) · [Gemini API key](#gemini-api-key) · [Dictating](#dictating) · [Recognition modes and the lab](#recognition-modes-and-the-lab) · [Settings](#settings) · [The settings file](#the-settings-file) · [Privacy and limitations](#privacy-and-limitations) · [Start at sign-in and the taskbar](#start-at-sign-in-and-the-taskbar) · [Updating](#updating) · [Uninstalling](#uninstalling) · [Troubleshooting](#troubleshooting)

## Requirements

- **Windows 11 x64.** The installer refuses to run on ARM64 PCs, Windows 10 and Windows Server.
- **Microsoft WebView2 Runtime.** Windows 11 usually has it. If it's missing, the installer downloads it, so install with the internet on.
- **An internet connection** and a country where the [Gemini API is available](https://ai.google.dev/gemini-api/docs/available-regions).
- **A microphone** that Windows can see.

## Install

Download `Speechek_<version>_x64-setup.exe` from [Releases](https://github.com/girte/speechek/releases) and run it.

- It installs for your user only, into `%LOCALAPPDATA%\Speechek`. No administrator rights needed.
- On a first install it offers **Create a desktop shortcut** and **Start Speechek when I sign in to Windows**. Both are unchecked; tick what you want.
- **Run Speechek** on the last page is checked, so the app starts right away.
- The installer isn't code-signed. SmartScreen may warn you: click **More info → Run anyway**. Smart App Control or a strict company policy can block it completely.

Speechek has no main window. After the first start it lives in the system tray (the area by the clock; look under the **^** arrow if the icon is hidden) and opens the settings window on the **API keys** tab, so you can add your key straight away.

## Gemini API key

Speechek has no servers of its own. It sends audio straight to Google Gemini under your own key, so you need one.

### Getting a key

1. Open [Google AI Studio → API keys](https://aistudio.google.com/apikey) and sign in with your Google account.
2. Accept the terms. On your first visit AI Studio creates a project and a key for you. If it doesn't, create a project right there in AI Studio (you don't need the Google Cloud console) and click **Create API key**.
3. Copy the key.

You don't need to add a payment method: the Free Tier works on a project without billing. Free limits depend on the model and can change; current numbers are on Google's [rate limits](https://ai.google.dev/gemini-api/docs/rate-limits) and [pricing](https://ai.google.dev/gemini-api/docs/pricing) pages. AI Studio shows how much each project has used.

### Adding it to Speechek

Right-click the tray icon → **Settings** → **API keys** → paste the key → **Apply keys**. No restart needed.

Until at least one key is saved, the hotkey doesn't record. It opens this tab instead.

### Several keys

Google counts free limits **per project, not per key**. Two keys from the same project share one quota and give you nothing extra. To get more free dictation, create a **separate project for each key** in AI Studio and add all the keys to Speechek, one per line.

Speechek then uses them in turn: the first dictation goes to the first key, the next one to the second, and so on in a circle. A few details:

- If you change the list, counting starts again from the first key. A dictation already in progress finishes on the old list.
- If Gemini rejects the key chosen for a dictation (quota used up, key revoked), that dictation ends with an error. Speechek doesn't retry it on another key; the next dictation simply gets the next key.
- A request that fails before it reaches Gemini, for example because the recording is damaged, doesn't use up a turn.

## Dictating

1. Click into the text field where the text should go.
2. Press **F2** (or your own hotkey) and speak. A small pill at the bottom of the screen shows **Recording**, a level meter and a timer.
3. Press **F2** again. The pill switches to **Processing**.
4. When the text is ready, the pill disappears and the text is pasted into whatever field is active at that moment. The text also stays on your clipboard.

The pill never takes focus, so you can keep working while it's visible. One recording can be up to 10 minutes long.

### Cancelling

Press **Esc** while recording or while the pill says **Processing**. The pill disappears, the microphone turns off, nothing is pasted, and your clipboard stays as it was.

Two limits:

- Audio already sent to Gemini can't be called back. Esc only guarantees that the answer won't be pasted.
- Once the paste has started, it's too late: Windows has already received Ctrl+V.

Outside a dictation, Esc works as usual in your other apps.

### When the text isn't pasted automatically

Sometimes Speechek can't confirm that the target app accepted the paste. Then the pill shows **Copied — paste manually** for a few seconds. The text is on your clipboard: click into the field and press **Ctrl+V**.

This also happens when:

- the Speechek settings window is the active window when the text is ready (Speechek doesn't paste into its own fields);
- the target app runs as administrator, and Windows blocks a normal app from pasting into it.

If you copy something else while Speechek is working, your newer copy wins: Speechek won't overwrite it.

## Recognition modes and the lab

| Mode | When the text arrives | What you get |
|---|---|---|
| **Live Smart** | Recognized while you speak, pasted after you stop | Clean text without filler words, formatted by meaning |
| **Smart** | The whole recording is processed after you stop | Clean text without filler words, formatted by meaning |
| **Verbatim** | The whole recording is processed after you stop | Your exact words, without Smart editing |

Live Smart pastes only the final text, never the partial results it sees along the way.

**The lab** helps you choose. In **Settings → General settings**, click **Compare modes in real time**: you record once and see the results of all three modes side by side. One lab recording costs three Gemini requests. The lab button is unavailable while a dictation is running, and closing the lab window stops its recording.

## Settings

Right-click the tray icon → **Settings**. Left-clicking the icon does nothing. The menu has just two items: **Settings** and **Exit**.

You can open settings at any time, even mid-dictation; the recording carries on. Closing the window doesn't quit Speechek. Only **Exit** in the tray menu does that, and it frees the microphone, the hotkey and the local port.

Most settings apply the moment you change them. API keys are the exception: they apply only when you click **Apply keys**.

If you change settings during a dictation, the current recording finishes with the settings it started with. The changes apply from the next one. The interface language is the only thing that switches immediately.

### Interface language

The **Language** selector is in the Settings header: **English** or **Русский**. The **?** beside its label shows help. The choice is saved and applied at once to every window, the pill and the tray menu, with no restart. It doesn't stop a recording, doesn't touch unapplied keys, and doesn't translate your dictated text.

On the very first start Speechek picks the language you chose in the installer. Without one, it follows the Windows display language: Russian if Windows is in Russian, English otherwise.

### General settings

**Recognition mode.** Live Smart, Smart or Verbatim; see [the table above](#recognition-modes-and-the-lab).

**Dictation language.** **Auto-detect** (the default) suits most cases. If you dictate in one language, choose it: Gemini gets a hint that helps it recognize speech more accurately. If you mix two, for example Russian speech with English terms, choose the second one in the picker that appears next to the first. Dictate in the language you chose. The setting works in Live Smart, Smart and Verbatim and in the lab, applies from the next recording, and a dictation already running keeps the languages it started with.

**Hotkey.** Type a key or a combination, for example `F2` or `Ctrl+Shift+Space`, then press Enter or click elsewhere. It applies immediately.

- You can use a letter (A–Z), a digit (0–9), F1–F24 except F12, Space, Enter, Tab, the arrows, Home/End, PageUp/PageDown, Insert, Delete or Backspace. Add `Ctrl`, `Alt`, `Shift` or `Super` (the Windows key) in front if you like.
- A modifier is optional, but a bare letter, digit or Space would take that key away from every other app. Function keys or combinations are the safer choice.
- Not allowed: F12 (reserved), Fn (Windows doesn't report it to apps), Esc on its own (it cancels dictation), modifiers on their own.
- If the combination is invalid or taken by another program, you'll see why next to the field, and the old key keeps working.
- The hotkey can't be changed while a dictation is running.
- While your cursor is in this field, pressing the hotkey types it instead of starting a recording.

**Microphone.** The system default or a specific device. The choice applies to the next recording, in both dictation and the lab. If the selected microphone gets disconnected, Speechek records from the system default and warns you. Your choice stays saved, so once the device is back, Speechek uses it again. If the warning doesn't go away after you reconnect it, select the device in the list again.

**Mute sound while recording.** Silences Windows audio on the default playback device while the microphone is on, then restores the previous state. If you change the volume yourself during a recording, Speechek leaves it as you set it. If muting fails, the recording still goes on and the pill shows a warning. Off by default.

**Start Speechek when signing in to Windows.** The same autostart setting as the installer's checkbox. If you disabled Speechek in Windows startup apps, turning this on enables it again. If Speechek can't read its autostart entry (for example, another program's entry has the same name), the switch is greyed out and says so instead of guessing.

**Custom vocabulary.** Words and phrases you want recognition to lean toward: names, brands, terms and abbreviations as they should be spelled, one per line. It works in Live Smart, Smart and Verbatim and in the lab, and it biases recognition instead of replacing text. The list is saved when you leave the field or press Ctrl+Enter (a plain Enter starts a new line); there is no separate button. It applies from the next recording, and you can edit it during a dictation — the recording already running keeps the list it started with. The field shows the saved list, with empty lines and exact, case-sensitive repeats removed; `Speechek` and `speechek` are two different terms. Up to 1000 terms after that, and a longer list is refused with an error while the previous vocabulary stays in use. Above 100 terms Speechek suggests a shorter list: that is Google's own recommendation for recognition.

### API keys

- Saved keys are hidden; the tab shows only how many there are. **Show keys** reveals the list. Switching tabs, applying or closing the window hides it again.
- Edit the list in the text box, one key per line. Empty lines and duplicates are dropped; a key with a space inside is flagged with its line number.
- **Apply keys** saves the list. Nothing else does.
- **Clear list** empties the box after you confirm. The saved keys are deleted only when you then click **Apply keys**. With no keys left, dictation stops working until you add one.
- **Check keys** tests whether each key in the box can reach the Gemini API. It saves nothing and doesn't spend a dictation turn. Passing the check doesn't tell you how much quota is left.
- If you close the window or exit with unapplied changes, Speechek asks: **Apply keys**, **Do not apply** (discards only the key changes) or **Return**.
- If the saved key file can't be read (for example, it was copied from another Windows account), the button says **Enter keys**: you can type a new list, and Speechek asks before replacing the old one.

## The settings file

You never have to edit files by hand, since the settings window covers everything. If you want to, settings live in `%APPDATA%\Speechek\settings.json`. Paste that path into the Explorer address bar to open the folder.

- It's plain JSON. Comments are allowed only as whole lines starting with `//`. Trailing commas aren't allowed.
- `hotkey` and `mode` must appear exactly once. Other fields can be left out; then the defaults apply.
- When Speechek saves settings, it changes only its own values and keeps your comments and any other text in the file.
- Hand-editing isn't required, but if you write `custom_vocabulary` yourself, whitespace around a term, empty lines and exact repeats are removed exactly as they are in the field. A value that isn't a list of strings, a term containing a line break, or more than 1000 terms stops Speechek at startup with an error naming `custom_vocabulary`.
- `dictation_languages` is checked strictly: an unknown code, a different spelling (`ru-ru` instead of `ru-RU`), a repeat or more than two codes stops Speechek at startup with an error naming `dictation_languages`.

| Field | Values | Default |
|---|---|---|
| `hotkey` | A key or combination, same rules as in [General settings](#general-settings) | `"F2"` |
| `mode` | `"live"` (Live Smart), `"smart"`, `"verbatim"` | `"live"` |
| `mute_during_recording` | `true` or `false` | `false` |
| `input_device` | A microphone ID as Windows reports it, or `null` for the system default | `null` |
| `language` | `"en"` or `"ru"` | Picked on first start, see [Interface language](#interface-language) |
| `custom_vocabulary` | A list of words and phrases that recognition leans toward; an empty list turns the vocabulary off | `[]` |
| `dictation_languages` | Up to two different language codes from Gemini's list, for example `["ru-RU"]` or `["ru-RU", "en-US"]`; an empty list means Auto-detect | `[]` |

API keys are never in this file. They're stored encrypted in `secrets.bin` in the same folder.

Speechek's windows use a local server on `127.0.0.1`. Windows chooses a free port at every start; there is nothing to configure. An old `port` line in `settings.json` is ignored and removed the next time you save settings.

If `settings.json` is broken (bad JSON or a duplicated field), Speechek shows an error at startup and doesn't start. It never overwrites your file; fix it and start Speechek again. Missing keys or a hotkey taken by another program don't stop the app: it starts and opens the settings tab where you can fix the problem.

Speechek writes `settings.json` and `secrets.bin` one after the other. If something interrupts the save between the two, the settings window warns that the save was only partial. Speechek keeps running on your previous settings, and the next successful apply rewrites both files and clears the warning.

## Privacy and limitations

> [!IMPORTANT]
> Speechek sends your speech to Google for recognition. On the Free Tier, Google may use what you send to improve its products, including human review; see the [Gemini API terms](https://ai.google.dev/gemini-api/terms). Don't dictate anything confidential unless you accept that.

**Your keys**

- They're encrypted with Windows DPAPI for your Windows account and stored in `%APPDATA%\Speechek\secrets.bin`. Only your account and SYSTEM can open the file. There's no plain-text copy on disk.
- DPAPI protects the keys from other Windows accounts, not from programs running under your own account. Any program you run can decrypt them the same way Speechek does.
- Keys appear on screen only when you click **Show keys**. Error messages never include them. Speechek can't guarantee that no copy is ever left in memory or system logs.

**Your recordings**

- Speechek keeps your last dictations (10 by default), audio and recognized text, in `%APPDATA%\Speechek\history`, encrypted with Windows DPAPI for your user account and stored only on this computer. A normal (Production) build keeps no journals and no other copies of audio or text. Only a separate debug Development build with the explicit `dev-diagnostics` flag additionally writes a plaintext local archive; see [Building from source](#building-from-source).
- For Smart and Verbatim, the recording is uploaded to Gemini temporarily and deleted after recognition. Speechek also asks Gemini not to store the request.
- Live Smart streams the audio to Gemini while you speak.
- A custom vocabulary, if you set one, is sent to Google with every recording, in all three modes. It's plain text in `settings.json`, never in `secrets.bin`.

**Limits**

- No offline mode: recognition always happens at Google.
- Speechek depends on Google's Free Tier, quotas and model availability, and Google can change any of them. On a project with billing enabled, requests may be charged.
- Several keys add headroom only if they come from different projects. A rejected request isn't retried on another key.

## Start at sign-in and the taskbar

The installer offers **start at sign-in** once, on the first install. Later, use **Start Speechek when signing in to Windows** in General settings: it changes the same Windows setting. Updates don't turn autostart back on if you switched it off.

To pin Speechek to the taskbar, find it in the Start menu, right-click it and choose **Pin to taskbar**. The installer doesn't pin anything itself.

## Updating

Speechek doesn't update itself and doesn't check for updates in the background. To update, download the new `*-setup.exe` from [Releases](https://github.com/girte/speechek/releases) and run it. It updates in place and keeps your settings and keys. If the installed copy is damaged, running the installer again also repairs it.

The installer closes the running Speechek automatically, but only the installed copy; it warns you first. A dictation in progress and unapplied key changes are lost, so finish them before updating. If you run a separate portable build, close it yourself first.

## Uninstalling

Open **Windows Settings → Apps → Installed apps → Speechek → Uninstall**. The uninstaller closes Speechek first.

By default your settings and keys stay on the PC, in case you reinstall. To remove them too, tick **Delete settings and API keys**: that deletes `%APPDATA%\Speechek` and Speechek's WebView2 data. Nothing else is touched.

## Troubleshooting

**Windows won't run the installer.** It isn't code-signed. On a SmartScreen warning, click **More info → Run anyway**. If Smart App Control or a company policy blocks unsigned apps, Speechek can't run on that PC.

**Speechek shows an error at startup and no tray icon appears.** Usually `settings.json` is broken; the message says what's wrong. Fix the file (see [The settings file](#the-settings-file)) or delete it: Speechek creates a fresh one with defaults on the next start, and your keys stay intact.

**The hotkey opens settings instead of recording.** No key is saved yet, or the saved key file can't be read. Add a key on the **API keys** tab and click **Apply keys**.

**The hotkey does nothing.** Another program may have taken the combination. Speechek then opens **General settings** at startup; choose a different hotkey there. No restart needed.

**"Recording did not start" or a microphone error.** Check that Windows lets apps use the microphone: **Windows Settings → Privacy & security → Microphone**. Also check which microphone is selected in General settings.

**The pill warns that it's recording from the system microphone.** Your selected microphone is disconnected. Plug it back in; your choice stays saved.

**The pill says "Copied — paste manually".** The text is on the clipboard: click into the field and press Ctrl+V. See [above](#when-the-text-isnt-pasted-automatically) for why.

**The pill says Smart and Verbatim fail on Google's side.** This message appears for a specific Gemini HTTP 400 error. [Google reported the outage fixed on October 7, 2026](https://github.com/googleapis/js-genai/issues/2011#issuecomment-6044792622); Speechek's live API has not been rechecked since that report. If you encounter the error, temporarily switch to **Live Smart** in General settings and report it using the form below. A fix on Google's side does not require a Speechek update.

**An error says the key was rejected by Google.** The key was deleted, restricted or mistyped. Check it in AI Studio, and click **Check keys** after fixing the list.

**An error mentions Google HTTP 429.** The quota for that key's project is used up. Wait until it resets (daily limits reset at midnight Pacific time) or add a key from another project, which has its own quota.

**Still stuck?** Open an issue using the [bug report form](https://github.com/girte/speechek/issues/new/choose). Never attach your API keys, `secrets.bin`, `settings.json` or audio recordings.

## Building from source

See [CONTRIBUTING.md](../CONTRIBUTING.md).

### Development diagnostics

The portable **Development** debug build can keep a local archive of its own takes for troubleshooting ([issue #6](https://github.com/girte/speechek/issues/6)). It's a developer tool; the shipped app never has it.

- **Off unless asked for.** The `dev-diagnostics` Cargo feature is optional and never a default; Production and the isolated **Test** profile never enable it. Enable it for a local debug build:

  ```bat
  cargo build --manifest-path src-tauri\Cargo.toml --locked --no-default-features --features dev-diagnostics
  ```

  `scripts/prepare-preview.ps1` builds the official portable preview this way (the Development build is last, so no Test profile or `test-provider` remains in the executable). A debug build without the feature simply doesn't archive.
- **What is written.** One lossless FLAC of the raw PCM (16 kHz, mono, 16-bit) per native take, plus a technical JSONL journal of counters and events, plus the take's complete content: the settings and config snapshot, every recognition reply (interim, final and error), the provider requests and replies (`config`, `result`), the exact text handed to the paste step and its outcome. The snapshot also names the build that produced the take: the official portable preview records the exact commit it was built from, while a debug build compiled by hand without that identity records none rather than guessing one. Packets and replies that are not text — opaque WebSocket frames such as binary, Ping and Pong, or a body whose bytes aren't valid text — are kept as encoded data after the same key redaction, and plain text is never cut short. All of it stays inside the archive's own size budget, so anything that won't fit is dropped, counted and marked as incomplete. All outcomes are archived — successful, failed and cancelled takes. In the lab comparison, one take links its three Gemini requests to that single audio file. A take with no audio still carries its content, never an empty FLAC.
- **Self-contained.** A take directory is meant to be diagnosable on its own: with the archived audio and content, nobody has to retype the dictated speech or the recognized text to reproduce the problem.
- **Where.** `logs/` next to the Development `settings.json`; for the canonical debug build that's `src-tauri\target\debug\logs\`. Production creates no `logs` directory and writes no diagnostic audio, content or journals (its encrypted dictation history is separate), and no setting, URL or HTTP route can turn the archive on. The files are never served over HTTP.
- **Limits of the evidence.** The archive holds the audio Speechek sent and the replies it received; that cannot prove how Google's service interpreted the audio, and not every bug leaves a distinguishable trace.
- **Incomplete takes.** If the byte cap is reached or a write fails, a take records only what was saved and advertises itself as incomplete: dropped content, audio gaps and disk errors are counted, never presented as a complete take.
- **Retention.** Files are removed after 10 days or earlier if the archive outgrows a total cap of 100,000,000 bytes (decimal 100 MB) across audio, journals, content records, partial files and staging. Partial files and staging copies count toward the cap, as does the space reserved to finish an active take. Cleanup runs at startup, once a minute while Development runs and when a take closes; with the app closed, the files stay until the next launch.
- **Privacy.** The FLAC is plaintext personal audio and the archived content is plaintext recognized text — not encryption. Both are readable by your Windows user and SYSTEM (protected DACL), and therefore by any software running as you. Nothing is uploaded automatically and nothing in the archive is sent anywhere; it never contains API keys, authorization headers or its own WebSocket connection URL. The archived text is ordinary content, so a URL you dictated, or one Google returns in a reply, is kept like any other words. Use synthetic data only in automated checks; never use real keys or real speech.
- **Custom vocabulary.** A take never stores the terms themselves: the archived setup records only how many terms the vocabulary had.
