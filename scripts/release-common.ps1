<#
    scripts/release-common.ps1

    Shared helpers for the Speechek release scripts (build-release.ps1,
    build-test-installer.ps1). Dot-source it from a script in
    the same directory:

        . (Join-Path $PSScriptRoot 'release-common.ps1')

    Design notes:
      * PowerShell 5.1 (Windows PowerShell) and 7 must both run this file; no
        `??`, no ternary `? :`, no `Join-String`/`Get-FileHash`, no pipeline
        chain operators.
      * Hashing is done with .NET SHA-256, never a cmdlet that may be absent.
      * Child processes that need a modified environment receive a full
        environment map; the caller's environment is never mutated, so the
        release scripts cannot leak RUSTFLAGS or the VS developer variables
        into the surrounding shell.
#>

function Get-SpeechekRepoRoot {
    param([Parameter(Mandatory = $true)][string]$ScriptRoot)
    return (Split-Path -Parent $ScriptRoot)
}

function Get-SpeechekSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "SHA-256: file not found: $Path"
    }
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $bytes = $sha.ComputeHash($stream)
    }
    finally {
        $stream.Dispose()
        $sha.Dispose()
    }
    return (-join ($bytes | ForEach-Object { $_.ToString('x2') }))
}

function Test-SpeechekBytesEqual {
    param(
        [Parameter(Mandatory = $true)][byte[]]$A,
        [Parameter(Mandatory = $true)][byte[]]$B
    )
    if ($A.Length -ne $B.Length) { return $false }
    for ($i = 0; $i -lt $A.Length; $i++) {
        if ($A[$i] -ne $B[$i]) { return $false }
    }
    return $true
}

function Get-SpeechekGitCommit {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)

    $output = & git -C $RepoRoot rev-parse HEAD 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "git rev-parse HEAD failed in '$RepoRoot': $output"
    }
    return ($output | Out-String).Trim()
}

function Get-SpeechekWorkTreeStatus {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)

    $output = & git -C $RepoRoot status --porcelain 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "git status failed in '$RepoRoot': $output"
    }
    return @($output | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
}

function Assert-SpeechekCleanTree {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)

    $dirty = @(Get-SpeechekWorkTreeStatus $RepoRoot)
    if ($dirty.Count -gt 0) {
        $shown = ($dirty | Select-Object -First 20) -join "`n  "
        throw ("the source tree is not clean; commit or stash every change before a release build" +
               " (the release gate is never bypassed):`n  " + $shown)
    }
}

function New-SpeechekEnvironment {
    param([hashtable]$Overrides)

    # Windows environment names are case-insensitive; keep the map that way so
    # `Path`/`PATH` resolve identically to how the child process sees them.
    $map = New-Object System.Collections.Hashtable ([System.StringComparer]::OrdinalIgnoreCase)
    $current = [System.Environment]::GetEnvironmentVariables()
    foreach ($entry in $current.GetEnumerator()) {
        $map[[string]$entry.Key] = [string]$entry.Value
    }
    if ($Overrides) {
        foreach ($key in $Overrides.Keys) {
            if ($null -eq $Overrides[$key]) {
                $map.Remove([string]$key)
            }
            else {
                $map[[string]$key] = [string]$Overrides[$key]
            }
        }
    }
    return $map
}

function Set-SpeechekProcessEnvironment {
    param(
        [Parameter(Mandatory = $true)][System.Diagnostics.ProcessStartInfo]$Psi,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment
    )

    # .NET Framework exposes EnvironmentVariables, .NET Core exposes both.
    $target = $null
    if ($Psi.PSObject.Properties.Match('Environment').Count -gt 0) {
        $target = $Psi.Environment
    }
    else {
        $target = $Psi.EnvironmentVariables
    }
    foreach ($key in $Environment.Keys) {
        $target[[string]$key] = [string]$Environment[$key]
    }
}

function ConvertTo-SpeechekArgument {
    param([string]$Value)

    if ($Value -notmatch '[\s"]') { return $Value }
    # Standard CreateProcess quoting: escape backslashes that precede a quote
    # and double a trailing run of backslashes.
    $escaped = $Value -replace '(\\*)"', '$1$1\"'
    $escaped = $escaped -replace '(\\+)$', '$1$1'
    return '"' + $escaped + '"'
}

function New-SpeechekProcessStartInfo {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments,
        [string]$WorkingDirectory,
        $Environment,
        [switch]$CaptureOutput
    )

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    if ($Arguments) {
        $psi.Arguments = (($Arguments | ForEach-Object { ConvertTo-SpeechekArgument ([string]$_) }) -join ' ')
    }
    else {
        $psi.Arguments = ''
    }
    if ($WorkingDirectory) { $psi.WorkingDirectory = $WorkingDirectory }
    $psi.UseShellExecute = $false
    if ($CaptureOutput) {
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.CreateNoWindow = $true
    }
    if ($Environment) {
        Set-SpeechekProcessEnvironment $psi $Environment
    }
    return $psi
}

# Runs a child process with the console inherited (build output streams live).
# Returns the exit code; the caller decides whether that is a failure.
function Invoke-SpeechekProcess {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments,
        [string]$WorkingDirectory,
        $Environment
    )

    $psi = New-SpeechekProcessStartInfo -FilePath $FilePath -Arguments $Arguments `
        -WorkingDirectory $WorkingDirectory -Environment $Environment
    $process = [System.Diagnostics.Process]::Start($psi)
    $process.WaitForExit()
    return $process.ExitCode
}

# Runs a child process and returns { StandardOutput, StandardError, ExitCode }.
function Get-SpeechekProcessResult {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments,
        [string]$WorkingDirectory,
        $Environment
    )

    $psi = New-SpeechekProcessStartInfo -FilePath $FilePath -Arguments $Arguments `
        -WorkingDirectory $WorkingDirectory -Environment $Environment -CaptureOutput
    $process = [System.Diagnostics.Process]::Start($psi)
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    return [pscustomobject]@{
        StandardOutput = $stdoutTask.Result
        StandardError  = $stderrTask.Result
        ExitCode       = $process.ExitCode
    }
}

function Resolve-SpeechekExecutable {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [hashtable]$Environment
    )

    $pathValue = $null
    if ($Environment) {
        if ($Environment.ContainsKey('Path')) { $pathValue = $Environment['Path'] }
        elseif ($Environment.ContainsKey('PATH')) { $pathValue = $Environment['PATH'] }
    }
    if ([string]::IsNullOrWhiteSpace($pathValue)) { $pathValue = $env:Path }
    foreach ($directory in $pathValue.Split(';')) {
        if ([string]::IsNullOrWhiteSpace($directory)) { continue }
        foreach ($extension in @('.exe', '.cmd', '.bat', '')) {
            $candidate = Join-Path $directory ($Name + $extension)
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                return (Resolve-Path -LiteralPath $candidate).Path
            }
        }
    }
    return $null
}

function Resolve-SpeechekBun {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)

    # A bun.ps1/bun.cmd shim (for example a version-manager module) cannot be
    # started with Process.Start, so only a real bun.exe is ever returned.
    function Test-SpeechekBunExe([string]$Path) {
        if ([string]::IsNullOrWhiteSpace($Path)) { return $false }
        if ([System.IO.Path]::GetExtension($Path) -ne '.exe') { return $false }
        return (Test-Path -LiteralPath $Path -PathType Leaf)
    }

    if (Test-SpeechekBunExe $env:SPEECHEK_BUN) {
        return (Resolve-Path -LiteralPath $env:SPEECHEK_BUN).Path
    }
    foreach ($directory in $env:Path.Split(';')) {
        if ([string]::IsNullOrWhiteSpace($directory)) { continue }
        $candidate = Join-Path $directory 'bun.exe'
        if (Test-SpeechekBunExe $candidate) { return (Resolve-Path -LiteralPath $candidate).Path }
    }
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        $perUser = Join-Path $env:USERPROFILE '.bun\bin\bun.exe'
        if (Test-SpeechekBunExe $perUser) { return (Resolve-Path -LiteralPath $perUser).Path }
    }
    # Local developer cache on the reference machine; it does not exist in a
    # public checkout, so it is only a convenience fallback after PATH.
    $localCache = Join-Path $RepoRoot '.private\tools\bun-windows-x64\bun.exe'
    if (Test-SpeechekBunExe $localCache) { return (Resolve-Path -LiteralPath $localCache).Path }

    throw ("Bun 1.4.2 was not found as a real bun.exe. Put bun.exe on PATH, or set the " +
           "SPEECHEK_BUN environment variable to a bun.exe path (PowerShell/cmd shims are not usable).")
}

# Resolves the Visual Studio x64 developer environment (vswhere + VsDevCmd) and
# returns it as a full environment map for the build children.
function Get-SpeechekVsEnvironment {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)

    $vswhere = $null
    foreach ($base in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
        if ([string]::IsNullOrWhiteSpace($base)) { continue }
        $candidate = Join-Path $base 'Microsoft Visual Studio\Installer\vswhere.exe'
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { $vswhere = $candidate; break }
    }
    if (-not $vswhere) {
        $onPath = Get-Command vswhere -ErrorAction SilentlyContinue
        if ($onPath) { $vswhere = $onPath.Source }
    }
    if (-not $vswhere) {
        throw ("vswhere.exe was not found. Install Visual Studio 2022 Build Tools with the " +
               "Desktop development with C++ workload (x64) and retry.")
    }

    $install = & $vswhere -latest -products '*' -requires 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64' -property installationPath 2>&1
    $install = ($install | Out-String).Trim()
    if ([string]::IsNullOrWhiteSpace($install)) {
        throw ("Visual Studio 2022 Build Tools with the x64 C++ tools (Microsoft.VisualStudio.Component.VC.Tools.x86.x64) " +
               "was not found by vswhere.")
    }
    $vsDevCmd = Join-Path $install 'Common7\Tools\VsDevCmd.bat'
    if (-not (Test-Path -LiteralPath $vsDevCmd -PathType Leaf)) {
        throw "VsDevCmd.bat was not found at '$vsDevCmd'."
    }

    # `set` after VsDevCmd dumps the fully expanded environment. The path is
    # passed through an environment variable so the cmd quoting stays simple.
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = 'cmd.exe'
    $psi.Arguments = '/c ""%SPEECHEK_VSDEVCMD%" -no_logo -arch=x64 -host_arch=x64 >nul && set"'
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    Set-SpeechekProcessEnvironment $psi (New-SpeechekEnvironment @{ SPEECHEK_VSDEVCMD = $vsDevCmd })

    $process = [System.Diagnostics.Process]::Start($psi)
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    $stdout = $stdoutTask.Result
    $stderr = $stderrTask.Result
    if ($process.ExitCode -ne 0) {
        throw "VsDevCmd activation failed (exit $($process.ExitCode)): $stderr"
    }

    $map = New-Object System.Collections.Hashtable ([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($line in ($stdout -split "`r?`n")) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        $equals = $line.IndexOf('=')
        if ($equals -le 0) { continue }
        $key = $line.Substring(0, $equals)
        if ($key.StartsWith('=')) { continue }  # cmd's internal `=C:` style entries
        $map[$key] = $line.Substring($equals + 1)
    }
    if (-not $map.ContainsKey('VSCMD_ARG_TGT_ARCH') -or $map['VSCMD_ARG_TGT_ARCH'] -ne 'x64') {
        throw "VsDevCmd did not activate the x64 target architecture."
    }
    return [pscustomobject]@{ VsDevCmd = $vsDevCmd; InstallPath = $install; Environment = $map }
}

# Confirms the executable is a 64-bit PE with no dynamic CRT import.
function Assert-SpeechekX64StaticBinary {
    param(
        [Parameter(Mandatory = $true)][string]$ExePath,
        [Parameter(Mandatory = $true)][string]$DumpbinPath,
        [string]$WorkingDirectory,
        $Environment
    )

    $headers = Get-SpeechekProcessResult -FilePath $DumpbinPath -Arguments @('/nologo', '/headers', $ExePath) `
        -WorkingDirectory $WorkingDirectory -Environment $Environment
    if ($headers.ExitCode -ne 0) {
        throw "dumpbin /headers failed for '$ExePath': $($headers.StandardError)"
    }
    if ($headers.StandardOutput -notmatch '8664 machine \(x64\)') {
        throw "the release executable is not a native x64 PE image: $ExePath"
    }

    $imports = Get-SpeechekProcessResult -FilePath $DumpbinPath -Arguments @('/nologo', '/imports', $ExePath) `
        -WorkingDirectory $WorkingDirectory -Environment $Environment
    if ($imports.ExitCode -ne 0) {
        throw "dumpbin /imports failed for '$ExePath': $($imports.StandardError)"
    }
    foreach ($pattern in @('VCRUNTIME', 'MSVCP', 'ucrtbase', 'api-ms-win-crt')) {
        if ($imports.StandardOutput -match [regex]::Escape($pattern)) {
            throw ("the release executable still imports the dynamic C runtime " +
                   "($pattern); the static-CRT child build did not take effect: $ExePath")
        }
    }
}

function Write-SpeechekUtf8NoBom {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text
    )

    $directory = Split-Path -Parent $Path
    if ($directory -and -not (Test-Path -LiteralPath $directory)) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($false)))
}

function Write-SpeechekSha256Sums {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Hash,
        [Parameter(Mandatory = $true)][string]$BaseName
    )

    Write-SpeechekUtf8NoBom -Path $Path -Text ($Hash + '  ' + $BaseName + "`n")
}

function Write-SpeechekReleaseManifest {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)]$Manifest
    )

    Write-SpeechekUtf8NoBom -Path $Path -Text ((ConvertTo-Json -InputObject $Manifest -Depth 6) + "`n")
}

# ── Application version mirrors ────────────────────────────────────────────

function Get-SpeechekAppVersion {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)

    $confPath = Join-Path $RepoRoot 'src-tauri\tauri.conf.json'
    $conf = Get-Content -LiteralPath $confPath -Raw | ConvertFrom-Json
    if ([string]::IsNullOrWhiteSpace($conf.version)) {
        throw "src-tauri/tauri.conf.json has no version."
    }
    return [string]$conf.version
}

function Get-SpeechekVersionMirrors {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)

    $confPath = Join-Path $RepoRoot 'src-tauri\tauri.conf.json'
    $cargoPath = Join-Path $RepoRoot 'src-tauri\Cargo.toml'
    $packagePath = Join-Path $RepoRoot 'package.json'
    $lockPath = Join-Path $RepoRoot 'src-tauri\Cargo.lock'

    $conf = Get-Content -LiteralPath $confPath -Raw | ConvertFrom-Json
    $package = Get-Content -LiteralPath $packagePath -Raw | ConvertFrom-Json
    $cargoText = Get-Content -LiteralPath $cargoPath -Raw
    $lockText = Get-Content -LiteralPath $lockPath -Raw

    $cargoMatch = [regex]::Match($cargoText, '(?ms)^\[package\]\r?\n.*?^version = "([^"]+)"')
    if (-not $cargoMatch.Success) { throw "could not read the [package] version from src-tauri/Cargo.toml." }
    $lockMatch = [regex]::Match($lockText, '(?ms)^name = "speechek"\r?\nversion = "([^"]+)"')
    if (-not $lockMatch.Success) { throw "could not read the speechek entry from src-tauri/Cargo.lock." }

    return [pscustomobject]@{
        TauriConf  = [string]$conf.version
        CargoToml  = $cargoMatch.Groups[1].Value
        PackageJson = [string]$package.version
        CargoLock  = $lockMatch.Groups[1].Value
    }
}

function Assert-SpeechekVersionMirrors {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)

    $mirrors = Get-SpeechekVersionMirrors $RepoRoot
    $expected = $mirrors.TauriConf
    if ([string]::IsNullOrWhiteSpace($expected)) { throw "src-tauri/tauri.conf.json has no version." }
    if ($expected -notmatch '^\d+\.\d+\.\d+$') {
        throw "invalid application version '$expected': expected X.Y.Z."
    }
    foreach ($pair in @(
            @{ Name = 'src-tauri/Cargo.toml'; Value = $mirrors.CargoToml },
            @{ Name = 'package.json'; Value = $mirrors.PackageJson },
            @{ Name = 'src-tauri/Cargo.lock'; Value = $mirrors.CargoLock })) {
        if ($pair.Value -ne $expected) {
            throw ("version mirror mismatch: src-tauri/tauri.conf.json is '$expected' but " +
                   "$($pair.Name) is '$($pair.Value)'.")
        }
    }
    return $expected
}

function Get-SpeechekVersionWithBuild {
    param([Parameter(Mandatory = $true)][string]$Version)

    $match = [regex]::Match($Version, '^(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)(?:\+(?<build>\d+))?$')
    if (-not $match.Success) { throw "unsupported application version '$Version'." }
    $base = "$($match.Groups['major'].Value).$($match.Groups['minor'].Value).$($match.Groups['patch'].Value)"
    if ($match.Groups['build'].Success) { return "$base.$($match.Groups['build'].Value)" }
    return "$base.0"
}

function ConvertTo-SpeechekNsisValue {
    param([Parameter(Mandatory = $true)][string]$Value)
    # NSIS string literals: `$` starts a variable and `"` ends the literal.
    return $Value.Replace('$', '$$').Replace('"', '$\"')
}

function New-SpeechekPayloadNsh {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Exe,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$VersionWithBuild,
        [Parameter(Mandatory = $true)][string]$License,
        [Parameter(Mandatory = $true)][string]$Notices,
        [Parameter(Mandatory = $true)][string]$ExeSha256,
        [Parameter(Mandatory = $true)][string]$LicenseSha256,
        [Parameter(Mandatory = $true)][string]$NoticesSha256,
        [int]$TestInstaller = 0
    )

    $lines = @(
        '; Generated by scripts/build-release.ps1 - do not edit.'
        '; This file lives in the ignored build output next to the rendered'
        '; installer.nsi; the custom template includes it via __FILEDIR__/../..'
        ''
        ('!define SPEECHEK_PAYLOAD_EXE "' + (ConvertTo-SpeechekNsisValue $Exe) + '"')
        ('!define SPEECHEK_PAYLOAD_EXE_SHA256 "' + $ExeSha256 + '"')
        ('!define SPEECHEK_PAYLOAD_VERSION "' + (ConvertTo-SpeechekNsisValue $Version) + '"')
        ('!define SPEECHEK_PAYLOAD_VERSIONWITHBUILD "' + (ConvertTo-SpeechekNsisValue $VersionWithBuild) + '"')
        ('!define SPEECHEK_PAYLOAD_LICENSE "' + (ConvertTo-SpeechekNsisValue $License) + '"')
        ('!define SPEECHEK_PAYLOAD_LICENSE_SHA256 "' + $LicenseSha256 + '"')
        ('!define SPEECHEK_PAYLOAD_NOTICES "' + (ConvertTo-SpeechekNsisValue $Notices) + '"')
        ('!define SPEECHEK_PAYLOAD_NOTICES_SHA256 "' + $NoticesSha256 + '"')
        ('!define SPEECHEK_TEST_INSTALLER ' + $TestInstaller)
        ''
    )
    Write-SpeechekUtf8NoBom -Path $Path -Text (($lines -join "`n") + "`n")
}

# ── Cargo.toml rewrite recovery (see docs/releasing.md) ────────────────────

# tauri-cli must not silently rewrite src-tauri/Cargo.toml. If it does, the
# dependency is first restored byte-for-byte, then the inline `tauri = { ... }`
# table is moved to an equivalent `[dependencies.tauri]` table so the CLI has
# nothing left to normalize before bundling is retried once.
function Repair-SpeechekTauriInlineTable {
    param([Parameter(Mandatory = $true)][string]$CargoTomlPath)

    $text = [System.IO.File]::ReadAllText($CargoTomlPath)
    $newline = if ($text.Contains("`r`n")) { "`r`n" } else { "`n" }
    $pattern = '(?m)^tauri = \{ version = "(?<ver>[^"]+)", features = \[(?<feat>[^\]]*)\] \}\r?\n'
    $match = [regex]::Match($text, $pattern)
    if (-not $match.Success) { return $false }

    $version = $match.Groups['ver'].Value
    $features = $match.Groups['feat'].Value
    $without = $text.Remove($match.Index, $match.Length)
    if (-not [regex]::IsMatch($without, '(?m)^\[features\]')) {
        throw "cannot normalize src-tauri/Cargo.toml: the [features] section was not found."
    }
    $table = "[dependencies.tauri]$newline" +
             "version = `"$version`"$newline" +
             "features = [$features]$newline$newline"
    $normalized = [regex]::Replace($without, '(?m)^\[features\]', ($table + '[features]'), 1)
    [System.IO.File]::WriteAllText($CargoTomlPath, $normalized, (New-Object System.Text.UTF8Encoding($false)))
    return $true
}

# Resolves a rustc that is exactly $RequiredVersion. rust-toolchain.toml stays
# the canonical pin, but when rustup cannot install that exact toolchain (for
# example offline or blocked by a socket/network policy) an installed toolchain
# whose rustc reports exactly the required version is accepted. Returns the
# RUSTUP_TOOLCHAIN value to pin for the build children, or $null when the
# repository pin resolves on its own.
function Resolve-SpeechekRustToolchain {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [Parameter(Mandatory = $true)][string]$RequiredVersion,
        [Parameter(Mandatory = $true)][string]$RequiredHost,
        [Parameter(Mandatory = $true)][string]$RustcPath
    )

    $attempts = New-Object System.Collections.ArrayList
    if (-not [string]::IsNullOrWhiteSpace($env:RUSTUP_TOOLCHAIN)) {
        [void]$attempts.Add($env:RUSTUP_TOOLCHAIN.Trim())
    }
    [void]$attempts.Add('')                  # whatever rust-toolchain.toml selects
    [void]$attempts.Add('stable')
    [void]$attempts.Add($RequiredVersion)

    $failures = New-Object System.Collections.ArrayList
    $seen = New-Object System.Collections.Hashtable ([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($toolchain in $attempts) {
        if ($seen.ContainsKey($toolchain)) { continue }
        $seen[$toolchain] = $true
        $label = if ($toolchain -eq '') { 'rust-toolchain.toml' } else { $toolchain }
        $environment = $null
        if ($toolchain -ne '') { $environment = New-SpeechekEnvironment @{ RUSTUP_TOOLCHAIN = $toolchain } }
        $result = Get-SpeechekProcessResult -FilePath $RustcPath -Arguments @('-vV') `
            -WorkingDirectory $RepoRoot -Environment $environment
        if ($result.ExitCode -ne 0) {
            [void]$failures.Add("${label}: $($result.StandardError.Trim())")
            continue
        }
        $versionMatch = [regex]::Match($result.StandardOutput, '(?m)^rustc (\S+) ')
        $hostMatch = [regex]::Match($result.StandardOutput, '(?m)^host: (\S+)')
        if (-not $versionMatch.Success -or -not $hostMatch.Success -or
            $versionMatch.Groups[1].Value -ne $RequiredVersion -or
            $hostMatch.Groups[1].Value -ne $RequiredHost) {
            $foundVersion = if ($versionMatch.Success) { $versionMatch.Groups[1].Value } else { '?' }
            $foundHost = if ($hostMatch.Success) { $hostMatch.Groups[1].Value } else { '?' }
            [void]$failures.Add("${label}: rustc $foundVersion host $foundHost")
            continue
        }
        if ($toolchain -eq '') { return $null }
        return $toolchain
    }

    throw ("Rust $RequiredVersion ($RequiredHost) is required but no usable toolchain was found. " +
           "Run 'rustup toolchain install $RequiredVersion', or select an installed toolchain whose " +
           "rustc is exactly that version via RUSTUP_TOOLCHAIN (for example RUSTUP_TOOLCHAIN=stable). " +
           "Attempts: " + ($failures -join '; '))
}
