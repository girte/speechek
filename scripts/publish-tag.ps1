<#
    scripts/publish-tag.ps1

    One-command release: dispatches exactly one candidate run for the current
    `main` HEAD, waits for that run, validates the installer it produced against
    this checkout and the requested version, then creates and pushes the single
    annotated release tag `vX.Y.Z` that .github/workflows/release.yml consumes:

        powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/publish-tag.ps1 `
            -Version 0.2.0

    The script refuses (without creating or pushing a tag) unless every condition
    holds:
      * the checkout is on `main`, the tracked tree is clean, the version mirrors
        all equal -Version, and the git identity is the public noreply handle
        (so no personal e-mail is embedded in the tag);
      * the tag vX.Y.Z does not exist locally or on origin yet - checked before
        anything is dispatched;
      * POST /repos/{owner}/{repo}/actions/workflows/candidate.yml/dispatches for
        ref `main` with inputs commit=<HEAD> and a fresh unique requestId is sent
        exactly once; the documented HTTP 200 body with workflow_run_id is used
        when present, and the observed HTTP 204 empty body is recovered by
        locating the single run named `release-<requestId>` through the
        workflow-runs API (repository, workflow path, event, branch and head_sha
        re-checked) - a missing, ambiguous or mismatched run aborts the script,
        no run is inferred, and no previous/"latest" candidate is ever reused;
      * that exact run belongs to this repository (repository id), runs
        .github/workflows/candidate.yml, is a workflow_dispatch of `main` that
        concluded success at exactly this HEAD, and still holds the unexpired
        `windows-x64-<HEAD>` artifact;
      * the artifact's release-manifest.json, SHA256SUMS.txt and installer bytes
        all agree with each other, with HEAD, with -Version and with the run id.

    The annotated tag message records exactly
    `candidate-run:<id> installer-sha256:<hex>`, and only that single tag ref is
    pushed. Nothing downloaded from a run is executed and no local verification
    report is consulted.
    The tag push is judged only by git's exit code: git writes its push progress
    to stderr, which Windows PowerShell 5.1 must not turn into a reported
    failure.

    Runs on Windows PowerShell 5.1 and PowerShell 7+.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [ValidateRange(1, 720)][int]$WaitMinutes = 150,
    [ValidateRange(1, 300)][int]$PollSeconds = 15
)

$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'release-common.ps1')

$repoRoot = Get-SpeechekRepoRoot -ScriptRoot $PSScriptRoot

# Resolved once, so every git call goes through the .NET child-process helper.
$gitPath = Resolve-SpeechekExecutable -Name 'git'
if (-not $gitPath) {
    throw "git was not found on PATH; install Git before publishing."
}

# Git is invoked as a .NET child process (Get-SpeechekProcessResult), never
# through PowerShell's native `2>&1` redirection. On Windows PowerShell 5.1 a
# native command that writes to stderr while $ErrorActionPreference is 'Stop'
# raises a terminating NativeCommandError even when the command succeeded, and
# `git push` writes its progress to stderr: that previously reported a
# successful tag push as a failure. Only the child exit code counts as success.
function Invoke-SpeechekGit {
    param([string[]]$Arguments)
    $result = Get-SpeechekProcessResult -FilePath $gitPath -Arguments (@('-C', $repoRoot) + $Arguments) -WorkingDirectory $repoRoot
    if ($result.ExitCode -ne 0) {
        throw ("git " + ($Arguments -join ' ') + " failed (exit $($result.ExitCode)): " + $result.StandardError.Trim())
    }
    return (($result.StandardOutput | Out-String).Trim())
}

function Get-SpeechekGitText {
    param([string[]]$Arguments)
    return (Invoke-SpeechekGit -Arguments $Arguments)
}

# Deletes the local tag after a failed tag or push. Tolerant on purpose: this
# runs on an error path where the tag may already be gone.
function Remove-SpeechekLocalTag {
    try {
        Invoke-SpeechekGit @('tag', '-d', $tagName) | Out-Null
    }
    catch {
        Write-Warning "could not delete the local tag ${tagName}: $($_.Exception.Message)"
    }
}

function Get-SpeechekJsonProperty {
    param($Object, [string]$Name)
    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Get-SpeechekGhApiJson {
    param([string]$Endpoint)
    $result = Get-SpeechekProcessResult -FilePath $ghPath -Arguments @('api', $Endpoint) -WorkingDirectory $repoRoot
    if ($result.ExitCode -ne 0) {
        throw "gh api $Endpoint failed: " + $result.StandardError.Trim()
    }
    if ([string]::IsNullOrWhiteSpace($result.StandardOutput)) {
        throw "gh api $Endpoint returned an empty body."
    }
    try {
        return ($result.StandardOutput | ConvertFrom-Json)
    }
    catch {
        throw "gh api $Endpoint returned invalid JSON: $($_.Exception.Message)"
    }
}

# Locates the single candidate run carrying this invocation's unique requestId
# in its run name (`release-<requestId>`). Used when the dispatch API returns no
# workflow_run_id (the observed HTTP 204 empty body, or an unusable 200 body).
# The listing is filtered to this workflow, `workflow_dispatch`, `main` and the
# exact head commit; only an exact display_title match counts. Zero matches keep
# polling until the bounded $Deadline, more than one match is an error, and a
# run is never chosen by recency.
function Find-SpeechekCandidateRun {
    param(
        [Parameter(Mandatory = $true)][string]$RequestId,
        [Parameter(Mandatory = $true)][string]$HeadSha,
        [Parameter(Mandatory = $true)][datetime]$Deadline,
        [Parameter(Mandatory = $true)][int]$PollSeconds
    )
    $expectedTitle = 'release-' + $RequestId
    $endpoint = "repos/$slug/actions/workflows/candidate.yml/runs" +
                "?event=workflow_dispatch&branch=main&head_sha=$HeadSha&per_page=100"
    $consecutiveFailures = 0
    while ($true) {
        $listed = $false
        $runs = @()
        try {
            $runsJson = Get-SpeechekGhApiJson $endpoint
            $runs = @(Get-SpeechekJsonProperty $runsJson 'workflow_runs' | Where-Object { $null -ne $_ })
            $listed = $true
            $consecutiveFailures = 0
        }
        catch {
            $consecutiveFailures++
            if ($consecutiveFailures -ge 5) {
                throw "could not list candidate runs for '$expectedTitle' five times in a row: $($_.Exception.Message)"
            }
            Write-Warning "listing candidate runs failed (attempt $consecutiveFailures): $($_.Exception.Message)"
        }
        if ($listed) {
            # Exact, case-sensitive token match: the requestId is only ever
            # produced by this script, so no other run can carry this name.
            $titleMatches = @($runs | Where-Object { [string](Get-SpeechekJsonProperty $_ 'display_title') -ceq $expectedTitle })
            if ($titleMatches.Count -eq 1) {
                return $titleMatches[0]
            }
            if ($titleMatches.Count -gt 1) {
                throw ("found $($titleMatches.Count) candidate runs named '$expectedTitle' at $HeadSha; " +
                       "refusing to guess which one to tag. No tag was created.")
            }
        }
        if ((Get-Date) -ge $Deadline) {
            throw ("no candidate run named '$expectedTitle' (workflow_dispatch, main, head_sha $HeadSha) " +
                   "appeared before the wait window expired; no tag was created. The dispatch may still " +
                   "start a run later; do not reuse this invocation - dispatch a new candidate instead.")
        }
        Write-Host "waiting for candidate run '$expectedTitle' to appear..."
        Start-Sleep -Seconds $PollSeconds
    }
}

# ── 1. Requested version ───────────────────────────────────────────────────

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "invalid -Version '$Version': expected X.Y.Z."
}

# ── 2. Local checkout preconditions (checked before anything is dispatched) ─

$branch = Get-SpeechekGitText @('rev-parse', '--abbrev-ref', 'HEAD')
if ($branch -ne 'main') {
    throw "publish-tag must run on branch 'main' (currently '$branch')."
}

Assert-SpeechekCleanTree $repoRoot

$head = Get-SpeechekGitCommit $repoRoot
if ($head -notmatch '^[0-9a-f]{40}$') {
    throw "git rev-parse HEAD returned '$head'; expected a full lowercase 40-hex commit SHA."
}

$mirrorVersion = Assert-SpeechekVersionMirrors $repoRoot
if ($mirrorVersion -ne $Version) {
    throw "version mirrors currently say '$mirrorVersion' but -Version is '$Version'."
}

$identityEmail = Get-SpeechekGitText @('config', 'user.email')
if ($identityEmail -notmatch '^[^@]+@users\.noreply\.github\.com$') {
    throw ("git user.email '$identityEmail' is not a GitHub noreply address; set " +
           "git config user.email '<handle>@users.noreply.github.com' so no personal " +
           "address is embedded in the tag.")
}

$tagName = "v$Version"
if ((Get-SpeechekGitText @('tag', '-l', $tagName)) -ne '') {
    throw "tag $tagName already exists locally."
}
$remoteTag = Get-SpeechekGitText @('ls-remote', '--tags', 'origin', "refs/tags/$tagName")
if ($remoteTag -ne '') {
    throw "tag $tagName already exists on origin."
}

# ── 3. GitHub CLI and repository identity ──────────────────────────────────

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

$repoJson = Get-SpeechekGhApiJson "repos/$slug"
$repoId = [string](Get-SpeechekJsonProperty $repoJson 'id')
if ($repoId -notmatch '^[0-9]+$') {
    throw "could not read the numeric repository id for $slug (got '$repoId')."
}

# ── 4. Dispatch exactly one candidate run for this commit ──────────────────

# A fresh random requestId travels both as a dispatch input and (through the
# workflow `run-name`) as this run's name, so a body-less response can still be
# correlated to exactly this invocation.
$requestId = [System.Guid]::NewGuid().ToString('N')
$runDisplayTitle = 'release-' + $requestId

$payload = @{
    ref    = 'main'
    inputs = @{
        commit    = $head
        requestId = $requestId
    }
} | ConvertTo-Json -Depth 4 -Compress

$payloadPath = Join-Path ([System.IO.Path]::GetTempPath()) ('speechek-dispatch-' + [System.Guid]::NewGuid().ToString('N') + '.json')
Write-SpeechekUtf8NoBom -Path $payloadPath -Text $payload
try {
    $dispatch = Get-SpeechekProcessResult -FilePath $ghPath -Arguments @(
        'api', '--method', 'POST', "repos/$slug/actions/workflows/candidate.yml/dispatches",
        '--input', $payloadPath, '--include'
    ) -WorkingDirectory $repoRoot
}
finally {
    Remove-Item -LiteralPath $payloadPath -Force -ErrorAction SilentlyContinue
}
if ($dispatch.ExitCode -ne 0) {
    throw ("dispatching .github/workflows/candidate.yml for commit $head failed: " +
           $dispatch.StandardError.Trim())
}

$response = $dispatch.StandardOutput
$statusMatches = [regex]::Matches($response, '(?m)^HTTP/\S+[ \t]+(?<code>\d{3})\b')
if ($statusMatches.Count -eq 0) {
    throw "could not read an HTTP status line from the dispatch response; refusing to tag. Response:`n$response"
}
$statusCode = $statusMatches[$statusMatches.Count - 1].Groups['code'].Value
$bodyText = ''
# `gh api --include` prints the final response; when a redirect chain ever shows
# more than one header block, the body is what follows the last blank line.
$blankMatches = [regex]::Matches($response, '\r?\n\r?\n')
if ($blankMatches.Count -gt 0) {
    $lastBlank = $blankMatches[$blankMatches.Count - 1]
    $bodyText = $response.Substring($lastBlank.Index + $lastBlank.Length)
}

if ($statusCode -ne '200' -and $statusCode -ne '204') {
    throw ("the workflow dispatch API answered HTTP $statusCode; expected HTTP 200 with a " +
           "workflow_run_id body or HTTP 204 with no body, so no candidate run could be " +
           "correlated and no tag was created. Response body:`n$bodyText")
}

# The documented success response is HTTP 200 with workflow_run_id, but the real
# endpoint has been observed answering HTTP 204 with an empty body. Use the
# returned id only when it is present and well formed; otherwise locate exactly
# the run this invocation created by its unique name - never a "latest" or
# otherwise guessed run.
$dispatchJson = $null
try {
    if (-not [string]::IsNullOrWhiteSpace($bodyText)) {
        $dispatchJson = $bodyText | ConvertFrom-Json
    }
}
catch {
    Write-Warning "the dispatch response body is not valid JSON; correlating by requestId instead: $($_.Exception.Message)"
}

$returnedRunId = ''
if ($null -ne $dispatchJson) {
    $returnedRunId = [string](Get-SpeechekJsonProperty $dispatchJson 'workflow_run_id')
}
$runId = ''
$runHtmlUrl = ''
if ($returnedRunId -match '^[0-9]+$') {
    $runUrl = [string](Get-SpeechekJsonProperty $dispatchJson 'run_url')
    if ($runUrl -notmatch ('/runs/' + [regex]::Escape($returnedRunId) + '$')) {
        throw "the dispatch response run_url '$runUrl' does not reference run id $returnedRunId; no tag was created."
    }
    $runId = $returnedRunId
    $runHtmlUrl = [string](Get-SpeechekJsonProperty $dispatchJson 'html_url')
}
else {
    Write-Host ("the dispatch response returned no usable workflow_run_id (HTTP $statusCode); " +
                "correlating the run by requestId $requestId instead.")
}

$deadline = (Get-Date).AddMinutes($WaitMinutes)
if ($runId -eq '') {
    $dispatchedRun = Find-SpeechekCandidateRun -RequestId $requestId -HeadSha $head -Deadline $deadline -PollSeconds $PollSeconds
    $runId = [string](Get-SpeechekJsonProperty $dispatchedRun 'id')
    if ($runId -notmatch '^[0-9]+$') {
        throw "the correlated candidate run carries no usable id (got '$runId'); no tag was created."
    }
    $runHtmlUrl = [string](Get-SpeechekJsonProperty $dispatchedRun 'html_url')
}

Write-Host "Dispatched candidate run $runId for commit $head (requestId $requestId)"
if (-not [string]::IsNullOrWhiteSpace($runHtmlUrl)) { Write-Host "Run: $runHtmlUrl" }

# ── 5. Wait for that exact run and require success ─────────────────────────

# The $deadline set when the dispatch was resolved (section 4) bounds this
# completion wait as well.
$runEndpoint = "repos/$slug/actions/runs/$runId"
$consecutiveFailures = 0
$run = $null
while ($true) {
    try {
        $run = Get-SpeechekGhApiJson $runEndpoint
        $consecutiveFailures = 0
    }
    catch {
        $consecutiveFailures++
        if ($consecutiveFailures -ge 5) {
            throw "could not read run $runId from the GitHub API five times in a row: $($_.Exception.Message)"
        }
        Write-Warning "reading run $runId failed (attempt $consecutiveFailures): $($_.Exception.Message)"
        Start-Sleep -Seconds $PollSeconds
        continue
    }
    $status = [string](Get-SpeechekJsonProperty $run 'status')
    if ($status -eq 'completed') { break }
    if ((Get-Date) -ge $deadline) {
        throw ("candidate run $runId is still '$status' after $WaitMinutes minutes; no tag was " +
               "created. Inspect the run and retry: $runHtmlUrl")
    }
    Write-Host "candidate run $runId is '$status'; polling again in ${PollSeconds}s"
    Start-Sleep -Seconds $PollSeconds
}

$failures = @()
if ([string](Get-SpeechekJsonProperty $run 'id') -ne $runId) {
    $failures += "the run object id '$([string](Get-SpeechekJsonProperty $run 'id'))' does not match the dispatched run $runId"
}
if ([string](Get-SpeechekJsonProperty $run 'conclusion') -ne 'success') {
    $failures += "the run conclusion is '$([string](Get-SpeechekJsonProperty $run 'conclusion'))', not success"
}
$runRepository = Get-SpeechekJsonProperty $run 'repository'
if ([string](Get-SpeechekJsonProperty $runRepository 'id') -ne $repoId) {
    $failures += "the run does not belong to repository id $repoId"
}
if ([string](Get-SpeechekJsonProperty $run 'path') -ne '.github/workflows/candidate.yml') {
    $failures += "the run workflow path is '$([string](Get-SpeechekJsonProperty $run 'path'))', not .github/workflows/candidate.yml"
}
if ([string](Get-SpeechekJsonProperty $run 'event') -ne 'workflow_dispatch') {
    $failures += "the run event is '$([string](Get-SpeechekJsonProperty $run 'event'))', not workflow_dispatch"
}
if ([string](Get-SpeechekJsonProperty $run 'head_branch') -ne 'main') {
    $failures += "the run branch is '$([string](Get-SpeechekJsonProperty $run 'head_branch'))', not main"
}
if ([string](Get-SpeechekJsonProperty $run 'head_sha') -ne $head) {
    $failures += "the run head_sha '$([string](Get-SpeechekJsonProperty $run 'head_sha'))' is not this checkout's HEAD '$head'"
}
if ([string](Get-SpeechekJsonProperty $run 'display_title') -cne $runDisplayTitle) {
    $failures += "the run display_title '$([string](Get-SpeechekJsonProperty $run 'display_title'))' is not this dispatch's '$runDisplayTitle' (requestId $requestId)"
}
if ($failures.Count -gt 0) {
    foreach ($failure in $failures) { Write-Host "FAIL: $failure" }
    throw "candidate run $runId does not satisfy the publication policy; no tag was created ($runHtmlUrl)."
}

# ── 6. The artifact must exist, be unexpired and carry this commit ─────────

$artifactName = "windows-x64-$head"
$artifactsJson = Get-SpeechekGhApiJson "repos/$slug/actions/runs/$runId/artifacts?per_page=100"
$allArtifacts = @(Get-SpeechekJsonProperty $artifactsJson 'artifacts' | Where-Object { $null -ne $_ })
$artifactMatches = @($allArtifacts | Where-Object { [string](Get-SpeechekJsonProperty $_ 'name') -eq $artifactName })
if ($artifactMatches.Count -ne 1) {
    $available = (@($allArtifacts | ForEach-Object { [string](Get-SpeechekJsonProperty $_ 'name') }) -join ', ')
    throw "expected exactly one artifact '$artifactName' on run $runId, found $($artifactMatches.Count) (available: $available)."
}
$artifact = $artifactMatches[0]
$expired = Get-SpeechekJsonProperty $artifact 'expired'
if ($expired -eq $true -or ([string]$expired) -eq 'true') {
    throw "artifact '$artifactName' has expired; dispatch a new candidate and verify that file instead."
}
$artifactRun = Get-SpeechekJsonProperty $artifact 'workflow_run'
if ([string](Get-SpeechekJsonProperty $artifactRun 'id') -ne $runId) {
    throw "artifact '$artifactName' belongs to a different run."
}
if ([string](Get-SpeechekJsonProperty $artifactRun 'head_sha') -ne $head) {
    throw "artifact '$artifactName' was produced from a different commit."
}

# ── 7. Download and validate the artifact bytes ────────────────────────────

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ('speechek-tag-' + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempDir | Out-Null
try {
    $downloadDir = Join-Path $tempDir 'artifact'
    New-Item -ItemType Directory -Path $downloadDir | Out-Null
    $download = Get-SpeechekProcessResult -FilePath $ghPath -Arguments @(
        'run', 'download', $runId, '--repo', $slug, '--name', $artifactName, '--dir', $downloadDir
    ) -WorkingDirectory $repoRoot
    if ($download.ExitCode -ne 0) {
        throw "downloading candidate artifact '$artifactName' failed: " + $download.StandardError.Trim()
    }

    $manifestFiles = @(Get-ChildItem -LiteralPath $downloadDir -Recurse -File -Filter 'release-manifest.json')
    if ($manifestFiles.Count -ne 1) {
        throw "expected exactly one release-manifest.json inside the artifact, found $($manifestFiles.Count)."
    }
    $manifest = Get-Content -LiteralPath $manifestFiles[0].FullName -Raw | ConvertFrom-Json
    if ($null -eq $manifest) {
        throw "the artifact release-manifest.json is empty."
    }

    if ([string](Get-SpeechekJsonProperty $manifest 'schemaVersion') -ne '1') {
        throw "artifact manifest schemaVersion is not 1."
    }
    if ([string](Get-SpeechekJsonProperty $manifest 'flavor') -ne 'production') {
        throw "artifact manifest flavor is not 'production'."
    }
    if ([string](Get-SpeechekJsonProperty $manifest 'target') -ne 'x86_64-pc-windows-msvc') {
        throw "artifact manifest target is not x86_64-pc-windows-msvc."
    }
    if ([string](Get-SpeechekJsonProperty $manifest 'commit') -ne $head) {
        throw "artifact manifest commit '$([string](Get-SpeechekJsonProperty $manifest 'commit'))' is not this checkout's HEAD '$head'."
    }
    if ([string](Get-SpeechekJsonProperty $manifest 'version') -ne $Version) {
        throw "artifact manifest version '$([string](Get-SpeechekJsonProperty $manifest 'version'))' does not match -Version '$Version'."
    }
    if ([string](Get-SpeechekJsonProperty $manifest 'runId') -ne $runId) {
        throw "artifact manifest runId '$([string](Get-SpeechekJsonProperty $manifest 'runId'))' is not the dispatched run $runId."
    }

    $manifestExe = Get-SpeechekJsonProperty $manifest 'exe'
    if ([string](Get-SpeechekJsonProperty $manifestExe 'file') -ne 'speechek.exe') {
        throw "artifact manifest exe.file is not 'speechek.exe'."
    }
    $manifestExeSha = [string](Get-SpeechekJsonProperty $manifestExe 'sha256')
    if ($manifestExeSha -notmatch '^[0-9a-f]{64}$') {
        throw "artifact manifest exe.sha256 is not a lowercase 64-hex SHA-256."
    }
    $manifestInstaller = Get-SpeechekJsonProperty $manifest 'installer'
    $installerFile = [string](Get-SpeechekJsonProperty $manifestInstaller 'file')
    $manifestInstallerSha = [string](Get-SpeechekJsonProperty $manifestInstaller 'sha256')
    if ($installerFile -notmatch '^[^\\/]+\.exe$') {
        throw "artifact manifest installer.file '$installerFile' is not a plain .exe file name."
    }
    if ($manifestInstallerSha -notmatch '^[0-9a-f]{64}$') {
        throw "artifact manifest installer.sha256 is not a lowercase 64-hex SHA-256."
    }

    $installerFiles = @(Get-ChildItem -LiteralPath $downloadDir -Recurse -File | Where-Object { $_.Name -eq $installerFile })
    if ($installerFiles.Count -ne 1) {
        throw "expected exactly one installer '$installerFile' inside the artifact, found $($installerFiles.Count)."
    }
    $installerSha = Get-SpeechekSha256 -Path $installerFiles[0].FullName
    if ($installerSha -ne $manifestInstallerSha) {
        throw "downloaded installer SHA-256 '$installerSha' does not match the artifact manifest '$manifestInstallerSha'."
    }

    $exeFiles = @(Get-ChildItem -LiteralPath $downloadDir -Recurse -File | Where-Object { $_.Name -eq 'speechek.exe' })
    if ($exeFiles.Count -eq 1) {
        $exeSha = Get-SpeechekSha256 -Path $exeFiles[0].FullName
        if ($exeSha -ne $manifestExeSha) {
            throw "the packaged speechek.exe SHA-256 '$exeSha' does not match the artifact manifest '$manifestExeSha'."
        }
    }

    $sumFiles = @(Get-ChildItem -LiteralPath $downloadDir -Recurse -File -Filter 'SHA256SUMS.txt')
    if ($sumFiles.Count -ne 1) {
        throw "expected exactly one SHA256SUMS.txt inside the artifact, found $($sumFiles.Count)."
    }
    $sumText = (Get-Content -LiteralPath $sumFiles[0].FullName -Raw) -replace "`r`n", "`n"
    $expectedSumText = $installerSha + '  ' + $installerFile + "`n"
    if ($sumText -ne $expectedSumText) {
        throw "SHA256SUMS.txt does not list '$installerFile' with the installer SHA-256 '$installerSha'."
    }
}
finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}

# ── 8. Annotated tag and single-ref push ───────────────────────────────────

$tagMessage = "Speechek $Version release`n`ncandidate-run:$runId installer-sha256:$installerSha"
try {
    Invoke-SpeechekGit @('tag', '-a', $tagName, '-m', $tagMessage) | Out-Null
}
catch {
    throw "git tag -a $tagName failed: $($_.Exception.Message)"
}
$tagType = Invoke-SpeechekGit @('cat-file', '-t', $tagName)
if ($tagType -ne 'tag') {
    Remove-SpeechekLocalTag
    throw "the created tag $tagName is not annotated (object type '$tagType'); removed it."
}

# The child exit code is the only success signal: git push writes progress to
# stderr, which must never be mistaken for a failure.
try {
    Invoke-SpeechekGit @('push', 'origin', "refs/tags/$tagName") | Out-Null
}
catch {
    Remove-SpeechekLocalTag
    throw ("pushing tag $tagName failed; the local tag was deleted so the publish can be retried: " +
           $_.Exception.Message)
}

Write-Host "Pushed annotated tag $tagName for commit $head"
Write-Host "Candidate run: $runId (installer SHA-256 $installerSha)"
if (-not [string]::IsNullOrWhiteSpace($runHtmlUrl)) { Write-Host "Run URL: $runHtmlUrl" }
Write-Host "Release pipeline: https://github.com/$slug/actions/workflows/release.yml"
Write-Host "Release: https://github.com/$slug/releases/tag/$tagName"
