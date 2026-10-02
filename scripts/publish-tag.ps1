<#
    scripts/publish-tag.ps1

    Creates and pushes the single annotated release tag `vX.Y.Z` for an already
    built and locally verified candidate run:

        powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/publish-tag.ps1 `
            -Version 0.2.0 -CandidateRun <run-id> -VerificationReport <absolute-path>

    The script refuses (without creating a tag) unless every condition holds:
      * the verification report lives under the ignored .private/ directory and
        records the tested commit, the candidate run id, the installer SHA-256,
        the Windows build and a checklist whose values are all the boolean true;
      * the checkout is on `main`, the tracked tree is clean, HEAD is exactly the
        recorded commit, and the three version mirrors equal -Version;
      * the tag does not exist locally or on origin, and the git identity is the
        public noreply handle (so no personal e-mail is embedded in the tag);
      * the named candidate run belongs to this repository, is a push to `main`
        of .github/workflows/candidate.yml that concluded success at the recorded
        commit, and still has the unexpired `windows-x64-<commit>` artifact;
      * the artifact's release-manifest.json, SHA256SUMS.txt and installer bytes
        all agree with the recorded installer SHA-256.

    It never fills in the verification for the operator. Documents/scripts are
    only read and hashed; nothing downloaded is executed.

    Runs on Windows PowerShell 5.1 and PowerShell 7+.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$CandidateRun,
    [Parameter(Mandatory = $true)][string]$VerificationReport
)

$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'release-common.ps1')

$repoRoot = Get-SpeechekRepoRoot -ScriptRoot $PSScriptRoot

# The plan's machine-readable checklist. All of these keys are required and every
# value present in `checks` must be the boolean true; a false extra key is never
# allowed to stand in for a required one.
$requiredChecks = @(
    'isolation',
    'fakeProvider',
    'installRuEn',
    'updateGraceful',
    'updateForce',
    'updateCancel',
    'shortcutsStartup',
    'uninstallKeepDelete',
    'productionInstall'
)

function Get-SpeechekObjectProperty {
    param($Object, [string]$Name)
    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Get-SpeechekGitText {
    param([string[]]$Arguments)
    $output = & git -C $repoRoot @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw ("git " + ($Arguments -join ' ') + " failed: " + (($output | Out-String).Trim()))
    }
    return (($output | Out-String).Trim())
}

# ── 1. Command line shapes ─────────────────────────────────────────────────

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "invalid -Version '$Version': expected X.Y.Z."
}
$CandidateRun = $CandidateRun.Trim()
if ($CandidateRun -notmatch '^[0-9]+$') {
    throw "invalid -CandidateRun '$CandidateRun': expected a decimal GitHub Actions run id."
}

# ── 2. Verification report: location, privacy, fields, checklist ───────────

if ([string]::IsNullOrWhiteSpace($VerificationReport)) {
    throw "-VerificationReport is required."
}
if (-not [System.IO.Path]::IsPathRooted($VerificationReport)) {
    throw "-VerificationReport must be an absolute path."
}
$reportPath = [System.IO.Path]::GetFullPath($VerificationReport)
$privateDir = [System.IO.Path]::GetFullPath((Join-Path $repoRoot '.private'))
$sep = [System.IO.Path]::DirectorySeparatorChar
$privatePrefix = $privateDir.TrimEnd([char[]]@($sep, [System.IO.Path]::AltDirectorySeparatorChar)) + $sep
if (-not $reportPath.StartsWith($privatePrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw ("the verification report must be a file inside the ignored .private/ directory " +
           "(got '$reportPath'); a report outside it could leak personal data into the repository.")
}
if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) {
    throw "verification report not found: $reportPath"
}

$reportText = Get-Content -LiteralPath $reportPath -Raw

$privacyPatterns = @(
    @{ Name = 'Google API key'; Regex = 'AIza[0-9A-Za-z_\-]{20,}' },
    @{ Name = 'PEM/private key block'; Regex = '-----BEGIN[ A-Z]+-----' },
    @{ Name = 'credential field'; Regex = '(?i)(client_secret|refresh_token|access_token|api[_-]?key|secrets\.bin)' },
    @{ Name = 'absolute user profile path'; Regex = '(?i)([A-Za-z]:\\{1,2}Users|/Users/|/home/)' }
)
foreach ($pattern in $privacyPatterns) {
    if ([regex]::IsMatch($reportText, $pattern.Regex)) {
        throw ("the verification report looks like it contains personal or secret data " +
               "($($pattern.Name)); the report must record only the commit, run id, installer " +
               "SHA-256, Windows build and checklist.")
    }
}

try {
    $report = $reportText | ConvertFrom-Json
} catch {
    throw "the verification report is not valid JSON: $($_.Exception.Message)"
}
if ($null -eq $report) {
    throw "the verification report is empty."
}

$commit = [string](Get-SpeechekObjectProperty $report 'commit')
$runId = [string](Get-SpeechekObjectProperty $report 'runId')
$installerSha256 = [string](Get-SpeechekObjectProperty $report 'installerSha256')
$windowsBuildRaw = Get-SpeechekObjectProperty $report 'windowsBuild'
$checks = Get-SpeechekObjectProperty $report 'checks'

if ($commit -notmatch '^[0-9a-f]{40}$') {
    throw "report 'commit' must be a full lowercase 40-hex git SHA (got '$commit')."
}
if ($runId -notmatch '^[0-9]+$') {
    throw "report 'runId' must be a decimal string (got '$runId')."
}
if ($runId -ne $CandidateRun) {
    throw "report runId '$runId' does not match -CandidateRun '$CandidateRun'."
}
if ($installerSha256 -notmatch '^[0-9a-f]{64}$') {
    throw "report 'installerSha256' must be a lowercase 64-hex SHA-256 (got '$installerSha256')."
}
$windowsBuild = 0
if (-not [long]::TryParse([string]$windowsBuildRaw, [ref]$windowsBuild)) {
    throw "report 'windowsBuild' must be an integer (got '$windowsBuildRaw')."
}
if ($windowsBuild -lt 22000) {
    throw "report 'windowsBuild' is $windowsBuild; a Windows 11 build (>= 22000) must have been tested."
}
$reportVersion = [string](Get-SpeechekObjectProperty $report 'version')
if (-not [string]::IsNullOrWhiteSpace($reportVersion) -and $reportVersion -ne $Version) {
    throw "report 'version' '$reportVersion' does not match -Version '$Version'."
}

if ($null -eq $checks) {
    throw "report 'checks' must be an object with the verification checklist."
}
foreach ($name in $requiredChecks) {
    if ($null -eq $checks.PSObject.Properties[$name]) {
        throw "verification checklist is missing required check '$name'."
    }
}
foreach ($property in $checks.PSObject.Properties) {
    if ($property.Value -isnot [bool] -or -not $property.Value) {
        throw ("verification checklist value '$($property.Name)' must be the boolean true; " +
               "the script never fills in or substitutes a check.")
    }
}

# ── 3. Local checkout, versions, identity, tag uniqueness ──────────────────

$branch = Get-SpeechekGitText @('rev-parse', '--abbrev-ref', 'HEAD')
if ($branch -ne 'main') {
    throw "publish-tag must run on branch 'main' (currently '$branch')."
}

Assert-SpeechekCleanTree $repoRoot

$head = Get-SpeechekGitCommit $repoRoot
if ($head -ne $commit) {
    throw "report commit '$commit' does not match HEAD '$head'; refusing to tag a different commit."
}

$mirrorVersion = Assert-SpeechekVersionMirrors $repoRoot
if ($mirrorVersion -ne $Version) {
    throw "version mirrors currently say '$mirrorVersion' but -Version is '$Version'."
}

$identityEmail = Get-SpeechekGitText @('config', 'user.email')
if ($identityEmail -notmatch '^[^@]+@users\.noreply\.github\.com$') {
    throw ("the git identity e-mail '$identityEmail' is not the public noreply handle; " +
           "set 'git config user.name girte' and 'git config user.email girte@users.noreply.github.com' " +
           "so the annotated tag carries no personal address.")
}

$tagName = "v$Version"
if ((Get-SpeechekGitText @('tag', '-l', $tagName)) -ne '') {
    throw "tag $tagName already exists locally."
}
$remoteTag = Get-SpeechekGitText @('ls-remote', '--tags', 'origin', "refs/tags/$tagName")
if ($remoteTag -ne '') {
    throw "tag $tagName already exists on origin."
}

# ── 4. GitHub candidate run and artifact ──────────────────────────────────

$ghPath = Resolve-SpeechekExecutable -Name 'gh'
if (-not $ghPath) {
    throw "GitHub CLI (gh) was not found on PATH; install and authenticate it before publishing."
}
$auth = Get-SpeechekProcessResult -FilePath $ghPath -Arguments @('auth', 'status') -WorkingDirectory $repoRoot
if ($auth.ExitCode -ne 0) {
    throw "gh is not authenticated; run 'gh auth login' first. (" + $auth.StandardError.Trim() + ")"
}

$remoteUrl = Get-SpeechekGitText @('remote', 'get-url', 'origin')
$remoteMatch = [regex]::Match($remoteUrl, 'github\.com[:/](?<owner>[^/]+)/(?<repo>[^/]+?)(?:\.git)?/?$')
if (-not $remoteMatch.Success) {
    throw "origin remote is not a github.com URL: $remoteUrl"
}
$slug = $remoteMatch.Groups['owner'].Value + '/' + $remoteMatch.Groups['repo'].Value

function Get-SpeechekGhApiJson {
    param([string[]]$Arguments)
    $result = Get-SpeechekProcessResult -FilePath $ghPath -Arguments $Arguments -WorkingDirectory $repoRoot
    if ($result.ExitCode -ne 0) {
        throw ("gh " + ($Arguments -join ' ') + " failed: " + $result.StandardError.Trim())
    }
    if ([string]::IsNullOrWhiteSpace($result.StandardOutput)) { return $null }
    return ($result.StandardOutput | ConvertFrom-Json)
}

$repoJson = Get-SpeechekGhApiJson @('api', "repos/$slug")
$repoId = [string](Get-SpeechekObjectProperty $repoJson 'id')
if ([string]::IsNullOrWhiteSpace($repoId)) {
    throw "could not read the repository id for $slug."
}

$run = Get-SpeechekGhApiJson @('api', "repos/$slug/actions/runs/$CandidateRun")
if ($null -eq $run) {
    throw "candidate run $CandidateRun was not found in $slug."
}
if ([string](Get-SpeechekObjectProperty $run 'id') -ne $CandidateRun) {
    throw "the API returned run '$([string](Get-SpeechekObjectProperty $run 'id'))' instead of '$CandidateRun'."
}
$runPath = [string](Get-SpeechekObjectProperty $run 'path')
if ($runPath -ne '.github/workflows/candidate.yml') {
    throw "run $CandidateRun is not from the candidate workflow (path '$runPath')."
}
if ([string](Get-SpeechekObjectProperty $run 'event') -ne 'push') {
    throw "run $CandidateRun is not a push event (event '$([string](Get-SpeechekObjectProperty $run 'event'))')."
}
if ([string](Get-SpeechekObjectProperty $run 'head_branch') -ne 'main') {
    throw "run $CandidateRun was not built from branch 'main'."
}
if ([string](Get-SpeechekObjectProperty $run 'status') -ne 'completed') {
    throw "run $CandidateRun has not completed."
}
if ([string](Get-SpeechekObjectProperty $run 'conclusion') -ne 'success') {
    throw "run $CandidateRun did not succeed (conclusion '$([string](Get-SpeechekObjectProperty $run 'conclusion'))')."
}
if ([string](Get-SpeechekObjectProperty $run 'head_sha') -ne $commit) {
    throw ("run $CandidateRun was built from commit '$([string](Get-SpeechekObjectProperty $run 'head_sha'))', " +
           "not the recorded commit '$commit'.")
}
$runRepository = Get-SpeechekObjectProperty $run 'repository'
if ([string](Get-SpeechekObjectProperty $runRepository 'id') -ne $repoId) {
    throw "run $CandidateRun does not belong to repository $slug (id $repoId)."
}

$artifactName = "windows-x64-$commit"
$artifactsJson = Get-SpeechekGhApiJson @('api', "repos/$slug/actions/runs/$CandidateRun/artifacts?per_page=100")
$candidateArtifacts = @($artifactsJson.artifacts | Where-Object { $_.name -eq $artifactName })
if ($candidateArtifacts.Count -ne 1) {
    throw "expected exactly one artifact '$artifactName' on run $CandidateRun, found $($candidateArtifacts.Count)."
}
$artifact = $candidateArtifacts[0]
if ([string](Get-SpeechekObjectProperty $artifact 'expired') -eq 'True' -or (Get-SpeechekObjectProperty $artifact 'expired') -eq $true) {
    throw "artifact '$artifactName' has expired; run a new candidate and re-verify that file."
}
$artifactRun = Get-SpeechekObjectProperty $artifact 'workflow_run'
if ([string](Get-SpeechekObjectProperty $artifactRun 'id') -ne $CandidateRun) {
    throw "artifact '$artifactName' belongs to a different run."
}
if ([string](Get-SpeechekObjectProperty $artifactRun 'head_sha') -ne $commit) {
    throw "artifact '$artifactName' was produced from a different commit."
}

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ('speechek-tag-' + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempDir | Out-Null
try {
    $downloadDir = Join-Path $tempDir 'artifact'
    New-Item -ItemType Directory -Path $downloadDir | Out-Null
    $download = Get-SpeechekProcessResult -FilePath $ghPath `
        -Arguments @('run', 'download', $CandidateRun, '--repo', $slug, '--name', $artifactName, '--dir', $downloadDir) `
        -WorkingDirectory $repoRoot
    if ($download.ExitCode -ne 0) {
        throw "downloading candidate artifact '$artifactName' failed: " + $download.StandardError.Trim()
    }

    $manifestFiles = @(Get-ChildItem -LiteralPath $downloadDir -Recurse -File -Filter 'release-manifest.json')
    if ($manifestFiles.Count -ne 1) {
        throw "expected one release-manifest.json inside the artifact, found $($manifestFiles.Count)."
    }
    $manifest = Get-Content -LiteralPath $manifestFiles[0].FullName -Raw | ConvertFrom-Json
    if ([string](Get-SpeechekObjectProperty $manifest 'schemaVersion') -ne '1') {
        throw "artifact manifest schemaVersion is not 1."
    }
    if ([string](Get-SpeechekObjectProperty $manifest 'commit') -ne $commit) {
        throw "artifact manifest commit does not match the recorded commit."
    }
    if ([string](Get-SpeechekObjectProperty $manifest 'version') -ne $Version) {
        throw "artifact manifest version '$([string](Get-SpeechekObjectProperty $manifest 'version'))' does not match '$Version'."
    }
    if ([string](Get-SpeechekObjectProperty $manifest 'flavor') -ne 'production') {
        throw "artifact manifest flavor is not 'production'."
    }
    if ([string](Get-SpeechekObjectProperty $manifest 'target') -ne 'x86_64-pc-windows-msvc') {
        throw "artifact manifest target is not x86_64-pc-windows-msvc."
    }
    if ([string](Get-SpeechekObjectProperty $manifest 'runId') -ne $CandidateRun) {
        throw "artifact manifest runId does not match -CandidateRun."
    }
    $manifestInstaller = Get-SpeechekObjectProperty $manifest 'installer'
    $installerFile = [string](Get-SpeechekObjectProperty $manifestInstaller 'file')
    $manifestInstallerSha = [string](Get-SpeechekObjectProperty $manifestInstaller 'sha256')
    if ($manifestInstallerSha -ne $installerSha256) {
        throw "artifact manifest installer SHA-256 does not match the recorded installerSha256."
    }

    $installerFiles = @(Get-ChildItem -LiteralPath $downloadDir -Recurse -File | Where-Object { $_.Name -eq $installerFile })
    if ($installerFiles.Count -ne 1) {
        throw "expected exactly one installer '$installerFile' inside the artifact, found $($installerFiles.Count)."
    }
    $actualInstallerSha = Get-SpeechekSha256 -Path $installerFiles[0].FullName
    if ($actualInstallerSha -ne $installerSha256) {
        throw "downloaded installer SHA-256 '$actualInstallerSha' does not match the recorded '$installerSha256'."
    }

    $sumFiles = @(Get-ChildItem -LiteralPath $downloadDir -Recurse -File -Filter 'SHA256SUMS.txt')
    if ($sumFiles.Count -ne 1) {
        throw "expected one SHA256SUMS.txt inside the artifact, found $($sumFiles.Count)."
    }
    $sumText = (Get-Content -LiteralPath $sumFiles[0].FullName -Raw) -replace "`r`n", "`n"
    $expectedSumText = $installerSha256 + '  ' + $installerFile + "`n"
    if ($sumText -ne $expectedSumText) {
        throw "SHA256SUMS.txt does not list '$installerFile' with the recorded installer SHA-256."
    }
}
finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}

# ── 5. Annotated tag and single-ref push ──────────────────────────────────

$tagMessage = "Speechek $Version release`n`ncandidate-run:$CandidateRun installer-sha256:$installerSha256"
$tagOutput = & git -C $repoRoot tag -a $tagName -m $tagMessage 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "git tag -a $tagName failed: " + (($tagOutput | Out-String).Trim())
}
$tagType = (& git -C $repoRoot cat-file -t $tagName 2>&1 | Out-String).Trim()
if ($tagType -ne 'tag') {
    & git -C $repoRoot tag -d $tagName 2>&1 | Out-Null
    throw "the created tag $tagName is not annotated (type '$tagType'); removed it."
}

$pushOutput = & git -C $repoRoot push origin "refs/tags/$tagName" 2>&1
if ($LASTEXITCODE -ne 0) {
    & git -C $repoRoot tag -d $tagName 2>&1 | Out-Null
    throw ("pushing tag $tagName failed; the local tag was deleted so a corrected report can be retried: " +
           (($pushOutput | Out-String).Trim()))
}

Write-Host "Pushed annotated tag $tagName for commit $commit (candidate run $CandidateRun)."
Write-Host "Release pipeline: https://github.com/$slug/actions/workflows/release.yml"
Write-Host "Release: https://github.com/$slug/releases/tag/$tagName"
