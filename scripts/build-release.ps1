<#
    scripts/build-release.ps1

    Hermetic production build of the Speechek Windows installer (release plan
    section 5; contract in docs/releasing.md).

    It performs, in order:
      1. a non-bypassable clean-source gate and the version-mirror check;
      2. pinned tool checks (Rust 1.97.1, Bun 1.4.2, tauri-cli 2.12.0);
      3. a Bun install with --frozen-lockfile;
      4. the x64 Visual Studio developer environment (vswhere + VsDevCmd);
      5. `cargo build --locked --release --no-default-features` with
         `-C target-feature=+crt-static` applied only to that child process;
      6. an x64 PE / static-CRT import check on the produced executable;
      7. the generated NSIS payload include and the shared common.nsh staged
         into the ignored src-tauri/target/release area;
      8. `tauri bundle --bundles nsis --no-binary-patching --no-sign --ci`;
      9. the installer plus SHA256SUMS.txt and release-manifest.json.

    The script takes no profile/provider/target-dir arguments and never stops a
    running Speechek: if the output executable is locked, close the app and
    retry. There is no second target directory or fallback release folder.

    PowerShell 5.1 and 7 compatible.
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'release-common.ps1')

$rustVersion = '1.97.1'
$bunVersion = '1.4.2'
$tauriCliVersion = '2.12.0'
$targetTriple = 'x86_64-pc-windows-msvc'

$repoRoot = Get-SpeechekRepoRoot $PSScriptRoot
$tauriDir = Join-Path $repoRoot 'src-tauri'
$outDir = Join-Path $tauriDir 'target\release'   # fixed canonical artifact path
$exePath = Join-Path $outDir 'speechek.exe'
$cargoManifest = Join-Path $tauriDir 'Cargo.toml'
$noticesPath = Join-Path $repoRoot 'third_party\THIRD-PARTY-NOTICES.txt'
$licensePath = Join-Path $repoRoot 'LICENSE'
$commonSource = Join-Path $tauriDir 'nsis\common.nsh'
$bundleOutDir = Join-Path $outDir 'bundle\nsis'

function Assert-SpeechekNoTargetOverride {
    foreach ($name in @('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_TARGET')) {
        $value = [System.Environment]::GetEnvironmentVariable($name)
        if (-not [string]::IsNullOrWhiteSpace($value)) {
            throw "$name is set ('$value'); release builds use the canonical target directory only."
        }
    }
}

function Assert-SpeechekToolVersions {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [Parameter(Mandatory = $true)][string]$BunExe
    )

    $rustcPath = Resolve-SpeechekExecutable -Name 'rustc'
    if (-not $rustcPath) { throw "rustc was not found on PATH; install rustup with Rust $rustVersion." }
    $rustOverride = Resolve-SpeechekRustToolchain -RepoRoot $RepoRoot -RequiredVersion $rustVersion `
        -RequiredHost $targetTriple -RustcPath $rustcPath
    if ($rustOverride) {
        Write-Warning ("the pinned Rust $rustVersion toolchain is unavailable; using toolchain '$rustOverride' (verified rustc $rustVersion).")
    }

    $cargoPath = Resolve-SpeechekExecutable -Name 'cargo'
    if (-not $cargoPath) { throw "cargo was not found on PATH; install rustup." }
    $checkEnvironment = $null
    if ($rustOverride) { $checkEnvironment = New-SpeechekEnvironment @{ RUSTUP_TOOLCHAIN = $rustOverride } }
    $cargoCheck = Get-SpeechekProcessResult -FilePath $cargoPath -Arguments @('--version') `
        -WorkingDirectory $RepoRoot -Environment $checkEnvironment
    if ($cargoCheck.ExitCode -ne 0) { throw "cargo is not usable: $($cargoCheck.StandardError.Trim())" }

    $toolchainPath = Join-Path $RepoRoot 'rust-toolchain.toml'
    if (-not (Test-Path -LiteralPath $toolchainPath -PathType Leaf)) {
        throw "rust-toolchain.toml is missing."
    }
    $toolchain = Get-Content -LiteralPath $toolchainPath -Raw
    if ($toolchain -notmatch ('(?m)^channel\s*=\s*"' + [regex]::Escape($rustVersion) + '"\s*$')) {
        throw ('rust-toolchain.toml does not pin channel = "' + $rustVersion + '".')
    }
    if ($toolchain -notmatch [regex]::Escape($targetTriple)) {
        throw "rust-toolchain.toml does not list the $targetTriple target."
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
    $declared = (Get-Content -LiteralPath (Join-Path $RepoRoot 'package.json') -Raw | ConvertFrom-Json).devDependencies.'@tauri-apps/cli'
    if ($declared -ne $tauriCliVersion) {
        throw "package.json declares '@tauri-apps/cli' as $declared instead of the pinned $tauriCliVersion."
    }
    return $rustOverride
}

Write-Host '== Speechek production release build =='
Write-Host "repository : $repoRoot"

# 1. Clean source gate (never bypassable) and version mirrors.
Assert-SpeechekCleanTree $repoRoot
$commit = Get-SpeechekGitCommit $repoRoot
$version = Assert-SpeechekVersionMirrors $repoRoot
Write-Host "commit     : $commit"
Write-Host "version    : $version"

# 2. Tools.
Assert-SpeechekNoTargetOverride
$bunExe = Resolve-SpeechekBun $repoRoot
Write-Host "bun        : $bunExe"

# 3. Frozen dependency install (writes only ignored node_modules).
Write-Host '-- bun install --frozen-lockfile'
$bunInstall = Invoke-SpeechekProcess -FilePath $bunExe -Arguments @('install', '--frozen-lockfile') -WorkingDirectory $repoRoot
if ($bunInstall -ne 0) { throw "bun install --frozen-lockfile failed with exit code $bunInstall." }

$rustOverride = Assert-SpeechekToolVersions -RepoRoot $repoRoot -BunExe $bunExe
$cargoExe = Resolve-SpeechekExecutable -Name 'cargo'

# 4. Visual Studio x64 developer environment.
$vs = Get-SpeechekVsEnvironment $repoRoot
Write-Host "vs         : $($vs.InstallPath)"

# 5. Release build with a statically linked CRT for this child only.
if (Test-Path -LiteralPath $exePath -PathType Leaf) {
    try {
        $probe = [System.IO.File]::Open($exePath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
        $probe.Dispose()
    }
    catch {
        throw ("$exePath is locked by a running process. Close the running Speechek and retry; " +
               "the release script never stops it for you.")
    }
}

$cargoEnvironment = New-SpeechekEnvironment @{}
foreach ($key in $vs.Environment.Keys) { $cargoEnvironment[$key] = $vs.Environment[$key] }
$cargoEnvironment['RUSTFLAGS'] = '-C target-feature=+crt-static'
if ($rustOverride) { $cargoEnvironment['RUSTUP_TOOLCHAIN'] = $rustOverride }

Write-Host '-- cargo build --locked --release --no-default-features'
$cargoExit = Invoke-SpeechekProcess -FilePath $cargoExe `
    -Arguments @('build', '--manifest-path', 'src-tauri/Cargo.toml', '--locked', '--release', '--no-default-features') `
    -WorkingDirectory $repoRoot -Environment $cargoEnvironment
if ($cargoExit -ne 0) {
    throw ("cargo build failed with exit code $cargoExit. If src-tauri/target/release/speechek.exe " +
           "is locked by a running Speechek, close it and retry.")
}
if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
    throw "the release build did not produce $exePath."
}

# 6. Prove x64 + static CRT.
$dumpbin = Resolve-SpeechekExecutable -Name 'dumpbin' -Environment $vs.Environment
if (-not $dumpbin) { throw "dumpbin.exe was not found in the Visual Studio x64 environment." }
Assert-SpeechekX64StaticBinary -ExePath $exePath -DumpbinPath $dumpbin -WorkingDirectory $repoRoot -Environment $vs.Environment
$exeSha = Get-SpeechekSha256 $exePath
Write-Host "exe sha256 : $exeSha"

# 7. Stage the include files the custom NSIS template expects.
if (-not (Test-Path -LiteralPath $noticesPath -PathType Leaf)) {
    throw "third_party/THIRD-PARTY-NOTICES.txt is missing; run scripts/collect-notices.ps1 first."
}
Copy-Item -LiteralPath $commonSource -Destination (Join-Path $outDir 'common.nsh') -Force
New-SpeechekPayloadNsh -Path (Join-Path $outDir 'payload.nsh') `
    -Exe $exePath -Version $version -VersionWithBuild (Get-SpeechekVersionWithBuild $version) `
    -License $licensePath -Notices $noticesPath `
    -ExeSha256 $exeSha -LicenseSha256 (Get-SpeechekSha256 $licensePath) -NoticesSha256 (Get-SpeechekSha256 $noticesPath) `
    -TestInstaller 0

# 8. Bundle. tauri-cli must not rewrite Cargo.toml and must not touch the EXE.
$cargoSnapshot = [System.IO.File]::ReadAllBytes($cargoManifest)
$exeHashBeforeBundle = Get-SpeechekSha256 $exePath

function Invoke-SpeechekBundle {
    Write-Host '-- tauri bundle --bundles nsis --no-binary-patching --no-sign --ci'
    $exit = Invoke-SpeechekProcess -FilePath $bunExe `
        -Arguments @('run', 'tauri', 'bundle', '--bundles', 'nsis', '--no-binary-patching', '--no-sign', '--ci') `
        -WorkingDirectory $repoRoot -Environment $vs.Environment
    if ($exit -ne 0) { throw "tauri bundle failed with exit code $exit." }
}

Invoke-SpeechekBundle

if ((Get-SpeechekSha256 $exePath) -ne $exeHashBeforeBundle) {
    throw "tauri bundle modified the built executable; publication is blocked (expected --no-binary-patching to preserve it)."
}

$cargoAfter = [System.IO.File]::ReadAllBytes($cargoManifest)
if (-not (Test-SpeechekBytesEqual $cargoSnapshot $cargoAfter)) {
    [System.IO.File]::WriteAllBytes($cargoManifest, $cargoSnapshot)
    Write-Warning 'tauri bundle rewrote src-tauri/Cargo.toml; the snapshot was restored.'
    if (-not (Repair-SpeechekTauriInlineTable -CargoTomlPath $cargoManifest)) {
        throw ("the tauri CLI rewrote src-tauri/Cargo.toml and the tauri dependency is already a table; " +
               "publication is blocked because there is nothing to normalize.")
    }
    Write-Warning 'Normalized the tauri dependency to [dependencies.tauri]; retrying the bundle once.'
    $cargoSnapshot = [System.IO.File]::ReadAllBytes($cargoManifest)
    Invoke-SpeechekBundle
    if ((Get-SpeechekSha256 $exePath) -ne $exeHashBeforeBundle) {
        throw "tauri bundle modified the built executable on the retry; publication is blocked."
    }
    if (-not (Test-SpeechekBytesEqual $cargoSnapshot ([System.IO.File]::ReadAllBytes($cargoManifest)))) {
        throw ("the tauri CLI still rewrites src-tauri/Cargo.toml after normalization; publication is blocked. " +
               "Commit the normalized Cargo.toml before the next build.")
    }
}

# 9. Locate the real installer and write the checksum + manifest.
if (-not (Test-Path -LiteralPath $bundleOutDir)) { throw "the bundle output directory was not produced: $bundleOutDir" }
$expectedInstaller = Join-Path $bundleOutDir ("Speechek_{0}_x64-setup.exe" -f $version)
if (-not (Test-Path -LiteralPath $expectedInstaller -PathType Leaf)) {
    $found = @(Get-ChildItem -LiteralPath $bundleOutDir -Filter '*.exe' | Select-Object -ExpandProperty Name)
    throw "expected installer '$expectedInstaller' was not produced. Found: $($found -join ', ')"
}
$installerName = Split-Path -Leaf $expectedInstaller
$installerSha = Get-SpeechekSha256 $expectedInstaller

$runId = $null
if (-not [string]::IsNullOrWhiteSpace($env:GITHUB_RUN_ID)) { $runId = [string]$env:GITHUB_RUN_ID }

$manifest = [ordered]@{
    schemaVersion = 1
    commit        = $commit
    version       = $version
    flavor        = 'production'
    target        = $targetTriple
    rust          = $rustVersion
    bun           = $bunVersion
    tauriCli      = $tauriCliVersion
    runId         = $runId
    exe           = [ordered]@{ file = 'speechek.exe'; sha256 = $exeSha }
    installer     = [ordered]@{ file = $installerName; sha256 = $installerSha }
}

Write-SpeechekSha256Sums -Path (Join-Path $bundleOutDir 'SHA256SUMS.txt') -Hash $installerSha -BaseName $installerName
Write-SpeechekReleaseManifest -Path (Join-Path $bundleOutDir 'release-manifest.json') -Manifest $manifest

Write-Host ''
Write-Host '== Release build complete =='
Write-Host "installer  : $expectedInstaller"
Write-Host "installer  : $installerSha"
Write-Host "sums       : $(Join-Path $bundleOutDir 'SHA256SUMS.txt')"
Write-Host "manifest   : $(Join-Path $bundleOutDir 'release-manifest.json')"
