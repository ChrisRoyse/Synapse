<#
.SYNOPSIS
  Stages an EXPLICIT path list for commit, under a repo-level advisory lock, and
  fails closed when the working tree holds changes the caller did not name.
  Issue #1967.

.DESCRIPTION
  Two agent sessions were working in this checkout on `main` at the same time.
  One of them ran `git add -A`, which cannot tell its own edits from the other
  session's in-flight work, and commit `f4645cb5` shipped with a message
  describing the #1950 row-table sharding over a diff that was entirely another
  session's unfinished #1965 backfill. Nothing broke; the damage was to
  provenance, which is worse in a repo where `git log` and `git blame` are the
  audit trail. The window is silent, and it recurs any time two sessions overlap.

  This script is the mechanism #1967 ask 1 and ask 2 call for. It refuses the
  two ways that commit could have happened:

    1. `git add -A` semantics. There is no "stage everything" mode. `-Paths` is
       mandatory and every staged path is named.

    2. Staging while the tree holds work you did not name. Any modified,
       deleted, or untracked path outside `-Paths` is reported and the run is
       REFUSED, unless `-AllowUnstagedOthers` is passed to say "yes, I know that
       other work is in flight and mine is disjoint from it". The refusal is the
       point: an agent that has no idea another session is mid-edit finds out
       here rather than in a commit message that lies.

  It also takes an advisory lock (`.git/agent-stage.lock`) across the whole
  stage, carrying the holder's PID and session label, so two sessions cannot
  interleave a stage. The lock FAILS CLOSED rather than queueing — a session
  that waits would just stage a moment later against the same ambiguous tree.
  A lock whose recorded PID is gone is stale and is broken automatically, with
  the takeover reported.

  This does not commit. Staging and committing are separate so the caller can
  read the printed staged set before writing a message about it.

.PARAMETER Paths
  Repo-relative paths to stage. Mandatory. Directories are refused: a directory
  is `git add -A` wearing a hat, and would reintroduce the exact defect.

.PARAMETER Session
  Label recorded in the lock so a conflicting holder is identifiable. Defaults
  to the process id.

.PARAMETER AllowUnstagedOthers
  Proceed even though the tree holds changes outside `-Paths`. Use when another
  session's work is known to be in flight and disjoint. The unnamed paths are
  always printed either way.

.PARAMETER BreakStaleLock
  Break a lock whose recorded PID is still alive. Requires a human decision;
  a dead holder's lock is broken without this.

.EXAMPLE
  pwsh -File scripts/agent-stage.ps1 -Paths crates/synapse-calyx/src/lib.rs,crates/synapse-calyx/src/ward.rs
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string[]] $Paths,
    [string]   $Session = "pid-$PID",
    [switch]   $AllowUnstagedOthers,
    [switch]   $BreakStaleLock
)

$ErrorActionPreference = 'Stop'

function Fail($Code, $Detail, $Remediation) {
    Write-Host ""
    Write-Host "REFUSED $Code" -ForegroundColor Red
    Write-Host "  detail      : $Detail"
    Write-Host "  remediation : $Remediation"
    exit 1
}

$repoRoot = (git rev-parse --show-toplevel 2>$null)
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($repoRoot)) {
    Fail 'AGENT_STAGE_NOT_A_REPO' 'git rev-parse --show-toplevel failed' 'run this from inside the synapse checkout'
}
$repoRoot = $repoRoot.Trim()
$gitDir = (git rev-parse --git-dir).Trim()
if (-not [System.IO.Path]::IsPathRooted($gitDir)) { $gitDir = Join-Path $repoRoot $gitDir }
$lockPath = Join-Path $gitDir 'agent-stage.lock'

# ---------------------------------------------------------------- advisory lock
if (Test-Path $lockPath) {
    $held = $null
    try { $held = Get-Content $lockPath -Raw | ConvertFrom-Json } catch { $held = $null }
    $holderPid = if ($held) { [int]$held.pid } else { 0 }
    $alive = $false
    if ($holderPid -gt 0) {
        try { $alive = $null -ne (Get-Process -Id $holderPid -ErrorAction Stop) } catch { $alive = $false }
    }
    if ($alive -and -not $BreakStaleLock) {
        Fail 'AGENT_STAGE_LOCK_HELD' `
            "session '$($held.session)' (pid $holderPid, taken $($held.taken_at)) holds the staging lock and is still running" `
            'wait for that session to finish staging, or pass -BreakStaleLock if you have confirmed it is not staging'
    }
    if ($alive) {
        Write-Host "WARN breaking a LIVE holder's lock: session '$($held.session)' pid $holderPid" -ForegroundColor Yellow
    } else {
        Write-Host "INFO breaking a stale lock: session '$(if($held){$held.session}else{'unreadable'})' pid $holderPid is gone" -ForegroundColor Yellow
    }
    Remove-Item $lockPath -Force
}

@{
    session  = $Session
    pid      = $PID
    taken_at = (Get-Date).ToString('o')
} | ConvertTo-Json -Compress | Set-Content -Path $lockPath -Encoding utf8

try {
    Write-Host "agent-stage  (#1967)"
    Write-Host "  repo    : $repoRoot"
    Write-Host "  session : $Session (pid $PID)"
    Write-Host "  lock    : $lockPath"

    # ------------------------------------------------------- validate the paths
    if ($Paths.Count -eq 0) {
        Fail 'AGENT_STAGE_NO_PATHS' 'no paths were named' 'name every path this session touched; there is deliberately no stage-everything mode'
    }
    # `pwsh -File script.ps1 -Paths a,b` hands the whole token over as ONE
    # string rather than binding a two-element array, so a caller naming three
    # files would silently get one nonsense entry. It fails closed either way,
    # but on the wrong reason, which is its own kind of lie. Split explicitly.
    $named = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $expanded = @($Paths | ForEach-Object { $_ -split ',' })
    foreach ($p in $expanded) {
        $normalized = $p.Trim().Replace('\', '/').TrimStart('./')
        if ([string]::IsNullOrWhiteSpace($normalized)) { continue }
        $full = Join-Path $repoRoot $normalized
        if (Test-Path -Path $full -PathType Container) {
            Fail 'AGENT_STAGE_PATH_IS_DIRECTORY' `
                "'$normalized' is a directory" `
                'name individual files; staging a directory is `git add -A` in disguise and is what #1967 is about'
        }
        [void]$named.Add($normalized)
    }

    # ------------------------------------- read the tree BEFORE touching the index
    $porcelain = @(git status --porcelain=v1 --untracked-files=all)
    $treePaths = [System.Collections.Generic.List[object]]::new()
    foreach ($line in $porcelain) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        $code = $line.Substring(0, 2)
        $path = $line.Substring(3).Trim().Trim('"')
        # A rename reads as "old -> new"; the new name is what is stageable.
        if ($path -match '^(.*) -> (.*)$') { $path = $Matches[2] }
        $treePaths.Add([pscustomobject]@{ Code = $code; Path = $path.Replace('\', '/') })
    }

    Write-Host ""
    Write-Host "  BEFORE: working tree holds $($treePaths.Count) changed path(s)"
    foreach ($entry in $treePaths) {
        $mine = if ($named.Contains($entry.Path)) { 'NAMED  ' } else { 'unnamed' }
        Write-Host ("    {0}  {1}  {2}" -f $mine, $entry.Code, $entry.Path)
    }

    # Every named path must actually have a change; staging a clean path is a
    # sign the caller's idea of what it touched is wrong, which is the same
    # class of error as staging someone else's file.
    $treeSet = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($entry in $treePaths) { [void]$treeSet.Add($entry.Path) }
    $missing = @($named | Where-Object { -not $treeSet.Contains($_) })
    if ($missing.Count -gt 0) {
        Fail 'AGENT_STAGE_NAMED_PATH_UNCHANGED' `
            "named path(s) have no change in the working tree: $($missing -join ', ')" `
            'name only paths this session actually modified; a clean path in the list means the list is wrong'
    }

    # Anything already staged that this run did not name came from somewhere
    # else and would ride along in the commit. That is precisely f4645cb5.
    $alreadyStaged = @($treePaths | Where-Object { $_.Code[0] -ne ' ' -and $_.Code[0] -ne '?' -and -not $named.Contains($_.Path) })
    if ($alreadyStaged.Count -gt 0) {
        Fail 'AGENT_STAGE_FOREIGN_INDEX_ENTRY' `
            "the index already holds $($alreadyStaged.Count) path(s) this run did not name: $(($alreadyStaged | ForEach-Object { $_.Path }) -join ', ')" `
            'run `git restore --staged <path>` for work that is not yours, or finish and commit it deliberately first'
    }

    $others = @($treePaths | Where-Object { -not $named.Contains($_.Path) })
    if ($others.Count -gt 0 -and -not $AllowUnstagedOthers) {
        Fail 'AGENT_STAGE_UNNAMED_TREE_CHANGES' `
            "$($others.Count) changed path(s) are not in -Paths: $(($others | ForEach-Object { $_.Path }) -join ', ')" `
            'confirm whether another session is mid-edit. If your change is disjoint from theirs, re-run with -AllowUnstagedOthers'
    }

    # --------------------------------------------------------------- stage them
    foreach ($p in $named) {
        git add -- $p
        if ($LASTEXITCODE -ne 0) {
            Fail 'AGENT_STAGE_GIT_ADD_FAILED' "git add -- $p exited $LASTEXITCODE" 'inspect the path and re-run'
        }
    }

    # ------------------------------------------- prove the index is exactly this
    $stagedAfter = @(git diff --cached --name-only) | ForEach-Object { $_.Trim().Replace('\', '/') } | Where-Object { $_ }
    $stagedSet = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($p in $stagedAfter) { [void]$stagedSet.Add($p) }

    Write-Host ""
    Write-Host "  AFTER: index holds $($stagedSet.Count) path(s)"
    foreach ($p in ($stagedAfter | Sort-Object)) { Write-Host "    staged  $p" }

    $extra = @($stagedSet | Where-Object { -not $named.Contains($_) })
    $absent = @($named | Where-Object { -not $stagedSet.Contains($_) })
    if ($extra.Count -gt 0 -or $absent.Count -gt 0) {
        Fail 'AGENT_STAGE_INDEX_MISMATCH' `
            "index does not equal the named set (unexpected: $($extra -join ', ') | missing: $($absent -join ', '))" `
            'inspect `git diff --cached --name-only`; the index must equal exactly what was named'
    }

    Write-Host ""
    Write-Host "OK: the index equals the named set exactly ($($stagedSet.Count) path(s))." -ForegroundColor Green
    Write-Host "    Now write a commit message that describes THIS diff."
} finally {
    if (Test-Path $lockPath) { Remove-Item $lockPath -Force }
}
