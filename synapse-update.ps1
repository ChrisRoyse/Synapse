<#
.SYNOPSIS
  ┌──────────────────────────────────────────────────────────────────────────┐
  │  SYNAPSE UPDATER  —  one command to get on the latest Synapse.            │
  │  Pull the newest code  ->  rebuild the binary  ->  reconnect every client │
  └──────────────────────────────────────────────────────────────────────────┘

  RUN THIS DAILY (or whenever you want the newest build). Synapse ships fixes
  and new tools frequently, so a daily run keeps your local daemon and every
  connected client (Claude Code, Codex, Claude Desktop) current with zero fuss.

      powershell -ExecutionPolicy Bypass -File .\synapse-update.ps1

.DESCRIPTION
  This is the supported "keep me up to date" entrypoint for end users. It does
  exactly three things, in order, and fails loud (never silently) if any step
  cannot complete:

    1. PULL    git fetch + fast-forward pull of this checkout from its origin
               remote, so you are building the newest committed code.
    2. BUILD   rebuild and install the native synapse-mcp.exe daemon by handing
               off to scripts/synapse-setup.ps1 (the battle-tested installer).
    3. RECONNECT  the installer also restarts the auto-start daemon and re-wires
               the Windows-side MCP clients, then verifies `health`.

  WHY THIS WRAPPER EXISTS (vs. running the installer directly):
    * It adds the `git pull` that the installer intentionally does not do.
    * It sizes the build to the FULL machine. Cargo normally uses all logical
      CPUs, but a user-level ~/.cargo/config.toml `[build] jobs = N` cap
      silently serializes the build on any machine that has one (a jobs=1 cap
      once turned native dependency builds into long serial waits). This script
      sets CARGO_BUILD_JOBS — which outranks every config file — to the host's
      logical CPU count, RAM-guarded so low-memory machines don't swap.

  PORTABILITY: nothing here is machine-specific. The repo location is the
  script's own folder, Visual Studio is located via the standard `vswhere`,
  and no tokens, absolute user paths, or secrets are embedded. It is safe to
  commit and to run on any Windows 10/11 x64 system.

  PREREQUISITES (the script checks and tells you exactly what is missing):
    * git, and a Rust toolchain (rustup/cargo) — https://rustup.rs

.PARAMETER NoPull
  Skip the git pull and just rebuild/reconnect from the current checkout.

.PARAMETER SetupArgs
  Any remaining arguments are forwarded verbatim to scripts/synapse-setup.ps1
  (e.g. -Bind 127.0.0.1:7700, -SkipClientWiring, -Remove).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\synapse-update.ps1

.EXAMPLE
  # Update the code+binary but leave client configs untouched:
  .\synapse-update.ps1 -SkipClientWiring
#>
[CmdletBinding()]
param(
    [switch]$NoPull,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$SetupArgs
)

$ErrorActionPreference = 'Stop'
function Info($m) { Write-Host "[synapse-update] $m" }
function Step($m) { Write-Host "`n=== $m ===" -ForegroundColor Cyan }
function Die($m)  { throw "[synapse-update] FATAL: $m" }

$RepoRoot   = $PSScriptRoot
$SetupScript = Join-Path $RepoRoot 'scripts\synapse-setup.ps1'

Step "Preflight"
if (-not (Test-Path $SetupScript)) {
    Die "Cannot find scripts\synapse-setup.ps1 next to this script. Run synapse-update.ps1 from inside the Synapse repo checkout."
}
foreach ($tool in 'git', 'cargo') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Die "'$tool' is not on PATH. Install it first (cargo: https://rustup.rs) and re-run."
    }
}
Info "repo: $RepoRoot"

# ---------------------------------------------------------------------------
# 1. PULL — fast-forward this checkout to the newest committed code.
# ---------------------------------------------------------------------------
if ($NoPull) {
    Info "Skipping git pull (-NoPull)."
} else {
    Step "Pulling latest from origin"
    if (-not (git -C $RepoRoot rev-parse --is-inside-work-tree 2>$null)) {
        Die "$RepoRoot is not a git checkout. Clone with: git clone https://github.com/ChrisRoyse/Synapse.git"
    }
    $before = (git -C $RepoRoot rev-parse --short HEAD).Trim()
    git -C $RepoRoot fetch --prune origin
    if ($LASTEXITCODE -ne 0) { Die "git fetch failed." }
    git -C $RepoRoot pull --ff-only
    if ($LASTEXITCODE -ne 0) {
        Die "git pull --ff-only failed. You likely have local commits or changes that diverge from origin. Resolve them (e.g. 'git stash' or commit on a branch) and re-run."
    }
    $after = (git -C $RepoRoot rev-parse --short HEAD).Trim()
    if ($before -eq $after) { Info "Already up to date at $after." }
    else { Info "Updated $before -> $after." }
}

# ---------------------------------------------------------------------------
# 2. BUILD PARALLELISM — size the build to the full machine, on any machine.
#    CARGO_BUILD_JOBS (env) outranks any `[build] jobs = N` cap in a user or
#    repo config.toml, so this guarantees full-core builds regardless of local
#    cargo configuration. An explicit CARGO_BUILD_JOBS set by the caller for
#    this session is honored untouched.
# ---------------------------------------------------------------------------
Step "Sizing build parallelism"
if ($env:CARGO_BUILD_JOBS) {
    Info "CARGO_BUILD_JOBS=$($env:CARGO_BUILD_JOBS) already set for this session; honoring it."
} else {
    $logicalCpus = [Environment]::ProcessorCount
    $ramGb = [math]::Floor((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
    # Heavy rustc/cl.exe jobs peak around 1.5 GB each. On low-RAM machines cap
    # the job count so the build doesn't swap — swapping costs far more time
    # than running fewer jobs. On well-provisioned machines this never binds
    # and every logical CPU gets a job.
    $ramCappedJobs = [int][math]::Max(1, [math]::Floor($ramGb / 1.5))
    $jobs = [int][math]::Min($logicalCpus, $ramCappedJobs)
    $env:CARGO_BUILD_JOBS = "$jobs"
    Info "build jobs: $jobs (logical CPUs: $logicalCpus, RAM: $ramGb GB)"
}
# cmake-driven -sys crates parallelize from this; harmless where unused.
if (-not $env:CMAKE_BUILD_PARALLEL_LEVEL) {
    $env:CMAKE_BUILD_PARALLEL_LEVEL = $env:CARGO_BUILD_JOBS
}

# ---------------------------------------------------------------------------
# 3. BUILD + RECONNECT — hand off to the installer (build, install, restart the
#    auto-start daemon, re-wire MCP clients, verify health).
# ---------------------------------------------------------------------------
Step "Rebuilding and reconnecting (scripts\synapse-setup.ps1)"
$setupParams = @{ SourceDir = $RepoRoot }
# Forward only real pass-through args. When no -SetupArgs are given, $SetupArgs is
# $null; splatting it as `@SetupArgs` passes a stray positional $null that binds to
# synapse-setup.ps1's first positional parameter (-Bind), coercing it to '' and
# failing with "Cannot bind argument to parameter 'Bind' because it is an empty
# string." Filtering to a clean array (empty when none) splats to nothing instead.
$forwardSetupArgs = @($SetupArgs | Where-Object { $null -ne $_ })
& $SetupScript @setupParams @forwardSetupArgs
if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne $null) {
    Die "synapse-setup.ps1 exited with code $LASTEXITCODE."
}

# ---------------------------------------------------------------------------
# 5. DISK HYGIENE — ensure the scheduled maintenance task exists so stale git
#    worktrees and Cargo target/ artifacts are pruned automatically and never
#    pile up again (they previously grew to ~200 GB across ~46 worktrees and
#    starved builds). Non-fatal: a scheduling hiccup must never block an update.
# ---------------------------------------------------------------------------
Step "Ensuring disk-hygiene maintenance task"
try {
    $taskScript = Join-Path $RepoRoot 'scripts\install-maintenance-task.ps1'
    $pwshExe = (Get-Command pwsh -ErrorAction SilentlyContinue)
    if (-not (Test-Path $taskScript)) {
        Info "scripts\install-maintenance-task.ps1 not found; skipping (older checkout?)."
    } elseif (-not $pwshExe) {
        Info "pwsh (PowerShell 7) not found; install it to enable auto-maintenance: winget install Microsoft.PowerShell"
    } elseif (Get-ScheduledTask -TaskName 'SynapseRepoMaintenance' -ErrorAction SilentlyContinue) {
        Info "Maintenance task already registered (weekly worktree+target cleanup). OK."
    } else {
        & $pwshExe.Source -NoProfile -ExecutionPolicy Bypass -File $taskScript | ForEach-Object { Info $_ }
    }
} catch {
    Info "Could not ensure maintenance task ($($_.Exception.Message)). Register it manually with: pwsh -File scripts\install-maintenance-task.ps1"
}

Step "Done"
Info "Synapse is updated, rebuilt, and reconnected. Run this script daily to stay current."
