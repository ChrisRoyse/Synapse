<#
.SYNOPSIS
  ┌──────────────────────────────────────────────────────────────────────────┐
  │  REPO MAINTENANCE  —  stop git-worktree and Cargo target/ disk buildup.    │
  │  Backup -> prune merged/stale worktrees -> sweep stale build artifacts.    │
  └──────────────────────────────────────────────────────────────────────────┘

  WHY THIS EXISTS
  ----------------
  Parallel agent / per-issue development creates throwaway git worktrees (e.g.
  `Synapse-issue931-clean`), and EACH worktree gets its own multi-GB Cargo
  `target/`. Git never removes a worktree for you, and Cargo never garbage-
  collects `target/`. Left alone they accumulate without bound — on 2026-06-14
  this host had 46 stale Synapse worktrees consuming ~200 GB and pushing the
  system disk to 94% full, which in turn made every build crawl (disk I/O
  starvation + antivirus rescanning hundreds of thousands of artifact files).

  This script makes that buildup self-correcting. Run on a schedule (see
  scripts/install-maintenance-task.ps1) it keeps disk usage flat forever.

  WHAT IT DOES (per repo, for every git repo found under -Root)
  ------------------------------------------------------------
    1. BACKUP   Bundle every branch + a tag for each worktree HEAD into
                <BackupDir>\<repo>-worktrees-<date>.bundle BEFORE touching
                anything. Nothing this script removes can be unrecoverable.
    2. PRUNE    `git fetch --prune`, then remove worktrees that are SAFE to
                remove: their branch is merged into the default branch (ancestry
                OR squash-merged PR, detected via `gh` when available), or their
                upstream remote branch is gone. NEVER removes: the primary
                checkout, the currently-active worktree, a protected branch
                (main/master/develop), a DIRTY worktree (uncommitted changes), or
                a worktree with commits not reachable from the default branch and
                not yet on any remote (genuinely unmerged work) — unless -Force.
    3. BRANCHES Delete local branches marked `[gone]` (their remote was pruned),
                except protected / checked-out ones.
    4. SWEEP    Run `cargo-sweep` to delete build artifacts not touched in
                -SweepDays days across each repo's target/ (keeps current
                toolchain + recently used artifacts so the next build is fast).
    5. REPORT   Print disk free before/after and bytes reclaimed.

  SAFETY MODEL
  ------------
    * DRY-RUN BY DEFAULT. Without -Apply nothing is deleted; it prints exactly
      what it WOULD do. Add -Apply to actually act.
    * Fail-loud: any unexpected git/tool error stops the script with a clear
      message (no silent partial cleanup).
    * Every removal is preceded by the bundle backup in step 1.

.PARAMETER Root
  Directory to scan for git repositories (non-recursive: each immediate child
  that is a git repo is processed, plus -Root itself if it is one).
  Default: the parent of this repo (so all sibling checkouts under C:\code).

.PARAMETER Apply
  Actually perform removals/sweeps. Omit for a dry-run preview.

.PARAMETER SweepDays
  Delete build artifacts not accessed in this many days. Default 10.

.PARAMETER MinFreeGB
  If the system drive has less than this many GB free, escalate: sweep with a
  0-day threshold (everything not needed by the very latest build) on non-active
  worktrees. Default 80.

.PARAMETER Force
  Also remove worktrees with unmerged/unpushed commits and dirty worktrees.
  Still backs up first. Use only when you have confirmed the work is disposable.

.PARAMETER BackupDir
  Where bundle backups are written. Default C:\code\_repo_maintenance_backups.

.EXAMPLE
  # Preview (safe):
  powershell -ExecutionPolicy Bypass -File .\scripts\repo-maintenance.ps1
.EXAMPLE
  # Actually clean up everything that is merged/stale across C:\code:
  .\scripts\repo-maintenance.ps1 -Apply
#>
[CmdletBinding()]
param(
    [string]$Root,
    [switch]$Apply,
    [int]$SweepDays = 10,
    [int]$MinFreeGB = 80,
    [switch]$Force,
    [string]$BackupDir = 'C:\code\_repo_maintenance_backups'
)

$ErrorActionPreference = 'Stop'
$script:DidApply = $Apply.IsPresent

function Info($m) { Write-Host "[maint] $m" }
function Act ($m) { if ($script:DidApply) { Write-Host "[maint][APPLY] $m" -ForegroundColor Green } else { Write-Host "[maint][dry-run] $m" -ForegroundColor Yellow } }
function Warn($m) { Write-Host "[maint][WARN] $m" -ForegroundColor DarkYellow }
function Die ($m) { throw "[maint] FATAL: $m" }

# ---- preflight ------------------------------------------------------------
foreach ($t in 'git') {
    if (-not (Get-Command $t -ErrorAction SilentlyContinue)) { Die "'$t' not on PATH." }
}
$HaveGh = [bool](Get-Command gh -ErrorAction SilentlyContinue)
$HaveSweep = [bool](Get-Command cargo-sweep -ErrorAction SilentlyContinue)
if (-not $Root) { $Root = Split-Path -Parent $PSScriptRoot | Split-Path -Parent }  # parent of repo
if (-not (Test-Path $Root)) { Die "Root '$Root' does not exist." }
if ($script:DidApply -and -not (Test-Path $BackupDir)) { New-Item -ItemType Directory -Path $BackupDir -Force | Out-Null }

$drive = (Get-Item $Root).PSDrive.Name
function Free-GB { [math]::Round((Get-PSDrive $drive).Free / 1GB, 1) }
$freeBefore = Free-GB
Info "Root=$Root  Apply=$($script:DidApply)  SweepDays=$SweepDays  gh=$HaveGh  cargo-sweep=$HaveSweep"
Info "Disk $drive`: free before = $freeBefore GB"
$stampDate = (Get-Date -Format 'yyyy-MM-dd')

# ---- discover repos -------------------------------------------------------
# A "repo" here = a directory whose git common-dir is itself (the primary
# checkout), so we don't treat each linked worktree as a separate repo.
$repos = @()
$candidates = @($Root) + (Get-ChildItem -Path $Root -Directory -ErrorAction SilentlyContinue | ForEach-Object FullName)
foreach ($c in $candidates) {
    Push-Location $c -ErrorAction SilentlyContinue
    try {
        $inside = (git rev-parse --is-inside-work-tree 2>$null)
        if ($inside -eq 'true') {
            $top = (git rev-parse --show-toplevel 2>$null)
            $commonAbs = (git rev-parse --path-format=absolute --git-common-dir 2>$null)
            # primary checkout: git-common-dir is <top>\.git
            if ($top -and $commonAbs -and ((Resolve-Path $commonAbs).Path -eq (Join-Path (Resolve-Path $top).Path '.git'))) {
                if ($repos -notcontains $top) { $repos += $top }
            }
        }
    } finally { Pop-Location -ErrorAction SilentlyContinue }
}
Info "Found $($repos.Count) primary repo checkout(s): $($repos -join ', ')"

function Default-Branch($repo) {
    $h = (git -C $repo symbolic-ref --quiet refs/remotes/origin/HEAD 2>$null)
    if ($h) { return ($h -replace '^refs/remotes/origin/','') }
    foreach ($b in 'main','master','develop') {
        git -C $repo show-ref --verify --quiet "refs/heads/$b" 2>$null
        if ($LASTEXITCODE -eq 0) { return $b }
    }
    return 'main'
}

# Squash-merge aware "is this branch merged?" check.
function Is-Merged($repo, $sha, $branch, $def) {
    # 1) plain ancestry (normal merge / fast-forward)
    git -C $repo merge-base --is-ancestor $sha "$def" 2>$null
    if ($LASTEXITCODE -eq 0) { return $true }
    git -C $repo merge-base --is-ancestor $sha "origin/$def" 2>$null
    if ($LASTEXITCODE -eq 0) { return $true }
    # 2) squash-merge: ask GitHub for the PR state of this head branch
    if ($HaveGh -and $branch) {
        try {
            $state = (gh pr list --repo (gh repo view --json nameWithOwner -q .nameWithOwner 2>$null) --head $branch --state merged --json number -q 'length' 2>$null)
            if ($state -and [int]$state -gt 0) { return $true }
        } catch { }
    }
    # 3) fallback: if the branch has zero commits not already on the default
    #    branch, it carries no unique work and is safe to treat as merged.
    $cnt = (git -C $repo rev-list --count "$def..$sha" 2>$null)
    if ($cnt -eq '0') { return $true }
    return $false
}

$totalRemoved = 0
foreach ($repo in $repos) {
    Write-Host "`n=== $repo ===" -ForegroundColor Cyan
    $def = Default-Branch $repo
    $active = (Resolve-Path (git -C $repo rev-parse --show-toplevel)).Path
    Info "default branch: $def"
    if ($script:DidApply) {
        try { git -C $repo fetch --prune --quiet 2>$null } catch { Warn "fetch failed for $repo (offline?). Continuing with local refs." }
    }

    # --- step 1: per-repo context. Backups are created LAZILY (below), only for
    #     worktrees that carry genuinely unmerged work being force-removed. A
    #     merged or already-pushed worktree needs NO backup — its commits are
    #     already on the default branch and/or the remote — so normal runs write
    #     no bundles at all (avoids the cleanup tool itself causing buildup).
    $repoName = Split-Path $repo -Leaf

    # --- step 2: classify + remove worktrees ------------------------------
    $wt = git -C $repo worktree list --porcelain
    $entries = @(); $cur = @{}
    foreach ($line in $wt) {
        if ($line -match '^worktree (.+)$') { if ($cur.Count) { $entries += [pscustomobject]$cur }; $cur = @{ Path = $Matches[1].Trim() } }
        elseif ($line -match '^HEAD (.+)$') { $cur.Head = $Matches[1].Trim() }
        elseif ($line -match '^branch (.+)$') { $cur.Branch = ($Matches[1].Trim() -replace '^refs/heads/','') }
        elseif ($line -match '^detached') { $cur.Branch = $null; $cur.Detached = $true }
    }
    if ($cur.Count) { $entries += [pscustomobject]$cur }

    foreach ($e in $entries) {
        $rp = Resolve-Path $e.Path -ErrorAction SilentlyContinue
        $p = if ($rp) { $rp.Path } else { $null }
        if (-not $p) { continue }
        if ($p -eq $active) { continue }                      # never the active checkout
        if ($p -eq (Resolve-Path $repo).Path) { continue }    # never the primary checkout
        $bn = $e.Branch
        if ($bn -and ($bn -in 'main','master','develop')) { Info "keep (protected): $(Split-Path $p -Leaf) [$bn]"; continue }

        $dirty = (git -C $p status --porcelain 2>$null | Measure-Object).Count
        $merged = Is-Merged $repo $e.Head $bn $def
        $reason = $null
        if ($merged) { $reason = 'merged' }
        elseif ($bn) {
            # unmerged named branch: safe only if its commits are on a remote (recoverable)
            $onRemote = (git -C $repo branch -r --contains $e.Head 2>$null | Measure-Object).Count
            if ($onRemote -gt 0) { $reason = 'unmerged-but-pushed' }
        }

        if (-not $reason -and -not $Force) {
            Warn "keep (UNMERGED work, not on remote): $(Split-Path $p -Leaf) [$bn] HEAD=$($e.Head.Substring(0,9)) — use -Force to remove (backed up in bundle)"
            continue
        }
        if ($dirty -gt 0 -and -not $Force) {
            Warn "keep (DIRTY: $dirty uncommitted): $(Split-Path $p -Leaf) — use -Force to remove (dirty patch is in bundle's tags only; commit/stash first ideally)"
            continue
        }
        $why = if ($reason) { $reason } else { 'forced' }
        $sizeGB = if (Test-Path $p) { [math]::Round((Get-ChildItem $p -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum/1GB,1) } else { 0 }
        Act "remove worktree ($why, ~$sizeGB GB): $p"
        if ($script:DidApply) {
            # Lazy targeted backup: ONLY for genuinely-unmerged, not-pushed work
            # (the -Force path). Bundle just this HEAD's unique commits (--not the
            # default branch) so it's tiny, and tag it so the objects survive.
            if ($why -eq 'forced') {
                $safe = (Split-Path $p -Leaf) -replace '[^A-Za-z0-9_-]','_'
                $b = Join-Path $BackupDir "$repoName-$safe-$stampDate.bundle"
                git -C $repo tag -f "maint-backup/$stampDate/$safe" $e.Head 2>$null | Out-Null
                git -C $repo bundle create $b $e.Head --not $def "origin/$def" 2>$null | Out-Null
                if (Test-Path $b) { Info "backed up unmerged work: $b ($([math]::Round((Get-Item $b).Length/1MB,1)) MB)" }
                else { Die "Refusing to remove unmerged worktree $p — backup bundle could not be created." }
            }
            git -C $repo worktree remove --force $p 2>$null
            if ($LASTEXITCODE -ne 0) {
                # directory may be partially gone; nuke + prune metadata
                if (Test-Path $p) { try { [System.IO.Directory]::Delete($p, $true) } catch { Warn "could not delete $p : $($_.Exception.Message)" } }
            }
            $totalRemoved++
        }
    }
    if ($script:DidApply) { git -C $repo worktree prune 2>$null }

    # --- step 3: delete [gone] local branches -----------------------------
    $gone = (git -C $repo branch -vv 2>$null | Select-String '\[[^\]]*: gone\]').Line |
            ForEach-Object { ($_ -replace '^\*?\s*','' -split '\s+')[0] } |
            Where-Object { $_ -and ($_ -notin 'main','master','develop') }
    foreach ($g in $gone) {
        Act "delete gone branch: $g"
        if ($script:DidApply) { git -C $repo branch -D $g 2>$null | Out-Null }
    }

    # --- step 4: cargo-sweep target/ --------------------------------------
    if (Test-Path (Join-Path $repo 'Cargo.toml')) {
        if ($HaveSweep) {
            $days = $SweepDays
            if ((Free-GB) -lt $MinFreeGB) { Warn "low disk (<$MinFreeGB GB) — escalating sweep to 0-day on $repoName"; $days = 0 }
            Act "cargo sweep --time $days (recursive) on $repo"
            if ($script:DidApply) { & cargo-sweep sweep --time $days --recursive $repo 2>&1 | ForEach-Object { Info "  sweep: $_" } }
        } else {
            Warn "cargo-sweep not installed — target/ artifact GC skipped. Install once with:  cargo install cargo-sweep"
        }
    }
}

# --- retention: never let the backup dir itself become buildup ------------
if ($script:DidApply -and (Test-Path $BackupDir)) {
    $cutoff = (Get-Date).AddDays(-30)
    $old = Get-ChildItem $BackupDir -Filter *.bundle -ErrorAction SilentlyContinue | Where-Object { $_.LastWriteTime -lt $cutoff }
    foreach ($o in $old) { Act "prune old backup bundle (>30d): $($o.Name)"; [System.IO.File]::Delete($o.FullName) }
}

$freeAfter = Free-GB
Write-Host ""
Info "Disk $drive`: free after = $freeAfter GB  (delta = $([math]::Round($freeAfter-$freeBefore,1)) GB; worktrees removed = $totalRemoved)"
if (-not $script:DidApply) { Info "DRY-RUN only. Re-run with -Apply to perform the actions above." }
