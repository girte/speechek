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

The **Language** switch sits at the top of the window: **English** or **Русский**. The choice is saved and applied at once to every window, the pill and the tray menu, with no restart. It doesn't stop a recording, doesn't touch unapplied keys, and doesn't translate your dictated text.

On the very first start Speechek picks the language you chose in the installer. Without one, it follows the Windows display language: Russian if Windows is in Russian, English otherwise.

### General settings

**Recognition mode.** Live Smart, Smart or Verbatim; see [the table above](#recognition-modes-and-the-lab).

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

**Local port.** The port Speechek's windows use internally. Default `4173`; you'll rarely need to touch it. A new value is saved immediately but applies after a restart. If the port is busy at startup, Speechek borrows a free one for that session and tells you.

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

| Field | Values | Default |
|---|---|---|
| `hotkey` | A key or combination, same rules as in [General settings](#general-settings) | `"F2"` |
| `mode` | `"live"` (Live Smart), `"smart"`, `"verbatim"` | `"live"` |
| `mute_during_recording` | `true` or `false` | `false` |
| `port` | A whole number from 1 to 65535 | `4173` |
| `input_device` | A microphone ID as Windows reports it, or `null` for the system default | `null` |
| `language` | `"en"` or `"ru"` | Picked on first start, see [Interface language](#interface-language) |

API keys are never in this file. They're stored encrypted in `secrets.bin` in the same folder.

If `settings.json` is broken (bad JSON, a duplicated field, a port out of range), Speechek shows an error at startup and doesn't start. It never overwrites your file; fix it and start Speechek again. Missing keys or a hotkey taken by another program don't stop the app: it starts and opens the settings tab where you can fix the problem.

Speechek writes `settings.json` and `secrets.bin` one after the other. If something interrupts the save between the two, the settings window warns that the save was only partial. Speechek keeps running on your previous settings, and the next successful apply rewrites both files and clears the warning.

## Privacy and limitations

> [!IMPORTANT]
> Speechek sends your speech to Google for recognition. On the Free Tier, Google may use what you send to improve its products, including human review; see the [Gemini API terms](https://ai.google.dev/gemini-api/terms). Don't dictate anything confidential unless you accept that.

**Your keys**

- They're encrypted with Windows DPAPI for your Windows account and stored in `%APPDATA%\Speechek\secrets.bin`. Only your account and SYSTEM can open the file. There's no plain-text copy on disk.
- DPAPI protects the keys from other Windows accounts, not from programs running under your own account. Any program you run can decrypt them the same way Speechek does.
- Keys appear on screen only when you click **Show keys**. Error messages never include them. Speechek can't guarantee that no copy is ever left in memory or system logs.

**Your recordings**

- Speechek doesn't save audio files.
- For Smart and Verbatim, the recording is uploaded to Gemini temporarily and deleted after recognition. Speechek also asks Gemini not to store the request.
- Live Smart streams the audio to Gemini while you speak.

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

**The pill says Smart and Verbatim fail on Google's side.** Google is rejecting these modes; Speechek can't fix that from its end. Switch to **Live Smart** in General settings for now. Once Google fixes the problem, Smart and Verbatim work again without an update.

**An error says the key was rejected by Google.** The key was deleted, restricted or mistyped. Check it in AI Studio, and click **Check keys** after fixing the list.

**An error mentions Google HTTP 429.** The quota for that key's project is used up. Wait until it resets (daily limits reset at midnight Pacific time) or add a key from another project, which has its own quota.

**Settings say the port is busy and another one is used.** Something else took port 4173 before Speechek started. Speechek works normally on the temporary port; nothing to do. If it keeps happening, pick another port in General settings.

**Still stuck?** Open an issue using the [bug report form](https://github.com/girte/speechek/issues/new/choose). Never attach your API keys, `secrets.bin`, `settings.json` or audio recordings.

## Building from source

See [CONTRIBUTING.md](../CONTRIBUTING.md).
