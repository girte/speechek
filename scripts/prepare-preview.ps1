<#
    scripts/prepare-preview.ps1

    Builds the portable owner-preview Development executable and writes the
    preview manifest that publish-tag.ps1 requires before it will dispatch a
    candidate.

    It performs, in order:
      1. the non-bypassable clean-source gate and the version-mirror check;
      2. an exact Rust 1.97.1 / x86_64-pc-windows-msvc toolchain resolution;
      3. the Visual Studio x64 developer environment (vswhere + VsDevCmd);
      4. `cargo check --locked --no-default-features`;
         then localization source checks and the Git changed-message report;
      5. `cargo test --locked --no-default-features --features test-provider`
         (GUI-free unit tests only; that feature is never used by the final
         build);
      6. `cargo build --locked --no-default-features` - the Development debug
         profile, with no --release and no bundle/installer step, into the
         canonical src-tauri/target/debug/speechek.exe;
      7. a clean-source / same-HEAD recheck, then
         src-tauri/target/debug/preview-manifest.json (schemaVersion 1, full
         commit, version mirrors, flavor=development, exe file + sha256).

    The cargo children receive the Visual Studio x64 environment with
    RUSTFLAGS, CARGO_ENCODED_RUSTFLAGS, TAURI_CONFIG, SPEECHEK_CONFIG_PATH,
    SPEECHEK_TEST_PROVIDER_HTTP, SPEECHEK_TEST_PROVIDER_WSS and
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS removed, plus RUSTUP_TOOLCHAIN only
    when rust-toolchain.toml cannot resolve the required rustc. The caller's
    environment is never modified.

    The script takes no profile/provider/target-dir arguments, never runs
    `cargo clean`, never deletes src-tauri/target/debug (the Development
    settings.json and secrets.bin live there), never starts the executable and
    never touches an installer. If src-tauri/target/debug/speechek.exe is
    locked by a running Dev/Test build, close it and retry; the script never
    stops or copies it.

    PowerShell 5.1 and 7 compatible.
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'release-common.ps1')

$rustVersion = '1.97.1'
$targetTriple = 'x86_64-pc-windows-msvc'
# Removed from every child map: the preview build never inherits build or
# runtime overrides from the caller or from the Visual Studio environment.
$clearedChildVariables = @(
    'RUSTFLAGS',
    'CARGO_ENCODED_RUSTFLAGS',
    'TAURI_CONFIG',
    'SPEECHEK_CONFIG_PATH',
    'SPEECHEK_TEST_PROVIDER_HTTP',
    'SPEECHEK_TEST_PROVIDER_WSS',
    'WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS'
)

$repoRoot = Get-SpeechekRepoRoot $PSScriptRoot
$tauriDir = Join-Path $repoRoot 'src-tauri'
$outDir = Join-Path $tauriDir 'target\debug'   # fixed canonical Development artifact path
$exePath = Join-Path $outDir 'speechek.exe'
$manifestPath = Join-Path $outDir 'preview-manifest.json'
$script:rustToolchain = $null

function Assert-SpeechekNoTargetOverride {
    # Same canonical-target guard as build-release.ps1 / build-test-installer.ps1.
    foreach ($name in @('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_TARGET')) {
        $value = [System.Environment]::GetEnvironmentVariable($name)
        if (-not [string]::IsNullOrWhiteSpace($value)) {
            throw "$name is set ('$value'); the preview build uses the canonical target directory only."
        }
    }
}

# Full child environment: the Visual Studio x64 environment over the caller's,
# with the override variables removed and the resolved toolchain pinned only in
# the child map.
function New-SpeechekPreviewEnvironment {
    param([Parameter(Mandatory = $true)]$Vs)

    $child = New-SpeechekEnvironment @{}
    foreach ($key in $Vs.Environment.Keys) { $child[$key] = $Vs.Environment[$key] }
    foreach ($name in $clearedChildVariables) { [void]$child.Remove($name) }
    if (-not [string]::IsNullOrWhiteSpace($script:rustToolchain)) {
        $child['RUSTUP_TOOLCHAIN'] = $script:rustToolchain
    }
    return $child
}

# Runs one cargo step with captured output (CreateNoWindow) and relays it to the
# current console; the caller's environment is unchanged.
function Invoke-SpeechekCargoStep {
    param(
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment
    )

    Write-Host ('-- cargo ' + ($Arguments -join ' '))
    $result = Get-SpeechekProcessResult -FilePath $cargoPath -Arguments $Arguments `
        -WorkingDirectory $repoRoot -Environment $Environment
    if (-not [string]::IsNullOrWhiteSpace($result.StandardOutput)) {
        Write-Host $result.StandardOutput.TrimEnd()
    }
    if (-not [string]::IsNullOrWhiteSpace($result.StandardError)) {
        [Console]::Error.WriteLine($result.StandardError.TrimEnd())
    }
    if ($result.ExitCode -ne 0) {
        throw ("cargo $Label failed with exit code $($result.ExitCode). If $exePath is locked by a running " +
               "Speechek Dev/Test build, close it and retry; the preview script never stops it for you.")
    }
}

Write-Host '== Speechek portable preview build =='
Write-Host "repository : $repoRoot"

# 1. Clean source gate (never bypassable) and version mirrors.
Assert-SpeechekCleanTree $repoRoot
$commit = Get-SpeechekGitCommit $repoRoot
$version = Assert-SpeechekVersionMirrors $repoRoot
Write-Host "commit     : $commit"
Write-Host "version    : $version"

# 2. Rust exactly 1.97.1 / x86_64-pc-windows-msvc, pinned for the children only.
Assert-SpeechekNoTargetOverride
$rustcPath = Resolve-SpeechekExecutable -Name 'rustc'
if (-not $rustcPath) { throw "rustc was not found on PATH; install rustup with Rust $rustVersion." }
$script:rustToolchain = Resolve-SpeechekRustToolchain -RepoRoot $repoRoot -RequiredVersion $rustVersion `
    -RequiredHost $targetTriple -RustcPath $rustcPath
if ($script:rustToolchain) {
    Write-Warning ("the pinned Rust $rustVersion toolchain is unavailable; using toolchain '$script:rustToolchain' (verified rustc $rustVersion).")
}
$cargoPath = Resolve-SpeechekExecutable -Name 'cargo'
if (-not $cargoPath) { throw "cargo was not found on PATH; install rustup." }

# 3. Visual Studio x64 developer environment.
$vs = Get-SpeechekVsEnvironment $repoRoot
Write-Host "vs         : $($vs.InstallPath)"
$childEnvironment = New-SpeechekPreviewEnvironment -Vs $vs

# The cargo probe and every build step use the same child map.
$cargoProbe = Get-SpeechekProcessResult -FilePath $cargoPath -Arguments @('--version') `
    -WorkingDirectory $repoRoot -Environment $childEnvironment
if ($cargoProbe.ExitCode -ne 0) { throw "cargo is not usable: $($cargoProbe.StandardError.Trim())" }

# A failed build must never leave a stale handoff manifest behind. Only the
# manifest is removed here: settings.json / secrets.bin beside the EXE and the
# rest of target/debug are never touched.
if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
    Remove-Item -LiteralPath $manifestPath -Force
}

# 4-6. GUI-free check, the unit tests, then the final Development build last so
# the test-provider feature does not remain in the canonical debug executable.
$cargoManifestArgument = 'src-tauri/Cargo.toml'
if (Test-Path -LiteralPath $exePath -PathType Leaf) {
    try {
        $probe = [System.IO.File]::Open($exePath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
        $probe.Dispose()
    }
    catch {
        throw ("$exePath is locked by a running process. Close the running Speechek Dev/Test build and retry; " +
               "the preview script never stops it for you.")
    }
}

Invoke-SpeechekCargoStep -Label 'check' -Environment $childEnvironment `
    -Arguments @('check', '--manifest-path', $cargoManifestArgument, '--locked', '--no-default-features')
# Catalog validation already ran in build.rs above; reuse it, not a second schema.
. (Join-Path $PSScriptRoot 'check-localization.ps1')
Invoke-SpeechekLocalizationSources -RepoRoot $repoRoot
Invoke-SpeechekCargoStep -Label 'test' -Environment $childEnvironment `
    -Arguments @('test', '--manifest-path', $cargoManifestArgument, '--locked', '--no-default-features', '--features', 'test-provider')
Invoke-SpeechekCargoStep -Label 'build' -Environment $childEnvironment `
    -Arguments @('build', '--manifest-path', $cargoManifestArgument, '--locked', '--no-default-features')

if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
    throw "the preview build did not produce $exePath."
}

# 7. The manifest may only describe a build of the committed HEAD that was just
# verified: recheck the clean tree, the HEAD and the version mirrors.
Assert-SpeechekCleanTree $repoRoot
$commitAfter = Get-SpeechekGitCommit $repoRoot
if ($commitAfter -ne $commit) {
    throw "the source HEAD changed during the preview build ($commit -> $commitAfter); the manifest is not written."
}
$versionAfter = Assert-SpeechekVersionMirrors $repoRoot
if ($versionAfter -ne $version) {
    throw "the application version changed during the preview build ($version -> $versionAfter); the manifest is not written."
}

$exeSha = Get-SpeechekSha256 $exePath
$manifest = [ordered]@{
    schemaVersion = 1
    commit        = $commit
    version       = $version
    flavor        = 'development'
    exe           = [ordered]@{ file = 'speechek.exe'; sha256 = $exeSha }
}
Write-SpeechekReleaseManifest -Path $manifestPath -Manifest $manifest

Write-Host ''
Write-Host '== Portable preview ready =='
Write-Host "exe        : file:///$($exePath.Replace('\', '/'))"
Write-Host "commit     : $commit"
Write-Host "version    : $version"
Write-Host "sha256     : $exeSha"
Write-Host "manifest   : $manifestPath"
Write-Host 'The manifest describes this executable only; settings.json and secrets.bin beside it stay untouched.'
