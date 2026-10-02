<#
    scripts/check-release.ps1

    Source-side release gate for Speechek. It never builds and never edits the
    tree: it only reads the repository and, optionally, an already-built EXE.

    Checks (each fails closed with a concrete diagnostic):
      1. versions        - the three mirrored X.Y.Z values (tauri.conf.json is
                           authoritative) and, when -Version is given, the tag.
      2. features        - production must not enable `test-provider` (debug
                           fake-provider harness only) nor `custom-protocol`
                           (that is exactly what `tauri build` would inject; the
                           release path uses `tauri bundle` over a prebuilt EXE).
      3. lockfiles       - Cargo.lock and bun.lock are tracked and Cargo.lock
                           carries the speechek package entry (locked graph).
      4. notices         - third_party/THIRD-PARTY-NOTICES.txt exists and is
                           byte-identical to a fresh locked-graph run, via
                           scripts/collect-notices.ps1 -Check.
      5. public manifest - scripts/public-files.json is well-formed, every
                           listed path exists, and the set of tracked files is
                           exactly that allowlist (no unlisted/private file can
                           be published).
      6. capabilities    - build.rs command inventory == tauri handler list ==
                           generated permission files, and every capability
                           permission id resolves.
      7. index hygiene   - no forbidden private path and no secret pattern is
                           present in the git index (matched text is never
                           echoed).
      8. clean tree      - when -RequireCleanWorkTree (default), no tracked file
                           is staged or modified.
      9. PE (optional)   - with -BinaryPath: x64 PE32+ image whose import table
                           links no dynamic CRT (VCRUNTIME/MSVCP/ucrtbase/
                           api-ms-win-crt/...).

    Contract:
      * params : -RepoRoot, -Version <X.Y.Z>, -BinaryPath <exe>,
                 -RequireCleanWorkTree:<bool> (default $true)
      * stdout : one `OK   [check] ...` line per passing check and, on failure,
                 `FAIL [check] ...` lines plus a final BLOCK summary.
      * exit   : 0 = all checks passed, 2 = at least one check failed (closed),
                 1 = the harness itself could not run (no git, bad root).
      * PowerShell 5.1 and PowerShell 7 compatible; no external modules.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = '',
    [string]$Version = '',
    [string]$BinaryPath = '',
    [bool]$RequireCleanWorkTree = $true
)

$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($RepoRoot)) { $RepoRoot = Split-Path -Parent $PSScriptRoot }
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path
$script:Failures = New-Object System.Collections.Generic.List[string]
$script:Passes = New-Object System.Collections.Generic.List[string]
# Failure count when the current check started; a check only reports OK while
# it has added no failures of its own.
$script:CheckFailBaseline = 0

# ---------------------------------------------------------------------------
# Result helpers
# ---------------------------------------------------------------------------

function Add-Fail([string]$Check, [string]$Message) {
    $script:Failures.Add(("[$Check] " + $Message)) | Out-Null
}

function Add-Pass([string]$Check, [string]$Message) {
    $script:Passes.Add(("[$Check] " + $Message)) | Out-Null
}

function Run-Check([string]$Name, [scriptblock]$Body) {
    $script:CheckFailBaseline = $script:Failures.Count
    try {
        & $Body
    } catch {
        Add-Fail $Name ("unexpected error: " + $_.Exception.Message)
    }
}

# ---------------------------------------------------------------------------
# Small IO / git helpers
# ---------------------------------------------------------------------------

function Get-RepoPath([string]$Relative) {
    return [System.IO.Path]::Combine($RepoRoot, ($Relative -replace '/', [System.IO.Path]::DirectorySeparatorChar))
}

function Read-RepoText([string]$Relative) {
    $full = Get-RepoPath $Relative
    if (-not (Test-Path -LiteralPath $full -PathType Leaf)) { return $null }
    return [System.IO.File]::ReadAllText($full)
}

function Invoke-Git([string[]]$GitArgs) {
    $out = & git -C $RepoRoot @GitArgs 2>$null
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = @($out) }
}

function Get-TrackedFiles() {
    $raw = & git -C $RepoRoot ls-files -z 2>$null
    if ($LASTEXITCODE -ne 0) { throw "git ls-files failed (exit $LASTEXITCODE)" }
    $joined = [string]::Join("`0", @($raw))
    return @($joined -split "`0" | Where-Object { $_ -ne '' })
}

function Normalize-Relative([string]$Path) {
    return ($Path -replace '\\', '/').Trim()
}

# ---------------------------------------------------------------------------
# 1. Versions / tag
# ---------------------------------------------------------------------------

function Get-CargoPackageVersion([string]$Text) {
    $m = [regex]::Match($Text, '(?ms)^\[package\]\s*$(?<body>.*?)(?=^\[|\z)')
    if (-not $m.Success) { return $null }
    $v = [regex]::Match($m.Groups['body'].Value, '(?m)^\s*version\s*=\s*"([^"]+)"')
    if (-not $v.Success) { return $null }
    return $v.Groups[1].Value
}

function Get-CargoLockVersion([string]$Text, [string]$PackageName) {
    $lines = $Text -split "`n"
    for ($i = 0; $i -lt $lines.Length - 1; $i++) {
        if ($lines[$i].Trim() -eq ('name = "' + $PackageName + '"')) {
            $v = [regex]::Match($lines[$i + 1], '^version\s*=\s*"([^"]+)"')
            if ($v.Success) { return $v.Groups[1].Value }
        }
    }
    return $null
}

function Test-VersionFormat([string]$Value) {
    return ($Value -match '^\d+\.\d+\.\d+$')
}

function Check-Versions {
    $expected = $Version
    if (-not [string]::IsNullOrWhiteSpace($expected) -and -not (Test-VersionFormat $expected)) {
        Add-Fail 'versions' ("-Version '$expected' is not X.Y.Z")
        return
    }

    $tauriText = Read-RepoText 'src-tauri/tauri.conf.json'
    if ($null -eq $tauriText) { Add-Fail 'versions' 'src-tauri/tauri.conf.json is missing'; return }
    try { $tauri = $tauriText | ConvertFrom-Json } catch { Add-Fail 'versions' ("src-tauri/tauri.conf.json is not valid JSON: " + $_.Exception.Message); return }
    $authoritative = [string]$tauri.version

    $packageText = Read-RepoText 'package.json'
    if ($null -eq $packageText) { Add-Fail 'versions' 'package.json is missing'; return }
    try { $package = $packageText | ConvertFrom-Json } catch { Add-Fail 'versions' ("package.json is not valid JSON: " + $_.Exception.Message); return }

    $cargoText = Read-RepoText 'src-tauri/Cargo.toml'
    if ($null -eq $cargoText) { Add-Fail 'versions' 'src-tauri/Cargo.toml is missing'; return }

    $lockText = Read-RepoText 'src-tauri/Cargo.lock'
    if ($null -eq $lockText) { Add-Fail 'versions' 'src-tauri/Cargo.lock is missing'; return }

    $mirrors = [ordered]@{
        'src-tauri/tauri.conf.json' = $authoritative
        'package.json'              = [string]$package.version
        'src-tauri/Cargo.toml'      = (Get-CargoPackageVersion $cargoText)
        'src-tauri/Cargo.lock'      = (Get-CargoLockVersion $lockText 'speechek')
    }

    if ([string]::IsNullOrWhiteSpace($authoritative)) {
        Add-Fail 'versions' 'src-tauri/tauri.conf.json has no version field (authoritative source)'
        return
    }
    if (-not (Test-VersionFormat $authoritative)) {
        Add-Fail 'versions' ("authoritative version '$authoritative' is not X.Y.Z")
        return
    }

    foreach ($entry in $mirrors.GetEnumerator()) {
        $value = $entry.Value
        if ($null -eq $value -or $value -eq '') {
            Add-Fail 'versions' ($entry.Key + ' has no version value')
        } elseif ($value -ne $authoritative) {
            Add-Fail 'versions' ($entry.Key + " version '$value' != authoritative '$authoritative'")
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($expected) -and $expected -ne $authoritative) {
        Add-Fail 'versions' ("tag/version argument '$expected' != manifest version '$authoritative'")
    }

    if ($script:Failures.Count -eq $script:CheckFailBaseline) {
        Add-Pass 'versions' ("all four mirrors = " + $authoritative)
    }
}

# ---------------------------------------------------------------------------
# 2. Forbidden production features
# ---------------------------------------------------------------------------

function Get-CargoFeatures([string]$Text) {
    $features = @{}
    $m = [regex]::Match($Text, '(?ms)^\[features\]\s*$(?<body>.*?)(?=^\[|\z)')
    if (-not $m.Success) { return $features }
    foreach ($fm in [regex]::Matches($m.Groups['body'].Value, '(?m)^([A-Za-z0-9_.-]+)\s*=\s*(?<val>\[[^\]]*\]|"[^"]*")')) {
        $features[$fm.Groups[1].Value] = $fm.Groups['val'].Value
    }
    return $features
}

function Check-Features {
    $cargo = Read-RepoText 'src-tauri/Cargo.toml'
    if ($null -eq $cargo) { Add-Fail 'features' 'src-tauri/Cargo.toml is missing'; return }

    if ([regex]::IsMatch($cargo, '\bcustom-protocol\b')) {
        Add-Fail 'features' 'Cargo.toml mentions custom-protocol; production must be built with `tauri bundle` over a prebuilt EXE, never `tauri build`'
    }

    $features = Get-CargoFeatures $cargo
    if ($features.ContainsKey('default') -and $features['default'] -match 'test-provider') {
        Add-Fail 'features' 'Cargo.toml default features enable test-provider (must stay opt-in for debug only)'
    }
    if ($features.ContainsKey('default') -and $features['default'] -match 'custom-protocol') {
        Add-Fail 'features' 'Cargo.toml default features enable custom-protocol'
    }

    $conf = Read-RepoText 'src-tauri/tauri.conf.json'
    if ($null -ne $conf) {
        if ($conf -match 'test-provider') { Add-Fail 'features' 'tauri.conf.json references test-provider (production config must not enable it)' }
        if ($conf -match 'custom-protocol') { Add-Fail 'features' 'tauri.conf.json references custom-protocol' }
    }

    if ($script:Failures.Count -eq $script:CheckFailBaseline) {
        Add-Pass 'features' 'test-provider not default; custom-protocol absent from Cargo.toml and tauri.conf.json'
    }
}

# ---------------------------------------------------------------------------
# 3. Lockfiles / locked graph
# ---------------------------------------------------------------------------

function Check-Lockfiles {
    $tracked = Get-TrackedFiles
    foreach ($required in @('src-tauri/Cargo.lock', 'bun.lock')) {
        if ($tracked -notcontains $required) { Add-Fail 'lockfiles' ($required + ' is not tracked') }
        elseif (-not (Test-Path -LiteralPath (Get-RepoPath $required) -PathType Leaf)) { Add-Fail 'lockfiles' ($required + ' is tracked but missing on disk') }
    }
    $lock = Read-RepoText 'src-tauri/Cargo.lock'
    if ($null -eq $lock) { Add-Fail 'lockfiles' 'src-tauri/Cargo.lock is missing' }
    elseif ($null -eq (Get-CargoLockVersion $lock 'speechek')) { Add-Fail 'lockfiles' 'Cargo.lock has no speechek package entry' }
    if ($script:Failures.Count -eq $script:CheckFailBaseline) { Add-Pass 'lockfiles' 'Cargo.lock + bun.lock tracked, speechek locked entry present' }
}

# ---------------------------------------------------------------------------
# 4. Notices
# ---------------------------------------------------------------------------

function Check-Notices {
    $notices = Get-RepoPath 'third_party/THIRD-PARTY-NOTICES.txt'
    if (-not (Test-Path -LiteralPath $notices -PathType Leaf)) {
        Add-Fail 'notices' 'third_party/THIRD-PARTY-NOTICES.txt is missing'
    } elseif ((Get-Item -LiteralPath $notices).Length -le 0) {
        Add-Fail 'notices' 'third_party/THIRD-PARTY-NOTICES.txt is empty'
    }
    if (-not (Test-Path -LiteralPath (Get-RepoPath 'third_party/Handy.LICENSE') -PathType Leaf)) {
        Add-Fail 'notices' 'third_party/Handy.LICENSE is missing'
    }

    $collector = Get-RepoPath 'scripts/collect-notices.ps1'
    if (-not (Test-Path -LiteralPath $collector -PathType Leaf)) {
        Add-Fail 'notices' 'scripts/collect-notices.ps1 is missing'
        return
    }

    # Run the collector in a child host process: it drives cargo (and rustup)
    # itself, and its native stderr must not terminate this gate. Windows
    # PowerShell 5.1 turns native stderr into error records, so relax the
    # preference only around this call.
    $hostExe = (Get-Process -Id $PID).Path
    $previousEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $collectOutput = @(& $hostExe -NoProfile -ExecutionPolicy Bypass -File $collector -Check -RepoRoot $RepoRoot 2>&1)
        $collectCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousEap
    }
    if ($collectCode -ne 0) {
        $lines = @(($collectOutput | Out-String) -split "`r?`n" | Where-Object { $_.Trim() -ne '' })
        $picked = @($lines | Where-Object { $_ -match 'BLOCK:|failed|error:' } | Select-Object -First 2)
        if ($picked.Count -eq 0) { $picked = @($lines | Select-Object -Last 2) }
        Add-Fail 'notices' ("collect-notices -Check exit $collectCode : " + ($picked -join ' | '))
    } elseif ($script:Failures.Count -eq $script:CheckFailBaseline) {
        Add-Pass 'notices' 'THIRD-PARTY-NOTICES.txt matches the locked graph'
    }
}

# ---------------------------------------------------------------------------
# 5. Public manifest
# ---------------------------------------------------------------------------

$script:ForbiddenSegments = @(
    '.private/', '.agents/', 'node_modules/', 'src-tauri/target/', 'src-tauri/gen/',
    'docs/backlog.md', 'docs/release-research.md'
)
$script:ForbiddenBasenames = @('secrets.bin', 'keys.txt', 'config/settings.json')

function Get-ForbiddenPathReason([string]$Path) {
    $n = ($Path -replace '\\', '/').ToLowerInvariant()
    foreach ($seg in $script:ForbiddenSegments) {
        if ($n -eq $seg.TrimEnd('/') -or $n.StartsWith($seg)) { return $seg }
    }
    if ($script:ForbiddenBasenames -contains $n) { return $n }
    $bn = [System.IO.Path]::GetFileName($n)
    if ($bn -eq '.env') { return '.env' }
    if ($bn -like '.env.*' -and $bn -ne '.env.example') { return '.env credential file' }
    if ($n -match '\.(wav|pcm|webm|log)$') { return 'non-publishable file type' }
    return $null
}

function Check-PublicManifest {
    $manifestPath = 'scripts/public-files.json'
    $text = Read-RepoText $manifestPath
    if ($null -eq $text) { Add-Fail 'manifest' ($manifestPath + ' is missing'); return }
    try { $manifest = $text | ConvertFrom-Json } catch { Add-Fail 'manifest' ('public-files.json is not valid JSON: ' + $_.Exception.Message); return }

    if ([int]$manifest.schemaVersion -ne 1) { Add-Fail 'manifest' ('schemaVersion must be 1, got ' + $manifest.schemaVersion) }
    if ($null -eq $manifest.files -or @($manifest.files).Count -eq 0) { Add-Fail 'manifest' 'files array is empty'; return }

    $entries = @($manifest.files | ForEach-Object { [string]$_ })
    $seen = @{}
    $missing = New-Object System.Collections.Generic.List[string]
    foreach ($e in $entries) {
        $norm = Normalize-Relative $e
        if ($norm -eq '') { Add-Fail 'manifest' 'contains an empty path entry'; continue }
        if ($norm -ne $e) { Add-Fail 'manifest' ("entry '$e' is not already a normalized forward-slash relative path") }
        if ($norm.StartsWith('/') -or $norm -match '^[A-Za-z]:') { Add-Fail 'manifest' ("entry '$norm' is absolute") }
        if ($norm -match '[*?\[]') { Add-Fail 'manifest' ("entry '$norm' contains a glob") }
        if ($norm -split '/' -contains '..') { Add-Fail 'manifest' ("entry '$norm' escapes the repository root") }
        if ($norm.EndsWith('/')) { Add-Fail 'manifest' ("entry '$norm' is a directory, not a file") }
        if ($seen.ContainsKey($norm)) { Add-Fail 'manifest' ("duplicate entry '$norm'") }
        $seen[$norm] = $true
        $reason = Get-ForbiddenPathReason $norm
        if ($reason) { Add-Fail 'manifest' ("entry '$norm' matches forbidden private path '$reason'") }
        if (-not (Test-Path -LiteralPath (Get-RepoPath $norm) -PathType Leaf)) { $missing.Add($norm) | Out-Null }
    }
    if ($missing.Count -gt 0) {
        Add-Fail 'manifest' ("listed paths missing on disk (" + $missing.Count + "): " + (($missing | Select-Object -First 8) -join ', ') + $(if ($missing.Count -gt 8) { ', ...' } else { '' }))
    }

    $tracked = Get-TrackedFiles
    $trackedSet = @{}
    foreach ($t in $tracked) { $trackedSet[(Normalize-Relative $t)] = $true }
    $unlisted = @($tracked | Where-Object { -not $seen.ContainsKey((Normalize-Relative $_)) })
    $untracked = @($entries | Where-Object { -not $trackedSet.ContainsKey((Normalize-Relative $_)) })
    if ($unlisted.Count -gt 0) {
        Add-Fail 'manifest' ("tracked files not in the public allowlist (" + $unlisted.Count + "): " + ((@($unlisted | Select-Object -First 8)) -join ', ') + $(if ($unlisted.Count -gt 8) { ', ...' } else { '' }))
    }
    if ($untracked.Count -gt 0) {
        Add-Fail 'manifest' ("allowlisted files not tracked in the index (" + $untracked.Count + "): " + ((@($untracked | Select-Object -First 8)) -join ', ') + $(if ($untracked.Count -gt 8) { ', ...' } else { '' }))
    }

    if ($script:Failures.Count -eq $script:CheckFailBaseline) {
        Add-Pass 'manifest' ($entries.Count.ToString() + ' public paths exist and exactly match the tracked index')
    }
}

# ---------------------------------------------------------------------------
# 6. Capabilities / permission inventory
# ---------------------------------------------------------------------------

function Check-Capabilities {
    $buildText = Read-RepoText 'src-tauri/build.rs'
    $libText = Read-RepoText 'src-tauri/src/lib.rs'
    if ($null -eq $buildText) { Add-Fail 'capabilities' 'src-tauri/build.rs is missing'; return }
    if ($null -eq $libText) { Add-Fail 'capabilities' 'src-tauri/src/lib.rs is missing'; return }

    $buildBlock = [regex]::Match($buildText, '(?s)commands\(&\[(.*?)\]\)')
    if (-not $buildBlock.Success) { Add-Fail 'capabilities' 'build.rs has no commands(&[...]) inventory'; return }
    $buildCommands = @([regex]::Matches($buildBlock.Groups[1].Value, '"([^"]+)"') | ForEach-Object { $_.Groups[1].Value })
    if ($buildCommands.Count -eq 0) { Add-Fail 'capabilities' 'build.rs command inventory is empty'; return }

    $handlerBlock = [regex]::Match($libText, '(?s)generate_handler!\[(.*?)\]')
    if (-not $handlerBlock.Success) { Add-Fail 'capabilities' 'lib.rs has no generate_handler![...] list'; return }
    $handlerCommands = @($handlerBlock.Groups[1].Value -split ',' |
        ForEach-Object { ($_.Trim() -replace '^.*::', '') } |
        Where-Object { $_ -ne '' })
    if ($handlerCommands.Count -eq 0) { Add-Fail 'capabilities' 'lib.rs handler list is empty'; return }

    $missingInHandler = @($buildCommands | Where-Object { $handlerCommands -notcontains $_ })
    $extraInHandler = @($handlerCommands | Where-Object { $buildCommands -notcontains $_ })
    if ($missingInHandler.Count -gt 0 -or $extraInHandler.Count -gt 0) {
        $parts = New-Object System.Collections.Generic.List[string]
        if ($missingInHandler.Count -gt 0) { $parts.Add('missing from handler: ' + ($missingInHandler -join ', ')) | Out-Null }
        if ($extraInHandler.Count -gt 0) { $parts.Add('missing from build.rs: ' + ($extraInHandler -join ', ')) | Out-Null }
        Add-Fail 'capabilities' ('build.rs vs generate_handler! mismatch: ' + ($parts -join '; '))
    }

    $permDir = Get-RepoPath 'src-tauri/permissions/autogenerated'
    if (-not (Test-Path -LiteralPath $permDir -PathType Container)) { Add-Fail 'capabilities' 'src-tauri/permissions/autogenerated is missing'; return }
    $permFiles = @(Get-ChildItem -LiteralPath $permDir -Filter '*.toml' -File)
    $declaredCommands = @{}
    $identifiers = @{}
    foreach ($f in $permFiles) {
        $t = [System.IO.File]::ReadAllText($f.FullName)
        foreach ($im in [regex]::Matches($t, '(?m)^identifier\s*=\s*"([^"]+)"')) { $identifiers[$im.Groups[1].Value] = $f.Name }
        foreach ($cm in [regex]::Matches($t, '(?m)^commands\.allow\s*=\s*\[\s*"([^"]+)"\s*\]')) {
            $cmd = $cm.Groups[1].Value
            if (-not $declaredCommands.ContainsKey($cmd)) { $declaredCommands[$cmd] = $f.Name }
        }
    }
    $missingPerms = @($buildCommands | Where-Object { -not $declaredCommands.ContainsKey($_) })
    if ($missingPerms.Count -gt 0) {
        Add-Fail 'capabilities' ('no generated permission file declares command(s): ' + ($missingPerms -join ', '))
    }
    if ($permFiles.Count -ne $buildCommands.Count) {
        Add-Fail 'capabilities' ('generated permission file count ' + $permFiles.Count + ' != command count ' + $buildCommands.Count)
    }

    $capDir = Get-RepoPath 'src-tauri/capabilities'
    if (-not (Test-Path -LiteralPath $capDir -PathType Container)) { Add-Fail 'capabilities' 'src-tauri/capabilities is missing'; return }
    $capIds = @{}
    foreach ($f in @(Get-ChildItem -LiteralPath $capDir -Filter '*.json' -File)) {
        try { $cap = [System.IO.File]::ReadAllText($f.FullName) | ConvertFrom-Json } catch { Add-Fail 'capabilities' ($f.Name + ' is not valid JSON: ' + $_.Exception.Message); continue }
        $cid = [string]$cap.identifier
        if ([string]::IsNullOrWhiteSpace($cid)) { Add-Fail 'capabilities' ($f.Name + ' has no identifier'); continue }
        if ($capIds.ContainsKey($cid)) { Add-Fail 'capabilities' ('duplicate capability identifier ' + $cid) }
        $capIds[$cid] = $f.Name
        if ($null -eq $cap.windows -or @($cap.windows).Count -eq 0) { Add-Fail 'capabilities' ($f.Name + ' has no windows') }
        foreach ($perm in @($cap.permissions)) {
            $p = [string]$perm
            if ([string]::IsNullOrWhiteSpace($p)) { Add-Fail 'capabilities' ($f.Name + ' has an empty permission entry'); continue }
            if ($p -notmatch ':') {
                if (-not $identifiers.ContainsKey($p)) { Add-Fail 'capabilities' ($f.Name + " permission '" + $p + "' has no generated permission file") }
            }
        }
    }

    if ($script:Failures.Count -eq $script:CheckFailBaseline) {
        Add-Pass 'capabilities' ($buildCommands.Count.ToString() + ' commands == handler == generated permissions; ' + $capIds.Count + ' capabilities resolve')
    }
}

# ---------------------------------------------------------------------------
# 7. Index hygiene (private paths already checked via the manifest allowlist)
# ---------------------------------------------------------------------------

$script:SecretPatterns = @(
    @{ Name = 'gemini-api-key'; Regex = 'AIza[0-9A-Za-z_\-]{35}' },
    @{ Name = 'private-key-block'; Regex = 'BEGIN [A-Z ]*PRIVATE KEY' },
    @{ Name = 'aws-access-key'; Regex = 'AKIA[0-9A-Z]{16}' },
    @{ Name = 'google-oauth-secret'; Regex = 'GOCSPX-[0-9A-Za-z_-]{10,}' },
    @{ Name = 'slack-token'; Regex = 'xox[baprs]-[0-9A-Za-z-]{10,}' },
    @{ Name = 'personal-windows-path'; Regex = '[A-Za-z]:[\\/]{1,2}Users[\\/]{1,2}' }
)

function Check-IndexHygiene {
    if (-not (Test-Path -LiteralPath (Get-RepoPath '.git'))) { Add-Fail 'index' 'not a git repository (no .git)'; return }

    # 7a. forbidden private paths that must never be tracked (defence in depth:
    # the manifest allowlist check covers this too, but it must hold even if the
    # manifest itself is missing or malformed).
    $offenders = New-Object System.Collections.Generic.List[string]
    foreach ($t in @(Get-TrackedFiles)) {
        $reason = Get-ForbiddenPathReason $t
        if ($reason) { $offenders.Add($t + ' (' + $reason + ')') | Out-Null }
    }
    if ($offenders.Count -gt 0) {
        Add-Fail 'index' ('forbidden private path(s) tracked (' + $offenders.Count + '): ' + (($offenders | Select-Object -First 8) -join ', ') + $(if ($offenders.Count -gt 8) { ', ...' } else { '' }))
    }

    # 7b. secret patterns anywhere in the index; matched text is never echoed.
    foreach ($pat in $script:SecretPatterns) {
        $res = Invoke-Git @('grep', '--cached', '-I', '-l', '-E', '-e', $pat.Regex)
        if ($res.ExitCode -eq 0) {
            $hits = @($res.Output | Where-Object { $_ -ne '' })
            $shown = @($hits | Select-Object -First 8)
            Add-Fail 'index' ("secret/private pattern '" + $pat.Name + "' in (" + $hits.Count + "): " + ($shown -join ', ') + $(if ($hits.Count -gt 8) { ', ...' } else { '' }))
        } elseif ($res.ExitCode -notin @(0, 1)) {
            Add-Fail 'index' ("git grep for '" + $pat.Name + "' failed (exit " + $res.ExitCode + ')')
        }
    }
    if ($script:Failures.Count -eq $script:CheckFailBaseline) { Add-Pass 'index' 'no private path and no secret pattern in the git index' }
}

# ---------------------------------------------------------------------------
# 8. Clean tracked tree
# ---------------------------------------------------------------------------

function Check-CleanTree {
    $status = Invoke-Git @('status', '--porcelain', '--untracked-files=no')
    if ($status.ExitCode -ne 0) { Add-Fail 'clean-tree' ('git status failed (exit ' + $status.ExitCode + ')'); return }
    $lines = @($status.Output | Where-Object { $_ -ne '' })
    if ($lines.Count -gt 0) {
        Add-Fail 'clean-tree' ("tracked tree is dirty (" + $lines.Count + "): " + ((@($lines | Select-Object -First 8)) -join '; ') + $(if ($lines.Count -gt 8) { '; ...' } else { '' }))
    } else {
        Add-Pass 'clean-tree' 'no staged/unstaged changes to tracked files'
    }
}

# ---------------------------------------------------------------------------
# 9. PE image (optional)
# ---------------------------------------------------------------------------

function Read-U16([byte[]]$Bytes, [int]$Offset) {
    if ($Offset -lt 0 -or ($Offset + 2) -gt $Bytes.Length) { throw "PE read out of bounds at $Offset" }
    return [System.BitConverter]::ToUInt16($Bytes, $Offset)
}
function Read-U32([byte[]]$Bytes, [int]$Offset) {
    if ($Offset -lt 0 -or ($Offset + 4) -gt $Bytes.Length) { throw "PE read out of bounds at $Offset" }
    return [System.BitConverter]::ToUInt32($Bytes, $Offset)
}
function Read-AsciiZ([byte[]]$Bytes, [int]$Offset) {
    if ($Offset -lt 0 -or $Offset -ge $Bytes.Length) { throw "PE string offset out of bounds at $Offset" }
    $sb = New-Object System.Text.StringBuilder
    for ($i = $Offset; $i -lt $Bytes.Length -and $sb.Length -lt 512; $i++) {
        if ($Bytes[$i] -eq 0) { break }
        [void]$sb.Append([char]$Bytes[$i])
    }
    return $sb.ToString()
}

function Get-PeInfo([string]$Path) {
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 64) { throw 'file is too small to be a PE image' }
    if ((Read-U16 $bytes 0) -ne 0x5A4D) { throw 'missing MZ signature' }
    $lfanew = [int](Read-U32 $bytes 0x3C)
    if ($lfanew -lt 0 -or ($lfanew + 24) -gt $bytes.Length) { throw 'e_lfanew points outside the file' }
    if ((Read-U32 $bytes $lfanew) -ne 0x00004550) { throw 'missing PE\0\0 signature' }
    $machine = Read-U16 $bytes ($lfanew + 4)
    $numSections = Read-U16 $bytes ($lfanew + 6)
    $sizeOfOptional = Read-U16 $bytes ($lfanew + 20)
    $characteristics = Read-U16 $bytes ($lfanew + 22)

    $opt = $lfanew + 24
    if (($opt + 2) -gt $bytes.Length) { throw 'optional header is truncated' }
    $magic = Read-U16 $bytes $opt
    $isPe32Plus = ($magic -eq 0x20B)

    $dataDirOffset = 0
    $numDirs = 0
    if ($isPe32Plus) {
        $numDirs = [int](Read-U32 $bytes ($opt + 108))
        $dataDirOffset = $opt + 112
    } else {
        $numDirs = [int](Read-U32 $bytes ($opt + 92))
        $dataDirOffset = $opt + 96
    }

    $sections = @()
    $secTable = $opt + $sizeOfOptional
    for ($s = 0; $s -lt $numSections; $s++) {
        $so = $secTable + ($s * 40)
        $sections += [pscustomobject]@{
            VirtualAddress  = [int](Read-U32 $bytes ($so + 12))
            VirtualSize     = [int](Read-U32 $bytes ($so + 8))
            SizeOfRawData   = [int](Read-U32 $bytes ($so + 16))
            PointerToRawData = [int](Read-U32 $bytes ($so + 20))
        }
    }

    $rvaToOffset = {
        param([int]$Rva)
        foreach ($sec in $sections) {
            $size = [Math]::Max($sec.VirtualSize, $sec.SizeOfRawData)
            if ($Rva -ge $sec.VirtualAddress -and $Rva -lt ($sec.VirtualAddress + $size)) {
                return $sec.PointerToRawData + ($Rva - $sec.VirtualAddress)
            }
        }
        throw ("RVA 0x" + $Rva.ToString('X') + ' is not mapped by any section')
    }

    $imports = @()
    if ($numDirs -gt 1) {
        $importRva = [int](Read-U32 $bytes ($dataDirOffset + 8))
        if ($importRva -ne 0) {
            $descOff = & $rvaToOffset $importRva
            $maxDescriptors = 4096
            for ($d = 0; $d -lt $maxDescriptors; $d++) {
                $o = $descOff + ($d * 20)
                $nameRva = Read-U32 $bytes ($o + 12)
                $firstThunk = Read-U32 $bytes ($o + 16)
                $originalThunk = Read-U32 $bytes ($o + 0)
                if ($nameRva -eq 0 -and $firstThunk -eq 0 -and $originalThunk -eq 0) { break }
                if ($nameRva -eq 0) { break }
                $nameOff = & $rvaToOffset ([int]$nameRva)
                $imports += (Read-AsciiZ $bytes $nameOff)
            }
        }
    }

    return [pscustomobject]@{
        Machine       = $machine
        IsPe32Plus    = $isPe32Plus
        Characteristics = $characteristics
        ImportCount   = ($imports | Measure-Object).Count
        Imports       = $imports
    }
}

function Check-PeImage {
    $path = $BinaryPath
    if (-not [System.IO.Path]::IsPathRooted($path)) { $path = Get-RepoPath $path }
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { Add-Fail 'pe' ('binary not found: ' + $path); return }

    $info = $null
    try { $info = Get-PeInfo -Path $path } catch { Add-Fail 'pe' ('not a readable PE image: ' + $_.Exception.Message); return }

    if ($info.Machine -ne 0x8664) {
        Add-Fail 'pe' ('machine 0x' + $info.Machine.ToString('X4') + ' is not x64 (0x8664)')
    }
    if (-not $info.IsPe32Plus) { Add-Fail 'pe' 'not a PE32+ (64-bit) optional header' }
    if (($info.Characteristics -band 0x0002) -eq 0) { Add-Fail 'pe' 'IMAGE_FILE_EXECUTABLE_IMAGE is not set' }
    if (($info.Characteristics -band 0x2000) -ne 0) { Add-Fail 'pe' 'image is a DLL (IMAGE_FILE_DLL set)' }

    $crtRegex = '^(vcruntime|msvcp|msvcr|ucrtbase|api-ms-win-crt|concrt|vccorlib)'
    $dynamicCrt = @($info.Imports | Where-Object { $_.ToLowerInvariant() -match $crtRegex })
    if ($dynamicCrt.Count -gt 0) {
        Add-Fail 'pe' ('dynamic CRT imports present: ' + ($dynamicCrt -join ', ') + ' (must link the static CRT)')
    }

    if ($script:Failures.Count -eq $script:CheckFailBaseline) {
        Add-Pass 'pe' ('x64 PE32+ executable, ' + $info.ImportCount.ToString() + ' imports, no dynamic CRT')
    }
}

# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

function Invoke-CheckRelease {
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        Write-Host 'check-release: git is required' -ForegroundColor Red
        exit 1
    }
    if (-not (Test-Path -LiteralPath (Get-RepoPath '.git'))) {
        Write-Host ('check-release: ' + $RepoRoot + ' is not a git repository') -ForegroundColor Red
        exit 1
    }

    Run-Check 'versions'     { Check-Versions }
    Run-Check 'features'     { Check-Features }
    Run-Check 'lockfiles'    { Check-Lockfiles }
    Run-Check 'notices'      { Check-Notices }
    Run-Check 'manifest'     { Check-PublicManifest }
    Run-Check 'capabilities' { Check-Capabilities }
    Run-Check 'index'        { Check-IndexHygiene }
    if ($RequireCleanWorkTree) { Run-Check 'clean-tree' { Check-CleanTree } }
    else { Add-Pass 'clean-tree' 'skipped (-RequireCleanWorkTree:$false)' }

    if (-not [string]::IsNullOrWhiteSpace($BinaryPath)) {
        Run-Check 'pe' { Check-PeImage }
    } else {
        Add-Pass 'pe' 'skipped (no -BinaryPath; build-release.ps1/release gate must pass the built EXE)'
    }

    Write-Host ''
    foreach ($p in $script:Passes) { Write-Host ('OK   ' + $p) }
    if ($script:Failures.Count -gt 0) {
        Write-Host ''
        Write-Host ('BLOCK: ' + $script:Failures.Count + ' release check(s) failed:') -ForegroundColor Red
        foreach ($f in $script:Failures) { Write-Host ('FAIL ' + $f) -ForegroundColor Red }
        exit 2
    }
    Write-Host ''
    Write-Host 'check-release: all checks passed'
    exit 0
}

if ($MyInvocation.InvocationName -ne '.') { Invoke-CheckRelease }
