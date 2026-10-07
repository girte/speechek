<#
    GUI-free localization control. Reuses build.rs catalog validation through
    cargo check, then checks UI source references and reports changed messages.
    No application launch, translation statuses or additional owner approval.
    Dot-source to reuse Invoke-SpeechekLocalizationSources after a cargo check.
    PowerShell 5.1 and 7 compatible.
#>
[CmdletBinding()]
param([string]$BaseRef = '')

function Invoke-SpeechekLocalizationSources {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [string]$BaseRef = ''
    )
    . (Join-Path $RepoRoot 'scripts/release-common.ps1')
    $bun = Resolve-SpeechekBun $RepoRoot
    # Frozen install prepares only ignored development dependencies. It cannot
    # rewrite package.json/bun.lock or run dependency lifecycle scripts.
    $code = Invoke-SpeechekProcess -FilePath $bun -Arguments @('install', '--frozen-lockfile', '--ignore-scripts') -WorkingDirectory $RepoRoot
    if ($code -ne 0) { throw "Localization development dependency install failed (exit $code)." }
    $code = Invoke-SpeechekProcess -FilePath $bun -WorkingDirectory $RepoRoot `
        -Arguments @('test', './scripts/localization-sources.test.mjs', './scripts/localization-report.test.mjs')
    if ($code -ne 0) { throw "Localization checker regressions failed (exit $code)." }
    $arguments = @((Join-Path $RepoRoot 'scripts/check-localization.mjs'))
    if (-not [string]::IsNullOrWhiteSpace($BaseRef)) { $arguments += @('--base', $BaseRef) }
    $code = Invoke-SpeechekProcess -FilePath $bun -Arguments $arguments -WorkingDirectory $RepoRoot
    if ($code -ne 0) { throw "Localization source checks failed (exit $code)." }
}

if ($MyInvocation.InvocationName -ne '.') {
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'
    try {
        . (Join-Path $PSScriptRoot 'release-common.ps1')
        $repoRoot = Get-SpeechekRepoRoot $PSScriptRoot
        foreach ($name in @('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_TARGET')) {
            if (-not [string]::IsNullOrWhiteSpace([System.Environment]::GetEnvironmentVariable($name))) {
                throw "$name overrides the canonical target directory; unset it before checking localization."
            }
        }
        $rustc = Resolve-SpeechekExecutable -Name 'rustc'
        $cargo = Resolve-SpeechekExecutable -Name 'cargo'
        if (-not $rustc -or -not $cargo) { throw 'Rust/cargo were not found on PATH.' }
        $toolchain = Resolve-SpeechekRustToolchain -RepoRoot $repoRoot -RequiredVersion '1.97.1' `
            -RequiredHost 'x86_64-pc-windows-msvc' -RustcPath $rustc
        $vs = Get-SpeechekVsEnvironment $repoRoot
        $child = New-SpeechekEnvironment @{}
        foreach ($name in $vs.Environment.Keys) { $child[$name] = $vs.Environment[$name] }
        foreach ($name in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'TAURI_CONFIG', 'SPEECHEK_CONFIG_PATH', `
                'SPEECHEK_TEST_PROVIDER_HTTP', 'SPEECHEK_TEST_PROVIDER_WSS', 'WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS')) {
            [void]$child.Remove($name)
        }
        if ($toolchain) { $child['RUSTUP_TOOLCHAIN'] = $toolchain }
        $code = Invoke-SpeechekProcess -FilePath $cargo -WorkingDirectory $repoRoot -Environment $child `
            -Arguments @('check', '--manifest-path', 'src-tauri/Cargo.toml', '--locked', '--no-default-features')
        if ($code -ne 0) { throw "Catalog/compiler validation failed (exit $code)." }
        Write-Host 'OK [localization-catalog] Existing Rust catalog and MessageId validation passed.'
        Invoke-SpeechekLocalizationSources -RepoRoot $repoRoot -BaseRef $BaseRef
    }
    catch {
        [Console]::Error.WriteLine(('FAIL [localization] ' + $_.Exception.Message))
        exit 2
    }
}
