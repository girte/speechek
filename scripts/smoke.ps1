<#
    scripts/smoke.ps1

    Safe runtime smoke for the isolated Speechek test fixture (release plan
    section 8). It exercises the *test* identity only:

        productName : Speechek Test
        identifier  : app.speechek.test

    and never touches the production Speechek process, profile, shortcuts or
    registry, never touches the Windows clipboard, and never terminates a
    process it did not start. Processes are stopped only by captured PID after
    the image path is confirmed to live under an allowed root; there is no
    `taskkill /IM`, no kill by name, and no Stop-Process on an unverified PID.

    Scenarios
      GateSelfTest   pure ownership-decision table (no host mutation)
      FixtureSelfTest starts the embedded fake provider and asserts the HTTP +
                     WebSocket transcript contract with synthetic PCM, no app
      FakeProvider   starts the debug/Test executable with the fake-provider
                     overrides and asserts /api/health, /api/settings,
                     POST /api/transcribe (smart + verbatim) and the /api/live
                     WebSocket end to end; generates a WAV in memory, never a
                     microphone recording
      FreshInstall   GUI install with the default (unchecked) options
      OptIn          GUI install with desktop + autostart checked (fresh only)
      Upgrade        GUI install of fixture B over fixture A (needs -FixtureB)
      Downgrade      GUI run of older fixture A over installed B; must refuse
      UninstallKeep  GUI uninstall without the data checkbox
      UninstallDelete GUI uninstall with the data checkbox (fixture data only)
      Cancel         GUI upgrade close-warning cancelled; installed state stays
      SameVersion    GUI reinstall of the installed version: in place, data kept
      CorruptVersion GUI run against a damaged/unknown version record; must
                     refuse before any change (both sub-cases: invalid text and
                     a missing DisplayVersion value); the record is restored
      PreserveOptions GUI upgrade with the owner's own changes in place: the
                     desktop shortcut deleted, the Run value kept but disabled
                     through Windows StartupApproved; none may be rewritten
      ForeignLocker  GUI run while a non-app process holds the canonical EXE;
                     must abort with the file-in-use message and touch nothing
      Rollback       GUI run with the staged notices file locked so the commit
                     fails after the EXE copy; the previous version must be
                     restored and reported
      ForceClose     GUI run over a suspended owned Test EXE (CreateProcessW
                     CREATE_SUSPENDED): it cannot answer the quit event, so the
                     installer must honour the shared 5 s grace and terminate
                     exactly the verified handle (exit code 1)

    Only the last five scenarios mutate installed state beyond a normal
    install/uninstall; all of them act on the Speechek Test identity and its
    ownership manifest only, never on production.

    The installer GUI is driven through real Win32 controls (EnumWindows/
    EnumChildWindows + BM_CLICK); silent/passive install is refused by the
    template itself and is never used here. The force-hang path is automated
    with a suspended owned Test EXE (see ForceClose). Manual-only branches
    (real microphone dictation, mute, Escape cancel, logon autostart, the
    interactive language dialog and the production candidate install) are
    printed at the end and are never faked.

    This file is UTF-8 *with* a BOM: it contains Russian installer control
    labels, and Windows PowerShell 5.1 only decodes non-ASCII string literals
    correctly when the BOM is present (without it smart-quote bytes break the
    parser). Keep the BOM when editing.

    PowerShell 5.1 and 7 compatible.
#>
[CmdletBinding()]
param(
    [ValidateSet('All', 'GateSelfTest', 'FixtureSelfTest', 'FakeProvider',
        'FreshInstall', 'OptIn', 'Upgrade', 'Downgrade', 'UninstallKeep',
        'UninstallDelete', 'Cancel',
        'SameVersion', 'CorruptVersion', 'PreserveOptions', 'ForeignLocker',
        'Rollback', 'ForceClose')]
    [string]$Scenario = 'All',

    [string]$Fixture,
    [string]$FixtureB,
    [string]$ExePath,

    [ValidateSet('en', 'ru')]
    [string]$Language = 'en',

    [int]$Port = 0,
    [int]$TimeoutSec = 300,
    [switch]$KeepRunning
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'release-common.ps1')

# ── Test identity (the only identity this script may act on) ─────────────────
$TestProductName = 'Speechek Test'
$TestIdentifier = 'app.speechek.test'
$TestManufacturer = 'girte'
$TestMainBinary = 'speechek'
$TestFakeKey = 'speechek-test-key'
$TestTranscript = 'Installer smoke transcript'

if ($TestProductName -eq 'Speechek' -or $TestIdentifier -eq 'app.speechek.desktop') {
    throw 'refusing to run: the smoke must target the test identity, not production.'
}

$repoRoot = Get-SpeechekRepoRoot $PSScriptRoot
$tauriDir = Join-Path $repoRoot 'src-tauri'
$outDir = Join-Path $tauriDir 'target\debug'
$debugExe = Join-Path $outDir ($TestMainBinary + '.exe')
$privateDir = Join-Path $repoRoot '.private\smoke'
$ownershipPath = Join-Path $privateDir 'test-ownership.json'
$fixtureServerJs = Join-Path $privateDir 'fixture-provider.mjs'
$liveProbeJs = Join-Path $privateDir 'live-probe.mjs'
$englishLang = Join-Path $tauriDir 'nsis\languages\English.nsh'
$russianLang = Join-Path $tauriDir 'nsis\languages\Russian.nsh'

$InstallDir = Join-Path $env:LOCALAPPDATA $TestProductName
$ProfileDir = Join-Path $env:APPDATA 'Speechek-Test'
$WebViewLocal = Join-Path $env:LOCALAPPDATA $TestIdentifier
$WebViewRoaming = Join-Path $env:APPDATA $TestIdentifier
$StartMenuLnk = Join-Path ([Environment]::GetFolderPath('Programs')) ($TestProductName + '.lnk')
$DesktopLnk = Join-Path ([Environment]::GetFolderPath('Desktop')) ($TestProductName + '.lnk')
$UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\' + $TestProductName
$ManuProductKey = 'HKCU:\Software\' + $TestManufacturer + '\' + $TestProductName
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$StartupApprovedKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
# 12-byte REG_BINARY blob Windows writes when a startup entry is disabled.
$SmokeStartupDisabled = [byte[]](0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00)

$script:StartedProcessIds = New-Object System.Collections.ArrayList
$script:StartedProcesses = New-Object System.Collections.ArrayList
$script:FixtureServer = $null
$script:Notes = New-Object System.Collections.ArrayList

function Add-Note {
    param([string]$Message)
    [void]$script:Notes.Add($Message)
    Write-Host "note: $Message"
}

function Get-SmokeLanguageId {
    if ($Language -eq 'ru') { return 1049 }
    return 1033
}

function Get-SmokeLabels {
    param([string]$ForLanguage = $Language)
    if ($ForLanguage -eq 'ru') {
        return @{
            Forward        = @('Далее >', 'Установить', 'Готово', 'Удалить', 'Закрыть')
            Back           = 'Назад'
            Cancel         = 'Отмена'
            Ok             = 'ОК'
            Agree          = @('Я принимаю', 'Принимаю', 'I Agree')
            Desktop        = 'Создать ярлык на рабочем столе'
            Startup        = 'Запускать Speechek при входе в Windows'
            Run            = 'Запустить Speechek'
            DeleteData     = 'Удалить настройки и API-ключи'
            CloseWarning   = 'Speechek будет закрыт.'
            FileInUse      = 'используется другим процессом'
            RepairRequired = 'Требуется ручная диагностика'
            DowngradeBlocked = 'не может её заменить'
            RollbackDone   = 'прежняя версия восстановлена'
            PrecheckReinstall = 'Будет выполнена переустановка на месте'
            LanguageTitles = @('Выберите язык установки', 'Select Setup Language')
        }
    }
    return @{
        Forward        = @('Next >', 'Install', 'Finish', 'Uninstall', 'Close')
        Back           = 'Back'
        Cancel         = 'Cancel'
        Ok             = 'OK'
        Agree          = @('I Agree', 'Принимаю')
        Desktop        = 'Create a desktop shortcut'
        Startup        = 'Start Speechek when I sign in to Windows'
        Run            = 'Run Speechek'
        DeleteData     = 'Delete settings and API keys'
        CloseWarning   = 'Speechek will be closed.'
        FileInUse      = 'is in use by another process'
        RepairRequired = 'Manual diagnosis is required'
        DowngradeBlocked = 'cannot replace it'
        RollbackDone   = 'previous version was restored'
        PrecheckReinstall = 'reinstalled in place'
        LanguageTitles = @('Select a Setup Language', 'Select Setup Language', 'Выберите язык установки')
    }
}

# The installer selects its UI language from the MUI language registry during
# .onInit; accept the expected message in either shipped language so the check
# does not depend on the machine's locale default.
function Test-SmokeMessageContains {
    param([Parameter(Mandatory = $true)][string]$Message, [Parameter(Mandatory = $true)][string]$Key)
    foreach ($language in @('en', 'ru')) {
        $fragment = [string](Get-SmokeLabels -ForLanguage $language).$Key
        if ($Message.Contains($fragment)) { return $true }
    }
    return $false
}

# MUI2 navigation buttons expose the keyboard accelerator in their caption
# ("&Next >", "&Back", "&Install", "&Finish", "&Uninstall"); the label table
# stores the plain display text, so the accelerator is stripped before the
# comparison. Checkbox captions carry no accelerator and are unaffected.
function Get-SmokeControlText {
    param([Parameter(Mandatory = $true)][IntPtr]$Hwnd)
    return ([SmokeWin32]::TextOf($Hwnd)).Replace('&', '')
}

# ── Win32 automation for the real installer GUI ─────────────────────────────
function Initialize-SmokeWin32 {
    if ('SmokeWin32' -as [type]) { return }
    $source = @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class SmokeWin32
{
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr hWndParent, EnumWindowsProc lpEnumFunc, IntPtr lParam);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsWindowEnabled(IntPtr hWnd);
    // Cross-process window messages are sent with a bounded timeout so a hung
    // installer UI thread can never block the driver forever.
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint msg, IntPtr wParam, StringBuilder lParam, uint flags, uint timeout, out IntPtr result);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam, uint flags, uint timeout, out IntPtr result);
    [DllImport("user32.dll")] public static extern IntPtr GetParent(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr hWnd, int index);

    public const uint BM_SETCHECK = 0x00F1;
    public const uint BM_GETCHECK = 0x00F0;
    public const uint WM_GETTEXT = 0x000D;
    public const uint WM_COMMAND = 0x0111;
    public const int BST_UNCHECKED = 0;
    public const int BST_CHECKED = 1;
    public const uint SMTO_BLOCK = 0x0001;
    public const uint SMTO_ABORTIFHUNG = 0x0002;
    public const uint TextTimeoutMs = 2000;
    public const uint ClickTimeoutMs = 5000;
    public const int GWL_STYLE = -16;
    public const int BS_TYPEMASK = 0x0000000F;
    public const int BS_CHECKBOX = 2;
    public const int BS_AUTOCHECKBOX = 3;
    public const int BS_3STATE = 5;
    public const int BS_AUTO3STATE = 6;

    // Reading text from a control in another process must not hang either, so
    // WM_GETTEXT is sent with a bounded timeout instead of GetWindowText.
    public static string TextOf(IntPtr hWnd)
    {
        StringBuilder builder = new StringBuilder(2048);
        IntPtr result;
        IntPtr ok = SendMessageTimeout(hWnd, WM_GETTEXT, new IntPtr(builder.Capacity), builder,
            SMTO_ABORTIFHUNG | SMTO_BLOCK, TextTimeoutMs, out result);
        return ok == IntPtr.Zero ? string.Empty : builder.ToString();
    }

    public static string ClassOf(IntPtr hWnd)
    {
        StringBuilder builder = new StringBuilder(256);
        GetClassName(hWnd, builder, builder.Capacity);
        return builder.ToString();
    }

    public static int ControlId(IntPtr hWnd)
    {
        return GetDlgCtrlID(hWnd);
    }

    public static bool IsCheckable(IntPtr hWnd)
    {
        int style = GetWindowLong(hWnd, GWL_STYLE) & BS_TYPEMASK;
        return style == BS_CHECKBOX || style == BS_AUTOCHECKBOX || style == BS_3STATE || style == BS_AUTO3STATE;
    }

    public static List<IntPtr> TopWindows(uint pid)
    {
        List<IntPtr> result = new List<IntPtr>();
        EnumWindows(delegate(IntPtr hWnd, IntPtr lParam)
        {
            uint windowPid;
            GetWindowThreadProcessId(hWnd, out windowPid);
            if (windowPid == pid && IsWindowVisible(hWnd)) { result.Add(hWnd); }
            return true;
        }, IntPtr.Zero);
        return result;
    }

    public static List<IntPtr> Children(IntPtr parent)
    {
        List<IntPtr> result = new List<IntPtr>();
        EnumChildWindows(parent, delegate(IntPtr hWnd, IntPtr lParam)
        {
            result.Add(hWnd);
            return true;
        }, IntPtr.Zero);
        return result;
    }

    // Activating a wizard button means posting the WM_COMMAND notification the
    // parent expects. A synthetic BM_CLICK advances the built-in MUI pages but
    // not a custom nsDialogs page; WM_COMMAND advances every page and message
    // box, so it is used for all controls.
    public static bool ClickControl(IntPtr hWnd)
    {
        int id = GetDlgCtrlID(hWnd);
        IntPtr result;
        return SendMessageTimeout(GetParent(hWnd), WM_COMMAND, new IntPtr(id & 0xFFFF), hWnd,
            SMTO_ABORTIFHUNG | SMTO_BLOCK, ClickTimeoutMs, out result) != IntPtr.Zero;
    }

    // Sets a check box to the exact requested state (no toggle), so repeated
    // polling cannot flip a user's choice back and forth.
    public static void SetChecked(IntPtr hWnd, bool state)
    {
        IntPtr result;
        SendMessageTimeout(hWnd, BM_SETCHECK, new IntPtr(state ? BST_CHECKED : BST_UNCHECKED), IntPtr.Zero,
            SMTO_ABORTIFHUNG | SMTO_BLOCK, TextTimeoutMs, out result);
    }

    public static bool GetChecked(IntPtr hWnd)
    {
        IntPtr result;
        if (SendMessageTimeout(hWnd, BM_GETCHECK, IntPtr.Zero, IntPtr.Zero,
            SMTO_ABORTIFHUNG | SMTO_BLOCK, TextTimeoutMs, out result) == IntPtr.Zero) { return false; }
        return result.ToInt64() != 0;
    }

    // ── Suspended child creation for the force-close scenario ───────────────
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct STARTUPINFO
    {
        public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
        public int dwX; public int dwY; public int dwXSize; public int dwYSize;
        public int dwXCountChars; public int dwYCountChars; public int dwFillAttribute;
        public int dwFlags; public short wShowWindow; public short cbReserved2;
        public IntPtr lpReserved2; public IntPtr hStdInput; public IntPtr hStdOutput; public IntPtr hStdError;
    }
    [StructLayout(LayoutKind.Sequential)]
    public struct PROCESS_INFORMATION { public IntPtr hProcess; public IntPtr hThread; public int dwProcessId; public int dwThreadId; }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern bool CreateProcess(string lpApplicationName, string lpCommandLine, IntPtr lpProcessAttributes, IntPtr lpThreadAttributes, bool bInheritHandles, uint dwCreationFlags, IntPtr lpEnvironment, string lpCurrentDirectory, ref STARTUPINFO lpStartupInfo, out PROCESS_INFORMATION lpProcessInformation);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool CloseHandle(IntPtr handle);

    // CREATE_SUSPENDED: the process never runs, so it cannot service the quit
    // event and the installer must reach its shared grace/force path. The
    // primary-thread handle is closed immediately (a suspended process stays
    // suspended without it); the caller keeps only the PID and uses a fresh
    // Process handle for waiting, so no kernel handle is leaked.
    public static int StartSuspended(string application, string workingDirectory)
    {
        STARTUPINFO si = new STARTUPINFO();
        si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
        PROCESS_INFORMATION pi;
        if (!CreateProcess(application, null, IntPtr.Zero, IntPtr.Zero, false, 0x00000004u, IntPtr.Zero, workingDirectory, ref si, out pi))
        {
            throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        return pi.dwProcessId;
    }
}
'@
    Add-Type -TypeDefinition $source -Language CSharp
}

# ── HTTP / WAV / secrets helpers ────────────────────────────────────────────
function Invoke-SmokeHttp {
    param(
        [Parameter(Mandatory = $true)][string]$Method,
        [Parameter(Mandatory = $true)][string]$Url,
        [byte[]]$Body = $null,
        [string]$ContentType = $null,
        [int]$TimeoutSec = 15
    )
    if (-not ('System.Net.Http.HttpClient' -as [type])) {
        # Windows PowerShell 5.1 does not load System.Net.Http by default.
        Add-Type -AssemblyName System.Net.Http | Out-Null
    }
    $client = [System.Net.Http.HttpClient]::new()
    $client.Timeout = [TimeSpan]::FromSeconds($TimeoutSec)
    try {
        $request = [System.Net.Http.HttpRequestMessage]::new([System.Net.Http.HttpMethod]::new($Method), $Url)
        if ($null -ne $Body) {
            $request.Content = [System.Net.Http.ByteArrayContent]::new([byte[]]$Body)
            if ($ContentType) {
                $request.Content.Headers.ContentType = [System.Net.Http.Headers.MediaTypeHeaderValue]::new($ContentType)
            }
        }
        $response = $client.SendAsync($request).GetAwaiter().GetResult()
        $text = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
        return [pscustomobject]@{ Status = [int]$response.StatusCode; Body = $text }
    }
    finally {
        $client.Dispose()
    }
}

function New-SmokeWav {
    param(
        [int]$SampleRate = 16000,
        [int]$Milliseconds = 500
    )
    $samples = [int]($SampleRate * $Milliseconds / 1000)
    $dataBytes = $samples * 2
    $stream = [System.IO.MemoryStream]::new()
    $writer = [System.IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([char[]]'RIFF')
        $writer.Write([int](36 + $dataBytes))
        $writer.Write([char[]]'WAVE')
        $writer.Write([char[]]'fmt ')
        $writer.Write([int]16)
        $writer.Write([int16]1)
        $writer.Write([int16]1)
        $writer.Write([int]$SampleRate)
        $writer.Write([int]($SampleRate * 2))
        $writer.Write([int16]2)
        $writer.Write([int16]16)
        $writer.Write([char[]]'data')
        $writer.Write([int]$dataBytes)
        # A quiet 440 Hz tone: synthetic signal, never a microphone recording.
        for ($i = 0; $i -lt $samples; $i++) {
            $value = [int16]([Math]::Sin(2 * [Math]::PI * 440 * $i / $SampleRate) * 2000)
            $writer.Write($value)
        }
    }
    finally {
        $writer.Dispose()
        $stream.Dispose()
    }
    return $stream.ToArray()
}

function New-SmokeSecretContainer {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Key
    )
    Add-Type -AssemblyName System.Security
    $json = '{"version":1,"keys":["' + $Key + '"]}'
    $plain = [System.Text.Encoding]::UTF8.GetBytes($json)
    $blob = [System.Security.Cryptography.ProtectedData]::Protect(
        $plain, $null, [System.Security.Cryptography.DataProtectionScope]::CurrentUser)
    $container = [byte[]]::new(4 + $blob.Length)
    [System.Text.Encoding]::ASCII.GetBytes('SPK1').CopyTo($container, 0)
    $blob.CopyTo($container, 4)
    [System.IO.File]::WriteAllBytes($Path, $container)
}

function Write-SmokeSettings {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][int]$ListenPort,
        [string]$Hotkey = 'Ctrl+Shift+F10',
        [string]$Mode = 'live'
    )
    $document = [ordered]@{
        hotkey                = $Hotkey
        mode                  = $Mode
        mute_during_recording = $false
        port                  = $ListenPort
        input_device          = $null
    }
    Write-SpeechekUtf8NoBom -Path $Path -Text ((ConvertTo-Json -InputObject $document -Compress) + "`n")
    New-SmokeSecretContainer -Path (Join-Path (Split-Path -Parent $Path) 'secrets.bin') -Key $TestFakeKey
}

# ── Fixture server (embedded, written to the ignored .private/smoke area) ───
function Write-SmokeFixtureScripts {
    $server = @'
const port = Number(process.argv[2]);
const TEXT = "Installer smoke transcript";
const server = Bun.serve({
  port,
  hostname: "127.0.0.1",
  fetch(req, srv) {
    const url = new URL(req.url);
    if (url.pathname.startsWith("/ws/")) {
      if (srv.upgrade(req)) return undefined;
      return new Response("upgrade failed", { status: 400 });
    }
    if (req.method === "GET" && url.pathname === "/v1beta/models") {
      return Response.json({ models: [{ name: "models/gemini-3.5-transcribe" }], nextPageToken: "" });
    }
    if (req.method === "POST" && url.pathname === "/upload/v1beta/files") {
      return new Response("", {
        status: 200,
        headers: { "x-goog-upload-url": `http://127.0.0.1:${port}/upload/v1beta/files/smoke-file` },
      });
    }
    if (req.method === "POST" && url.pathname === "/upload/v1beta/files/smoke-file") {
      return Response.json({
        file: { name: "files/smoke-file", uri: `http://127.0.0.1:${port}/v1beta/files/smoke-file`, state: "ACTIVE" },
      });
    }
    if (req.method === "POST" && url.pathname === "/v1beta/interactions") {
      return Response.json({ output_text: TEXT, status: "completed" });
    }
    if (req.method === "DELETE" && url.pathname.startsWith("/v1beta/files/")) {
      return new Response(null, { status: 204 });
    }
    return new Response("not found", { status: 404 });
  },
  websocket: {
    message(ws, message) {
      const text = typeof message === "string" ? message : message.toString();
      if (text.indexOf("\"setup\"") !== -1) { ws.send(JSON.stringify({ setupComplete: {} })); return; }
      if (text.indexOf("activityEnd") !== -1) {
        ws.send(JSON.stringify({ serverContent: { inputTranscription: { text: TEXT }, turnComplete: true } }));
      }
    },
  },
});
console.log("SPEECHEK_FIXTURE_READY " + server.port);
'@
    Write-SpeechekUtf8NoBom -Path $fixtureServerJs -Text $server

    $probe = @'
const url = process.argv[2];
const protocol = process.argv[3] || "browser";
let finalText = null;
let settled = false;
const finish = (code) => {
  console.log(JSON.stringify({ finalText, done: code === 0 }));
  process.exit(code);
};
const timer = setTimeout(() => {
  console.log(JSON.stringify({ error: "timeout", finalText }));
  process.exit(1);
}, 30000);
const ws = new WebSocket(url);
const audio = Buffer.alloc(3200).toString("base64");

if (protocol === "gemini") {
  // The fixture provider itself: speak the Gemini Live wire protocol.
  ws.onopen = () => {
    ws.send(JSON.stringify({ setup: { model: "models/gemini-3.5-transcribe" } }));
    setTimeout(() => ws.send(JSON.stringify({ realtimeInput: { audio: { data: audio, mimeType: "audio/pcm;rate=16000" } } })), 100);
    setTimeout(() => ws.send(JSON.stringify({ realtimeInput: { activityEnd: {} } })), 300);
  };
  ws.onmessage = (event) => {
    let message;
    try { message = JSON.parse(event.data); } catch { return; }
    if (message.serverContent && message.serverContent.inputTranscription) {
      finalText = message.serverContent.inputTranscription.text;
      settled = true; clearTimeout(timer);
      finish(0);
    }
    if (message.error) {
      settled = true; clearTimeout(timer);
      console.log(JSON.stringify({ error: (message.error && message.error.message) || "error", finalText }));
      process.exit(1);
    }
  };
} else {
  // The application's /api/live relay: speak the browser-side protocol.
  ws.onopen = () => {
    ws.send(JSON.stringify({ type: "start", mode: "smart" }));
    ws.send(JSON.stringify({ type: "audio", data: audio }));
    setTimeout(() => ws.send(JSON.stringify({ type: "end" })), 200);
  };
  ws.onmessage = (event) => {
    let message;
    try { message = JSON.parse(event.data); } catch { return; }
    if (message.type === "final") { finalText = message.text; }
    if (message.type === "done") { settled = true; clearTimeout(timer); finish(0); }
    if (message.type === "error") {
      settled = true; clearTimeout(timer);
      console.log(JSON.stringify({ error: message.message, finalText }));
      process.exit(1);
    }
  };
}
ws.onerror = () => {
  if (settled) return;
  settled = true; clearTimeout(timer);
  console.log(JSON.stringify({ error: "socket error", finalText }));
  process.exit(1);
};
'@
    Write-SpeechekUtf8NoBom -Path $liveProbeJs -Text $probe
}

function Get-SmokeFreePort {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    try { return ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port }
    finally { $listener.Stop() }
}

function Start-SmokeFixtureServer {
    param([Parameter(Mandatory = $true)][int]$ListenPort)
    $bunExe = Resolve-SpeechekBun $repoRoot
    $psi = New-SpeechekProcessStartInfo -FilePath $bunExe -Arguments @($fixtureServerJs, [string]$ListenPort) `
        -WorkingDirectory $privateDir -CaptureOutput
    $process = [System.Diagnostics.Process]::Start($psi)
    $script:FixtureServer = $process
    [void]$script:StartedProcessIds.Add($process.Id)
    $base = 'http://127.0.0.1:' + $ListenPort
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    $lastProbeError = ''
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($process.HasExited) {
            $errorText = $process.StandardError.ReadToEnd()
            throw "the fixture provider exited early (code $($process.ExitCode)): $errorText"
        }
        try {
            $probe = Invoke-SmokeHttp -Method 'GET' -Url ($base + '/v1beta/models') -TimeoutSec 2
            if ($probe.Status -eq 200) { $script:FixtureServer = $process; return $base }
        }
        catch {
            $lastProbeError = $_.Exception.Message
            Start-Sleep -Milliseconds 200
        }
    }
    throw "the fixture provider did not answer on $base within 15 s (last probe error: $lastProbeError)."
}

function Invoke-SmokeLiveProbe {
    param(
        [Parameter(Mandatory = $true)][string]$WsUrl,
        [ValidateSet('browser', 'gemini')][string]$Protocol = 'browser',
        [int]$TimeoutSec = 40
    )
    $bunExe = Resolve-SpeechekBun $repoRoot
    $result = Get-SpeechekProcessResult -FilePath $bunExe -Arguments @($liveProbeJs, $WsUrl, $Protocol) `
        -WorkingDirectory $privateDir
    if ($result.ExitCode -ne 0) {
        throw "the Live WebSocket probe failed: $($result.StandardError) $($result.StandardOutput)"
    }
    return ($result.StandardOutput | ConvertFrom-Json)
}

# ── Ownership gate ──────────────────────────────────────────────────────────
function Get-SmokeArtifacts {
    $artifacts = New-Object System.Collections.ArrayList
    foreach ($dir in @($InstallDir, $ProfileDir, $WebViewLocal, $WebViewRoaming)) {
        if (Test-Path -LiteralPath $dir) { [void]$artifacts.Add(('dir: ' + $dir)) }
    }
    foreach ($link in @($StartMenuLnk, $DesktopLnk)) {
        if (Test-Path -LiteralPath $link) { [void]$artifacts.Add(('link: ' + $link)) }
    }
    foreach ($key in @($UninstallKey, $ManuProductKey)) {
        if (Test-Path -LiteralPath $key) { [void]$artifacts.Add(('reg: ' + $key)) }
    }
    try {
        $runValue = Get-ItemProperty -LiteralPath $RunKey -Name $TestProductName -ErrorAction SilentlyContinue
        if ($null -ne $runValue) { [void]$artifacts.Add('run: ' + $TestProductName) }
    }
    catch { }
    return ,$artifacts.ToArray()
}

function Read-SmokeOwnership {
    if (-not (Test-Path -LiteralPath $ownershipPath -PathType Leaf)) { return $null }
    try {
        return (Get-Content -LiteralPath $ownershipPath -Raw | ConvertFrom-Json)
    }
    catch {
        return $null
    }
}

function Get-SmokeOwnershipDecision {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][object[]]$Artifacts,
        [object]$Ownership
    )
    if ($Artifacts.Count -eq 0) { return 'fresh' }
    if ($null -eq $Ownership) { return 'foreign' }
    if ([string]$Ownership.productName -ne $TestProductName) { return 'foreign' }
    if ([string]$Ownership.identifier -ne $TestIdentifier) { return 'foreign' }
    if ([string]$Ownership.installDir -ne $InstallDir) { return 'foreign' }
    if ([string]::IsNullOrWhiteSpace([string]$Ownership.sessionId)) { return 'foreign' }
    return 'owned'
}

function Assert-SmokeOwnership {
    $artifacts = Get-SmokeArtifacts
    $decision = Get-SmokeOwnershipDecision -Artifacts $artifacts -Ownership (Read-SmokeOwnership)
    if ($decision -eq 'foreign') {
        throw ("refusing destructive smoke: a Speechek Test installation/profile already exists without " +
               "this script's ownership manifest. Existing: $($artifacts -join '; '). " +
               "Inspect or repair it manually (current installer) or remove the test artifacts by hand; " +
               "this smoke never deletes a foreign Test profile.")
    }
    return $decision
}

function Write-SmokeOwnership {
    $record = [ordered]@{
        schemaVersion = 1
        sessionId     = [guid]::NewGuid().ToString()
        startedUtc    = [DateTime]::UtcNow.ToString('o')
        productName   = $TestProductName
        identifier    = $TestIdentifier
        installDir    = $InstallDir
        profileDir    = $ProfileDir
    }
    Write-SpeechekReleaseManifest -Path $ownershipPath -Manifest $record
}

function Assert-NoForeignTestInstance {
    $running = @()
    foreach ($process in @(Get-Process -Name $TestMainBinary -ErrorAction SilentlyContinue)) {
        try {
            $path = $process.MainModule.FileName
            if ($path -eq $debugExe -or $path -like (Join-Path $InstallDir '*')) {
                $running += ('pid ' + $process.Id + ' ' + $path)
            }
        }
        catch { }
    }
    if ($running.Count -gt 0) {
        throw ("a Speechek executable is already running ($($running -join '; ')). Close it and retry; " +
               "the smoke never stops a process it did not start.")
    }
}

# ── Process lifecycle (own PIDs only) ───────────────────────────────────────
function Get-SmokeAllowedRoots {
    return @($outDir, $InstallDir, (Join-Path $outDir 'bundle\nsis'), $env:TEMP)
}

function Test-SmokePathAllowed {
    param([string]$Path, [string[]]$Roots)
    if ([string]::IsNullOrWhiteSpace($Path)) { return $false }
    foreach ($root in $Roots) {
        if ($Path.StartsWith($root.TrimEnd('\') + '\', [System.StringComparison]::OrdinalIgnoreCase)) { return $true }
    }
    return $false
}

function Stop-SmokeProcesses {
    $roots = Get-SmokeAllowedRoots
    foreach ($process in @($script:StartedProcesses)) {
        if ($null -eq $process) { continue }
        if ($process.HasExited) { continue }
        $path = $null
        try { $path = $process.MainModule.FileName } catch { }
        if (Test-SmokePathAllowed -Path $path -Roots $roots) {
            try { $process.Kill(); $process.WaitForExit(5000) | Out-Null } catch { }
        }
        else {
            Write-Warning "not stopping pid $($process.Id): image path '$path' is outside the allowed test roots."
        }
    }
}

function Start-SmokeTestApp {
    param(
        [Parameter(Mandatory = $true)][string]$ApplicationPath,
        [Parameter(Mandatory = $true)][string]$ConfigPath,
        [Parameter(Mandatory = $true)][string]$HttpBase,
        [Parameter(Mandatory = $true)][string]$WsBase
    )
    $environment = New-SpeechekEnvironment @{
        SPEECHEK_CONFIG_PATH        = $ConfigPath
        SPEECHEK_TEST_PROVIDER_HTTP = $HttpBase
        SPEECHEK_TEST_PROVIDER_WSS  = $WsBase
    }
    $psi = New-SpeechekProcessStartInfo -FilePath $ApplicationPath -WorkingDirectory (Split-Path -Parent $ApplicationPath) -Environment $environment
    $process = [System.Diagnostics.Process]::Start($psi)
    [void]$script:StartedProcessIds.Add($process.Id)
    [void]$script:StartedProcesses.Add($process)
    return $process
}

function Wait-SmokeHealth {
    param(
        [Parameter(Mandatory = $true)][string]$HttpBase,
        [Parameter(Mandatory = $true)][System.Diagnostics.Process]$Process,
        [int]$TimeoutSec = 30
    )
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSec)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($Process.HasExited) {
            throw ('the Test executable exited early (code ' + $Process.ExitCode + '); a pre-existing Test ' +
                   'singleton may still hold the identity.')
        }
        try {
            $probe = Invoke-SmokeHttp -Method 'GET' -Url ($HttpBase + '/api/health') -TimeoutSec 2
            if ($probe.Status -eq 200) { return }
        }
        catch { Start-Sleep -Milliseconds 250 }
    }
    throw "the Test backend did not answer on $HttpBase within $TimeoutSec s."
}

# ── Fixture manifest ────────────────────────────────────────────────────────
function Read-SmokeFixture {
    param([Parameter(Mandatory = $true)][string]$FixturePath)
    if (-not (Test-Path -LiteralPath $FixturePath -PathType Leaf)) {
        throw "fixture manifest not found: $FixturePath (run scripts/build-test-installer.ps1 -Version X.Y.Z)."
    }
    $manifest = Get-Content -LiteralPath $FixturePath -Raw | ConvertFrom-Json
    if ([string]$manifest.productName -ne $TestProductName -or [string]$manifest.identifier -ne $TestIdentifier) {
        throw "fixture manifest $FixturePath is not a Speechek Test fixture; refusing to install it."
    }
    $installerPath = Join-Path (Join-Path $outDir 'bundle\nsis') ([string]$manifest.installer.file)
    if (-not (Test-Path -LiteralPath $installerPath -PathType Leaf)) {
        throw "fixture installer is missing: $installerPath"
    }
    $actualSha = Get-SpeechekSha256 $installerPath
    if ($actualSha -ne [string]$manifest.installer.sha256) {
        throw "fixture installer checksum mismatch for $installerPath (manifest and file disagree)."
    }
    return [pscustomobject]@{
        Manifest      = $manifest
        InstallerPath = $installerPath
        Version       = [string]$manifest.version
    }
}

function Get-InstalledTestVersion {
    try {
        $value = Get-ItemProperty -LiteralPath $UninstallKey -Name 'DisplayVersion' -ErrorAction SilentlyContinue
        if ($null -eq $value) { return $null }
        return [string]$value.DisplayVersion
    }
    catch { return $null }
}

# ── NSIS GUI driver (real Win32 controls) ───────────────────────────────────
function Invoke-NsisWizard {
    param(
        [Parameter(Mandatory = $true)][System.Diagnostics.Process]$Process,
        [Parameter(Mandatory = $true)][hashtable]$Options,
        [switch]$ExpectFatal,
        [hashtable]$State,
        [System.Diagnostics.Process]$WatchProcess,
        [int]$DriverTimeoutSec = 300
    )
    Initialize-SmokeWin32
    $labels = Get-SmokeLabels
    $cancelledWarning = $false
    $deadline = [DateTime]::UtcNow.AddSeconds($DriverTimeoutSec)
    $timedOut = $true
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($Process.HasExited) { $timedOut = $false; break }
        if ($null -ne $WatchProcess -and $null -ne $State -and $WatchProcess.HasExited `
            -and -not $State.ContainsKey('WatchedProcessExitUtc')) {
            $State['WatchedProcessExitUtc'] = [DateTime]::UtcNow
        }
        foreach ($window in @([SmokeWin32]::TopWindows([uint32]$Process.Id))) {
            if ([SmokeWin32]::ClassOf($window) -ne '#32770') { continue }
            $title = [SmokeWin32]::TextOf($window)
            if ($labels.LanguageTitles -contains $title) {
                Add-Note 'the installer language dialog appeared; it is driven manually (the smoke pre-seeds the installer language).'
                continue
            }
            $controls = @([SmokeWin32]::Children($window))
            $buttons = @($controls | Where-Object { [SmokeWin32]::ClassOf($_) -eq 'Button' })
            $texts = @($controls | Where-Object { [SmokeWin32]::ClassOf($_) -eq 'Static' } |
                ForEach-Object { [SmokeWin32]::TextOf($_) } |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) })

            $hasForward = $false
            $hasCancel = $false
            foreach ($button in $buttons) {
                $buttonText = Get-SmokeControlText $button
                if ($labels.Forward -contains $buttonText) { $hasForward = $true }
                if ($buttonText -eq $labels.Cancel) { $hasCancel = $true }
            }

            # The shared close warning is a MessageBox carrying the known sentence.
            $isWarning = $false
            foreach ($text in $texts) { if ($text.Contains($labels.CloseWarning)) { $isWarning = $true } }
            if ($isWarning) {
                $confirm = $false
                if ($Options.ContainsKey('ConfirmClose')) { $confirm = [bool]$Options['ConfirmClose'] }
                # MessageBox button captions are localised by the OS, not by the
                # installer language, but the control IDs are stable:
                # IDOK=1, IDCANCEL=2. Match by ID, else fall back to the caption.
                $target = if ($confirm) { $labels.Ok } else { $labels.Cancel }
                $targetId = if ($confirm) { 1 } else { 2 }
                $clicked = $false
                foreach ($button in $buttons) {
                    if ([SmokeWin32]::ControlId($button) -eq $targetId) { [void][SmokeWin32]::ClickControl($button); $clicked = $true; break }
                }
                if (-not $clicked) {
                    foreach ($button in $buttons) {
                        if ((Get-SmokeControlText $button) -eq $target) { [void][SmokeWin32]::ClickControl($button); break }
                    }
                }
                if (-not $confirm) {
                    # Keep driving: the installer shows a follow-up
                    # "installation cancelled" box and then aborts, so the
                    # process only really ends once that box is dismissed.
                    $cancelledWarning = $true
                }
                if ($confirm -and $null -ne $State) { $State['CloseWarningConfirmedUtc'] = [DateTime]::UtcNow }
                Start-Sleep -Milliseconds 300
                continue
            }

            # A message box has no wizard button at all (error/refusal/information).
            if (-not $hasForward -and -not $hasCancel) {
                $message = ''
                foreach ($text in $texts) { if ($text -ne $title) { $message = $text; break } }
                # Dismiss by control ID (captions are OS-localised): IDOK=1,
                # IDCANCEL=2, otherwise the sole enabled button.
                $dismissed = $false
                foreach ($id in @(1, 2)) {
                    foreach ($button in $buttons) {
                        if ([SmokeWin32]::ControlId($button) -eq $id) { [void][SmokeWin32]::ClickControl($button); $dismissed = $true; break }
                    }
                    if ($dismissed) { break }
                }
                if (-not $dismissed) {
                    foreach ($button in $buttons) {
                        if ([SmokeWin32]::IsWindowEnabled($button)) { [void][SmokeWin32]::ClickControl($button); break }
                    }
                }
                if ($cancelledWarning) { Start-Sleep -Milliseconds 300; continue }
                return [pscustomobject]@{ Cancelled = $false; Fatal = $message; TimedOut = $false }
            }

            # License page (any page whose forward button is disabled): the
            # accept control is a default push button rendering "I &Agree" (not
            # a checkbox), so click the one enabled button that is not the
            # Back/Cancel/forward/OK navigation button. The MUI Back caption is
            # "< &Back", hence the suffix match.
            $enabledForward = @($buttons | Where-Object {
                $labels.Forward -contains (Get-SmokeControlText $_) -and [SmokeWin32]::IsWindowEnabled($_)
            })
            $enabledBack = @($buttons | Where-Object {
                (Get-SmokeControlText $_).EndsWith($labels.Back, [System.StringComparison]::Ordinal) -and [SmokeWin32]::IsWindowEnabled($_)
            })
            $accepted = $false
            if ($enabledForward.Count -eq 0 -and $enabledBack.Count -gt 0) {
                foreach ($button in $buttons) {
                    if (-not [SmokeWin32]::IsWindowEnabled($button)) { continue }
                    $navText = Get-SmokeControlText $button
                    if ($navText -eq $labels.Cancel) { continue }
                    if ($navText -eq $labels.Ok) { continue }
                    if ($labels.Forward -contains $navText) { continue }
                    if ($navText.EndsWith($labels.Back, [System.StringComparison]::Ordinal)) { continue }
                    [void][SmokeWin32]::ClickControl($button); $accepted = $true; break
                }
            }

            # After the accept click the page has changed, so the cached child
            # handles may already belong to the next page's controls; stop
            # touching this window and re-enumerate on the next iteration.
            if (-not $accepted) {
                # Checkbox options: only the known labels are touched; a box
                # whose option is not in the table is left exactly as it is.
                foreach ($box in $buttons) {
                    if (-not [SmokeWin32]::IsCheckable($box)) { continue }
                    $boxText = Get-SmokeControlText $box
                    $wanted = $null
                    if ($boxText -eq $labels.Desktop -and $Options.ContainsKey('Desktop')) { $wanted = [bool]$Options['Desktop'] }
                    elseif ($boxText -eq $labels.Startup -and $Options.ContainsKey('Startup')) { $wanted = [bool]$Options['Startup'] }
                    elseif ($boxText -eq $labels.Run -and $Options.ContainsKey('Run')) { $wanted = [bool]$Options['Run'] }
                    elseif ($boxText -eq $labels.DeleteData -and $Options.ContainsKey('DeleteData')) { $wanted = [bool]$Options['DeleteData'] }
                    if ($null -ne $wanted -and ([SmokeWin32]::GetChecked($box)) -ne $wanted) {
                        [SmokeWin32]::SetChecked($box, $wanted)
                    }
                }

                $forward = $null
                foreach ($button in $buttons) {
                    if ($labels.Forward -contains (Get-SmokeControlText $button) -and [SmokeWin32]::IsWindowEnabled($button)) {
                        $forward = $button; break
                    }
                }
                if ($null -ne $forward) { [void][SmokeWin32]::ClickControl($forward) }
            }
        }
        Start-Sleep -Milliseconds 300
    }
    return [pscustomobject]@{ Cancelled = $cancelledWarning; Fatal = $null; TimedOut = $timedOut }
}

function Test-SmokeWindowPresent {
    param([int]$ProcessId)
    Initialize-SmokeWin32
    return (@([SmokeWin32]::TopWindows([uint32]$ProcessId)).Count -gt 0)
}

function Start-NsisInstaller {
    param([Parameter(Mandatory = $true)][string]$InstallerPath)
    $psi = New-SpeechekProcessStartInfo -FilePath $InstallerPath -WorkingDirectory (Split-Path -Parent $InstallerPath)
    $process = [System.Diagnostics.Process]::Start($psi)
    [void]$script:StartedProcessIds.Add($process.Id)
    [void]$script:StartedProcesses.Add($process)
    return $process
}

# The NSIS uninstaller copies itself to %TEMP% and relaunches as a child
# process (Un.exe), so the process we started usually owns no window. Prefer
# the started process; otherwise follow exactly one level of child processes
# (parent PID match) whose image lives under an allowed root, so a foreign
# process can never be selected.
function Resolve-SmokeUiProcess {
    param(
        [Parameter(Mandatory = $true)][System.Diagnostics.Process]$Root,
        [int]$TimeoutSec = 20
    )
    Initialize-SmokeWin32
    $roots = @(Get-SmokeAllowedRoots)
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSec)
    while ([DateTime]::UtcNow -lt $deadline) {
        $candidates = New-Object System.Collections.ArrayList
        [void]$candidates.Add($Root)
        try {
            foreach ($child in @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $($Root.Id)" -ErrorAction SilentlyContinue)) {
                $childProcess = Get-Process -Id ([int]$child.ProcessId) -ErrorAction SilentlyContinue
                if ($null -ne $childProcess) { [void]$candidates.Add($childProcess) }
            }
        }
        catch { }
        foreach ($candidate in $candidates) {
            if ($candidate.HasExited) { continue }
            if (-not (Test-SmokeWindowPresent $candidate.Id)) { continue }
            $path = $null
            try { $path = $candidate.MainModule.FileName } catch { }
            if (-not (Test-SmokePathAllowed -Path $path -Roots $roots)) { continue }
            return $candidate
        }
        Start-Sleep -Milliseconds 200
    }
    return $Root
}

function Set-SmokeInstallerLanguage {
    if (-not (Test-Path -LiteralPath $ManuProductKey)) {
        New-Item -Path $ManuProductKey -Force | Out-Null
    }
    Set-ItemProperty -LiteralPath $ManuProductKey -Name 'Installer Language' -Value ([string](Get-SmokeLanguageId))
}

function Invoke-SmokeInstall {
    param(
        [Parameter(Mandatory = $true)][string]$InstallerPath,
        [Parameter(Mandatory = $true)][hashtable]$Options,
        [switch]$ExpectFatal,
        [hashtable]$State,
        [System.Diagnostics.Process]$WatchProcess,
        [int]$DriverTimeoutSec = 300
    )
    Set-SmokeInstallerLanguage
    $root = Start-NsisInstaller -InstallerPath $InstallerPath
    $process = Resolve-SmokeUiProcess -Root $root
    if ($process.Id -ne $root.Id) {
        [void]$script:StartedProcessIds.Add($process.Id)
        [void]$script:StartedProcesses.Add($process)
    }
    $result = Invoke-NsisWizard -Process $process -Options $Options -ExpectFatal:$ExpectFatal -State $State -WatchProcess $WatchProcess -DriverTimeoutSec $DriverTimeoutSec
    if (-not $process.HasExited) {
        try { $process.WaitForExit(5000) | Out-Null } catch { }
    }
    return $result
}

# ── Scenarios ───────────────────────────────────────────────────────────────
function Invoke-GateSelfTest {
    Write-Host '-- GateSelfTest: ownership-decision table'
    $owned = [pscustomobject]@{
        productName = $TestProductName; identifier = $TestIdentifier
        installDir  = $InstallDir; sessionId = 'x'
    }
    $cases = @(
        @{ Name = 'fresh'; Artifacts = @(); Ownership = $null; Expect = 'fresh' },
        @{ Name = 'foreign artifacts, no manifest'; Artifacts = @('dir: x'); Ownership = $null; Expect = 'foreign' },
        @{ Name = 'foreign manifest product'; Artifacts = @('dir: x'); Ownership = [pscustomobject]@{ productName = 'Other'; identifier = $TestIdentifier; installDir = $InstallDir; sessionId = 'x' }; Expect = 'foreign' },
        @{ Name = 'foreign manifest identifier'; Artifacts = @('dir: x'); Ownership = [pscustomobject]@{ productName = $TestProductName; identifier = 'app.other'; installDir = $InstallDir; sessionId = 'x' }; Expect = 'foreign' },
        @{ Name = 'foreign manifest dir'; Artifacts = @('dir: x'); Ownership = [pscustomobject]@{ productName = $TestProductName; identifier = $TestIdentifier; installDir = 'C:\elsewhere'; sessionId = 'x' }; Expect = 'foreign' },
        @{ Name = 'owned'; Artifacts = @('dir: x'); Ownership = $owned; Expect = 'owned' }
    )
    foreach ($case in $cases) {
        $decision = Get-SmokeOwnershipDecision -Artifacts @($case.Artifacts) -Ownership $case.Ownership
        if ($decision -ne $case.Expect) {
            throw "ownership gate case '$($case.Name)' produced '$decision', expected '$($case.Expect)'."
        }
        Write-Host ("   ok: " + $case.Name + " -> " + $decision)
    }
}

function Invoke-FixtureSelfTest {
    param([Parameter(Mandatory = $true)][int]$ListenPort, [Parameter(Mandatory = $true)][string]$HttpBase)
    Write-Host '-- FixtureSelfTest: provider HTTP + WebSocket contract'
    $models = Invoke-SmokeHttp -Method 'GET' -Url ($HttpBase + '/v1beta/models')
    if ($models.Status -ne 200) { throw "models check returned $($models.Status)." }
    $start = Invoke-SmokeHttp -Method 'POST' -Url ($HttpBase + '/upload/v1beta/files') -Body ([byte[]]@(0)) -ContentType 'application/json'
    if ($start.Status -ne 200) { throw "upload start returned $($start.Status)." }
    $finalize = Invoke-SmokeHttp -Method 'POST' -Url ($HttpBase + '/upload/v1beta/files/smoke-file') -Body ([byte[]]@(1, 2, 3)) -ContentType 'audio/wav'
    if ($finalize.Status -ne 200 -or $finalize.Body -notmatch 'files/smoke-file') {
        throw "upload finalize did not return the file identity: $($finalize.Status) $($finalize.Body)"
    }
    $interaction = Invoke-SmokeHttp -Method 'POST' -Url ($HttpBase + '/v1beta/interactions') -Body ([byte[]]@(0)) -ContentType 'application/json'
    if ($interaction.Status -ne 200 -or $interaction.Body -notmatch [regex]::Escape($TestTranscript)) {
        throw "interaction did not return the transcript: $($interaction.Status) $($interaction.Body)"
    }
    $delete = Invoke-SmokeHttp -Method 'DELETE' -Url ($HttpBase + '/v1beta/files/smoke-file')
    if ($delete.Status -ne 204) { throw "upload delete returned $($delete.Status)." }
    $live = Invoke-SmokeLiveProbe -Protocol gemini -WsUrl ('ws://127.0.0.1:' + $ListenPort + '/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key=test')
    if ([string]$live.finalText -ne $TestTranscript -or -not [bool]$live.done) {
        throw "the fixture Live WebSocket did not return the transcript: $($live | ConvertTo-Json -Compress)"
    }
    Write-Host '   ok: HTTP model check, resumable upload, interaction and Live transcript contract'
}

function Invoke-FakeProvider {
    param([Parameter(Mandatory = $true)][string]$HttpBase, [Parameter(Mandatory = $true)][int]$ListenPort)
    Write-Host '-- FakeProvider: debug/Test executable end to end'
    if (-not (Test-Path -LiteralPath $debugExe -PathType Leaf)) {
        throw "the debug/Test executable is missing: $debugExe (run scripts/build-test-installer.ps1 first)."
    }
    Assert-NoForeignTestInstance
    $configDir = Join-Path $outDir 'fixture-runtime'
    $configPath = Join-Path $configDir 'settings.json'
    if (-not (Test-Path -LiteralPath $configDir)) { New-Item -ItemType Directory -Path $configDir -Force | Out-Null }
    $appPort = Get-SmokeFreePort
    while ($appPort -eq $ListenPort) { $appPort = Get-SmokeFreePort }
    $appBase = 'http://127.0.0.1:' + $appPort
    Write-SmokeSettings -Path $configPath -ListenPort $appPort
    $process = Start-SmokeTestApp -ApplicationPath $debugExe -ConfigPath $configPath `
        -HttpBase $HttpBase -WsBase ('ws://127.0.0.1:' + $ListenPort)
    try {
        Wait-SmokeHealth -HttpBase $appBase -Process $process
        $settings = Invoke-SmokeHttp -Method 'GET' -Url ($appBase + '/api/settings')
        if ($settings.Status -ne 200) { throw "/api/settings returned $($settings.Status)." }
        $wav = New-SmokeWav
        foreach ($mode in @('smart', 'verbatim')) {
            $result = Invoke-SmokeHttp -Method 'POST' -Url ($appBase + '/api/transcribe?mode=' + $mode) `
                -Body $wav -ContentType 'audio/wav' -TimeoutSec 60
            $payload = $result.Body | ConvertFrom-Json
            if ($result.Status -ne 200 -or [string]$payload.text -ne $TestTranscript) {
                throw "/api/transcribe?mode=$mode returned $($result.Status): $($result.Body)"
            }
        }
        $live = Invoke-SmokeLiveProbe -WsUrl ($appBase.Replace('http://', 'ws://') + '/api/live')
        if ([string]$live.finalText -ne $TestTranscript -or -not [bool]$live.done) {
            throw "the app Live relay did not return the transcript: $($live | ConvertTo-Json -Compress)"
        }
        if (-not (Test-Path -LiteralPath $WebViewLocal)) {
            Add-Note "the isolated WebView2 local data directory was not observed at $WebViewLocal."
        }
        Write-Host '   ok: /api/health, /api/settings, /api/transcribe smart+verbatim, /api/live'
    }
    finally {
        if (-not $KeepRunning) { Stop-SmokeProcesses }
    }
}

function Invoke-FreshInstall {
    param([Parameter(Mandatory = $true)][pscustomobject]$FixtureObject, [bool]$OptIn = $false)
    Write-Host "-- installer GUI: $(if ($OptIn) { 'OptIn' } else { 'FreshInstall' }) ($Language)"
    $options = @{ Desktop = $OptIn; Startup = $OptIn; Run = $false }
    $result = Invoke-SmokeInstall -InstallerPath $FixtureObject.InstallerPath -Options $options
    if ($result.TimedOut) { throw 'the installer GUI did not finish within the driver timeout.' }
    $installedExe = Join-Path $InstallDir ($TestMainBinary + '.exe')
    if (-not (Test-Path -LiteralPath $installedExe -PathType Leaf)) {
        throw "the installed executable is missing after the install: $installedExe"
    }
    if ((Get-InstalledTestVersion) -ne $FixtureObject.Version) {
        throw "installed DisplayVersion '$((Get-InstalledTestVersion))' is not the fixture version '$($FixtureObject.Version)'."
    }
    if (-not (Test-Path -LiteralPath $StartMenuLnk)) { throw "the Start Menu shortcut is missing: $StartMenuLnk" }
    $desktopPresent = Test-Path -LiteralPath $DesktopLnk
    $runValue = $null
    try { $runValue = (Get-ItemProperty -LiteralPath $RunKey -Name $TestProductName -ErrorAction SilentlyContinue) } catch { }
    if ($OptIn) {
        if (-not $desktopPresent) { throw 'the desktop shortcut was requested but is missing.' }
        if ($null -eq $runValue) { throw 'the autostart entry was requested but is missing.' }
    }
    else {
        if ($desktopPresent) { throw 'the desktop shortcut must not be created without opt-in.' }
        if ($null -ne $runValue) { throw 'the autostart entry must not be created without opt-in.' }
    }
    Write-Host '   ok: files, DisplayVersion, Start Menu, and desktop/autostart opt-in state'
}

function Invoke-UpgradeCheck {
    param([Parameter(Mandatory = $true)][pscustomobject]$FixtureBObject)
    Write-Host '-- installer GUI: Upgrade'
    $marker = Join-Path $ProfileDir 'smoke-upgrade-marker.txt'
    Write-SpeechekUtf8NoBom -Path $marker -Text 'preserve me'
    $options = @{ Run = $false }
    $result = Invoke-SmokeInstall -InstallerPath $FixtureBObject.InstallerPath -Options $options
    if ($result.TimedOut) { throw 'the upgrade installer GUI did not finish within the driver timeout.' }
    if (-not [string]::IsNullOrWhiteSpace([string]$result.Fatal)) { throw "upgrade installer refused: $($result.Fatal)" }
    if ((Get-InstalledTestVersion) -ne $FixtureBObject.Version) {
        throw "after the upgrade DisplayVersion '$((Get-InstalledTestVersion))' is not '$($FixtureBObject.Version)'."
    }
    if (-not (Test-Path -LiteralPath $marker)) { throw 'the upgrade deleted the profile marker (data must be preserved).' }
    Write-Host '   ok: in-place upgrade kept the profile and updated the version'
}

function Invoke-DowngradeCheck {
    param([Parameter(Mandatory = $true)][pscustomobject]$FixtureAObject, [Parameter(Mandatory = $true)][string]$InstalledVersion)
    Write-Host '-- installer GUI: Downgrade (must refuse)'
    $options = @{ Run = $false }
    $result = Invoke-SmokeInstall -InstallerPath $FixtureAObject.InstallerPath -Options $options -ExpectFatal
    if ([string]::IsNullOrWhiteSpace([string]$result.Fatal)) {
        throw 'the older installer did not show a refusal for the newer installed version.'
    }
    if ((Get-InstalledTestVersion) -ne $InstalledVersion) {
        throw "the refused downgrade changed DisplayVersion to '$((Get-InstalledTestVersion))'."
    }
    Write-Host "   ok: downgrade refused with '$($result.Fatal)'"
}

function Invoke-Uninstall {
    param([bool]$DeleteData)
    Write-Host "-- installer GUI: Uninstall (DeleteData=$DeleteData)"
    $uninstaller = Join-Path $InstallDir 'uninstall.exe'
    if (-not (Test-Path -LiteralPath $uninstaller -PathType Leaf)) { throw "uninstaller is missing: $uninstaller" }
    if (-not (Test-Path -LiteralPath $ProfileDir)) {
        [void](New-Item -ItemType Directory -Path $ProfileDir -Force)
        Write-SmokeSettings -Path (Join-Path $ProfileDir 'settings.json') -ListenPort 4175
    }
    $settingsBefore = Get-SpeechekSha256 (Join-Path $ProfileDir 'settings.json')
    $vaultBefore = Get-SpeechekSha256 (Join-Path $ProfileDir 'secrets.bin')
    $options = @{ DeleteData = $DeleteData }
    $result = Invoke-SmokeInstall -InstallerPath $uninstaller -Options $options
    if ($result.TimedOut) { throw 'the uninstaller GUI did not finish within the driver timeout.' }
    if (Test-Path -LiteralPath (Join-Path $InstallDir ($TestMainBinary + '.exe'))) {
        throw 'the installed executable survived the uninstall.'
    }
    if (Test-Path -LiteralPath $StartMenuLnk) { throw 'the Start Menu shortcut survived the uninstall.' }
    $profilePresent = Test-Path -LiteralPath $ProfileDir
    if ($DeleteData) {
        if ($profilePresent) { throw 'the profile survived an uninstall with the data checkbox checked.' }
    }
    else {
        if (-not $profilePresent) { throw 'the uninstall without the data checkbox removed the profile.' }
        if ($settingsBefore -ne (Get-SpeechekSha256 (Join-Path $ProfileDir 'settings.json')) -or $vaultBefore -ne (Get-SpeechekSha256 (Join-Path $ProfileDir 'secrets.bin'))) { throw 'the keep-data uninstall modified the Test settings or fake vault.' }
    }
    Write-Host '   ok: known files removed and the data checkbox behaved as requested'
}

function Invoke-CancelCheck {
    param([Parameter(Mandatory = $true)][string]$HttpBase, [Parameter(Mandatory = $true)][int]$ListenPort,
        [Parameter(Mandatory = $true)][pscustomobject]$FixtureObject)
    Write-Host '-- installer GUI: close-warning Cancel'
    $configPath = Join-Path (Join-Path $outDir 'fixture-runtime') 'settings.json'
    $installed = Get-InstalledTestVersion
    if ([string]::IsNullOrWhiteSpace($installed)) { throw 'Cancel requires an installed Test fixture.' }
    if ([version]$FixtureObject.Version -lt [version]$installed) {
        throw "Cancel needs the installed fixture version or newer (installed $installed, fixture $($FixtureObject.Version))."
    }
    $appPort = Get-SmokeFreePort
    while ($appPort -eq $ListenPort) { $appPort = Get-SmokeFreePort }
    $appBase = 'http://127.0.0.1:' + $appPort
    Write-SmokeSettings -Path $configPath -ListenPort $appPort
    $process = Start-SmokeTestApp -ApplicationPath (Join-Path $InstallDir ($TestMainBinary + '.exe')) `
        -ConfigPath $configPath -HttpBase $HttpBase -WsBase ('ws://127.0.0.1:' + $ListenPort)
    try {
        Wait-SmokeHealth -HttpBase $appBase -Process $process
        $before = Get-InstalledTestVersion
        $beforeSha = Get-SpeechekSha256 (Join-Path $InstallDir ($TestMainBinary + '.exe'))
        $options = @{ Run = $false; ConfirmClose = $false }
        $result = Invoke-SmokeInstall -InstallerPath $FixtureObject.InstallerPath -Options $options
        if (-not $result.Cancelled) { throw 'the close warning was not cancelled.' }
        if ($process.HasExited) { throw 'the running Test instance was stopped despite cancelling.' }
        if ((Get-InstalledTestVersion) -ne $before) { throw 'the cancelled upgrade changed the installed version.' }
        if ((Get-SpeechekSha256 (Join-Path $InstallDir ($TestMainBinary + '.exe'))) -ne $beforeSha) {
            throw 'the cancelled upgrade changed the installed executable.'
        }
        Write-Host '   ok: cancel kept the process, the installed version and the executable bytes'
    }
    finally {
        if (-not $KeepRunning) { Stop-SmokeProcesses }
    }
}

# ── Shared helpers for the destructive Test-only scenarios ──────────────────
function Assert-TestInstallPresent {
    param([string]$What = 'this scenario')
    if (-not (Test-Path -LiteralPath (Join-Path $InstallDir 'uninstall.exe'))) {
        throw "$What requires an installed Test fixture; run FreshInstall/Upgrade first."
    }
    if ([string]::IsNullOrWhiteSpace((Get-InstalledTestVersion))) {
        throw "$What requires the Test uninstall registry record; the installation looks damaged."
    }
}

function Get-SmokeRunValue {
    try {
        $property = Get-ItemProperty -LiteralPath $RunKey -Name $TestProductName -ErrorAction SilentlyContinue
        if ($null -eq $property) { return $null }
        return [string]$property.$TestProductName
    }
    catch { return $null }
}

function Get-SmokeRegBytes {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Name)
    try {
        $property = Get-ItemProperty -LiteralPath $Path -Name $Name -ErrorAction SilentlyContinue
        if ($null -eq $property) { return $null }
        $value = $property.$Name
        if ($null -eq $value) { return $null }
        return [byte[]]$value
    }
    catch { return $null }
}

function Set-SmokeRegBytes {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Name, [Parameter(Mandatory = $true)][byte[]]$Bytes)
    if (-not (Test-Path -LiteralPath $Path)) { New-Item -Path $Path -Force | Out-Null }
    New-ItemProperty -LiteralPath $Path -Name $Name -PropertyType Binary -Value ([byte[]]$Bytes) -Force | Out-Null
}

# Lowercase hex of a byte array, so two blobs can be compared without relying
# on .NET sequence equality across PowerShell versions.
function Get-SmokeHex {
    param([AllowNull()][byte[]]$Bytes)
    if ($null -eq $Bytes) { return '' }
    return (($Bytes | ForEach-Object { $_.ToString('x2') }) -join '')
}

# ── Registry ACL helpers (Rollback scenario only) ───────────────────────────
# The Rollback scenario provokes a *metadata* failure: it denies KEY_WRITE on
# the Test uninstall key so the installer's own registry write fails after the
# files were copied, which makes the installer restore the previous files and
# report a rollback. Denying only KEY_WRITE keeps read and WRITE_DAC access, so
# the denial can always be undone by the owner through WRITE_DAC alone.
function Initialize-SmokeRegSec {
    if ('SmokeRegSec' -as [type]) { return }
    $source = @'
using System;
using System.Runtime.InteropServices;

public static class SmokeRegSec
{
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern int RegOpenKeyEx(IntPtr hKey, string subKey, int options, int samDesired, out IntPtr phkResult);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern int RegGetKeySecurity(IntPtr hKey, int securityInformation, IntPtr pSecurityDescriptor, ref int lpcbSecurityDescriptor);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern int RegSetKeySecurity(IntPtr hKey, int securityInformation, IntPtr pSecurityDescriptor);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern int RegCloseKey(IntPtr hKey);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertStringSecurityDescriptorToSecurityDescriptor(string sddl, int rev, out IntPtr psd, out int size);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr h);

    static readonly IntPtr HKCU = new IntPtr(unchecked((int)0x80000001));
    const int DACL_SECURITY_INFORMATION = 4;
    const int KEY_READ = 0x20019;
    const int WRITE_DAC = 0x40000;
    const int ERROR_INSUFFICIENT_BUFFER = 122;

    public static byte[] GetDacl(string subKey)
    {
        IntPtr hk;
        int rc = RegOpenKeyEx(HKCU, subKey, 0, KEY_READ | WRITE_DAC, out hk);
        if (rc != 0) throw new Exception("RegOpenKeyEx " + rc);
        try
        {
            int size = 0;
            rc = RegGetKeySecurity(hk, DACL_SECURITY_INFORMATION, IntPtr.Zero, ref size);
            if (rc != ERROR_INSUFFICIENT_BUFFER && rc != 0) throw new Exception("RegGetKeySecurity " + rc);
            IntPtr buffer = Marshal.AllocHGlobal(size);
            try
            {
                rc = RegGetKeySecurity(hk, DACL_SECURITY_INFORMATION, buffer, ref size);
                if (rc != 0) throw new Exception("RegGetKeySecurity " + rc);
                byte[] result = new byte[size];
                Marshal.Copy(buffer, result, 0, size);
                return result;
            }
            finally { Marshal.FreeHGlobal(buffer); }
        }
        finally { RegCloseKey(hk); }
    }

    // Restoring only needs WRITE_DAC, which the KEY_WRITE deny never removes.
    public static void SetDaclBytes(string subKey, byte[] descriptor)
    {
        IntPtr hk;
        int rc = RegOpenKeyEx(HKCU, subKey, 0, WRITE_DAC, out hk);
        if (rc != 0) throw new Exception("RegOpenKeyEx " + rc);
        try
        {
            IntPtr buffer = Marshal.AllocHGlobal(descriptor.Length);
            try
            {
                Marshal.Copy(descriptor, 0, buffer, descriptor.Length);
                rc = RegSetKeySecurity(hk, DACL_SECURITY_INFORMATION, buffer);
                if (rc != 0) throw new Exception("RegSetKeySecurity " + rc);
            }
            finally { Marshal.FreeHGlobal(buffer); }
        }
        finally { RegCloseKey(hk); }
    }

    public static void SetDaclSddl(string subKey, string sddl)
    {
        IntPtr hk;
        int rc = RegOpenKeyEx(HKCU, subKey, 0, KEY_READ | WRITE_DAC, out hk);
        if (rc != 0) throw new Exception("RegOpenKeyEx " + rc);
        try
        {
            IntPtr sd; int size;
            if (!ConvertStringSecurityDescriptorToSecurityDescriptor(sddl, 1, out sd, out size)) throw new Exception("ConvertStringSecurityDescriptor " + Marshal.GetLastWin32Error());
            try
            {
                rc = RegSetKeySecurity(hk, DACL_SECURITY_INFORMATION, sd);
                if (rc != 0) throw new Exception("RegSetKeySecurity " + rc);
            }
            finally { LocalFree(sd); }
        }
        finally { RegCloseKey(hk); }
    }
}
'@
    Add-Type -TypeDefinition $source -Language CSharp
}

$script:SmokeAclBackupPath = Join-Path $privateDir 'uninstall-key-dacl.bin'

function Get-SmokeUninstallSubKey {
    return 'Software\Microsoft\Windows\CurrentVersion\Uninstall\' + $TestProductName
}

# Recovers the uninstall key ACL after an interrupted Rollback run; the saved
# descriptor is only present while the deny is applied.
function Restore-SmokeUninstallKeyDacl {
    if (-not (Test-Path -LiteralPath $script:SmokeAclBackupPath -PathType Leaf)) { return $false }
    if (-not (Test-Path -LiteralPath $UninstallKey)) {
        Remove-Item -LiteralPath $script:SmokeAclBackupPath -Force -ErrorAction SilentlyContinue
        return $false
    }
    Initialize-SmokeRegSec
    $bytes = [System.IO.File]::ReadAllBytes($script:SmokeAclBackupPath)
    [SmokeRegSec]::SetDaclBytes((Get-SmokeUninstallSubKey), $bytes)
    Remove-Item -LiteralPath $script:SmokeAclBackupPath -Force
    Write-Host '   recovered the uninstall key ACL from a previous interrupted run'
    return $true
}

# Starts the canonical installed Test EXE suspended: the process never runs, so
# it cannot answer the installer's per-PID quit event and the installer must
# reach its shared 5000 ms grace plus force-close path. RM reports a suspended
# process as the file owner (verified by the probe), and the installer verifies
# image path/creation time/session/SID before terminating exactly this PID.
function Start-SmokeSuspendedTestProcess {
    param([Parameter(Mandatory = $true)][string]$ApplicationPath)
    Initialize-SmokeWin32
    $processId = [SmokeWin32]::StartSuspended($ApplicationPath, (Split-Path -Parent $ApplicationPath))
    $process = [System.Diagnostics.Process]::GetProcessById($processId)
    [void]$script:StartedProcessIds.Add($processId)
    [void]$script:StartedProcesses.Add($process)
    return $process
}

function Invoke-SameVersionCheck {
    param([Parameter(Mandatory = $true)][pscustomobject]$FixtureObject)
    Write-Host '-- installer GUI: SameVersion reinstall (in place, data kept)'
    Assert-TestInstallPresent 'SameVersion'
    $installed = Get-InstalledTestVersion
    if ($installed -ne $FixtureObject.Version) {
        throw "SameVersion needs the fixture for the installed version $installed (got $($FixtureObject.Version))."
    }
    $exePath = Join-Path $InstallDir ($TestMainBinary + '.exe')
    $exeBefore = Get-SpeechekSha256 $exePath
    $marker = Join-Path $ProfileDir 'smoke-sameversion-marker.txt'
    Write-SpeechekUtf8NoBom -Path $marker -Text 'same version keep'
    $desktopBefore = Test-Path -LiteralPath $DesktopLnk
    $runBefore = Get-SmokeRunValue
    $result = Invoke-SmokeInstall -InstallerPath $FixtureObject.InstallerPath -Options @{ Run = $false }
    if ($result.TimedOut) { throw 'the same-version reinstall GUI did not finish within the driver timeout.' }
    if (-not [string]::IsNullOrWhiteSpace([string]$result.Fatal)) { throw "the same-version reinstall refused: $($result.Fatal)" }
    if ((Get-InstalledTestVersion) -ne $installed) { throw "the in-place reinstall changed DisplayVersion to '$((Get-InstalledTestVersion))'." }
    if ((Get-SpeechekSha256 $exePath) -ne $exeBefore) { throw 'the in-place reinstall changed the installed executable bytes.' }
    if ((Get-SpeechekSha256 $exePath) -ne [string]$FixtureObject.Manifest.exe.sha256) { throw 'the installed executable does not match the fixture payload.' }
    if (-not (Test-Path -LiteralPath $marker)) { throw 'the in-place reinstall deleted profile data.' }
    if ((Test-Path -LiteralPath $DesktopLnk) -ne $desktopBefore) { throw 'the reinstall changed the desktop shortcut (the options page must be skipped).' }
    if ((Get-SmokeRunValue) -ne $runBefore) { throw 'the reinstall rewrote the autostart entry.' }
    Write-Host '   ok: same version reinstalled in place; data, version and opt-in state unchanged'
}

function Invoke-CorruptVersionCheck {
    param([Parameter(Mandatory = $true)][pscustomobject]$FixtureObject)
    Write-Host '-- installer GUI: CorruptVersion refusal (damaged/unknown record)'
    Assert-TestInstallPresent 'CorruptVersion'
    $exePath = Join-Path $InstallDir ($TestMainBinary + '.exe')
    $exeBefore = Get-SpeechekSha256 $exePath
    $saved = Get-InstalledTestVersion
    $cases = @(
        @{ Name = 'invalid version text'; Value = '0.0.0-broken' },
        @{ Name = 'missing version value'; Value = $null }
    )
    try {
        foreach ($case in $cases) {
            if ($null -eq $case.Value) {
                Remove-ItemProperty -LiteralPath $UninstallKey -Name 'DisplayVersion' -ErrorAction Stop
            }
            else {
                Set-ItemProperty -LiteralPath $UninstallKey -Name 'DisplayVersion' -Value $case.Value
            }
            $result = Invoke-SmokeInstall -InstallerPath $FixtureObject.InstallerPath -Options @{ Run = $false } -ExpectFatal
            if ([string]::IsNullOrWhiteSpace([string]$result.Fatal)) {
                throw "the installer did not refuse the $($case.Name) record."
            }
            if (-not (Test-SmokeMessageContains -Message ([string]$result.Fatal) -Key 'RepairRequired')) {
                throw "unexpected refusal for the $($case.Name) record: $($result.Fatal)"
            }
            if ((Get-SpeechekSha256 $exePath) -ne $exeBefore) {
                throw "the refusal for the $($case.Name) record changed the installed executable."
            }
            Write-Host "   ok: $($case.Name) refused before any change"
        }
    }
    finally {
        Set-ItemProperty -LiteralPath $UninstallKey -Name 'DisplayVersion' -Value $saved
    }
}

function Invoke-PreserveOptionsCheck {
    param(
        [Parameter(Mandatory = $true)][pscustomobject]$FixtureObject,
        [Parameter(Mandatory = $true)][pscustomobject]$FixtureBObject
    )
    Write-Host '-- installer GUI: PreserveOptions (deleted desktop, autostart disabled)'
    Assert-TestInstallPresent 'PreserveOptions'
    $installed = Get-InstalledTestVersion
    if ($installed -ne $FixtureObject.Version) {
        throw "PreserveOptions needs -Fixture $installed (the version currently installed) and -FixtureB newer than it."
    }
    if ([version]$FixtureBObject.Version -le [version]$installed) {
        throw "-FixtureB ($($FixtureBObject.Version)) must be newer than the installed $installed."
    }
    # The owner's own changes after the opt-in install: the desktop shortcut is
    # deleted, and the autostart entry is kept but disabled through Windows
    # (StartupApproved), exactly what Windows writes for a disabled entry.
    if (Test-Path -LiteralPath $DesktopLnk) { Remove-Item -LiteralPath $DesktopLnk -Force }
    $exePath = Join-Path $InstallDir ($TestMainBinary + '.exe')
    Set-ItemProperty -LiteralPath $RunKey -Name $TestProductName -Value ('"' + $exePath + '"')
    Set-SmokeRegBytes -Path $StartupApprovedKey -Name $TestProductName -Bytes $SmokeStartupDisabled
    $runBefore = Get-SmokeRunValue
    $approvedBefore = Get-SmokeRegBytes -Path $StartupApprovedKey -Name $TestProductName
    $marker = Join-Path $ProfileDir 'smoke-preserve-marker.txt'
    Write-SpeechekUtf8NoBom -Path $marker -Text 'preserve options'
    $result = Invoke-SmokeInstall -InstallerPath $FixtureBObject.InstallerPath -Options @{ Run = $false }
    if ($result.TimedOut) { throw 'the preserve-options upgrade GUI did not finish within the driver timeout.' }
    if (-not [string]::IsNullOrWhiteSpace([string]$result.Fatal)) { throw "the preserve-options upgrade refused: $($result.Fatal)" }
    if ((Get-InstalledTestVersion) -ne $FixtureBObject.Version) { throw "the upgrade did not reach $($FixtureBObject.Version)." }
    if (Test-Path -LiteralPath $DesktopLnk) { throw 'the upgrade recreated the desktop shortcut the owner deleted.' }
    if ((Get-SmokeRunValue) -ne $runBefore) { throw 'the upgrade rewrote the autostart Run value.' }
    if ((Get-SmokeHex (Get-SmokeRegBytes -Path $StartupApprovedKey -Name $TestProductName)) -ne (Get-SmokeHex $approvedBefore)) {
        throw 'the upgrade changed the Windows StartupApproved state of the autostart entry.'
    }
    if (-not (Test-Path -LiteralPath $marker)) { throw 'the upgrade deleted profile data.' }
    Write-Host '   ok: upgrade preserved the deleted desktop, the Run value and the disabled startup state'
}

function Invoke-ForeignLockerCheck {
    param([Parameter(Mandatory = $true)][pscustomobject]$FixtureObject)
    Write-Host '-- installer GUI: ForeignLocker abort (file held by a non-app process)'
    Assert-TestInstallPresent 'ForeignLocker'
    $installed = Get-InstalledTestVersion
    if ([version]$FixtureObject.Version -lt [version]$installed) {
        throw "ForeignLocker needs the installed fixture version or newer (installed $installed, fixture $($FixtureObject.Version))."
    }
    $exePath = Join-Path $InstallDir ($TestMainBinary + '.exe')
    $exeBefore = Get-SpeechekSha256 $exePath
    # Hold the canonical EXE open from this (non-app) process, allowing readers
    # but denying writers/deleters (a realistic "file in use" lock). Reading is
    # left possible on purpose: the installer's version precheck reads the EXE
    # first, so the run reaches the close step. Restart Manager reports this
    # process as the owner; the installer's image-path check rejects it (the
    # image is powershell.exe, not speechek.exe) and must abort without
    # terminating the holder or touching installed files.
    $lock = [System.IO.File]::Open($exePath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
    try {
        $result = Invoke-SmokeInstall -InstallerPath $FixtureObject.InstallerPath -Options @{ Run = $false } -ExpectFatal
        if ([string]::IsNullOrWhiteSpace([string]$result.Fatal)) {
            throw 'the installer did not refuse the file held by a foreign process.'
        }
        if (-not (Test-SmokeMessageContains -Message ([string]$result.Fatal) -Key 'FileInUse')) {
            throw "unexpected foreign-locker refusal: $($result.Fatal)"
        }
        if ((Get-InstalledTestVersion) -ne $installed) { throw 'the refused foreign-locker run changed the installed version.' }
        if ((Get-SpeechekSha256 $exePath) -ne $exeBefore) { throw 'the refused foreign-locker run changed the installed executable.' }
        Write-Host "   ok: foreign locker refused with '$($result.Fatal)'; the holder and the installation were untouched"
    }
    finally {
        $lock.Dispose()
    }
}

function Invoke-RollbackCheck {
    param([Parameter(Mandatory = $true)][pscustomobject]$FixtureObject)
    Write-Host '-- installer GUI: Rollback (metadata failure restores the previous version)'
    Assert-TestInstallPresent 'Rollback'
    $installed = Get-InstalledTestVersion
    if ($installed -ne $FixtureObject.Version) {
        throw "Rollback needs -Fixture $installed (the version currently installed); it reinstalls that same version."
    }
    [void](Restore-SmokeUninstallKeyDacl)
    Initialize-SmokeRegSec
    $exePath = Join-Path $InstallDir ($TestMainBinary + '.exe')
    $exeBefore = Get-SpeechekSha256 $exePath
    $noticesPath = Join-Path $InstallDir 'THIRD-PARTY-NOTICES.txt'
    $noticesBefore = Get-SpeechekSha256 $noticesPath
    $origDacl = [SmokeRegSec]::GetDacl((Get-SmokeUninstallSubKey))
    [System.IO.File]::WriteAllBytes($script:SmokeAclBackupPath, $origDacl)
    $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    # Deny KEY_WRITE (KEY_SET_VALUE) on the Test uninstall key only: the staged
    # payload is verified, the installed files are backed up and copied, and
    # then the metadata registry write fails, so the installer rolls the files
    # back and reports a rollback. Read access and WRITE_DAC stay allowed, so
    # the owner can always restore the descriptor in 'finally' (and after an
    # interrupted run) - never a self-lockout.
    [SmokeRegSec]::SetDaclSddl((Get-SmokeUninstallSubKey), ("D:PAI(D;;KW;;;$sid)(A;;KA;;;SY)(A;;KA;;;BA)(A;;KA;;;$sid)"))
    try {
        $result = Invoke-SmokeInstall -InstallerPath $FixtureObject.InstallerPath -Options @{ Run = $false }
        if ([string]::IsNullOrWhiteSpace([string]$result.Fatal)) {
            throw 'the installer did not report the metadata failure (expected a rollback message box).'
        }
        if (-not (Test-SmokeMessageContains -Message ([string]$result.Fatal) -Key 'RollbackDone')) {
            throw "unexpected rollback message: $($result.Fatal)"
        }
        if ((Get-InstalledTestVersion) -ne $installed) { throw 'the rolled-back run changed the installed version.' }
        if ((Get-SpeechekSha256 $exePath) -ne $exeBefore) { throw 'the rollback did not restore the previous executable bytes.' }
        if ((Get-SpeechekSha256 $noticesPath) -ne $noticesBefore) { throw 'the rollback did not restore the previous notices bytes.' }
        Write-Host "   ok: metadata failure rolled back the files; the previous version is intact ('$($result.Fatal)')"
    }
    finally {
        [SmokeRegSec]::SetDaclBytes((Get-SmokeUninstallSubKey), $origDacl)
        Remove-Item -LiteralPath $script:SmokeAclBackupPath -Force -ErrorAction SilentlyContinue
        # Prove the write access is really back before the next scenario runs.
        Set-ItemProperty -LiteralPath $UninstallKey -Name 'DisplayVersion' -Value $installed -ErrorAction Stop
    }
}

function Invoke-ForceCloseCheck {
    param([Parameter(Mandatory = $true)][pscustomobject]$FixtureObject)
    Write-Host '-- installer GUI: ForceClose (suspended owned Test EXE, shared 5 s grace)'
    Assert-TestInstallPresent 'ForceClose'
    $installed = Get-InstalledTestVersion
    if ([version]$FixtureObject.Version -lt [version]$installed) {
        throw "ForceClose needs the installed fixture version or newer (installed $installed, fixture $($FixtureObject.Version))."
    }
    $exePath = Join-Path $InstallDir ($TestMainBinary + '.exe')
    $exeBefore = Get-SpeechekSha256 $exePath
    $suspended = Start-SmokeSuspendedTestProcess -ApplicationPath $exePath
    $state = @{}
    try {
        Start-Sleep -Milliseconds 400
        if ($suspended.HasExited) { throw 'the suspended Test process exited before the installer ran.' }
        $result = Invoke-SmokeInstall -InstallerPath $FixtureObject.InstallerPath -Options @{ Run = $false; ConfirmClose = $true } -State $state -WatchProcess $suspended
        if ($result.TimedOut) { throw 'the force-close installer GUI did not finish within the driver timeout.' }
        if (-not [string]::IsNullOrWhiteSpace([string]$result.Fatal)) { throw "the force-close install refused: $($result.Fatal)" }
        if (-not $state.ContainsKey('CloseWarningConfirmedUtc')) {
            throw 'the close warning was never confirmed: the suspended EXE was not reported as the installed owner.'
        }
        # The GUI driver polled the suspended process during the run and
        # recorded when it actually disappeared, so the grace period is
        # measured from the confirmed warning, not from the end of the install.
        if (-not $state.ContainsKey('WatchedProcessExitUtc')) {
            if (-not $suspended.HasExited) { throw 'the installer did not close the hung Test process.' }
            $state['WatchedProcessExitUtc'] = [DateTime]::UtcNow
        }
        $confirmed = [DateTime]$state['CloseWarningConfirmedUtc']
        $exitedUtc = [DateTime]$state['WatchedProcessExitUtc']
        $elapsedMs = [int](($exitedUtc.ToUniversalTime() - $confirmed.ToUniversalTime()).TotalMilliseconds)
        if ($elapsedMs -lt 4500) {
            throw ("the hung process was terminated after only $elapsedMs ms; the shared 5000 ms grace was not honoured.")
        }
        if ($elapsedMs -gt 9000) {
            throw ("the hung process was not terminated within the grace plus 1000 ms kill window ($elapsedMs ms).")
        }
        # TerminateProcess(handle, 1) yields exit code 1; the app's own graceful
        # event exit would be 0.
        $exitCode = $null
        try { $suspended.Refresh(); $exitCode = $suspended.ExitCode } catch { }
        if ($null -ne $exitCode -and $exitCode -ne 1) {
            throw "the hung process exited with code $exitCode; expected the forced-termination code 1."
        }
        $afterSha = Get-SpeechekSha256 $exePath
        if ($afterSha -ne [string]$FixtureObject.Manifest.exe.sha256) {
            throw 'the installed executable does not match the fixture payload after the force-close install.'
        }
        if ($FixtureObject.Version -eq $installed -and $afterSha -ne $exeBefore) {
            throw 'a same-version force-close reinstall changed the executable bytes.'
        }
        if ((Get-InstalledTestVersion) -ne $FixtureObject.Version) { throw 'the force-close install did not reach the fixture version.' }
        Write-Host "   ok: hung Test EXE force-terminated after $elapsedMs ms (verified handle, exit code 1); install completed"
    }
    finally {
        if (-not $suspended.HasExited) {
            try { $suspended.Kill(); $suspended.WaitForExit(2000) | Out-Null } catch { }
        }
    }
}

# ── Dispatch ────────────────────────────────────────────────────────────────
$reportHeader = '== Speechek test-fixture smoke =='
Write-Host $reportHeader
Write-Host "scenario : $Scenario"
Write-Host "language : $Language"

$fixtureObject = $null
$fixtureBObject = $null
if ($Fixture) { $fixtureObject = Read-SmokeFixture -FixturePath $Fixture }
if ($FixtureB) { $fixtureBObject = Read-SmokeFixture -FixturePath $FixtureB }

if (-not (Test-Path -LiteralPath $privateDir)) { New-Item -ItemType Directory -Path $privateDir -Force | Out-Null }
Write-SmokeFixtureScripts

$scenarios = if ($Scenario -eq 'All') {
    @('GateSelfTest', 'FixtureSelfTest', 'FakeProvider', 'FreshInstall', 'UninstallDelete',
      'OptIn', 'UninstallDelete', 'Upgrade', 'Downgrade', 'UninstallKeep')
} else {
    @($Scenario)
}

$startedFixtureServer = $false
try {
    foreach ($name in $scenarios) {
        switch ($name) {
            'GateSelfTest' { Invoke-GateSelfTest }
            'FixtureSelfTest' {
                $listenPort = if ($Port -gt 0) { $Port } else { Get-SmokeFreePort }
                $httpBase = Start-SmokeFixtureServer -ListenPort $listenPort
                $startedFixtureServer = $true
                Invoke-FixtureSelfTest -ListenPort $listenPort -HttpBase $httpBase
            }
            'FakeProvider' {
                $listenPort = if ($Port -gt 0) { $Port } else { Get-SmokeFreePort }
                $httpBase = Start-SmokeFixtureServer -ListenPort $listenPort
                $startedFixtureServer = $true
                Invoke-FakeProvider -HttpBase $httpBase -ListenPort $listenPort
            }
            'FreshInstall' {
                if ($null -eq $fixtureObject) { Add-Note 'FreshInstall skipped: pass -Fixture <manifest>.'; break }
                [void](Assert-SmokeOwnership)
                Write-SmokeOwnership
                Invoke-FreshInstall -FixtureObject $fixtureObject -OptIn $false
            }
            'OptIn' {
                if ($null -eq $fixtureObject) { Add-Note 'OptIn skipped: pass -Fixture <manifest>.'; break }
                $decision = Assert-SmokeOwnership
                if ($decision -eq 'owned') {
                    Add-Note 'OptIn needs a fresh state; run UninstallDelete first (the options page is skipped on an update).'
                    break
                }
                Write-SmokeOwnership
                Invoke-FreshInstall -FixtureObject $fixtureObject -OptIn $true
            }
            'Upgrade' {
                if ($null -eq $fixtureObject -or $null -eq $fixtureBObject) {
                    Add-Note 'Upgrade skipped: pass -Fixture A -FixtureB B (two versions, same source).'; break
                }
                [void](Assert-SmokeOwnership)
                Write-SmokeOwnership
                Invoke-UpgradeCheck -FixtureBObject $fixtureBObject
            }
            'Downgrade' {
                if ($null -eq $fixtureObject) { Add-Note 'Downgrade skipped: pass -Fixture A.'; break }
                [void](Assert-SmokeOwnership)
                Write-SmokeOwnership
                $installed = Get-InstalledTestVersion
                if ([string]::IsNullOrWhiteSpace($installed)) {
                    Add-Note 'Downgrade skipped: install the newer fixture first.'
                    break
                }
                Invoke-DowngradeCheck -FixtureAObject $fixtureObject -InstalledVersion $installed
            }
            'UninstallKeep' {
                [void](Assert-SmokeOwnership); Write-SmokeOwnership
                Invoke-Uninstall -DeleteData $false
            }
            'UninstallDelete' {
                if (-not (Test-Path -LiteralPath (Join-Path $InstallDir 'uninstall.exe'))) {
                    Add-Note 'UninstallDelete skipped: no Test installation is present.'
                    break
                }
                [void](Assert-SmokeOwnership); Write-SmokeOwnership
                Invoke-Uninstall -DeleteData $true
            }
            'Cancel' {
                if ($null -eq $fixtureObject) { Add-Note 'Cancel skipped: pass -Fixture <manifest>.'; break }
                [void](Assert-SmokeOwnership)
                $listenPort = if ($Port -gt 0) { $Port } else { Get-SmokeFreePort }
                $httpBase = Start-SmokeFixtureServer -ListenPort $listenPort
                $startedFixtureServer = $true
                Invoke-CancelCheck -HttpBase $httpBase -ListenPort $listenPort -FixtureObject $fixtureObject
            }
            'SameVersion' {
                if ($null -eq $fixtureObject) { Add-Note 'SameVersion skipped: pass -Fixture <manifest>.'; break }
                [void](Assert-SmokeOwnership); Write-SmokeOwnership
                Invoke-SameVersionCheck -FixtureObject $fixtureObject
            }
            'CorruptVersion' {
                if ($null -eq $fixtureObject) { Add-Note 'CorruptVersion skipped: pass -Fixture <manifest>.'; break }
                [void](Assert-SmokeOwnership); Write-SmokeOwnership
                Invoke-CorruptVersionCheck -FixtureObject $fixtureObject
            }
            'PreserveOptions' {
                if ($null -eq $fixtureObject -or $null -eq $fixtureBObject) {
                    Add-Note 'PreserveOptions skipped: pass -Fixture A -FixtureB B.'; break
                }
                [void](Assert-SmokeOwnership); Write-SmokeOwnership
                Invoke-PreserveOptionsCheck -FixtureObject $fixtureObject -FixtureBObject $fixtureBObject
            }
            'ForeignLocker' {
                if ($null -eq $fixtureObject) { Add-Note 'ForeignLocker skipped: pass -Fixture <manifest>.'; break }
                [void](Assert-SmokeOwnership); Write-SmokeOwnership
                Invoke-ForeignLockerCheck -FixtureObject $fixtureObject
            }
            'Rollback' {
                if ($null -eq $fixtureObject) { Add-Note 'Rollback skipped: pass -Fixture <manifest>.'; break }
                [void](Assert-SmokeOwnership); Write-SmokeOwnership
                Invoke-RollbackCheck -FixtureObject $fixtureObject
            }
            'ForceClose' {
                if ($null -eq $fixtureObject) { Add-Note 'ForceClose skipped: pass -Fixture <manifest>.'; break }
                [void](Assert-SmokeOwnership); Write-SmokeOwnership
                Invoke-ForceCloseCheck -FixtureObject $fixtureObject
            }
        }
    }
}
finally {
    if (-not $KeepRunning) { Stop-SmokeProcesses }
    try { [void](Restore-SmokeUninstallKeyDacl) } catch { Write-Warning "uninstall key ACL restore failed: $($_.Exception.Message)" }
    if ($startedFixtureServer -and $null -ne $script:FixtureServer -and -not $script:FixtureServer.HasExited) {
        try { $script:FixtureServer.Kill(); $script:FixtureServer.WaitForExit(5000) | Out-Null } catch { }
    }
}

Write-Host ''
Write-Host '== Manual-only branches (never faked) =='
Write-Host ' * Real microphone dictation -> Finalizing -> paste into the active field:'
Write-Host '   the automatic path drives /api/transcribe and /api/live with synthetic PCM, not the microphone.'
Write-Host ' * Windows mute off/on during a real take, and Escape cancelling a real dictation.'
Write-Host ' * Reboot/logon to prove the HKCU Run entry actually starts the app at sign-in.'
Write-Host ' * The force-hang path is automated by the ForceClose scenario (a suspended owned Test EXE);'
Write-Host '   the remaining force/grace variations and the production candidate install stay manual.'
Write-Host ' * The interactive installer language dialog: the smoke pre-seeds the installer language instead.'
Write-Host ' * The production candidate install on the owner profile, and real Google/Free-Tier reachability:'
Write-Host '   the fake provider proves wiring only, never that Google accepts the key.'
foreach ($note in $script:Notes) { Write-Host " * $note" }
Write-Host ''
Write-Host '== Smoke finished =='
