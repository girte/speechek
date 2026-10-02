<#
    scripts/build-test-installer.ps1

    Builds one versioned, isolated *test* fixture of the Speechek NSIS installer
    (release plan section 8; the runtime half of the section 3 installer work).

    The fixture is built from the same source as the production release, with the
    same custom template and the same helper includes, but with the test identity
    and a caller-supplied version:

        productName  : Speechek Test
        identifier   : app.speechek.test   (Test build flavor)
        version      : -Version <X.Y.Z>

    It never touches src-tauri/nsis/*, src-tauri/tauri.conf.json, src-tauri/Cargo.toml,
    src-tauri/Cargo.lock, package.json or any production manifest. Everything is
    configured through a temporary `TAURI_CONFIG` JSON (merged over the base
    config by tauri-build/tauri-codegen and passed to `tauri bundle --config`).

    The version is compiled into the executable's FILEVERSION, so it must be a
    parameter of the build and not only of the bundle step. To produce the two
    fixtures for the upgrade regression, run this script twice with the same
    source unchanged:

        powershell -File scripts/build-test-installer.ps1 -Version 0.2.0
        powershell -File scripts/build-test-installer.ps1 -Version 0.2.1

    It performs, in order:
      1. tool checks (no target-dir override, pinned rustc/bun/tauri-cli);
      2. the Visual Studio x64 developer environment (vswhere + VsDevCmd);
      3. the fixture TAURI_CONFIG JSON;
      4. `cargo build --locked --no-default-features --features test-provider`
         (debug profile, dynamic CRT, no --release, no --target);
      5. a Cargo.toml / Cargo.lock byte-for-byte "unchanged" check;
      6. the generated NSIS payload include + shared common.nsh staged into the
         ignored src-tauri/target/debug area (outside the wiped nsis/x64 dir);
      7. `tauri bundle --debug --bundles nsis ... --no-binary-patching --no-sign`;
      8. an "executable unchanged by the bundle" check;
      9. the real installer file plus a fixture manifest.

    The script never stops a running application: if src-tauri/target/debug/speechek.exe
    is locked, close the running debug/Test build and retry. There is no second
    target directory and no published copy of the executable.

    PowerShell 5.1 and 7 compatible.

    Fixture A/B for the §8 regression: build 0.2.0 and 0.2.1 from one source
    state, without editing any production manifest. The *.json manifest written
    beside the installer is consumed by scripts/smoke.ps1 -Fixture.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'release-common.ps1')

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "invalid fixture version '$Version': expected X.Y.Z (no build suffix)."
}

$rustVersion = '1.97.1'
$bunVersion = '1.4.2'
$tauriCliVersion = '2.12.0'
$targetTriple = 'x86_64-pc-windows-msvc'

$fixtureProductName = 'Speechek Test'
$fixtureIdentifier = 'app.speechek.test'
$fixtureMainBinaryName = 'speechek'

$repoRoot = Get-SpeechekRepoRoot $PSScriptRoot
$tauriDir = Join-Path $repoRoot 'src-tauri'
$outDir = Join-Path $tauriDir 'target\debug'          # fixed canonical debug artifact path
$exePath = Join-Path $outDir ($fixtureMainBinaryName + '.exe')
$cargoManifest = Join-Path $tauriDir 'Cargo.toml'
$cargoLock = Join-Path $tauriDir 'Cargo.lock'
$licensePath = Join-Path $repoRoot 'LICENSE'
$noticesPath = Join-Path $repoRoot 'third_party\THIRD-PARTY-NOTICES.txt'
$commonSource = Join-Path $tauriDir 'nsis\common.nsh'
$templatePath = Join-Path $tauriDir 'nsis\installer.nsi'
$englishLang = Join-Path $tauriDir 'nsis\languages\English.nsh'
$russianLang = Join-Path $tauriDir 'nsis\languages\Russian.nsh'
$iconPng = Join-Path $tauriDir 'icons\icon.png'
$iconIco = Join-Path $tauriDir 'icons\icon.ico'

$fixtureDir = Join-Path $outDir 'fixture'
$configPath = Join-Path $fixtureDir 'tauri.test.conf.json'
$bundleOutDir = Join-Path $outDir 'bundle\nsis'
$payloadPath = Join-Path $outDir 'payload.nsh'
$manifestPath = Join-Path $outDir ('speechek-test-fixture-{0}.json' -f $Version)

function Assert-SpeechekNoTargetOverride {
    foreach ($name in @('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_TARGET')) {
        $value = [System.Environment]::GetEnvironmentVariable($name)
        if (-not [string]::IsNullOrWhiteSpace($value)) {
            throw "$name is set ('$value'); the fixture uses the canonical target directory only."
        }
    }
}

function Assert-SpeechekFixturePrerequisites {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [Parameter(Mandatory = $true)][string]$BunExe
    )

    # Resolve the Rust toolchain the build children use. The repository pin in
    # rust-toolchain.toml is preferred; when local rustup cannot reach the
    # pinned channel (network-restricted machine) Resolve-SpeechekRustToolchain
    # accepts an already-installed toolchain whose rustc reports exactly
    # 1.97.1 / x86_64-pc-windows-msvc. The pin file is still verified below and
    # is never altered, so a version or host mismatch is never hidden.
    $rustcPath = Resolve-SpeechekExecutable -Name 'rustc'
    if (-not $rustcPath) { throw 'rustc was not found on PATH.' }
    $rustToolchain = Resolve-SpeechekRustToolchain -RepoRoot $RepoRoot `
        -RequiredVersion $rustVersion -RequiredHost $targetTriple -RustcPath $rustcPath

    $rustEnvironment = $null
    if (-not [string]::IsNullOrWhiteSpace($rustToolchain)) {
        $rustEnvironment = New-SpeechekEnvironment @{ RUSTUP_TOOLCHAIN = $rustToolchain }
    }
    $cargoResult = Get-SpeechekProcessResult -FilePath 'cargo' -Arguments @('--version') -Environment $rustEnvironment
    if ($cargoResult.ExitCode -ne 0) { throw "cargo is not available: $($cargoResult.StandardError.Trim())" }

    $toolchainPath = Join-Path $RepoRoot 'rust-toolchain.toml'
    if (-not (Test-Path -LiteralPath $toolchainPath -PathType Leaf)) {
        throw 'rust-toolchain.toml is missing.'
    }
    $toolchain = Get-Content -LiteralPath $toolchainPath -Raw
    if ($toolchain -notmatch ('(?m)^channel\s*=\s*"' + [regex]::Escape($rustVersion) + '"\s*$')) {
        throw ('rust-toolchain.toml does not pin channel = "' + $rustVersion + '".')
    }

    $bunOutput = (& $BunExe --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $bunOutput -ne $bunVersion) {
        throw "expected Bun $bunVersion, got '$bunOutput' from $BunExe."
    }

    $cliPackage = Join-Path $RepoRoot 'node_modules\@tauri-apps\cli\package.json'
    if (-not (Test-Path -LiteralPath $cliPackage -PathType Leaf)) {
        throw "tauri-cli is not installed; run 'bun install --frozen-lockfile' first."
    }
    $installed = [string](Get-Content -LiteralPath $cliPackage -Raw | ConvertFrom-Json).version
    if ($installed -ne $tauriCliVersion) {
        throw "expected tauri-cli $tauriCliVersion, found $installed in node_modules."
    }

    return $rustToolchain
 }

function Assert-SpeechekFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$What
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$What is missing: $Path"
    }
}

function Assert-SpeechekExeNotLocked {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return }
    try {
        $probe = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
        $probe.Dispose()
    }
    catch {
        throw ("$Path is locked by a running process. Close the running debug/Test Speechek and retry; " +
               "this script never stops it for you.")
    }
}

function Assert-SpeechekPayloadDefines {
    param([Parameter(Mandatory = $true)][string]$Path)
    $text = Get-Content -LiteralPath $Path -Raw
    $names = @(
        'SPEECHEK_PAYLOAD_EXE',
        'SPEECHEK_PAYLOAD_EXE_SHA256',
        'SPEECHEK_PAYLOAD_VERSION',
        'SPEECHEK_PAYLOAD_VERSIONWITHBUILD',
        'SPEECHEK_PAYLOAD_LICENSE',
        'SPEECHEK_PAYLOAD_LICENSE_SHA256',
        'SPEECHEK_PAYLOAD_NOTICES',
        'SPEECHEK_PAYLOAD_NOTICES_SHA256',
        'SPEECHEK_TEST_INSTALLER'
    )
    foreach ($name in $names) {
        if ($text -notmatch ('(?m)^!define\s+' + [regex]::Escape($name) + '\s')) {
            throw "generated payload.nsh does not define $name (the NSIS template consumes it)."
        }
    }
    if ($text -notmatch '(?m)^!define\s+SPEECHEK_TEST_INSTALLER\s+1\s*$') {
        throw 'generated payload.nsh must set SPEECHEK_TEST_INSTALLER to 1 for the test fixture.'
    }
}

Write-Host '== Speechek test installer fixture =='
Write-Host "repository : $repoRoot"
Write-Host "version    : $Version"
Write-Host "identity   : $fixtureProductName / $fixtureIdentifier"

# 1. Tools.
Assert-SpeechekNoTargetOverride
Assert-SpeechekFile -Path $licensePath -What 'LICENSE'
Assert-SpeechekFile -Path $noticesPath -What 'third_party/THIRD-PARTY-NOTICES.txt (run scripts/collect-notices.ps1)'
Assert-SpeechekFile -Path $commonSource -What 'src-tauri/nsis/common.nsh'
Assert-SpeechekFile -Path $templatePath -What 'src-tauri/nsis/installer.nsi'
Assert-SpeechekFile -Path $englishLang -What 'src-tauri/nsis/languages/English.nsh'
Assert-SpeechekFile -Path $russianLang -What 'src-tauri/nsis/languages/Russian.nsh'
Assert-SpeechekFile -Path $iconPng -What 'src-tauri/icons/icon.png'
Assert-SpeechekFile -Path $iconIco -What 'src-tauri/icons/icon.ico'

$bunExe = Resolve-SpeechekBun $repoRoot
Write-Host "bun        : $bunExe"
$rustToolchain = Assert-SpeechekFixturePrerequisites -RepoRoot $repoRoot -BunExe $bunExe
$rustLabel = if ([string]::IsNullOrWhiteSpace($rustToolchain)) { 'rust-toolchain.toml' } else { $rustToolchain }
Write-Host "rust       : $rustVersion via $rustLabel"

# 2. Visual Studio x64 developer environment.
$vs = Get-SpeechekVsEnvironment $repoRoot
Write-Host "vs         : $($vs.InstallPath)"

# 3. The temporary fixture configuration. Absolute paths: a --config file in
#    the build output must not resolve its resource paths against its own folder.
$versionWithBuild = Get-SpeechekVersionWithBuild $Version
$fixtureConfig = [ordered]@{
    '$schema'    = 'https://schema.tauri.app/config/2'
    productName  = $fixtureProductName
    version      = $Version
    identifier   = $fixtureIdentifier
    bundle       = [ordered]@{
        licenseFile = $licensePath
        icon        = @($iconPng, $iconIco)
        windows     = [ordered]@{
            nsis = [ordered]@{
                template           = $templatePath
                customLanguageFiles = [ordered]@{
                    English = $englishLang
                    Russian = $russianLang
                }
            }
        }
    }
}
$fixtureJson = ConvertTo-Json -InputObject $fixtureConfig -Depth 8 -Compress
Write-SpeechekUtf8NoBom -Path $configPath -Text ($fixtureJson + "`n")
Write-Host "config     : $configPath"

# 4. Debug build with the test-provider feature and the fixture config.
Assert-SpeechekExeNotLocked -Path $exePath

$cargoSnapshot = [System.IO.File]::ReadAllBytes($cargoManifest)
$lockSnapshot = [System.IO.File]::ReadAllBytes($cargoLock)

$cargoEnvironment = New-SpeechekEnvironment @{}
foreach ($key in $vs.Environment.Keys) { $cargoEnvironment[$key] = $vs.Environment[$key] }
if (-not [string]::IsNullOrWhiteSpace($rustToolchain)) { $cargoEnvironment['RUSTUP_TOOLCHAIN'] = $rustToolchain }
$cargoEnvironment['TAURI_CONFIG'] = $fixtureJson

Write-Host '-- cargo build --locked --no-default-features --features test-provider (debug)'
$cargoExit = Invoke-SpeechekProcess -FilePath 'cargo' `
    -Arguments @('build', '--manifest-path', 'src-tauri/Cargo.toml', '--locked', '--no-default-features', '--features', 'test-provider') `
    -WorkingDirectory $repoRoot -Environment $cargoEnvironment
if ($cargoExit -ne 0) {
    throw ("cargo build failed with exit code $cargoExit. If src-tauri/target/debug/speechek.exe " +
           'is locked by a running debug/Test Speechek, close it and retry.')
}
if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
    throw "the fixture build did not produce $exePath."
}

# 5. The build must not have rewritten the manifests.
if (-not (Test-SpeechekBytesEqual $cargoSnapshot ([System.IO.File]::ReadAllBytes($cargoManifest)))) {
    throw 'cargo build rewrote src-tauri/Cargo.toml; the fixture build is not acceptable.'
}
if (-not (Test-SpeechekBytesEqual $lockSnapshot ([System.IO.File]::ReadAllBytes($cargoLock)))) {
    throw 'cargo build rewrote src-tauri/Cargo.lock; the fixture build is not acceptable.'
}

$exeSha = Get-SpeechekSha256 $exePath
Write-Host "exe sha256 : $exeSha"

# 6. Stage the include files the custom NSIS template expects.
Copy-Item -LiteralPath $commonSource -Destination (Join-Path $outDir 'common.nsh') -Force
New-SpeechekPayloadNsh -Path $payloadPath `
    -Exe $exePath -Version $Version -VersionWithBuild $versionWithBuild `
    -License $licensePath -Notices $noticesPath `
    -ExeSha256 $exeSha -LicenseSha256 (Get-SpeechekSha256 $licensePath) -NoticesSha256 (Get-SpeechekSha256 $noticesPath) `
    -TestInstaller 1
# The shared writer names build-release.ps1; this artifact is produced by the
# fixture script, so correct the generator line (the define set is unchanged).
$payloadText = Get-Content -LiteralPath $payloadPath -Raw
$payloadText = $payloadText -replace '(?m)^; Generated by scripts/build-release\.ps1', '; Generated by scripts/build-test-installer.ps1'
Write-SpeechekUtf8NoBom -Path $payloadPath -Text $payloadText
Assert-SpeechekPayloadDefines -Path $payloadPath
Write-Host "payload    : $payloadPath"
Write-Host "common     : $(Join-Path $outDir 'common.nsh')"

# 7. Bundle the already-built debug executable.
$exeHashBeforeBundle = Get-SpeechekSha256 $exePath
Write-Host '-- tauri bundle --debug --bundles nsis --no-binary-patching --no-sign --ci'
$bundleExit = Invoke-SpeechekProcess -FilePath $bunExe `
    -Arguments @('run', 'tauri', 'bundle', '--debug', '--bundles', 'nsis', '--features', 'test-provider',
                 '--config', $configPath, '--no-binary-patching', '--no-sign', '--ci') `
    -WorkingDirectory $repoRoot -Environment $cargoEnvironment
if ($bundleExit -ne 0) { throw "tauri bundle failed with exit code $bundleExit." }

# 8. The bundle must not have touched the executable or the manifests.
if ((Get-SpeechekSha256 $exePath) -ne $exeHashBeforeBundle) {
    throw 'tauri bundle modified the built fixture executable (expected --no-binary-patching to preserve it).'
}
if (-not (Test-SpeechekBytesEqual $cargoSnapshot ([System.IO.File]::ReadAllBytes($cargoManifest)))) {
    throw 'tauri bundle rewrote src-tauri/Cargo.toml; the fixture bundle is not acceptable.'
}
if (-not (Test-SpeechekBytesEqual $lockSnapshot ([System.IO.File]::ReadAllBytes($cargoLock)))) {
    throw 'tauri bundle rewrote src-tauri/Cargo.lock; the fixture bundle is not acceptable.'
}

# 9. Locate the real installer and write the fixture manifest.
if (-not (Test-Path -LiteralPath $bundleOutDir)) {
    throw "the bundle output directory was not produced: $bundleOutDir"
}
$expectedName = '{0}_{1}_x64-setup.exe' -f $fixtureProductName, $Version
$installerPath = Join-Path $bundleOutDir $expectedName
if (-not (Test-Path -LiteralPath $installerPath -PathType Leaf)) {
    $candidates = @(Get-ChildItem -LiteralPath $bundleOutDir -Filter '*.exe' |
        Where-Object { $_.Name -like ('*' + $Version + '*-setup.exe') })
    if ($candidates.Count -eq 1) {
        $installerPath = $candidates[0].FullName
    }
    else {
        $found = @(Get-ChildItem -LiteralPath $bundleOutDir -Filter '*.exe' | Select-Object -ExpandProperty Name)
        throw "expected fixture installer '$expectedName' was not produced. Found: $($found -join ', ')"
    }
}
$installerName = Split-Path -Leaf $installerPath
$installerSha = Get-SpeechekSha256 $installerPath

$manifest = [ordered]@{
    schemaVersion    = 1
    productName      = $fixtureProductName
    identifier       = $fixtureIdentifier
    mainBinaryName   = $fixtureMainBinaryName
    version          = $Version
    versionWithBuild = $versionWithBuild
    flavor           = 'test'
    target           = $targetTriple
    rust             = $rustVersion
    bun              = $bunVersion
    tauriCli         = $tauriCliVersion
    exe              = [ordered]@{ file = ($fixtureMainBinaryName + '.exe'); sha256 = $exeSha }
    installer        = [ordered]@{ file = $installerName; sha256 = $installerSha }
    installDir       = '$LOCALAPPDATA\Speechek Test'
    profileDir       = '$APPDATA\Speechek-Test'
}
Write-SpeechekReleaseManifest -Path $manifestPath -Manifest $manifest

Write-Host ''
Write-Host '== Test installer fixture complete =='
Write-Host "installer  : $installerPath"
Write-Host "installer  : $installerSha"
Write-Host "manifest   : $manifestPath"
Write-Host ''
Write-Host 'Upgrade regression: run this script once with 0.2.0 and once with 0.2.1'
Write-Host 'from the same source state, without editing any production manifest.'
