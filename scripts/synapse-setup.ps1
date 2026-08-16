<#
.SYNOPSIS
  Windows-side Synapse setup: build/install the daemon binary, deploy bundled
  profiles, generate the bearer token, register the auto-start HTTP daemon, and
  (optionally) wire the Windows-side MCP clients. Idempotent and fail-loud.

.DESCRIPTION
  Synapse has exactly ONE controlling body: the Windows-native synapse-mcp.exe
  HTTP daemon. It is the only process that can do real Win32 SendInput / UI
  Automation / WGC-DXGI capture, and it controls BOTH Windows programs (native
  windows) and WSL programs (WSLg GUI apps render as real Windows windows;
  act_run_shell / act_launch reach WSL CLIs via wsl.exe). Every MCP client — on
  Windows or in WSL — connects to this one daemon.

  This script makes that body exist and run, then points the Windows-side
  clients at it. The WSL-side entry (scripts/synapse-install.sh) calls this same
  script through interop and then wires the WSL-side clients.

  Robustness decisions baked in here (learned the hard way):
    * Build from the LOCAL source path (cd into -SourceDir). Building over a
      \\wsl.localhost / pushd-mapped drive bakes transient Z:\ paths into the
      binary (CARGO_MANIFEST_DIR) and intermittently fails cargo's dep-info
      step. -SourceDir must be a real local path.
    * Deploy the bundled profiles NEXT TO the installed exe so the daemon's
      executable-relative profile lookup always resolves, and ALSO pass
      --profile-dir explicitly. A compile-time CARGO_MANIFEST_DIR profile path
      never exists on an installed host.
    * Build into the SOURCE CHECKOUT'S OWN `target` tree by default. Re-installs
      stay incremental (the tree persists across runs) and, because the tree
      belongs to exactly one checkout, two checkouts can never share and poison
      one fingerprint database -- the failure that originally motivated a
      per-checkout %LOCALAPPDATA% cache. Keeping it in the checkout additionally
      means the build artifacts are where the operator already accounts for
      them, instead of growing a hidden multi-GB tree under %LOCALAPPDATA%
      (#1857). Any other target directory is an explicit, footprint-reported
      opt-in via -CargoTarget with -AllowAlternateBuildTarget.

  Nothing here silently falls back: every prerequisite is checked and throws a
  clear error naming exactly what failed and how to fix it.

.PARAMETER SourceDir
  Path to a LOCAL synapse source checkout. Required for every install path,
  including -SkipBuild: candidate-daemon validation and embedded model-pin
  verification both read the checkout, so it is not a build-only input. Must be
  on a real local drive (not \\wsl.localhost or a pushd-mapped UNC drive).

.PARAMETER SkipBuild
  Do not build. The already-installed synapse-mcp.exe at -ExePath is what gets
  validated and deployed, so this does NOT pick up a fresh local
  `cargo build --release` output; omit -SkipBuild to build and deploy the
  checkout named by -SourceDir. -SourceDir is still required.

.PARAMETER BuildTimeoutMinutes
  Maximum time to allow the release build to run. The build process tree is
  launched inside a Windows Job Object with kill-on-close, so Cargo/rustc
  children cannot survive if this setup process exits or is killed.

.PARAMETER Stop
  Stop the running daemon and LEAVE it stopped, gracefully and reversibly
  (#2083). This is the correct way to take the daemon down for a controlled
  test; `Stop-ScheduledTask` alone is not, because Task Scheduler stops only
  the task instance it launched and orphans the hidden supervisor, which then
  relaunches the daemon and races any second supervisor you start afterwards.

  -Stop, in order: writes a durable supervisor stop-request that revokes the
  supervisor's restart authority beyond this process's own lifetime; stops and
  DISABLES the scheduled task (reversible, unlike -Remove which unregisters
  it); asks the daemon to drain over authenticated POST /shutdown so it
  finishes in-flight work, releases input leases, flushes and closes the Calyx
  vault and writes its graceful lifecycle exit record; waits for the supervisor
  to park; then verifies zero daemons, zero supervisors and a released bind,
  re-checking across a settle window so a backoff relaunch cannot hide in it.
  Every phase logs its own evidence line, and the whole transaction is
  persisted to logs\daemon-operator-lifecycle-current.json.

  Fails closed on active MCP clients unless -ForceStop/-ForceRestart is given.
  Undo with -Start. Requires no -SourceDir.

.PARAMETER Start
  Start the daemon that -Stop took down: clears the durable stop-request,
  re-enables and starts the scheduled task, waits for authenticated /health,
  and verifies exactly one supervisor and exactly one daemon. It also reads the
  boot-time previous-shutdown verdict back from /health and fails if it
  disagrees with the on-disk lifecycle record, so a dirty stop is visible at
  the next start instead of being silently inherited. Requires no -SourceDir.

.PARAMETER ForceRestart
  Also accepted as -ForceStop. Permit setup/remove/stop to stop the shared daemon even when active HTTP MCP
  sessions, live client TCP connections, or bridge children are present. Without
  this explicit maintenance flag, setup fails closed instead of interrupting
  another agent. The normal stop path is authenticated graceful shutdown; this
  flag also permits an exact-PID forced stop only after the graceful path fails
  for a verified legacy or unresponsive synapse-mcp.exe process.

.PARAMETER AllowedPermissions
  Explicit M3 permission grant list passed to the daemon as
  --allowed-permissions. Defaults to the full local agent grant set required by
  the installed Codex/Synapse transport. Pass an explicit empty string for the
  daemon's fail-closed read-only default. Use a whitespace/comma-separated list
  such as "READ_EVENTS READ_REFLEX READ_PROFILE READ_STORAGE WRITE_STORAGE".

.PARAMETER EnableAudio
  Persistently enable WASAPI loopback and speech-to-text in the supervised
  daemon. This is an explicit deployment setting: setup carries
  --enable-audio through candidate validation, live argument drift checks, and
  the generated restart supervisor. READ_AUDIO must also be present in
  -AllowedPermissions; inconsistent configurations fail before the build.

.PARAMETER CalyxConfigPath
  Optional Calyx tuning file passed explicitly to both the isolated candidate
  and the installed daemon as --calyx-config. Defaults to
  SYNAPSE_CALYX_CONFIG when set. Setup resolves the path and verifies the file
  is readable before building; the real candidate daemon validates its TOML
  content before handoff so candidate/live tuning cannot silently diverge.

.PARAMETER Bind
  Loopback address the daemon binds. Default 127.0.0.1:7700.

.PARAMETER ChromeNativeHostExePath
  Legacy diagnostic native-host path. The normal end-user Chrome bridge uses
  direct localhost HTTP registration plus WebSocket command delivery; setup
  does not install or launch native messaging because Chrome may create a
  visible cmd.exe wrapper for native hosts.

  Synapse does not mutate Chrome ExtensionSettings policy and does not inspect,
  disable, or reconfigure unrelated extensions. The normal authenticated-
  profile bridge has no debugger, nativeMessaging, or management permission and
  reloads itself with chrome.runtime.reload. Deep evaluation, trusted input,
  PDF, dialogs, file upload, drag/drop, and emulation run only on a session-owned
  raw-CDP browser launched by Synapse with a dedicated non-default profile.

.PARAMETER MaintenanceLockPath
  File-lock Source of Truth that serializes setup/remove across multiple
  agents. The file contents name the owning PID and cleanup policy; the held
  FileStream is the actual lock and is released by Windows when setup exits.

.PARAMETER WireClients
  Wire the Windows-side MCP clients (Claude Code and Codex via HTTP, Claude
  Desktop via the connect bridge). Default $true.

.PARAMETER Remove
  Uninstall: stop + unregister the scheduled task. Leaves the DB, token, and
  binary in place unless -Purge is also given.

.PARAMETER Purge
  With -Remove, also delete the daemon DB, deployed profiles, and token.
  Destroying a populated vault additionally requires -ConfirmVaultDestruction:
  the vault has no automatic backup, so a single flag must never be able to
  delete captured history (issue #1875).

.PARAMETER ConfirmVaultDestruction
  Required alongside -Purge when the Calyx vault at -DbPath holds any durable
  sequences. Setup writes a deletion record next to the vault directory (which
  therefore survives the deletion) before removing anything, and refuses to
  delete a populated vault it could not record.

.PARAMETER ActiveIssue
  Optional current GitHub issue number/ref to preserve in Codex restart
  handoffs. Defaults to SYNAPSE_ACTIVE_ISSUE when set. Accepts 1441, #1441, or
  a ChrisRoyse/Synapse issue URL.

.PARAMETER ManualInstallHealthRollbackProbe
  Operator-run rollback drill for manual FSV. Setup still builds and preflights
  a real candidate, installs it, and starts it through the real scheduled-task
  supervisor. After the installed daemon answers /health, setup rejects that
  health gate with an explicit manual-probe diagnostic so rollback must stop a
  real running candidate daemon, restore the previous binary, and re-read the
  daemon/Chrome bridge SoT. Requires -ForceRestart and a previous installed
  daemon binary; setup exits fail-loud after rollback.

.PARAMETER ManualInstallHealthRollbackPauseMode
  With -ManualInstallHealthRollbackProbe, controls the rollback maintenance
  pause edge. "normal" uses the real bridge pause result. "require_active_ack"
  waits for the candidate daemon to report a real active Chrome bridge host
  before rejecting install health, so rollback must receive a real bridge pause
  ACK. "force_unacknowledged" returns an explicit unacknowledged-pause
  diagnostic after reading /health so the rollback branch can be physically
  exercised without waiting for a random bridge outage.

.PARAMETER InstallHealthTimeoutSeconds
  Minimum seconds to wait for the installed daemon to answer /health after the
  scheduled task starts it. Defaults to 600 because the live Calyx-backed
  database can spend several minutes in startup preflights before the HTTP
  listener is bound. This is a floor, not a ceiling: past this deadline setup
  keeps waiting only while the daemon proves forward progress (new startup log
  phase, CPU time burned, disk I/O transferred, or vault files mutated), and
  fails closed the moment progress stops. Setup always prints
  process/socket/startup log readbacks before rollback.

.PARAMETER InstallHealthMaxSeconds
  Absolute ceiling for the installed-daemon and rollback-daemon startup gates.
  Reachable only while forward progress keeps being observed. A one-time Calyx
  WAL-backlog paydown at open has been measured at ~40 minutes on this host, so
  the default is 5400s (90 minutes). Hitting this ceiling is reported as its own
  verdict (progress observed, budget exhausted) and never as a dead daemon.
  Setup also writes the effective maximum of this value and
  InstallHealthTimeoutSeconds as Codex's required-MCP startup timeout so a real
  daemon cold start cannot outlive Codex's client bootstrap deadline.

.PARAMETER InstallHealthProgressStallSeconds
  How long the daemon may make zero forward progress (no new startup log phase,
  no CPU time, no disk I/O, no vault file mutation) before setup declares it
  hung and fails closed. Only armed after -InstallHealthTimeoutSeconds has
  elapsed, so it can never shorten today's minimum wait.

.PARAMETER ResumeChromeBridgePending
  Resume only a previously checkpointed chrome_bridge_activation phase. The
  checkpoint must match the installed binary, live daemon PID/path/arguments,
  daemon-run ledger, bearer-token bytes, setup/bridge installer bytes, and
  scheduled-task definition. This mode never builds, copies, drains, registers
  a task, or rewires clients.

.PARAMETER CargoTarget
  Cargo target directory for the release build. Defaults to the source
  checkout's own `target` directory (`<SourceDir>\target`). Supplying any other
  path additionally requires -AllowAlternateBuildTarget, and setup reports that
  tree's measured on-disk footprint before using it.

.PARAMETER AllowAlternateBuildTarget
  Authorizes a -CargoTarget outside the source checkout. Without it setup fails
  closed rather than silently growing a build tree the operator did not choose.

.PARAMETER ChromeBridgePendingPath
  Durable, versioned Source-of-Truth file for a pending or completed
  chrome_bridge_activation phase.
#>
[CmdletBinding()]
param(
    [string]$SourceDir,
    [switch]$SkipBuild,
    [string]$Bind        = '127.0.0.1:7700',
    [string]$ExePath     = "$env:USERPROFILE\.cargo\bin\synapse-mcp.exe",
    [string]$ChromeNativeHostExePath = "$env:USERPROFILE\.cargo\bin\synapse-chrome-native-host.exe",
    [string]$CargoTarget = '',
    [string]$DbPath      = "$env:LOCALAPPDATA\synapse\db-daemon",
    [string]$ProfilesDir = "$env:USERPROFILE\.cargo\bin\profiles",
    [string]$LogDir      = "$env:LOCALAPPDATA\synapse\logs",
    # Program artifacts the daemon's autostart depends on (the hidden launcher
    # and the supervisor script). Deliberately NOT under $LogDir: that directory
    # is what an operator or a retention sweep empties to reclaim space, and
    # deleting logs must never be able to disable autostart (#1862).
    [string]$RuntimeBinDir = "$env:LOCALAPPDATA\synapse\bin",
    [string]$TokenPath   = "$env:APPDATA\synapse\token.txt",
    [string]$CodexToolSurfaceSnapshotPath = "$env:APPDATA\synapse\codex-tool-surface.json",
    [string]$ActiveIssue = $env:SYNAPSE_ACTIVE_ISSUE,
    [string]$TaskName    = 'SynapseMcpDaemon',
    [string]$MaintenanceLockPath = "$env:LOCALAPPDATA\synapse\setup-maintenance.lock.json",
    [string]$ChromeBridgePendingPath = "$env:LOCALAPPDATA\synapse\setup-chrome-bridge-pending.json",
    [ValidateRange(1, 1440)][int]$BuildTimeoutMinutes = 90,
    [int]$PostExitParentPid = 0,
    [string]$PostExitContinuationReason = '',
    [string]$PostExitManifestPath = '',
    # -ForceStop is the same authority under the name that reads correctly at a
    # -Stop call site (#2083). It is an alias, not a second flag: there is
    # exactly one force authority threaded through Assert-SynapseRestartAllowed
    # and Stop-SynapseMcpProcesses.
    [Alias('ForceStop')]
    [switch]$ForceRestart,
    [switch]$EnableAudio,
    [AllowNull()][string]$AllowedPermissions = $(if ([string]::IsNullOrWhiteSpace($env:SYNAPSE_MCP_ALLOWED_PERMISSIONS)) { 'READ_EVENTS READ_REFLEX READ_PROFILE READ_STORAGE WRITE_STORAGE' } else { $env:SYNAPSE_MCP_ALLOWED_PERMISSIONS }),
    [AllowNull()][string]$CalyxConfigPath = $env:SYNAPSE_CALYX_CONFIG,
    [ValidateRange(60, 3600)]
    [int]$InstallHealthTimeoutSeconds = 600,
    [ValidateRange(600, 86400)]
    [int]$InstallHealthMaxSeconds = 5400,
    [ValidateRange(60, 3600)]
    [int]$InstallHealthProgressStallSeconds = 300,
    [switch]$ManualInstallHealthRollbackProbe,
    [ValidateSet('normal','require_active_ack','force_unacknowledged')]
    [string]$ManualInstallHealthRollbackPauseMode = 'normal',
    [switch]$ResumeChromeBridgePending,
    [switch]$AllowAlternateBuildTarget,
    [switch]$SkipClientWiring,
    [switch]$Stop,
    [switch]$Start,
    [switch]$Remove,
    [switch]$Purge,
    [switch]$ConfirmVaultDestruction
)

$ErrorActionPreference = 'Stop'
$CodexMcpStartupTimeoutSeconds = [Math]::Max($InstallHealthTimeoutSeconds, $InstallHealthMaxSeconds)
$SynapseChromeBridgeMaintenancePauseMs = 720000
$SynapseChromeBridgeMaintenanceCloseDrainMs = 7000
$SynapseChromeBridgeMaintenanceResumeProbeAfterMs = 30000
$SynapseChromeBridgeReconnectAlarmCushionMs = 45000
$SynapseChromeBridgeDefaultPostStartWaitMs = 30000
$SynapseChromeBridgeMaintenancePauseGuardMs = 60000
$SynapseChromeBridgeMaxPostStartWaitMs = $SynapseChromeBridgeMaintenancePauseMs + $SynapseChromeBridgeReconnectAlarmCushionMs + $SynapseChromeBridgeDefaultPostStartWaitMs
$SynapseChromeBridgeMaintenancePostStartWaitMs = $SynapseChromeBridgeMaintenanceResumeProbeAfterMs + $SynapseChromeBridgeReconnectAlarmCushionMs + $SynapseChromeBridgeDefaultPostStartWaitMs
# #2031: the per-request budget install-synapse-chrome-debugger.ps1 gives its
# own pre-Chrome authenticated /health readback. Setup must prove the
# post-handoff daemon answers inside this same budget before it invokes any
# path that runs that installer, otherwise a still-cold daemon fails the whole
# reload closed on startup timing alone.
$SynapseChromeBridgeInstallerHealthProbeTimeoutSec = 5
$SynapseChromeBridgeReloadReadinessTimeoutMs = 180000
$SynapseChromeBridgeReloadReadinessSuccessThreshold = 3
$SynapseChromeBridgeReloadReadinessSuccessSpacingMs = 250
$SynapseChromeBridgeReloadReadinessMinBackoffMs = 250
$SynapseChromeBridgeReloadReadinessMaxBackoffMs = 2000
$SynapseBindFinalDeadOwnerSettleSeconds = 15
$script:SynapseChromeBridgeMaintenancePauseUntilUnixMs = $null
$script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs = $null
$script:SynapseChromeBridgeMaintenancePausePrepared = $false
$script:SynapseChromeBridgeMaintenancePausePreparedBind = $null
$script:SynapseChromeBridgeMaintenancePausePreparedReason = $null
$script:SynapseChromeBridgeMaintenancePausePreparedResult = $null
$script:SynapseBindPostExitContinuationRequired = $false
$script:SynapseBindPostExitContinuationDetail = $null
$script:SynapsePostExitStartOnly = ($PostExitParentPid -gt 0 -and $PostExitContinuationReason -eq 'dead_owner_bind_after_install')
$script:SynapseManualInstallHealthRollbackProbe = [bool]$ManualInstallHealthRollbackProbe
$script:SynapseManualInstallHealthRollbackPauseMode = $ManualInstallHealthRollbackPauseMode
$script:SynapseSetupRepairManifestPath = $env:SYNAPSE_SETUP_REPAIR_MANIFEST
$script:SynapseSetupInvocationId = if (-not [string]::IsNullOrWhiteSpace($env:SYNAPSE_SETUP_INVOCATION_ID)) {
    $env:SYNAPSE_SETUP_INVOCATION_ID.Trim()
} else {
    "setup-$PID-$([guid]::NewGuid().ToString('N'))"
}
$script:SynapseChromeBridgePendingPath = $ChromeBridgePendingPath
$script:SynapseSetupPartialState = $null
$script:SynapseSetupPartialReadback = $null
$script:SynapseCurrentDaemonStagingDirectory = $null
$script:SynapseBundledProfilesManifestFileName = '.synapse-bundled-profiles.manifest.json'
$script:SynapseBundledProfilesQuarantineDirName = '.synapse-retired-bundled-profiles'
$script:SynapseBundledProfilesRollbackDirName = '.synapse-profile-reconcile-backups'
$script:SynapseLegacyRetiredBundledProfiles = @()
function Write-SynapsePostExitManifestState {
    param(
        [Parameter(Mandatory=$true)][string]$State,
        [string]$Message = '',
        [int]$ExitCode = 0,
        [AllowNull()]$Readback
    )

    if (-not $script:SynapsePostExitStartOnly) {
        return
    }
    if ([string]::IsNullOrWhiteSpace($PostExitManifestPath)) {
        throw "SYNAPSE_POST_EXIT_MANIFEST_PATH_MISSING reason=$PostExitContinuationReason remediation=post-exit continuation must receive -PostExitManifestPath so completion/failure state is physically recorded"
    }
    if (-not (Test-Path -LiteralPath $PostExitManifestPath -PathType Leaf)) {
        throw "SYNAPSE_POST_EXIT_MANIFEST_MISSING path=$PostExitManifestPath remediation=inspect the continuation launcher arguments and rerun setup; completion/failure cannot be accepted without manifest readback"
    }

    $manifest = Get-Content -Raw -LiteralPath $PostExitManifestPath | ConvertFrom-Json
    $now = (Get-Date).ToUniversalTime().ToString('o')
    $manifest | Add-Member -NotePropertyName state -NotePropertyValue $State -Force
    $manifest | Add-Member -NotePropertyName exit_code -NotePropertyValue $ExitCode -Force
    $manifest | Add-Member -NotePropertyName updated_at_utc -NotePropertyValue $now -Force
    if ($State -eq 'completed') {
        $manifest | Add-Member -NotePropertyName completed_at_utc -NotePropertyValue $now -Force
        $manifest | Add-Member -NotePropertyName failure -NotePropertyValue $null -Force
    } elseif ($State -eq 'failed') {
        $manifest | Add-Member -NotePropertyName failed_at_utc -NotePropertyValue $now -Force
        $manifest | Add-Member -NotePropertyName failure -NotePropertyValue ([ordered]@{
            message = $Message
            remediation = 'inspect stdout/stderr/readback fields and rerun setup after fixing the named fatal condition'
        }) -Force
    }
    if (-not [string]::IsNullOrWhiteSpace($Message)) {
        $manifest | Add-Member -NotePropertyName message -NotePropertyValue $Message -Force
    }
    if ($null -ne $Readback) {
        $manifest | Add-Member -NotePropertyName readback -NotePropertyValue $Readback -Force
    }
    $json = ($manifest | ConvertTo-Json -Depth 40) + "`n"
    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($PostExitManifestPath, $json, $encoding)
}

function Write-SynapseSetupRepairManifestState {
    param(
        [Parameter(Mandatory=$true)][ValidateSet('completed','failed','handoff_started','bridge_pending')][string]$State,
        [string]$Message = '',
        [int]$ExitCode = 0,
        [AllowNull()]$Readback,
        [string]$ContinuationManifestPath = ''
    )

    if ([string]::IsNullOrWhiteSpace($script:SynapseSetupRepairManifestPath)) {
        return
    }
    if (-not (Test-Path -LiteralPath $script:SynapseSetupRepairManifestPath -PathType Leaf)) {
        throw "SYNAPSE_SETUP_REPAIR_MANIFEST_MISSING path=$script:SynapseSetupRepairManifestPath remediation=external setup repair must stamp completion/failure in the parent manifest; inspect launch env SYNAPSE_SETUP_REPAIR_MANIFEST"
    }

    $manifest = Get-Content -Raw -LiteralPath $script:SynapseSetupRepairManifestPath | ConvertFrom-Json
    $now = (Get-Date).ToUniversalTime().ToString('o')
    $manifest | Add-Member -NotePropertyName state -NotePropertyValue $State -Force
    $manifest | Add-Member -NotePropertyName exit_code -NotePropertyValue $ExitCode -Force
    $manifest | Add-Member -NotePropertyName updated_at_utc -NotePropertyValue $now -Force
    if ($State -eq 'completed') {
        $manifest | Add-Member -NotePropertyName completed_at_utc -NotePropertyValue $now -Force
        $manifest | Add-Member -NotePropertyName failure -NotePropertyValue $null -Force
    } elseif ($State -eq 'failed') {
        $manifest | Add-Member -NotePropertyName failed_at_utc -NotePropertyValue $now -Force
        $manifest | Add-Member -NotePropertyName failure -NotePropertyValue ([ordered]@{
            message = $Message
            remediation = 'inspect stdout/stderr/readback fields and rerun setup repair after fixing the named fatal condition'
        }) -Force
    } elseif ($State -eq 'handoff_started') {
        $manifest | Add-Member -NotePropertyName handoff_started_at_utc -NotePropertyValue $now -Force
        $manifest | Add-Member -NotePropertyName failure -NotePropertyValue $null -Force
        if (-not [string]::IsNullOrWhiteSpace($ContinuationManifestPath)) {
            $manifest | Add-Member -NotePropertyName continuation_manifest_path -NotePropertyValue $ContinuationManifestPath -Force
        }
    } elseif ($State -eq 'bridge_pending') {
        $manifest | Add-Member -NotePropertyName bridge_pending_at_utc -NotePropertyValue $now -Force
        $manifest | Add-Member -NotePropertyName failure -NotePropertyValue $null -Force
    }
    if (-not [string]::IsNullOrWhiteSpace($Message)) {
        $manifest | Add-Member -NotePropertyName message -NotePropertyValue $Message -Force
    }
    if ($null -ne $Readback) {
        $manifest | Add-Member -NotePropertyName readback -NotePropertyValue $Readback -Force
    }
    $json = ($manifest | ConvertTo-Json -Depth 40) + "`n"
    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($script:SynapseSetupRepairManifestPath, $json, $encoding)
}

function Info($m)  { Write-Host "[synapse-setup] $m" }
# A non-fatal condition the operator must still see. Used for recorded
# capability gaps: the install is correct but something is genuinely
# unavailable, and silence would be dishonest (#1863).
function Warn($m)  { Write-Host "[synapse-setup] WARNING: $m" -ForegroundColor Yellow }

# ---------------------------------------------------------------------------
# Phase timing ledger
#
# Setup had no wall-clock instrumentation of any kind: not per phase, not in
# total, not on disk. That made "setup got slower" unfalsifiable and left every
# optimization argued from reading rather than from measurement -- which is how
# a 4x build-parallelism throttle sat in the build phase for two weeks without
# anyone being able to see its cost in a readback.
#
# Step() now closes the previous phase and opens the next, and the ledger is
# written to LogDir on BOTH outcomes. The failure case matters most: it names
# which phase a failed run was sitting in and for how long.
# ---------------------------------------------------------------------------
$script:SynapseSetupStartedAt = Get-Date
$script:SynapseSetupPhaseLedger = [System.Collections.Generic.List[object]]::new()
$script:SynapseSetupCurrentPhase = $null
$script:SynapseSetupCurrentPhaseStartedAt = $null
$script:SynapseSetupPhaseLedgerWritten = $false
$script:SynapseSetupPhaseLedgerWriteError = $null

function Stop-SynapseSetupPhaseTimer {
    if ($null -eq $script:SynapseSetupCurrentPhase) { return }
    $endedAt = Get-Date
    # A phase is an object with schema fields, not a dictionary whose keys only
    # happen to look like properties. Sort-Object resolves properties; an
    # OrderedDictionary therefore turns every named sort key into $null and
    # silently preserves insertion order (#2208).
    $script:SynapseSetupPhaseLedger.Add([pscustomobject][ordered]@{
        index = $script:SynapseSetupPhaseLedger.Count + 1
        phase = $script:SynapseSetupCurrentPhase
        started_at_utc = $script:SynapseSetupCurrentPhaseStartedAt.ToUniversalTime().ToString('o')
        ended_at_utc = $endedAt.ToUniversalTime().ToString('o')
        elapsed_seconds = [math]::Round(($endedAt - $script:SynapseSetupCurrentPhaseStartedAt).TotalSeconds, 3)
    })
    $script:SynapseSetupCurrentPhase = $null
    $script:SynapseSetupCurrentPhaseStartedAt = $null
}

function Step($m) {
    Stop-SynapseSetupPhaseTimer
    $script:SynapseSetupCurrentPhase = $m
    $script:SynapseSetupCurrentPhaseStartedAt = Get-Date
    $sinceStart = [int]((Get-Date) - $script:SynapseSetupStartedAt).TotalSeconds
    Write-Host "`n=== $m === (t+${sinceStart}s)" -ForegroundColor Cyan
}

function Write-SynapseSetupPhaseLedger {
    <#
      Durable per-phase wall-clock readback. Never throws: a ledger-write
      failure must not mask the real outcome, least of all a real failure.
      Returns the ledger path, or $null if it could not be written.
    #>
    param(
        [Parameter(Mandatory=$true)][ValidateSet('completed','failed')][string]$Outcome,
        [string]$Message = ''
    )

    if ($script:SynapseSetupPhaseLedgerWritten) { return $null }
    $script:SynapseSetupPhaseLedgerWriteError = $null
    $ledgerPath = $null
    $temporaryPath = $null
    $replacementBackupPath = $null
    $publishPhase = 'not_started'
    $publishCompleted = $false
    $readbackValidated = $false
    $expectedBytes = $null
    try {
        # Close whatever phase was open so a failed run still accounts for the
        # phase it died in rather than dropping it.
        Stop-SynapseSetupPhaseTimer
        $script:SynapseSetupPhaseLedgerWritten = $true
        $endedAt = Get-Date
        $ledgerDir = $LogDir
        if ([string]::IsNullOrWhiteSpace($ledgerDir)) {
            throw 'LogDir is empty; no phase-ledger target can be resolved'
        }
        New-Item -ItemType Directory -Force -Path $ledgerDir -ErrorAction Stop | Out-Null
        $ledgerPath = Join-Path $ledgerDir 'setup-phase-timings.json'
        $phases = @($script:SynapseSetupPhaseLedger)
        # Numeric conversion makes the ordering independent of formatting, and
        # -Stable keeps the original phase order when durations tie.
        $slowest = @(
            $phases |
                Sort-Object -Stable -Property @{
                    Expression = { [double]$_.elapsed_seconds }
                    Descending = $true
                } |
                Select-Object -First 5
        )
        $ledger = [ordered]@{
            schema = 'synapse_setup_phase_timing_ledger/v1'
            outcome = $Outcome
            message = $Message
            pid = $PID
            started_at_utc = $script:SynapseSetupStartedAt.ToUniversalTime().ToString('o')
            ended_at_utc = $endedAt.ToUniversalTime().ToString('o')
            total_elapsed_seconds = [math]::Round(($endedAt - $script:SynapseSetupStartedAt).TotalSeconds, 3)
            skip_build = [bool]$SkipBuild
            force_restart = [bool]$ForceRestart
            # Recorded so two runs are comparable: a build-phase time means
            # nothing without the parallelism it was produced at.
            cargo_build_jobs = $env:CARGO_BUILD_JOBS
            cmake_build_parallel_level = $env:CMAKE_BUILD_PARALLEL_LEVEL
            logical_cpus = [Environment]::ProcessorCount
            phase_count = $phases.Count
            phases = $phases
            slowest_phases = $slowest
        }
        $json = ($ledger | ConvertTo-Json -Depth 12) + "`n"
        $expectedBytes = [System.Text.UTF8Encoding]::new($false).GetBytes($json)
        $expectedSha256 = [Convert]::ToHexString([System.Security.Cryptography.SHA256]::HashData($expectedBytes))
        $temporaryPath = "$ledgerPath.tmp.$PID.$([guid]::NewGuid().ToString('N'))"
        $replacementBackupPath = "$ledgerPath.replace-backup.$PID.$([guid]::NewGuid().ToString('N'))"

        $publishPhase = 'temporary_write'
        $temporaryStream = [System.IO.FileStream]::new(
            $temporaryPath,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None,
            4096,
            [System.IO.FileOptions]::WriteThrough
        )
        try {
            $temporaryStream.Write($expectedBytes, 0, $expectedBytes.Length)
            $temporaryStream.Flush($true)
        } finally {
            $temporaryStream.Dispose()
        }

        if (Test-Path -LiteralPath $ledgerPath -PathType Leaf) {
            $publishPhase = 'replace'
            [System.IO.File]::Replace($temporaryPath, $ledgerPath, $replacementBackupPath, $false)
        } else {
            $publishPhase = 'create'
            [System.IO.File]::Move($temporaryPath, $ledgerPath)
        }
        $publishCompleted = $true

        $publishPhase = 'readback'
        $actualBytes = [System.IO.File]::ReadAllBytes($ledgerPath)
        $actualSha256 = [Convert]::ToHexString([System.Security.Cryptography.SHA256]::HashData($actualBytes))
        if ($actualBytes.Length -ne $expectedBytes.Length -or
            $actualSha256 -cne $expectedSha256 -or
            -not [System.Security.Cryptography.CryptographicOperations]::FixedTimeEquals($actualBytes, $expectedBytes)) {
            throw "published bytes differ from the exact serialized ledger expected_length=$($expectedBytes.Length) actual_length=$($actualBytes.Length) expected_sha256=$expectedSha256 actual_sha256=$actualSha256"
        }
        $readback = [System.Text.Encoding]::UTF8.GetString($actualBytes) | ConvertFrom-Json -ErrorAction Stop
        if ([string]$readback.schema -ne 'synapse_setup_phase_timing_ledger/v1' -or
            [string]$readback.outcome -ne $Outcome -or
            [int]$readback.pid -ne $PID) {
            throw ("published JSON identity mismatch expected_schema={0} actual_schema={1} expected_outcome={2} actual_outcome={3} expected_pid={4} actual_pid={5}" -f `
                    'synapse_setup_phase_timing_ledger/v1',
                    $readback.schema,
                    $Outcome,
                    $readback.outcome,
                    $PID,
                    $readback.pid)
        }
        $readbackValidated = $true

        $publishPhase = 'cleanup'
        if (Test-Path -LiteralPath $replacementBackupPath -PathType Leaf) {
            [System.IO.File]::Delete($replacementBackupPath)
        }
        if ((Test-Path -LiteralPath $temporaryPath) -or (Test-Path -LiteralPath $replacementBackupPath)) {
            throw "owned atomic-publish artifact remains temp=$temporaryPath backup=$replacementBackupPath"
        }
        $publishPhase = 'completed'
        return $ledgerPath
    } catch {
        $detail = ($_.Exception.Message -replace '\s+', ' ').Trim()
        $target = if ([string]::IsNullOrWhiteSpace($ledgerPath)) { '<unresolved>' } else { $ledgerPath }
        $targetState = '<unresolved>'
        if (-not [string]::IsNullOrWhiteSpace($ledgerPath)) {
            if (Test-Path -LiteralPath $ledgerPath -PathType Leaf) {
                try {
                    $failureTargetBytes = [System.IO.File]::ReadAllBytes($ledgerPath)
                    $failureTargetSha256 = [Convert]::ToHexString([System.Security.Cryptography.SHA256]::HashData($failureTargetBytes))
                    $exactExpected = (
                        $null -ne $expectedBytes -and
                        $failureTargetBytes.Length -eq $expectedBytes.Length -and
                        [System.Security.Cryptography.CryptographicOperations]::FixedTimeEquals($failureTargetBytes, $expectedBytes)
                    )
                    $targetState = "file length=$($failureTargetBytes.Length) sha256=$failureTargetSha256 exact_expected=$exactExpected"
                    if ($exactExpected) {
                        $publishCompleted = $true
                        try {
                            $failureReadback = [System.Text.Encoding]::UTF8.GetString($failureTargetBytes) | ConvertFrom-Json -ErrorAction Stop
                            $readbackValidated = (
                                [string]$failureReadback.schema -eq 'synapse_setup_phase_timing_ledger/v1' -and
                                [string]$failureReadback.outcome -eq $Outcome -and
                                [int]$failureReadback.pid -eq $PID
                            )
                        } catch {
                            $targetState += " json_identity_error=$(($_.Exception.Message -replace '\s+', ' ').Trim())"
                        }
                    }
                } catch {
                    $targetState = "file_unreadable detail=$(($_.Exception.Message -replace '\s+', ' ').Trim())"
                }
            } elseif (Test-Path -LiteralPath $ledgerPath -PathType Container) {
                $targetState = 'directory'
            } else {
                $targetState = 'absent'
            }
        }
        $publicationTruth = if ($readbackValidated) {
            'the ledger was published and exact readback passed, but owned-artifact cleanup failed; preserve the named paths and remove only the named temp/backup after inspection'
        } elseif ($publishCompleted) {
            'the target replacement completed but exact readback did not pass; preserve the named recovery backup and inspect or restore it before trusting the target'
        } else {
            'an exact new ledger was not observed; inspect the reported target state and named recovery paths before deciding which prior bytes are authoritative; no fallback path was used'
        }
        $script:SynapseSetupPhaseLedgerWriteError = (
            'SYNAPSE_SETUP_PHASE_LEDGER_WRITE_FAILED outcome={0} phase={1} published={2} readback_validated={3} log_dir=[{4}] target=[{5}] target_state=[{6}] temp=[{7}] recovery_backup=[{8}] detail=[{9}] remediation={10}; this diagnostic does not replace the primary setup outcome' -f `
                $Outcome,
                $publishPhase,
                $publishCompleted,
                $readbackValidated,
                $LogDir,
                $target,
                $targetState,
                $(if ([string]::IsNullOrWhiteSpace($temporaryPath)) { '<unresolved>' } else { $temporaryPath }),
                $(if ([string]::IsNullOrWhiteSpace($replacementBackupPath)) { '<unresolved>' } else { $replacementBackupPath }),
                $detail,
                $publicationTruth
        )
        return $null
    } finally {
        if (-not [string]::IsNullOrWhiteSpace($temporaryPath) -and (Test-Path -LiteralPath $temporaryPath -PathType Leaf)) {
            try {
                [System.IO.File]::Delete($temporaryPath)
            } catch {
                $cleanupDetail = ($_.Exception.Message -replace '\s+', ' ').Trim()
                if ([string]::IsNullOrWhiteSpace($script:SynapseSetupPhaseLedgerWriteError)) {
                    $script:SynapseSetupPhaseLedgerWriteError = "SYNAPSE_SETUP_PHASE_LEDGER_WRITE_FAILED outcome=$Outcome phase=finally_cleanup published=$publishCompleted readback_validated=$readbackValidated target=[$ledgerPath] temp=[$temporaryPath] recovery_backup=[$replacementBackupPath] detail=[owned temporary-file cleanup failed: $cleanupDetail] remediation=preserve and inspect the exact named temporary file; remove only that file after proving no setup process owns it; this diagnostic does not replace the primary setup outcome"
                } else {
                    $script:SynapseSetupPhaseLedgerWriteError += " cleanup_error=[owned temporary-file cleanup failed path=$temporaryPath detail=$cleanupDetail]"
                }
            }
        }
    }
}
function Die($m)   {
    # Written before anything else can throw: a failed run's phase timings are
    # the record of WHERE it was stuck, which is exactly what is lost today.
    $failureLedgerPath = Write-SynapseSetupPhaseLedger -Outcome 'failed' -Message $m
    if ($failureLedgerPath) {
        Write-Host "[synapse-setup] phase timing ledger -> $failureLedgerPath" -ForegroundColor Yellow
    } elseif (-not [string]::IsNullOrWhiteSpace($script:SynapseSetupPhaseLedgerWriteError)) {
        Write-Host "[synapse-setup] WARNING: $script:SynapseSetupPhaseLedgerWriteError" -ForegroundColor Yellow
    }
    if (-not [string]::IsNullOrWhiteSpace($script:SynapseSetupRepairManifestPath)) {
        $state = 'failed'
        $continuationManifestPath = ''
        if ($m -match 'SYNAPSE_BIND_POST_EXIT_CONTINUATION_STARTED') {
            $state = 'handoff_started'
            if ($m -match 'manifest=([^ ]+)') {
                $continuationManifestPath = $Matches[1]
            }
        }
        try {
            Write-SynapseSetupRepairManifestState `
                -State $state `
                -Message $m `
                -ExitCode 1 `
                -Readback $null `
                -ContinuationManifestPath $continuationManifestPath
        } catch {
            throw "[synapse-setup] FATAL: $m ; SYNAPSE_SETUP_REPAIR_MANIFEST_FAILURE_WRITE_FAILED path=$script:SynapseSetupRepairManifestPath error=$($_.Exception.Message)"
        }
    }
    if ($script:SynapsePostExitStartOnly) {
        try {
            Write-SynapsePostExitManifestState -State 'failed' -Message $m -ExitCode 1 -Readback $null
        } catch {
            throw "[synapse-setup] FATAL: $m ; SYNAPSE_POST_EXIT_MANIFEST_FAILURE_WRITE_FAILED path=$PostExitManifestPath error=$($_.Exception.Message)"
        }
    }
    throw "[synapse-setup] FATAL: $m"
}

function Die-SynapseChromeBridgePending {
    param(
        [Parameter(Mandatory=$true)][string]$Message,
        [Parameter(Mandatory=$true)]$Readback
    )

    try {
        $checkpointFile = Write-SynapseChromeBridgeCheckpoint `
            -Path $script:SynapseChromeBridgePendingPath `
            -Checkpoint $Readback
        $Readback['checkpoint_path'] = $checkpointFile.Path
        $Readback['checkpoint_sha256'] = $checkpointFile.Sha256
        $Readback['checkpoint_len_bytes'] = $checkpointFile.LenBytes
    } catch {
        throw "[synapse-setup] FATAL: $Message ; SYNAPSE_SETUP_BRIDGE_CHECKPOINT_WRITE_FAILED path=$script:SynapseChromeBridgePendingPath error=$($_.Exception.Message) remediation=repair the checkpoint directory permissions; setup cannot report a resumable phase without durable state"
    }
    $script:SynapseSetupPartialState = 'bridge_pending'
    $script:SynapseSetupPartialReadback = $Readback
    throw "[synapse-setup] FATAL: $Message"
}

function Get-SynapseUnixTimeMilliseconds {
    return [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
}

function Resolve-SynapseMsvcCcbinPath {
    param(
        [Parameter(Mandatory=$true)][string]$PathValue,
        [Parameter(Mandatory=$true)][string]$Context
    )

    $expanded = [System.Environment]::ExpandEnvironmentVariables($PathValue.Trim().Trim('"'))
    if ([string]::IsNullOrWhiteSpace($expanded)) {
        throw "SYNAPSE_CUDA_NVCC_CCBIN_EMPTY context=$Context remediation=NVCC_CCBIN must point to cl.exe or a directory containing cl.exe"
    }
    $resolved = (Resolve-Path -LiteralPath $expanded -ErrorAction Stop).Path
    $item = Get-Item -LiteralPath $resolved -ErrorAction Stop
    if ($item.PSIsContainer) {
        $cl = Join-Path $item.FullName 'cl.exe'
        if (Test-Path -LiteralPath $cl -PathType Leaf) {
            return $item.FullName
        }
    } elseif ($item.Name -ieq 'cl.exe' -and $item.DirectoryName) {
        return $item.DirectoryName
    }
    throw "SYNAPSE_CUDA_NVCC_CCBIN_INVALID context=$Context path=$PathValue resolved=$resolved remediation=NVCC_CCBIN must point to cl.exe or a directory containing cl.exe"
}

function Get-SynapseMsvcHostCompilerDir {
    $roots = @()
    foreach ($programRoot in @(
        [System.Environment]::GetEnvironmentVariable('ProgramFiles'),
        [System.Environment]::GetEnvironmentVariable('ProgramFiles(x86)')
    )) {
        if (-not [string]::IsNullOrWhiteSpace($programRoot)) {
            $vsRoot = Join-Path $programRoot 'Microsoft Visual Studio'
            if (Test-Path -LiteralPath $vsRoot -PathType Container) {
                $roots += $vsRoot
            }
        }
    }

    $candidates = New-Object System.Collections.Generic.List[string]
    foreach ($root in $roots) {
        foreach ($majorDir in @(Get-ChildItem -LiteralPath $root -Directory -ErrorAction SilentlyContinue)) {
            foreach ($editionDir in @(Get-ChildItem -LiteralPath $majorDir.FullName -Directory -ErrorAction SilentlyContinue)) {
                $msvcRoot = Join-Path $editionDir.FullName 'VC\Tools\MSVC'
                if (-not (Test-Path -LiteralPath $msvcRoot -PathType Container)) {
                    continue
                }
                foreach ($versionDir in @(Get-ChildItem -LiteralPath $msvcRoot -Directory -ErrorAction SilentlyContinue)) {
                    $ccbin = Join-Path $versionDir.FullName 'bin\Hostx64\x64'
                    if (Test-Path -LiteralPath (Join-Path $ccbin 'cl.exe') -PathType Leaf) {
                        $candidates.Add($ccbin)
                    }
                }
            }
        }
    }

    if ($candidates.Count -lt 1) {
        return $null
    }
    return @($candidates | Sort-Object)[-1]
}

function Add-SynapseNvccAppendFlag {
    param(
        [AllowNull()][string]$ExistingFlags,
        [Parameter(Mandatory=$true)][string]$RequiredFlag
    )

    $existing = if ($null -eq $ExistingFlags) { '' } else { $ExistingFlags.Trim() }
    if ($existing -match '/Zc:preprocessor-') {
        Die "SYNAPSE_CUDA_NVCC_APPEND_FLAGS_CONFLICT value=$existing remediation=remove the conflicting /Zc:preprocessor- flag before setup can enable CUDA 13.x CCCL builds"
    }
    if ($existing -match '/Zc:preprocessor(?!-)') {
        return $existing
    }
    if ([string]::IsNullOrWhiteSpace($existing)) {
        return $RequiredFlag
    }
    return "$existing $RequiredFlag"
}

function Get-SynapseDirectoryFootprint {
    <#
      Measures a directory's exact on-disk footprint by enumerating its files.
      Never estimates and never swallows enumeration failures: any unreadable
      subtree is counted and named so a reported size is known to be complete
      or known to be partial.
    #>
    param([Parameter(Mandatory=$true)][string]$Path)

    $result = [ordered]@{
        path = $Path
        exists = $false
        file_count = 0
        byte_len = [uint64]0
        gib = 0.0
        read_error_count = 0
        read_errors_sample = @()
        complete = $false
        measured_at_utc = (Get-Date).ToUniversalTime().ToString('o')
    }
    if (-not (Test-Path -LiteralPath $Path)) { $result.complete = $true; return [pscustomobject]$result }
    $result.exists = $true
    $errors = @()
    $files = Get-ChildItem -LiteralPath $Path -Recurse -File -Force -ErrorAction SilentlyContinue -ErrorVariable +errors
    $sum = ($files | Measure-Object -Property Length -Sum)
    $result.file_count = [int]$sum.Count
    $result.byte_len = [uint64]([math]::Max(0, [int64]($sum.Sum)))
    $result.gib = [math]::Round($result.byte_len / 1GB, 3)
    $result.read_error_count = @($errors).Count
    $result.read_errors_sample = @($errors | Select-Object -First 5 | ForEach-Object { $_.ToString() })
    $result.complete = ($result.read_error_count -eq 0)
    return [pscustomobject]$result
}

function Get-SynapseAlternateBuildTargetInventory {
    <#
      Inventories the legacy %LOCALAPPDATA%\synapse\build-target tree left by the
      pre-#1857 default. Reports only: removal requires exact operator ownership
      and age proof and is never performed implicitly by setup.
    #>
    param([string]$Root = (Join-Path $env:LOCALAPPDATA 'synapse\build-target'))

    $inventory = [ordered]@{
        schema = 'synapse_setup_alternate_build_target_inventory/v1'
        root = $Root
        exists = (Test-Path -LiteralPath $Root)
        observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
        trees = @()
        total_byte_len = [uint64]0
        total_gib = 0.0
    }
    if (-not $inventory.exists) { return [pscustomobject]$inventory }
    $trees = @()
    $total = [uint64]0
    foreach ($dir in (Get-ChildItem -LiteralPath $Root -Directory -Force -ErrorAction SilentlyContinue)) {
        $footprint = Get-SynapseDirectoryFootprint -Path $dir.FullName
        $trees += [pscustomobject][ordered]@{
            name = $dir.Name
            path = $dir.FullName
            created_utc = $dir.CreationTimeUtc.ToString('o')
            last_write_utc = $dir.LastWriteTimeUtc.ToString('o')
            age_days = [math]::Round(((Get-Date).ToUniversalTime() - $dir.LastWriteTimeUtc).TotalDays, 2)
            footprint = $footprint
        }
        $total += $footprint.byte_len
    }
    $inventory.trees = $trees
    $inventory.total_byte_len = $total
    $inventory.total_gib = [math]::Round($total / 1GB, 3)
    return [pscustomobject]$inventory
}

function Resolve-SynapseCargoTargetDirectory {
    <#
      Decides the exact Cargo target directory for the release build and proves
      the decision. The default is the source checkout's own `target` tree: it
      is inherently per-checkout (so it cannot reproduce the cross-checkout
      fingerprint poisoning that motivated the old hashed %LOCALAPPDATA% cache)
      and it is visible where the operator already accounts for build output.
      Anything else is an authorized, footprint-reported deviation (#1857).
    #>
    param(
        [Parameter(Mandatory=$true)][string]$SourceDir,
        [AllowEmptyString()][string]$Requested,
        [bool]$AllowAlternate
    )

    $resolvedSource = (Resolve-Path -LiteralPath $SourceDir).Path.TrimEnd('\')
    $canonical = Join-Path $resolvedSource 'target'
    if ([string]::IsNullOrWhiteSpace($Requested)) {
        return [pscustomobject][ordered]@{
            path = $canonical
            kind = 'canonical_checkout_target'
            source_dir = $resolvedSource
            authorized_by = 'default'
            alternate_footprint = $null
        }
    }

    $requestedFull = [System.IO.Path]::GetFullPath($Requested.TrimEnd('\'))
    if ($requestedFull -eq $canonical) {
        return [pscustomobject][ordered]@{
            path = $canonical
            kind = 'canonical_checkout_target'
            source_dir = $resolvedSource
            authorized_by = 'explicit_canonical_request'
            alternate_footprint = $null
        }
    }

    if (-not $AllowAlternate) {
        Die ("SYNAPSE_BUILD_TARGET_ALTERNATE_NOT_AUTHORIZED requested={0} canonical={1} source_dir={2} remediation=setup builds into the source checkout's own target tree; an alternate build target is a separate multi-GB artifact tree and must be authorized explicitly with -AllowAlternateBuildTarget, or omit -CargoTarget to use {1}" -f `
            $requestedFull, $canonical, $resolvedSource)
    }

    $footprint = Get-SynapseDirectoryFootprint -Path $requestedFull
    Info ("Alternate build target AUTHORIZED: path={0} exists={1} existing_files={2} existing_bytes={3} existing_gib={4} measurement_complete={5} canonical_target_not_used={6}" -f `
        $requestedFull, $footprint.exists, $footprint.file_count, $footprint.byte_len, $footprint.gib, $footprint.complete, $canonical)
    if (-not $footprint.complete) {
        Info ("WARN: alternate build target footprint measurement was incomplete ({0} unreadable entries); reported size is a lower bound. Sample: {1}" -f `
            $footprint.read_error_count, ($footprint.read_errors_sample -join ' | '))
    }
    return [pscustomobject][ordered]@{
        path = $requestedFull
        kind = 'operator_authorized_alternate'
        source_dir = $resolvedSource
        authorized_by = 'AllowAlternateBuildTarget'
        alternate_footprint = $footprint
    }
}

function Get-SynapseCudaBuildCapability {
    <#
      Decides whether this build compiles the Calyx CUDA kernels, from physical
      host evidence rather than an assumption (#1859).

      calyx-forge's build script shells out to nvcc from a CUDA 13.3 toolkit
      whenever its `cuda` feature is on, so enabling it unconditionally makes
      the daemon unbuildable on any host without that toolkit. Both facts must
      hold before the feature is selected:
        1. an NVIDIA display device is physically present (PCI vendor 10DE), and
        2. an nvcc executable is resolvable.

      A partial match is reported explicitly, never silently downgraded past the
      operator. SYNAPSE_CALYX_CUDA=require forces the feature on and fails
      closed when the evidence is absent; SYNAPSE_CALYX_CUDA=off forces it off.
    #>
    $nvidiaDevices = @()
    $deviceProbeError = $null
    try {
        $nvidiaDevices = @(Get-PnpDevice -ErrorAction Stop | Where-Object { $_.InstanceId -match 'VEN_10DE' } |
            ForEach-Object { "{0}|{1}|{2}" -f $_.Status, $_.Class, $_.FriendlyName })
    } catch {
        $deviceProbeError = $_.Exception.Message
    }

    $nvcc = (Get-Command nvcc -ErrorAction SilentlyContinue).Source
    if (-not $nvcc -and -not [string]::IsNullOrWhiteSpace($env:CUDA_PATH)) {
        $candidate = Join-Path $env:CUDA_PATH 'bin\nvcc.exe'
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { $nvcc = $candidate }
    }

    $deviceProven = ($null -eq $deviceProbeError) -and ($nvidiaDevices.Count -gt 0)
    $override = if ([string]::IsNullOrWhiteSpace($env:SYNAPSE_CALYX_CUDA)) { '' } else { $env:SYNAPSE_CALYX_CUDA.Trim().ToLowerInvariant() }
    if ($override -notin @('', 'auto', 'require', 'off')) {
        Die "SYNAPSE_CALYX_CUDA_OVERRIDE_INVALID value=$env:SYNAPSE_CALYX_CUDA remediation=set SYNAPSE_CALYX_CUDA to auto, require, or off"
    }

    $enabled = $deviceProven -and $nvcc
    $basis = "nvidia_pnp_devices=$($nvidiaDevices.Count); device_probe_error=$(if ($deviceProbeError) { $deviceProbeError } else { 'none' }); nvcc=$(if ($nvcc) { $nvcc } else { 'not_found' }); cuda_path=$(if ($env:CUDA_PATH) { $env:CUDA_PATH } else { 'unset' }); override=$(if ($override) { $override } else { 'auto' })"

    switch ($override) {
        'require' {
            if (-not $enabled) {
                Die "SYNAPSE_CALYX_CUDA_REQUIRED_BUT_UNAVAILABLE basis=$basis remediation=install an NVIDIA driver and the CUDA 13.3 toolkit (set CUDA_PATH to its root) or unset SYNAPSE_CALYX_CUDA=require"
            }
            $enabled = $true
        }
        'off' { $enabled = $false }
    }

    if ($enabled) {
        Info "Calyx CUDA kernels ENABLED for this build (--features calyx-cuda). Basis: $basis"
    } else {
        Info "Calyx CUDA kernels DISABLED for this build; the daemon will run CPU math. Basis: $basis"
        if ($deviceProven -and -not $nvcc) {
            Info "WARN: this host HAS an NVIDIA device but no nvcc, so the daemon will refuse math_backend=auto/cuda at runtime (SYNAPSE_CALYX_MATH_CUDA_NOT_COMPILED). Install the CUDA 13.3 toolkit and rerun setup to compile GPU kernels."
        }
    }

    return [pscustomobject][ordered]@{
        schema = 'synapse_setup_cuda_build_capability/v1'
        enabled = [bool]$enabled
        cargo_features = $(if ($enabled) { @('calyx-cuda') } else { @() })
        nvidia_pnp_device_count = $nvidiaDevices.Count
        nvidia_pnp_devices = $nvidiaDevices
        device_probe_error = $deviceProbeError
        nvcc_path = $nvcc
        cuda_path_env = $env:CUDA_PATH
        override = $(if ($override) { $override } else { 'auto' })
        basis = $basis
        observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
    }
}

function Set-SynapseCudaBuildEnvironment {
    $nvcc = Get-Command nvcc -ErrorAction SilentlyContinue
    if (-not $nvcc -and -not [string]::IsNullOrWhiteSpace($env:CUDA_PATH)) {
        $cudaPathNvcc = Join-Path $env:CUDA_PATH 'bin\nvcc.exe'
        if (Test-Path -LiteralPath $cudaPathNvcc -PathType Leaf) {
            $nvcc = [pscustomobject]@{ Source = $cudaPathNvcc }
        }
    }
    if (-not $nvcc) {
        Info "CUDA nvcc not found; skipping NVCC_CCBIN/NVCC_APPEND_FLAGS setup for optional CUDA builds."
        return
    }

    $existingCcbin = if (-not [string]::IsNullOrWhiteSpace($env:NVCC_CCBIN)) {
        $env:NVCC_CCBIN
    } else {
        [System.Environment]::GetEnvironmentVariable('NVCC_CCBIN', 'User')
    }
    $ccbin = $null
    if (-not [string]::IsNullOrWhiteSpace($existingCcbin)) {
        try {
            $ccbin = Resolve-SynapseMsvcCcbinPath -PathValue $existingCcbin -Context 'existing_NVCC_CCBIN'
        } catch {
            Info "WARN: existing NVCC_CCBIN is invalid and will be repaired by Visual Studio discovery: $($_.Exception.Message)"
        }
    }
    if ([string]::IsNullOrWhiteSpace($ccbin)) {
        $ccbin = Get-SynapseMsvcHostCompilerDir
    }
    if ([string]::IsNullOrWhiteSpace($ccbin)) {
        Die "SYNAPSE_CUDA_MSVC_HOST_COMPILER_MISSING nvcc=$($nvcc.Source) remediation=install Visual Studio Build Tools with MSVC x64 tools, then rerun setup so NVCC_CCBIN can be set for CUDA builds"
    }

    $requiredNvccAppendFlag = '-Xcompiler=/Zc:preprocessor'
    $existingAppendFlags = if (-not [string]::IsNullOrWhiteSpace($env:NVCC_APPEND_FLAGS)) {
        $env:NVCC_APPEND_FLAGS
    } else {
        [System.Environment]::GetEnvironmentVariable('NVCC_APPEND_FLAGS', 'User')
    }
    $appendFlags = Add-SynapseNvccAppendFlag -ExistingFlags $existingAppendFlags -RequiredFlag $requiredNvccAppendFlag

    $env:NVCC_CCBIN = $ccbin
    $env:NVCC_APPEND_FLAGS = $appendFlags
    [System.Environment]::SetEnvironmentVariable('NVCC_CCBIN', $ccbin, 'User')
    [System.Environment]::SetEnvironmentVariable('NVCC_APPEND_FLAGS', $appendFlags, 'User')
    Info "CUDA build env set: NVCC_CCBIN=$ccbin; NVCC_APPEND_FLAGS includes $requiredNvccAppendFlag."
}

function Assert-SynapseChromeBridgeMaintenancePauseBudget {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Phase
    )

    if ($null -eq $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs) {
        return
    }

    $nowMs = Get-SynapseUnixTimeMilliseconds
    $remainingMs = [int64]$script:SynapseChromeBridgeMaintenancePauseUntilUnixMs - [int64]$nowMs
    if ($remainingMs -le [int64]$SynapseChromeBridgeMaintenancePauseGuardMs) {
        Die ("SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_EXPIRING reason={0} bind={1} phase={2} pause_until_unix_ms={3} remaining_ms={4} guard_ms={5} remediation=setup refuses to continue daemon bind drain after the Chrome bridge maintenance pause is close to expiry. The Chrome bridge can reconnect and recreate NetworkService peers after this point; rerun setup with a maintenance pause that covers the full drain budget or investigate why Windows still reports stale dead-owner TCP rows." -f `
            $Reason,
            $Bind,
            $Phase,
            $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs,
            $remainingMs,
            $SynapseChromeBridgeMaintenancePauseGuardMs)
    }
}

function Invoke-SynapseChromeBridgeVerifier {
    param(
        [Parameter(Mandatory = $true)]
        [string]$InstallerPath,
        [Parameter(Mandatory = $true)]
        [string]$NativeHostExePath
    )

    if (-not (Test-Path -LiteralPath $InstallerPath -PathType Leaf)) {
        Die "SYNAPSE_CHROME_BRIDGE_INSTALLER_MISSING path=$InstallerPath remediation=setup requires the repo script that verifies the direct localhost Chrome bridge and removes stale nativeMessaging registration"
    }
    $chromeBridgeArgs = @{
        SynapseNativeHostExe = $NativeHostExePath
    }
    $readback = & $InstallerPath @chromeBridgeArgs
    if (-not $readback.ok) {
        Die "SYNAPSE_CHROME_BRIDGE_INSTALLER_FAILED path=$InstallerPath remediation=installer did not return ok=true"
    }
    $extensionDeploy = $readback.extension_deploy
    if (-not $extensionDeploy) {
        Die "SYNAPSE_CHROME_BRIDGE_EXTENSION_DEPLOY_READBACK_MISSING path=$InstallerPath remediation=setup requires the Chrome bridge installer to report deployed service worker hash and register-token injection readback"
    }
    if ($extensionDeploy.bridge_register_token_injected -ne $true -or
        $extensionDeploy.bridge_register_token_matches_expected -ne $true -or
        [int]$extensionDeploy.bridge_register_token_length -ne 64 -or
        [string]$extensionDeploy.bridge_register_token_sha256 -notmatch '^[0-9a-f]{64}$') {
        Die ("SYNAPSE_CHROME_BRIDGE_REGISTER_TOKEN_DEPLOY_READBACK_INVALID path={0} injected={1} matches_expected={2} length={3} sha256={4} remediation=setup must deploy a Chrome service worker with the host-local derived bridge register token before daemon restart; rerun setup after verifying the stable extension directory is writable" -f `
            $InstallerPath,
            $extensionDeploy.bridge_register_token_injected,
            $extensionDeploy.bridge_register_token_matches_expected,
            $extensionDeploy.bridge_register_token_length,
            $extensionDeploy.bridge_register_token_sha256)
    }
    $autoInstall = $readback.synapse_chrome_auto_install
    if (-not $autoInstall) {
        Die "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_READBACK_MISSING path=$InstallerPath remediation=setup requires the bridge installer to report synapse_chrome_auto_install so skipped or failed active-profile installation cannot pass silently"
    }
    if ([string]$autoInstall.reason -eq 'skip_auto_install_requested') {
        Die "SYNAPSE_CHROME_BRIDGE_AUTOINSTALL_SKIPPED path=$InstallerPath remediation=setup must auto-install the bundled Chrome bridge into the already-open active profile; remove -SkipAutoInstall and rerun from the interactive Windows desktop"
    }
    $profileInstallState = $readback.synapse_chrome_profile_install_state
    if (-not $profileInstallState) {
        Die "SYNAPSE_CHROME_BRIDGE_PROFILE_INSTALL_STATE_MISSING path=$InstallerPath remediation=setup requires active Chrome profile installation readback after bridge verification"
    }
    if ($profileInstallState.active_profile_installed -ne $true) {
        Die ("SYNAPSE_CHROME_BRIDGE_ACTIVE_PROFILE_NOT_INSTALLED active_profile={0} installed_profiles={1} auto_install_attempted={2} auto_install_reason={3} remediation=setup must auto-install the deployed stable Synapse Chrome bridge directory into the already-open active Chrome profile before daemon handoff can continue" -f `
            $profileInstallState.active_profile,
            (@($profileInstallState.installed_profiles) -join ','),
            $autoInstall.attempted,
            $autoInstall.reason)
    }
    return $readback
}

function Format-SynapseChromeBridgeProfileInstallState {
    param($Readback)

    $state = $Readback.synapse_chrome_profile_install_state
    if (-not $state) {
        return 'profile_install_state=missing'
    }
    $autoInstall = $Readback.synapse_chrome_auto_install
    $autoInstallAttempted = if ($autoInstall) { [string]$autoInstall.attempted } else { 'missing' }
    $autoInstallReason = if ($autoInstall) { [string]$autoInstall.reason } else { 'missing' }
    $extensionDir = if ($Readback.extension_dir) { [string]$Readback.extension_dir } else { 'missing' }
    $cleanup = $Readback.stale_bridge_build_cleanup
    $cleanupRemoved = if ($cleanup) { @($cleanup.removed_dirs).Count } else { 'missing' }
    $cleanupPreserved = if ($cleanup) { @($cleanup.preserved_dirs).Count } else { 'missing' }
    $cleanupFailed = if ($cleanup) { @($cleanup.failed_dirs).Count } else { 'missing' }
    return ("profile_install_state=installed:{0},profile_count:{1},installed_profile_count:{2},active_profile:{3},active_profile_installed:{4},reason:{5},auto_install_attempted:{6},auto_install_reason:{7},extension_dir:{8},stale_build_dirs_removed:{9},stale_build_dirs_preserved:{10},stale_build_dirs_failed:{11}" -f `
        $state.installed, `
        $state.profile_count, `
        $state.installed_profile_count, `
        $state.active_profile, `
        $state.active_profile_installed, `
        $state.reason, `
        $autoInstallAttempted, `
        $autoInstallReason, `
        $extensionDir, `
        $cleanupRemoved, `
        $cleanupPreserved, `
        $cleanupFailed)
}

$processTokenAtStart = $env:SYNAPSE_BEARER_TOKEN
$processToolSurfaceHashAtStart = $env:SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START
$processToolSurfaceSnapshotAtStart = $env:SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START
$script:SynapseMcpProtocolVersion = '2025-06-18'
# The setup-time MCP readback runs immediately after daemon replacement, while
# the real Calyx store and Codex reconnect path may still be warming. Keep this
# finite and diagnostic-rich; do not use the short interactive request budget.
$script:SynapseSetupMcpRequestTimeoutSec = 120
# DELETE waits for the daemon's real lifecycle cleanup/readbacks. On the
# operator Calyx vault, cold post-start cleanup can exceed the old 20s budget.
$script:SynapseMcpSessionDeleteTimeoutSec = 120
$script:SynapseSetupMaintenanceLockStream = $null
$script:SynapseSetupMaintenanceLockPath = $null
$script:SynapseSetupMaintenanceLockReason = $null
$script:SynapseSetupMaintenanceLockToken = $null

function Get-ProcessLineage {
    param([int]$StartPid = $PID)
    $lineage = @()
    $seen = @{}
    $current = $StartPid
    $child = $null
    while ($current -and -not $seen.ContainsKey($current)) {
        $seen[$current] = $true
        $p = Get-CimInstance Win32_Process -Filter "ProcessId=$current" -ErrorAction SilentlyContinue
        if (-not $p) { break }
        # Guard against Windows PID reuse: ParentProcessId is a bare number, so
        # once a real parent exits its PID can be recycled by an unrelated, newer
        # process. A genuine parent always starts no later than its child; if the
        # candidate "parent" started after the child it is a recycled PID, not a
        # true ancestor, so stop the walk rather than climb into a phantom chain
        # (e.g. wininit.exe <- cmd.exe, which falsely tripped the cmd-ancestor guard).
        if ($child -and $p.CreationDate -and $child.CreationDate -and $p.CreationDate -gt $child.CreationDate) {
            break
        }
        $lineage += $p
        $child = $p
        $current = [int]$p.ParentProcessId
    }
    return $lineage
}

function Get-SynapseCurrentCodexAncestor {
    $lineage = Get-ProcessLineage
    return ($lineage | Where-Object {
        $_.Name -ieq 'codex.exe' -or $_.CommandLine -match '@openai[\\/]+codex|codex\.js|codex-win32'
    } | Select-Object -First 1)
}

function Test-SynapseCodexProcess {
    param([AllowNull()]$Process)

    if ($null -eq $Process) {
        return $false
    }
    $name = [string]$Process.Name
    $commandLine = [string]$Process.CommandLine
    return (
        $name -ieq 'codex.exe' -or
        $commandLine -match '@openai[\\/]+codex|codex\.js|codex-win32|openai\.chatgpt'
    )
}

function Read-SynapseSetupMaintenanceLockOwner {
    param([Parameter(Mandatory=$true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        return '<missing>'
    }
    try {
        $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8, $true, 4096, $true)
            try {
                $text = $reader.ReadToEnd().Trim()
                if ([string]::IsNullOrWhiteSpace($text)) { return '<empty>' }
                return ($text -replace '\s+', ' ')
            } finally {
                $reader.Dispose()
            }
        } finally {
            $stream.Dispose()
        }
    } catch {
        return "<unreadable error=$($_.Exception.Message)>"
    }
}

function Acquire-SynapseSetupMaintenanceLock {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    try {
        $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::Read)
    } catch [System.IO.IOException] {
        $owner = Read-SynapseSetupMaintenanceLockOwner -Path $Path
        Die "SYNAPSE_SETUP_MAINTENANCE_LOCK_HELD reason=$Reason path=$Path owner=$owner remediation=another setup/remove process owns the maintenance lock; wait for that setup process to exit or inspect the named PID. Do not close terminal windows or broad shell processes to clear this condition."
    } catch {
        Die "SYNAPSE_SETUP_MAINTENANCE_LOCK_OPEN_FAILED reason=$Reason path=$Path error=$($_.Exception.Message) remediation=repair permissions on the synapse local appdata directory"
    }

    $script:SynapseSetupMaintenanceLockStream = $stream
    $script:SynapseSetupMaintenanceLockPath = $Path
    $script:SynapseSetupMaintenanceLockReason = $Reason
    $script:SynapseSetupMaintenanceLockToken = [Guid]::NewGuid().ToString('D')
    $self = Get-CimInstance Win32_Process -Filter "ProcessId=$PID" -ErrorAction SilentlyContinue
    $lineageText = (Get-ProcessLineage | ForEach-Object { "{0}:{1}" -f $_.ProcessId, $_.Name }) -join ' <- '
    $owner = [ordered]@{
        schema = 'synapse_setup_maintenance_lock/v2'
        state = 'held'
        lock_token = $script:SynapseSetupMaintenanceLockToken
        reason = $Reason
        pid = $PID
        parent_pid = $self.ParentProcessId
        process_name = $self.Name
        command_line = $self.CommandLine
        source_dir = $SourceDir
        bind = $Bind
        started_at_utc = (Get-Date).ToUniversalTime().ToString('o')
        lineage = $lineageText
        cleanup_policy = 'never close terminal windows globally; only exact process IDs spawned by this setup operation or verified synapse-mcp targets may be stopped'
    }
    $json = $owner | ConvertTo-Json -Depth 6
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json + "`n")
    $stream.SetLength(0)
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush($true)
    Info "Maintenance lock acquired reason=$Reason path=$Path pid=$PID"
}

function Release-SynapseSetupMaintenanceLock {
    param(
        [Parameter(Mandatory=$true)][ValidateSet('released','failed','bridge_pending')][string]$State,
        [string]$ErrorMessage,
        [switch]$BestEffort
    )

    if ($null -eq $script:SynapseSetupMaintenanceLockStream) {
        return
    }

    $stream = $script:SynapseSetupMaintenanceLockStream
    $lockPath = $script:SynapseSetupMaintenanceLockPath
    $lockReason = $script:SynapseSetupMaintenanceLockReason
    $lockToken = $script:SynapseSetupMaintenanceLockToken
    $releaseFailure = $null
    try {
        $stream.Seek(0, [System.IO.SeekOrigin]::Begin) | Out-Null
        $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8, $true, 4096, $true)
        try {
            $heldText = $reader.ReadToEnd().Trim()
        } finally {
            $reader.Dispose()
        }
        try {
            $held = $heldText | ConvertFrom-Json -ErrorAction Stop
        } catch {
            throw "SYNAPSE_SETUP_MAINTENANCE_LOCK_RELEASE_OWNER_INVALID phase=owner_readback path=$lockPath pid=$PID token=$lockToken detail=$($_.Exception.Message) remediation=preserve the lock record and inspect which setup invocation last wrote it"
        }
        if ([string]$held.state -ne 'held' -or [int]$held.pid -ne $PID -or [string]$held.lock_token -ne $lockToken) {
            throw ("SYNAPSE_SETUP_MAINTENANCE_LOCK_RELEASE_OWNER_MISMATCH phase=owner_validation path={0} expected_pid={1} actual_pid={2} expected_token={3} actual_token={4} actual_state={5} remediation=do not overwrite another setup invocation; inspect the exact record and live process table" -f `
                $lockPath, $PID, $held.pid, $lockToken, $held.lock_token, $held.state)
        }
        $self = Get-CimInstance Win32_Process -Filter "ProcessId=$PID" -ErrorAction SilentlyContinue
        $owner = [ordered]@{
            schema = 'synapse_setup_maintenance_lock/v2'
            state = $State
            lock_token = $lockToken
            reason = $lockReason
            pid = $PID
            parent_pid = if ($self) { $self.ParentProcessId } else { $null }
            process_name = if ($self) { $self.Name } else { $null }
            command_line = if ($self) { $self.CommandLine } else { $null }
            source_dir = $SourceDir
            bind = $Bind
            released_at_utc = (Get-Date).ToUniversalTime().ToString('o')
            cleanup_policy = 'never close terminal windows globally; only exact process IDs spawned by this setup operation or verified synapse-mcp targets may be stopped'
        }
        if (-not [string]::IsNullOrWhiteSpace($ErrorMessage)) {
            $owner.error = ($ErrorMessage -replace '\s+', ' ').Trim()
        }
        $json = $owner | ConvertTo-Json -Depth 6
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($json + "`n")
        $stream.Seek(0, [System.IO.SeekOrigin]::Begin) | Out-Null
        $stream.SetLength(0)
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)

        $stream.Seek(0, [System.IO.SeekOrigin]::Begin) | Out-Null
        $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8, $true, 4096, $true)
        try {
            $releasedText = $reader.ReadToEnd().Trim()
        } finally {
            $reader.Dispose()
        }
        try {
            $released = $releasedText | ConvertFrom-Json -ErrorAction Stop
        } catch {
            throw "SYNAPSE_SETUP_MAINTENANCE_LOCK_RELEASE_READBACK_INVALID phase=release_readback path=$lockPath pid=$PID token=$lockToken detail=$($_.Exception.Message) remediation=preserve the record and repair storage before running another setup"
        }
        if ([string]$released.state -ne $State -or [int]$released.pid -ne $PID -or [string]$released.lock_token -ne $lockToken) {
            throw ("SYNAPSE_SETUP_MAINTENANCE_LOCK_RELEASE_READBACK_MISMATCH phase=release_readback path={0} expected_state={1} actual_state={2} expected_pid={3} actual_pid={4} expected_token={5} actual_token={6} remediation=preserve the record and repair storage before running another setup" -f `
                $lockPath, $State, $released.state, $PID, $released.pid, $lockToken, $released.lock_token)
        }
    } catch {
        $releaseFailure = "SYNAPSE_SETUP_MAINTENANCE_LOCK_RELEASE_FAILED phase=release state=$State path=$lockPath pid=$PID token=$lockToken detail=$($_.Exception.Message) remediation=preserve and inspect the exact lock record plus process table; do not start another setup until the OS handle is free and ownership is understood"
    } finally {
        $stream.Dispose()
        $script:SynapseSetupMaintenanceLockStream = $null
        $script:SynapseSetupMaintenanceLockPath = $null
        $script:SynapseSetupMaintenanceLockReason = $null
        $script:SynapseSetupMaintenanceLockToken = $null
    }
    if ($releaseFailure) {
        if ($BestEffort) {
            try { Info "WARN: $releaseFailure" } catch {}
            return
        }
        throw $releaseFailure
    }
    try { Info "Maintenance lock $State path=$lockPath pid=$PID token=$lockToken" } catch {}
}

function Wait-SynapsePostExitParent {
    param(
        [int]$ParentPid,
        [string]$Reason
    )

    if ($ParentPid -le 0) {
        return
    }

    $parent = Get-Process -Id $ParentPid -ErrorAction SilentlyContinue
    if ($null -eq $parent) {
        Info "SYNAPSE_POST_EXIT_PARENT_ALREADY_GONE parent_pid=$ParentPid reason=$Reason"
        return
    }

    Info "SYNAPSE_POST_EXIT_PARENT_WAIT parent_pid=$ParentPid reason=$Reason"
    try {
        Wait-Process -Id $ParentPid -Timeout 180 -ErrorAction Stop
    } catch {
        $stillAlive = [bool](Get-Process -Id $ParentPid -ErrorAction SilentlyContinue)
        if ($stillAlive) {
            Die "SYNAPSE_POST_EXIT_PARENT_STILL_RUNNING parent_pid=$ParentPid reason=$Reason remediation=the setup continuation waits for the parent runner to exit before reacquiring the maintenance lock; inspect the parent process and setup logs, never kill terminal/IDE/WSL hosts globally"
        }
    }
    Start-Sleep -Seconds 1
    Info "SYNAPSE_POST_EXIT_PARENT_GONE parent_pid=$ParentPid reason=$Reason"
}

# ---------------------------------------------------------------------------
# #2092, #2188 -- deploy-scoped restart-authority bookkeeping.
#
# PowerShell hoists the top-level trap across this entire scriptblock, but it
# does not make a script-file function callable before execution reaches that
# function's definition. Keep every command the trap can call above the trap,
# including the earliest parameter/contract failures. The deploy drain records
# the durable revocation in script scope; the trap, post-exit continuation, and
# section-7 success path all consume the same state.
#
# This is the BEST-EFFORT restore used from the trap: it never calls Die
# (throwing inside a trap loses the original failure). The section-7 restore
# still uses the fail-closed Clear-/Resume- functions.
# ---------------------------------------------------------------------------
$script:SynapseDeployStopRequestPath = $null
$script:SynapseDeployTaskSuspendedName = $null

function Set-SynapseDeployRestartAuthorityRevocation {
    param(
        [Parameter(Mandatory=$true)][string]$StopRequestPath,
        [Parameter(Mandatory=$true)][string]$TaskName
    )
    $script:SynapseDeployStopRequestPath = $StopRequestPath
    $script:SynapseDeployTaskSuspendedName = $TaskName
}

function Clear-SynapseDeployRestartAuthorityRevocation {
    $script:SynapseDeployStopRequestPath = $null
    $script:SynapseDeployTaskSuspendedName = $null
}

function Restore-SynapseDeployRestartAuthorityBestEffort {
    param([Parameter(Mandatory=$true)][string]$Reason)

    $stopRequestPath = $script:SynapseDeployStopRequestPath
    $taskName = $script:SynapseDeployTaskSuspendedName
    if ([string]::IsNullOrWhiteSpace($stopRequestPath) -and [string]::IsNullOrWhiteSpace($taskName)) {
        return
    }
    Clear-SynapseDeployRestartAuthorityRevocation

    if (-not [string]::IsNullOrWhiteSpace($stopRequestPath)) {
        try {
            if (Test-Path -LiteralPath $stopRequestPath -PathType Leaf) {
                Remove-Item -LiteralPath $stopRequestPath -Force -ErrorAction Stop
            }
            $stillPresent = Test-Path -LiteralPath $stopRequestPath -PathType Leaf
            Info "SYNAPSE_DEPLOY_RESTART_AUTHORITY_RESTORED reason=$Reason stop_request_path=$stopRequestPath stop_request_present_after=$stillPresent"
            if ($stillPresent) {
                Info "WARN: SYNAPSE_DEPLOY_STOP_REQUEST_CLEAR_INCOMPLETE reason=$Reason path=$stopRequestPath remediation=delete this file by hand or run scripts/synapse-setup.ps1 -Start; the hidden supervisor parks at every launch point while it exists"
            }
        } catch {
            Info "WARN: SYNAPSE_DEPLOY_STOP_REQUEST_CLEAR_FAILED reason=$Reason path=$stopRequestPath error=$($_.Exception.Message) remediation=delete this file by hand or run scripts/synapse-setup.ps1 -Start; the hidden supervisor parks at every launch point while it exists"
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($taskName)) {
        try {
            $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
            if ($null -eq $task) {
                Info "SYNAPSE_DEPLOY_TASK_AUTHORITY_RESTORE_SKIPPED reason=$Reason task=$taskName task_present=false"
            } elseif ([string]$task.State -eq 'Disabled') {
                Enable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
                $after = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
                Info "SYNAPSE_DEPLOY_TASK_AUTHORITY_RESTORED reason=$Reason task=$taskName state_after=$($(if ($after) { $after.State } else { '<absent>' }))"
            } else {
                Info "SYNAPSE_DEPLOY_TASK_AUTHORITY_RESTORE_NOT_NEEDED reason=$Reason task=$taskName state=$($task.State)"
            }
        } catch {
            Info "WARN: SYNAPSE_DEPLOY_TASK_AUTHORITY_RESTORE_FAILED reason=$Reason task=$taskName error=$($_.Exception.Message) remediation=run Enable-ScheduledTask -TaskName $taskName by hand, or rerun setup; autostart stays disabled until it is enabled"
        }
    }
}

trap {
    $errorText = $_ | Out-String
    # Raw PowerShell/.NET failures do not pass through Die(). Record the same
    # phase failure here, but never let this secondary diagnostic replace the
    # primary error which caused the trap (#2209).
    try {
        $trapPhaseLedgerPath = Write-SynapseSetupPhaseLedger `
            -Outcome 'failed' `
            -Message (($errorText -replace '\s+', ' ').Trim())
        if ($trapPhaseLedgerPath) {
            Write-Host "[synapse-setup] phase timing ledger -> $trapPhaseLedgerPath" -ForegroundColor Yellow
        } elseif (-not [string]::IsNullOrWhiteSpace($script:SynapseSetupPhaseLedgerWriteError)) {
            Write-Host "[synapse-setup] WARNING: $script:SynapseSetupPhaseLedgerWriteError" -ForegroundColor Yellow
        }
    } catch {
        try {
            Info "WARN: SYNAPSE_SETUP_PHASE_LEDGER_TRAP_REPORT_FAILED detail=$($_.Exception.Message) remediation=preserve the primary setup error and inspect LogDir manually"
        } catch {}
    }
    # #2092: the deploy drain's restart-authority revocation is DURABLE -- it
    # outlives this process by design, which is exactly what makes it safe
    # against the install-handoff race and exactly what would strand the host if
    # a failed deploy left it behind. Restoring it is the first thing the trap
    # does, before any manifest/lock bookkeeping that could itself throw.
    try {
        Restore-SynapseDeployRestartAuthorityBestEffort -Reason 'setup_failed'
    } catch {
        Info "WARN: SYNAPSE_DEPLOY_RESTART_AUTHORITY_RESTORE_TRAP_FAILED error=$($_.Exception.Message)"
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$script:SynapseCurrentDaemonStagingDirectory)) {
        try {
            Remove-SynapseCurrentDaemonStagingArtifact
        } catch {
            $cleanupError = "SYNAPSE_DAEMON_STAGING_TRAP_CLEANUP_FAILED error=$($_.Exception.Message) remediation=inspect the exact setup-staging path named by the nested error; do not delete any path whose ownership validation failed"
            Info "FATAL: $cleanupError"
            $errorText = "$errorText`n$cleanupError"
        }
    }
    if ($script:SynapseSetupPartialState -eq 'bridge_pending') {
        try {
            Write-SynapseSetupRepairManifestState `
                -State 'bridge_pending' `
                -Message (($errorText -replace '\s+', ' ').Trim()) `
                -ExitCode 1 `
                -Readback $script:SynapseSetupPartialReadback
        } catch {
            Info "WARN: could not write setup repair manifest bridge-pending state path=$script:SynapseSetupRepairManifestPath error=$($_.Exception.Message)"
        }
        Release-SynapseSetupMaintenanceLock -State bridge_pending -ErrorMessage $errorText -BestEffort
        break
    }
    try {
        $preserveHandoff = $false
        if (-not [string]::IsNullOrWhiteSpace($script:SynapseSetupRepairManifestPath) -and (Test-Path -LiteralPath $script:SynapseSetupRepairManifestPath -PathType Leaf)) {
            $currentRepairManifest = Get-Content -Raw -LiteralPath $script:SynapseSetupRepairManifestPath | ConvertFrom-Json
            $preserveHandoff = ([string]$currentRepairManifest.state -eq 'handoff_started')
        }
        if (-not $preserveHandoff) {
            Write-SynapseSetupRepairManifestState -State 'failed' -Message (($errorText -replace '\s+', ' ').Trim()) -ExitCode 1 -Readback $null
        }
    } catch {
        Info "WARN: could not write setup repair manifest failure state path=$script:SynapseSetupRepairManifestPath error=$($_.Exception.Message)"
    }
    Release-SynapseSetupMaintenanceLock -State failed -ErrorMessage $errorText -BestEffort
    break
}

function Quote-WindowsCommandArgument {
    param([Parameter(Mandatory=$true)][string]$Value)
    if ($Value.Length -eq 0) { return '""' }
    if ($Value -notmatch '[\s"]') { return $Value }
    $escaped = $Value -replace '(\\*)"', '$1$1\"'
    $escaped = $escaped -replace '(\\+)$', '$1$1'
    return '"' + $escaped + '"'
}

function Quote-VbsString {
    param([Parameter(Mandatory=$true)][string]$Value)
    return '"' + ($Value -replace '"', '""') + '"'
}

function Quote-PowerShellSingleQuotedString {
    param([AllowNull()][string]$Value)
    if ($null -eq $Value) { $Value = '' }
    return "'" + ($Value -replace "'", "''") + "'"
}

function Normalize-SynapseAllowedPermissionsArgument {
    param([AllowNull()][string]$Value)
    if ([string]::IsNullOrWhiteSpace($Value)) {
        return ''
    }

    $tokens = @($Value -split '[,;\s]+' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($tokens.Count -eq 0) {
        return ''
    }

    return ($tokens -join ',')
}

$normalizedSetupPermissions = Normalize-SynapseAllowedPermissionsArgument -Value $AllowedPermissions
$setupReadAudioGranted = @($normalizedSetupPermissions -split ',' | Where-Object { $_ -ieq 'READ_AUDIO' }).Count -gt 0
if ([bool]$EnableAudio -ne $setupReadAudioGranted) {
    Die "SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID enable_audio=$([bool]$EnableAudio) read_audio_granted=$setupReadAudioGranted allowed_permissions=$normalizedSetupPermissions remediation=enable audio with -EnableAudio and include READ_AUDIO in -AllowedPermissions, or disable both; Synapse refuses a durable launch configuration where capture and authorization disagree"
}

function Vbs-Literal {
    param([Parameter(Mandatory=$true)][string]$Value)
    return '"' + ($Value -replace '"', '""') + '"'
}

function Get-SynapseDaemonArgumentText {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [bool]$EnableAudio,
        [AllowNull()][string]$AllowedPermissions,
        [AllowNull()][string]$CalyxConfigPath
    )

    $daemonArguments = @(
        '--mode', 'http',
        '--bind', (Quote-WindowsCommandArgument $Bind),
        '--db', (Quote-WindowsCommandArgument $DbPath),
        '--profile-dir', (Quote-WindowsCommandArgument $ProfilesDir),
        '--log-level', 'info'
    )
    if (-not [string]::IsNullOrWhiteSpace($CalyxConfigPath)) {
        $daemonArguments += @('--calyx-config', (Quote-WindowsCommandArgument $CalyxConfigPath))
    }
    if ($EnableAudio) {
        $daemonArguments += '--enable-audio'
    }
    $allowedPermissionsArgument = Normalize-SynapseAllowedPermissionsArgument -Value $AllowedPermissions
    if (-not [string]::IsNullOrWhiteSpace($allowedPermissionsArgument)) {
        $daemonArguments += @('--allowed-permissions', (Quote-WindowsCommandArgument $allowedPermissionsArgument))
    }
    return ($daemonArguments -join ' ')
}

# ---------------------------------------------------------------------------
# #2083 -- operator stop-request (durable supervisor restart-authority
# revocation).
#
# The single source of truth for whether the hidden supervisor is permitted to
# (re)launch the daemon. Both the generated supervisor and the -Stop/-Start
# entry points derive its path from the runtime bin directory, so there is
# exactly one file and exactly one schema.
# ---------------------------------------------------------------------------
$SynapseDaemonSupervisorStopRequestSchema = 'synapse_daemon_supervisor_stop_request/v1'
$SynapseDaemonSupervisorStopRequestFileName = 'daemon-supervisor-stop-request.json'

function Get-SynapseDaemonSupervisorStopRequestPath {
    param([Parameter(Mandatory=$true)][string]$RuntimeBinDir)
    return (Join-Path $RuntimeBinDir $SynapseDaemonSupervisorStopRequestFileName)
}

function Read-SynapseDaemonSupervisorStopRequest {
    param([Parameter(Mandatory=$true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    $text = ''
    try {
        $text = (Get-Content -Raw -LiteralPath $Path).Trim()
    } catch {
        Die "SYNAPSE_DAEMON_STOP_REQUEST_UNREADABLE path=$Path error=$($_.Exception.Message) remediation=the supervisor restart-authority record must be readable before setup can reason about daemon lifecycle; repair or delete the file"
    }
    if ([string]::IsNullOrWhiteSpace($text)) {
        Die "SYNAPSE_DAEMON_STOP_REQUEST_EMPTY path=$Path remediation=an empty stop-request cannot prove whether restart authority is revoked; delete the file and rerun"
    }
    try {
        $request = $text | ConvertFrom-Json
    } catch {
        Die "SYNAPSE_DAEMON_STOP_REQUEST_MALFORMED path=$Path error=$($_.Exception.Message) body=$text remediation=repair or delete the supervisor restart-authority record"
    }
    if ([string]$request.schema -ne $SynapseDaemonSupervisorStopRequestSchema) {
        Die "SYNAPSE_DAEMON_STOP_REQUEST_SCHEMA_MISMATCH path=$Path actual_schema=$([string]$request.schema) expected_schema=$SynapseDaemonSupervisorStopRequestSchema remediation=repair or delete the supervisor restart-authority record"
    }
    return $request
}

function Write-SynapseDaemonSupervisorStopRequest {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$SupervisorPath
    )

    $parent = Split-Path -Parent $Path
    try {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    } catch {
        Die "SYNAPSE_DAEMON_STOP_REQUEST_DIR_CREATE_FAILED path=$parent error=$($_.Exception.Message) remediation=the supervisor restart-authority record must live beside the supervisor script so log retention cannot silently restore restart authority (#1862)"
    }

    $record = [ordered]@{
        schema = $SynapseDaemonSupervisorStopRequestSchema
        state = 'requested'
        reason = $Reason
        bind = $Bind
        db_path = $DbPath
        supervisor_path = $SupervisorPath
        requested_by_pid = $PID
        requested_at_utc = (Get-Date).ToUniversalTime().ToString('o')
    }
    $json = ($record | ConvertTo-Json -Depth 8)
    $temp = "$Path.tmp-$PID"
    try {
        Set-Content -LiteralPath $temp -Value $json -Encoding ascii -NoNewline
        Move-Item -LiteralPath $temp -Destination $Path -Force
    } catch {
        try { Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue } catch { }
        Die "SYNAPSE_DAEMON_STOP_REQUEST_WRITE_FAILED path=$Path reason=$Reason error=$($_.Exception.Message) remediation=setup refuses to stop the daemon without a durable restart-authority revocation record; the supervisor would relaunch it"
    }

    $readback = Read-SynapseDaemonSupervisorStopRequest -Path $Path
    if ($null -eq $readback -or
        [string]$readback.state -ne 'requested' -or
        [string]$readback.bind -ne $Bind -or
        [int]$readback.requested_by_pid -ne $PID) {
        Die ("SYNAPSE_DAEMON_STOP_REQUEST_READBACK_FAILED path={0} expected_bind={1} expected_pid={2} actual={3} remediation=the durable restart-authority revocation did not persist; do not stop the daemon until it does" -f `
            $Path,
            $Bind,
            $PID,
            ($(if ($null -eq $readback) { '<missing>' } else { $readback | ConvertTo-Json -Compress -Depth 8 })))
    }
    Info "Synapse daemon supervisor restart authority revoked (durable): path=$Path reason=$Reason bind=$Bind requested_by_pid=$PID requested_at_utc=$($readback.requested_at_utc)"
    return $readback
}

function Clear-SynapseDaemonSupervisorStopRequest {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    $before = Read-SynapseDaemonSupervisorStopRequest -Path $Path
    if ($null -eq $before) {
        Info "Synapse daemon supervisor restart authority already granted: path=$Path reason=$Reason stop_request_present=false"
        return $null
    }
    try {
        Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
    } catch {
        Die "SYNAPSE_DAEMON_STOP_REQUEST_CLEAR_FAILED path=$Path reason=$Reason error=$($_.Exception.Message) remediation=the supervisor will park at every launch point until this restart-authority revocation record is removed"
    }
    if (Test-Path -LiteralPath $Path) {
        Die "SYNAPSE_DAEMON_STOP_REQUEST_CLEAR_READBACK_FAILED path=$Path reason=$Reason remediation=the restart-authority revocation record still exists after removal; the supervisor cannot launch while it does"
    }
    Info "Synapse daemon supervisor restart authority restored: path=$Path reason=$Reason cleared_stop_request=$($before | ConvertTo-Json -Compress -Depth 8)"
    return $before
}

function New-HiddenDaemonLauncher {
    param(
        [Parameter(Mandatory=$true)][string]$OutputPath,
        [Parameter(Mandatory=$true)][string]$ExePath,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [Parameter(Mandatory=$true)][string]$MaintenanceLockPath,
        [bool]$EnableAudio,
        [AllowNull()][string]$AllowedPermissions,
        [AllowNull()][string]$CalyxConfigPath
    )

    $daemonLogDir = $LogDir
    $launcherLog = Join-Path $LogDir 'daemon-launcher.log'
    $supervisorPath = Join-Path (Split-Path -Parent $OutputPath) 'synapse-daemon-supervisor.ps1'
    $supervisorState = Join-Path $LogDir 'daemon-supervisor-current.json'
    $supervisorEvents = Join-Path $LogDir 'daemon-supervisor-events.jsonl'
    # #2083: the operator stop-request (restart-authority revocation) record.
    # It lives beside the supervisor in $RuntimeBinDir, NOT in $LogDir, for the
    # same reason the launcher and supervisor moved out of $LogDir in #1862: a
    # log retention sweep must never be able to change autostart behaviour. A
    # stop-request deleted by a log sweep would silently RESTORE restart
    # authority, which is the fail-open direction.
    $supervisorStopRequestPath = Get-SynapseDaemonSupervisorStopRequestPath -RuntimeBinDir (Split-Path -Parent $OutputPath)
    $allowedPermissionsArgument = Normalize-SynapseAllowedPermissionsArgument -Value $AllowedPermissions
    $daemonArgumentText = Get-SynapseDaemonArgumentText `
        -Bind $Bind `
        -DbPath $DbPath `
        -ProfilesDir $ProfilesDir `
        -EnableAudio $EnableAudio `
        -AllowedPermissions $AllowedPermissions `
        -CalyxConfigPath $CalyxConfigPath
    $powerShellExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $powerShellExe -PathType Leaf)) {
        Die "SYNAPSE_HIDDEN_SUPERVISOR_POWERSHELL_MISSING path=$powerShellExe remediation=repair Windows PowerShell before registering the daemon supervisor"
    }

    $supervisorScript = @'
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ExePath = __EXE_PATH__
$Bind = __BIND__
$DbPath = __DB_PATH__
$ProfilesDir = __PROFILES_DIR__
$DaemonLogDir = __DAEMON_LOG_DIR__
$TokenPath = __TOKEN_PATH__
$LauncherLog = __LAUNCHER_LOG__
$SupervisorState = __SUPERVISOR_STATE__
$SupervisorEvents = __SUPERVISOR_EVENTS__
$MaintenanceLockPath = __MAINTENANCE_LOCK_PATH__
$SupervisorStopRequestPath = __SUPERVISOR_STOP_REQUEST_PATH__
$ExpectedCalyxConfigPath = __EXPECTED_CALYX_CONFIG_PATH__
$DaemonArgumentText = __DAEMON_ARGUMENT_TEXT__
$ExpectedAllowedPermissions = __EXPECTED_ALLOWED_PERMISSIONS__
$ExpectedEnableAudio = __EXPECTED_ENABLE_AUDIO__

$restartFloorSeconds = 2
$restartCeilingSeconds = 60
$rapidFailureWindowSeconds = 60
$rapidFailureLimit = 5
$deadOwnerBindDrainSeconds = 30
$deadOwnerBindDrainPollMilliseconds = 500
$daemonStartupSeconds = 300
$daemonStartupPollMilliseconds = 250
$generation = 0
$rapidFailures = New-Object 'System.Collections.Generic.Queue[datetime]'

function Write-LogLine {
    param([Parameter(Mandatory=$true)][string]$Message)
    $line = '{0} {1}' -f (Get-Date -Format o), $Message
    Add-Content -LiteralPath $LauncherLog -Value $line -Encoding ascii
}

function Write-SupervisorEvent {
    param(
        [Parameter(Mandatory=$true)][string]$Event,
        [hashtable]$Fields = @{}
    )
    $row = [ordered]@{
        ts_utc = (Get-Date).ToUniversalTime().ToString('o')
        event = $Event
        supervisor_pid = $PID
        bind = $Bind
        db_path = $DbPath
    }
    foreach ($key in $Fields.Keys) {
        $row[$key] = $Fields[$key]
    }
    ($row | ConvertTo-Json -Compress -Depth 8) | Add-Content -LiteralPath $SupervisorEvents -Encoding ascii
}

function Write-SupervisorState {
    param(
        [Parameter(Mandatory=$true)][string]$State,
        [int]$Generation,
        [AllowNull()][object]$ChildPid,
        [AllowNull()][object]$ExitCode,
        [Parameter(Mandatory=$true)][string]$Message
    )
    $stateObject = [ordered]@{
        updated_utc = (Get-Date).ToUniversalTime().ToString('o')
        state = $State
        generation = $Generation
        supervisor_pid = $PID
        child_pid = $ChildPid
        exit_code = $ExitCode
        bind = $Bind
        db_path = $DbPath
        exe_path = $ExePath
        message = $Message
    }
    $stateObject | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $SupervisorState -Encoding ascii
}

function Read-SynapseToken {
    if (-not (Test-Path -LiteralPath $TokenPath -PathType Leaf)) {
        throw "SYNAPSE_DAEMON_TOKEN_MISSING path=$TokenPath"
    }
    $token = (Get-Content -Raw -LiteralPath $TokenPath).Trim()
    if ($token.Length -lt 16) {
        throw "SYNAPSE_DAEMON_TOKEN_INVALID path=$TokenPath length=$($token.Length)"
    }
    return $token
}

function Get-BindParts {
    $lastColon = $Bind.LastIndexOf(':')
    if ($lastColon -lt 1 -or $lastColon -ge ($Bind.Length - 1)) {
        throw "SYNAPSE_DAEMON_BIND_INVALID bind=$Bind"
    }
    return [pscustomobject]@{
        Address = $Bind.Substring(0, $lastColon)
        Port = [int]$Bind.Substring($lastColon + 1)
    }
}

function Get-ProcessInfoForPid {
    param([Parameter(Mandatory=$true)][int]$ProcessId)
    Get-CimInstance Win32_Process -Filter ("ProcessId = {0}" -f $ProcessId) -ErrorAction SilentlyContinue
}

function Test-ExactBindAvailable {
    param(
        [Parameter(Mandatory=$true)][string]$Address,
        [Parameter(Mandatory=$true)][int]$Port
    )

    $probe = $null
    $available = $false
    $errorText = $null
    try {
        $ipAddress = [System.Net.IPAddress]::Parse($Address)
        $probe = [System.Net.Sockets.TcpListener]::new($ipAddress, $Port)
        $probe.ExclusiveAddressUse = $true
        $probe.Start()
        $available = $true
    } catch {
        $errorText = ($_.Exception.Message -replace '\s+', ' ').Trim()
    } finally {
        if ($null -ne $probe) {
            try { $probe.Stop() } catch { }
        }
    }

    return [pscustomobject]@{
        Available = $available
        Error = $errorText
    }
}

function Get-CommandLineArgumentValue {
    param(
        [string]$CommandLine,
        [Parameter(Mandatory=$true)][string]$Name
    )
    if ([string]::IsNullOrWhiteSpace($CommandLine)) { return $null }
    $escapedName = [regex]::Escape($Name)
    $pattern = "(?i)(?:^|\s)$escapedName(?:\s+|=)(?:""(?<quoted>[^""]*)""|(?<bare>\S+))"
    $match = [regex]::Match($CommandLine, $pattern)
    if (-not $match.Success) { return $null }
    if ($match.Groups['quoted'].Success) { return $match.Groups['quoted'].Value }
    return $match.Groups['bare'].Value
}

function Normalize-AllowedPermissionsArgument {
    param([AllowNull()][string]$Value)
    if ([string]::IsNullOrWhiteSpace($Value)) {
        return ''
    }

    $tokens = @($Value -split '[,;\s]+' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($tokens.Count -eq 0) {
        return ''
    }

    return ($tokens -join ',')
}

function Normalize-PathArgument {
    param([AllowNull()][string]$Value)
    if ([string]::IsNullOrWhiteSpace($Value)) { return '' }
    try { return ([System.IO.Path]::GetFullPath($Value)).TrimEnd([char[]]@([char]92, [char]47)) }
    catch { return $Value.Trim().TrimEnd([char[]]@([char]92, [char]47)) }
}

function Test-ExpectedDaemonProcess {
    param(
        [AllowNull()][object]$ProcessInfo,
        [AllowNull()][object]$ExpectedParentPid = $null
    )
    if ($null -eq $ProcessInfo) {
        return $false
    }
    if (-not [string]::Equals([string]$ProcessInfo.ExecutablePath, $ExePath, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $false
    }
    $commandLine = [string]$ProcessInfo.CommandLine
    $actualMode = Get-CommandLineArgumentValue -CommandLine $commandLine -Name '--mode'
    $actualBind = Get-CommandLineArgumentValue -CommandLine $commandLine -Name '--bind'
    $actualDb = Normalize-PathArgument -Value (Get-CommandLineArgumentValue -CommandLine $commandLine -Name '--db')
    $actualProfiles = Normalize-PathArgument -Value (Get-CommandLineArgumentValue -CommandLine $commandLine -Name '--profile-dir')
    $actualCalyxConfig = Normalize-PathArgument -Value (Get-CommandLineArgumentValue -CommandLine $commandLine -Name '--calyx-config')
    $actualParentPidRaw = Get-CommandLineArgumentValue -CommandLine $commandLine -Name '--parent-pid'
    $actualParentPid = 0
    if (-not [int]::TryParse([string]$actualParentPidRaw, [ref]$actualParentPid) -or $actualParentPid -le 0) {
        return $false
    }
    $actualEnableAudio = $commandLine -match '(?i)(?:^|\s)--enable-audio(?:\s|$)'
    $baseMatches = ($actualMode -ieq 'http') -and
        ($actualBind -ieq $Bind) -and
        ($actualDb -ieq (Normalize-PathArgument -Value $DbPath)) -and
        ($actualProfiles -ieq (Normalize-PathArgument -Value $ProfilesDir)) -and
        ($actualCalyxConfig -ieq (Normalize-PathArgument -Value $ExpectedCalyxConfigPath)) -and
        ($actualEnableAudio -eq [bool]::Parse($ExpectedEnableAudio))
    if (-not $baseMatches) {
        return $false
    }

    $actualAllowedRaw = Get-CommandLineArgumentValue -CommandLine $commandLine -Name '--allowed-permissions'
    $actualAllowed = Normalize-AllowedPermissionsArgument -Value $actualAllowedRaw
    $expectedAllowed = Normalize-AllowedPermissionsArgument -Value $ExpectedAllowedPermissions
    if ($actualAllowed -cne $expectedAllowed) {
        return $false
    }
    if ($null -ne $ExpectedParentPid -and $actualParentPid -ne [int]$ExpectedParentPid) {
        return $false
    }
    return $true
}

function Get-ExactChildIdentityState {
    param(
        [Parameter(Mandatory=$true)][int]$ChildPid,
        [Parameter(Mandatory=$true)][string]$CreationDate
    )
    $current = Get-ProcessInfoForPid -ProcessId $ChildPid
    if ($null -eq $current) {
        return [pscustomobject]@{ Live = $false; Terminal = $true; Reason = 'pid_absent'; ProcessInfo = $null }
    }
    if ([string]$current.CreationDate -cne $CreationDate) {
        return [pscustomobject]@{ Live = $false; Terminal = $true; Reason = 'pid_reused'; ProcessInfo = $current }
    }
    if (-not (Test-ExpectedDaemonProcess -ProcessInfo $current -ExpectedParentPid $PID)) {
        return [pscustomobject]@{ Live = $false; Terminal = $false; Reason = 'identity_mismatch'; ProcessInfo = $current }
    }
    return [pscustomobject]@{ Live = $true; Terminal = $false; Reason = 'exact_child_live'; ProcessInfo = $current }
}

function Test-ExactChildReady {
    param(
        [Parameter(Mandatory=$true)][int]$ChildPid,
        [Parameter(Mandatory=$true)][string]$CreationDate,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)]$BindParts
    )
    $identity = Get-ExactChildIdentityState -ChildPid $ChildPid -CreationDate $CreationDate
    if ($identity.Live -ne $true) {
        return [pscustomobject]@{ Ready = $false; Terminal = $identity.Terminal; Reason = $identity.Reason }
    }
    $listeners = @(Get-NetTCPConnection -LocalAddress $BindParts.Address -LocalPort $BindParts.Port -State Listen -ErrorAction SilentlyContinue)
    $owners = @($listeners | ForEach-Object { [int]$_.OwningProcess } | Sort-Object -Unique)
    if ($owners.Count -ne 1 -or $owners[0] -ne $ChildPid) {
        return [pscustomobject]@{ Ready = $false; Terminal = $false; Reason = "listener_owners_$($owners -join ',')" }
    }
    try {
        $headers = @{ Authorization = "Bearer $Token" }
        $response = Invoke-WebRequest -UseBasicParsing -Uri ("http://{0}/health" -f $Bind) -Headers $headers -TimeoutSec 2
        if ([int]$response.StatusCode -ne 200) {
            return [pscustomobject]@{ Ready = $false; Terminal = $false; Reason = "health_status_$([int]$response.StatusCode)" }
        }
    } catch {
        return [pscustomobject]@{ Ready = $false; Terminal = $false; Reason = "health_failed:$($_.Exception.Message)" }
    }
    $identityAfterHealth = Get-ExactChildIdentityState -ChildPid $ChildPid -CreationDate $CreationDate
    if ($identityAfterHealth.Live -ne $true) {
        return [pscustomobject]@{ Ready = $false; Terminal = $identityAfterHealth.Terminal; Reason = "post_health_$($identityAfterHealth.Reason)" }
    }
    return [pscustomobject]@{ Ready = $true; Terminal = $false; Reason = 'exact_identity_listener_health' }
}

function Test-SetupMaintenanceLockActive {
    if ([string]::IsNullOrWhiteSpace($MaintenanceLockPath) -or -not (Test-Path -LiteralPath $MaintenanceLockPath -PathType Leaf)) {
        return [pscustomobject]@{ Active = $false; Reason = 'missing' }
    }
    try {
        $text = (Get-Content -Raw -LiteralPath $MaintenanceLockPath).Trim()
        if ([string]::IsNullOrWhiteSpace($text)) {
            return [pscustomobject]@{ Active = $false; Reason = 'empty' }
        }
        $lock = $text | ConvertFrom-Json
        if ([string]$lock.schema -ne 'synapse_setup_maintenance_lock/v1') {
            return [pscustomobject]@{ Active = $false; Reason = 'schema_mismatch' }
        }
        if ([string]$lock.bind -ne $Bind) {
            return [pscustomobject]@{ Active = $false; Reason = 'bind_mismatch' }
        }
        if ([string]$lock.state -ne 'held') {
            return [pscustomobject]@{ Active = $false; Reason = "state_$($lock.state)" }
        }
        $lockPid = [int]$lock.pid
        $owner = Get-ProcessInfoForPid -ProcessId $lockPid
        if ($null -eq $owner) {
            return [pscustomobject]@{ Active = $false; Reason = "owner_missing_$lockPid" }
        }
        return [pscustomobject]@{ Active = $true; Reason = [string]$lock.reason; Pid = $lockPid }
    } catch {
        return [pscustomobject]@{ Active = $false; Reason = "read_failed:$($_.Exception.Message)" }
    }
}

function Stop-IfSetupMaintenanceActive {
    param(
        [Parameter(Mandatory=$true)][string]$Phase,
        [int]$Generation,
        [AllowNull()][object]$ChildPid,
        [AllowNull()][object]$ExitCode
    )
    $maintenance = Test-SetupMaintenanceLockActive
    if ($maintenance.Active -ne $true) {
        return
    }
    Write-LogLine "SYNAPSE_DAEMON_SUPERVISOR_STOP generation=$Generation reason=setup_maintenance phase=$Phase lock_reason=$($maintenance.Reason) lock_pid=$($maintenance.Pid)"
    Write-SupervisorEvent 'supervisor_stop' @{ generation = $Generation; reason = 'setup_maintenance'; phase = $Phase; lock_reason = $maintenance.Reason; lock_pid = $maintenance.Pid }
    Write-SupervisorState -State 'stopped' -Generation $Generation -ChildPid $ChildPid -ExitCode $ExitCode -Message "Setup maintenance lock is held by pid $($maintenance.Pid); supervisor stopped instead of launching/restarting during $Phase."
    exit 0
}

# #2083: the durable operator stop-request. The setup maintenance lock above is
# a TRANSIENT revocation -- it is only valid while the owning setup process is
# alive, and it is only consulted after a child exit. Neither property is what
# an operator stop needs: `synapse-setup.ps1 -Stop` must leave the daemon down
# after the stopping process itself has exited, and it must park a supervisor
# that is sitting in its restart backoff or that has not launched anything yet.
# This record is that authority, and it is checked at every point where the
# supervisor is about to (re)launch.
function Get-JsonFieldText {
    # Set-StrictMode -Version Latest turns a reference to a missing property into
    # a PropertyNotFoundException, which would replace the coded stop-request
    # diagnostics below with a generic PowerShell message. Read fields through
    # the property bag so a missing field is data, not an exception.
    param(
        [Parameter(Mandatory=$true)][AllowNull()][object]$Object,
        [Parameter(Mandatory=$true)][string]$Name
    )
    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    if ($null -eq $property.Value) { return $null }
    return [string]$property.Value
}

function Test-OperatorStopRequestActive {
    if ([string]::IsNullOrWhiteSpace($SupervisorStopRequestPath) -or -not (Test-Path -LiteralPath $SupervisorStopRequestPath -PathType Leaf)) {
        return [pscustomobject]@{ Active = $false; Reason = 'missing' }
    }
    # Deliberately NOT wrapped in a try/catch that degrades to "not active": an
    # unreadable or malformed stop-request is an ambiguous authority state, and
    # the fail-open direction (launch anyway) is exactly the #2083 bug. The trap
    # handler turns this into state=fatal with the exact path and parse error.
    $text = ''
    try {
        $text = (Get-Content -Raw -LiteralPath $SupervisorStopRequestPath).Trim()
    } catch {
        throw "SYNAPSE_DAEMON_SUPERVISOR_STOP_REQUEST_UNREADABLE path=$SupervisorStopRequestPath error=$($_.Exception.Message) remediation=repair or delete this file, or run scripts/synapse-setup.ps1 -Start to clear it; the supervisor refuses to launch while its restart authority is unreadable"
    }
    if ([string]::IsNullOrWhiteSpace($text)) {
        throw "SYNAPSE_DAEMON_SUPERVISOR_STOP_REQUEST_EMPTY path=$SupervisorStopRequestPath remediation=repair or delete this file, or run scripts/synapse-setup.ps1 -Start to clear it; an empty stop-request cannot prove whether restart authority was revoked"
    }
    $request = $null
    try {
        $request = $text | ConvertFrom-Json
    } catch {
        throw "SYNAPSE_DAEMON_SUPERVISOR_STOP_REQUEST_MALFORMED path=$SupervisorStopRequestPath error=$($_.Exception.Message) remediation=repair or delete this file, or run scripts/synapse-setup.ps1 -Start to clear it"
    }
    $requestSchema = Get-JsonFieldText -Object $request -Name 'schema'
    if ($requestSchema -ne 'synapse_daemon_supervisor_stop_request/v1') {
        throw "SYNAPSE_DAEMON_SUPERVISOR_STOP_REQUEST_SCHEMA_MISMATCH path=$SupervisorStopRequestPath actual_schema=$requestSchema expected_schema=synapse_daemon_supervisor_stop_request/v1 remediation=repair or delete this file, or run scripts/synapse-setup.ps1 -Start to clear it"
    }
    $requestBind = Get-JsonFieldText -Object $request -Name 'bind'
    if ($requestBind -ne $Bind) {
        # Scoped exactly like the maintenance lock: a stop-request written for a
        # different bind has no authority over this supervisor, and saying so is
        # a readback, not a degradation.
        return [pscustomobject]@{ Active = $false; Reason = "bind_mismatch_$requestBind" }
    }
    $requestState = Get-JsonFieldText -Object $request -Name 'state'
    if ($requestState -ne 'requested') {
        throw "SYNAPSE_DAEMON_SUPERVISOR_STOP_REQUEST_STATE_INVALID path=$SupervisorStopRequestPath actual_state=$requestState expected_state=requested remediation=repair or delete this file, or run scripts/synapse-setup.ps1 -Start to clear it"
    }
    return [pscustomobject]@{
        Active = $true
        Reason = (Get-JsonFieldText -Object $request -Name 'reason')
        RequestedByPid = (Get-JsonFieldText -Object $request -Name 'requested_by_pid')
        RequestedAtUtc = (Get-JsonFieldText -Object $request -Name 'requested_at_utc')
    }
}

function Stop-IfOperatorStopRequested {
    param(
        [Parameter(Mandatory=$true)][string]$Phase,
        [int]$Generation,
        [AllowNull()][object]$ChildPid,
        [AllowNull()][object]$ExitCode
    )
    $request = Test-OperatorStopRequestActive
    if ($request.Active -ne $true) {
        return
    }
    Write-LogLine "SYNAPSE_DAEMON_SUPERVISOR_STOP generation=$Generation reason=operator_stop_request phase=$Phase request_path=$SupervisorStopRequestPath request_reason=$($request.Reason) requested_by_pid=$($request.RequestedByPid) requested_at_utc=$($request.RequestedAtUtc)"
    Write-SupervisorEvent 'supervisor_stop' @{
        generation = $Generation
        reason = 'operator_stop_request'
        phase = $Phase
        request_path = $SupervisorStopRequestPath
        request_reason = $request.Reason
        requested_by_pid = $request.RequestedByPid
        requested_at_utc = $request.RequestedAtUtc
    }
    Write-SupervisorState -State 'stopped' -Generation $Generation -ChildPid $ChildPid -ExitCode $ExitCode -Message "Operator stop-request $SupervisorStopRequestPath (reason=$($request.Reason), requested_by_pid=$($request.RequestedByPid)) revoked restart authority; supervisor parked at $Phase instead of launching/restarting."
    exit 0
}

function Wait-AdoptedDaemon {
    param(
        [Parameter(Mandatory=$true)][int]$OwnerPid,
        [int]$Generation
    )
    Write-LogLine "SYNAPSE_DAEMON_SUPERVISOR_ADOPT_EXISTING generation=$Generation pid=$OwnerPid bind=$Bind"
    Write-SupervisorEvent 'adopt_existing' @{ generation = $Generation; child_pid = $OwnerPid }
    Write-SupervisorState -State 'adopted_existing' -Generation $Generation -ChildPid $OwnerPid -ExitCode $null -Message 'Existing expected daemon owns the listener; supervisor is waiting for it to exit before relaunching.'
    try {
        Wait-Process -Id $OwnerPid
    } catch {
        Write-LogLine "SYNAPSE_DAEMON_ADOPTED_WAIT_ERROR generation=$Generation pid=$OwnerPid error=$($_.Exception.Message)"
        Write-SupervisorEvent 'adopted_wait_error' @{ generation = $Generation; child_pid = $OwnerPid; error = $_.Exception.Message }
    }
    Write-LogLine "SYNAPSE_DAEMON_ADOPTED_EXIT generation=$Generation pid=$OwnerPid"
    Write-SupervisorEvent 'adopted_exit' @{ generation = $Generation; child_pid = $OwnerPid }
    Write-LogLine "SYNAPSE_DAEMON_SUPERVISOR_STOP generation=$Generation reason=adopted_daemon_exit"
    Write-SupervisorEvent 'supervisor_stop' @{ generation = $Generation; reason = 'adopted_daemon_exit' }
    Write-SupervisorState -State 'stopped' -Generation $Generation -ChildPid $OwnerPid -ExitCode 0 -Message 'Adopted daemon exited; supervisor stopped instead of launching during a maintenance handoff.'
    exit 0
}

function Register-RapidFailure {
    param([Parameter(Mandatory=$true)][datetime]$FailureTime)
    $rapidFailures.Enqueue($FailureTime)
    while ($rapidFailures.Count -gt 0 -and (($FailureTime - $rapidFailures.Peek()).TotalSeconds -gt $rapidFailureWindowSeconds)) {
        [void]$rapidFailures.Dequeue()
    }
    if ($rapidFailures.Count -ge $rapidFailureLimit) {
        throw "SYNAPSE_DAEMON_CRASH_LOOP rapid_failures=$($rapidFailures.Count) window_seconds=$rapidFailureWindowSeconds bind=$Bind"
    }
    $exponent = [Math]::Min($rapidFailures.Count - 1, 5)
    return [int][Math]::Min($restartCeilingSeconds, $restartFloorSeconds * [Math]::Pow(2, $exponent))
}

trap {
    $message = ($_.Exception.Message -replace '\s+', ' ').Trim()
    try {
        Write-LogLine "SYNAPSE_DAEMON_SUPERVISOR_FATAL error=$message"
        Write-SupervisorEvent 'fatal' @{ generation = $generation; error = $message }
        Write-SupervisorState -State 'fatal' -Generation $generation -ChildPid $null -ExitCode 1 -Message $message
    } catch {
    }
    exit 1
}

New-Item -ItemType Directory -Force -Path $DaemonLogDir | Out-Null
$bindParts = Get-BindParts
Write-LogLine "SYNAPSE_DAEMON_SUPERVISOR_START supervisor_pid=$PID bind=$Bind db=$DbPath exe=$ExePath"
Write-SupervisorEvent 'supervisor_start' @{ generation = $generation; exe_path = $ExePath }
Write-SupervisorState -State 'starting' -Generation $generation -ChildPid $null -ExitCode $null -Message 'Supervisor process started.'
# #2083: a supervisor started while restart authority is revoked (stray logon
# trigger, a task re-enabled by hand, a second launcher) must park before it
# touches the bind, not after it has already raced a second daemon into it.
Stop-IfOperatorStopRequested -Phase 'supervisor_start' -Generation $generation -ChildPid $null -ExitCode $null

while ($true) {
    Stop-IfOperatorStopRequested -Phase 'pre_launch' -Generation $generation -ChildPid $null -ExitCode $null
    $listeners = @(Get-NetTCPConnection -LocalAddress $bindParts.Address -LocalPort $bindParts.Port -State Listen -ErrorAction SilentlyContinue |
        Sort-Object OwningProcess, CreationTime)
    if ($listeners.Count -gt 0) {
        $expectedOwnerPid = $null
        $unexpectedOwners = @()
        $deadOwnerRows = @()
        foreach ($listener in $listeners) {
            $ownerPid = [int]$listener.OwningProcess
            $ownerInfo = Get-ProcessInfoForPid -ProcessId $ownerPid
            if ($null -eq $ownerInfo) {
                $deadOwnerRows += $listener
                continue
            }
            if (Test-ExpectedDaemonProcess -ProcessInfo $ownerInfo) {
                if ($null -eq $expectedOwnerPid) {
                    $expectedOwnerPid = $ownerPid
                }
                continue
            }
            $unexpectedOwners += $ownerInfo
        }

        if ($unexpectedOwners.Count -gt 0) {
            $unexpectedSummary = @($unexpectedOwners | ForEach-Object {
                'pid={0} exe={1} command={2}' -f [int]$_.ProcessId, [string]$_.ExecutablePath, (([string]$_.CommandLine -replace '\s+', ' ').Trim())
            }) -join ' | '
            throw "SYNAPSE_DAEMON_BIND_OCCUPIED bind=$Bind live_unexpected_owner_count=$($unexpectedOwners.Count) owners=$unexpectedSummary"
        }

        if ($null -ne $expectedOwnerPid) {
            Wait-AdoptedDaemon -OwnerPid $expectedOwnerPid -Generation $generation
            continue
        }

        $staleOwnerPids = @($deadOwnerRows | ForEach-Object { [int]$_.OwningProcess } | Sort-Object -Unique)
        $staleCreationTimes = @($deadOwnerRows | ForEach-Object { [string]$_.CreationTime })
        $drainStarted = Get-Date
        $drainAttempts = 0
        $lastBindProbeError = $null
        Write-LogLine "SYNAPSE_DAEMON_DEAD_OWNER_BIND_DRAIN_START generation=$generation bind=$Bind stale_owner_pids=$($staleOwnerPids -join ',') listener_count=$($deadOwnerRows.Count) timeout_seconds=$deadOwnerBindDrainSeconds"
        Write-SupervisorEvent 'dead_owner_bind_drain_start' @{
            generation = $generation
            stale_owner_pids = $staleOwnerPids
            listener_count = $deadOwnerRows.Count
            listener_creation_times = $staleCreationTimes
            timeout_seconds = $deadOwnerBindDrainSeconds
        }
        while ($true) {
            $drainAttempts += 1
            $bindProbe = Test-ExactBindAvailable -Address $bindParts.Address -Port $bindParts.Port
            if ($bindProbe.Available -eq $true) {
                break
            }
            $lastBindProbeError = $bindProbe.Error

            $drainListeners = @(Get-NetTCPConnection -LocalAddress $bindParts.Address -LocalPort $bindParts.Port -State Listen -ErrorAction SilentlyContinue |
                Sort-Object OwningProcess, CreationTime)
            $liveDrainOwners = @()
            foreach ($drainListener in $drainListeners) {
                $drainOwner = Get-ProcessInfoForPid -ProcessId ([int]$drainListener.OwningProcess)
                if ($null -ne $drainOwner) {
                    $liveDrainOwners += $drainOwner
                }
            }
            if ($liveDrainOwners.Count -gt 0) {
                $liveDrainSummary = @($liveDrainOwners | ForEach-Object {
                    'pid={0} exe={1} command={2}' -f [int]$_.ProcessId, [string]$_.ExecutablePath, (([string]$_.CommandLine -replace '\s+', ' ').Trim())
                }) -join ' | '
                throw "SYNAPSE_DAEMON_BIND_OCCUPIED_DURING_DEAD_OWNER_DRAIN bind=$Bind owners=$liveDrainSummary bind_probe_error=$lastBindProbeError"
            }

            $drainElapsedMs = [int64]((Get-Date) - $drainStarted).TotalMilliseconds
            if ($drainElapsedMs -ge ($deadOwnerBindDrainSeconds * 1000)) {
                throw "SYNAPSE_DAEMON_DEAD_OWNER_BIND_DRAIN_TIMEOUT bind=$Bind stale_owner_pids=$($staleOwnerPids -join ',') attempts=$drainAttempts elapsed_ms=$drainElapsedMs bind_probe_error=$lastBindProbeError"
            }
            if ($drainAttempts -eq 1 -or ($drainAttempts % 10) -eq 0) {
                Write-LogLine "SYNAPSE_DAEMON_DEAD_OWNER_BIND_DRAIN_PROGRESS generation=$generation bind=$Bind stale_owner_pids=$($staleOwnerPids -join ',') attempts=$drainAttempts elapsed_ms=$drainElapsedMs bind_probe_error=$lastBindProbeError"
                Write-SupervisorEvent 'dead_owner_bind_drain_progress' @{
                    generation = $generation
                    stale_owner_pids = $staleOwnerPids
                    attempts = $drainAttempts
                    elapsed_ms = $drainElapsedMs
                    bind_probe_error = $lastBindProbeError
                }
                Write-SupervisorState -State 'dead_owner_bind_drain' -Generation $generation -ChildPid $null -ExitCode $null -Message "Waiting for the kernel to release dead-owner TCP row(s) for pid(s) $($staleOwnerPids -join ','); attempt=$drainAttempts elapsed_ms=$drainElapsedMs last_bind_error=$lastBindProbeError"
            }
            Start-Sleep -Milliseconds $deadOwnerBindDrainPollMilliseconds
        }

        $drainElapsedMs = [int64]((Get-Date) - $drainStarted).TotalMilliseconds
        Write-LogLine "SYNAPSE_DAEMON_DEAD_OWNER_BIND_DRAINED generation=$generation bind=$Bind stale_owner_pids=$($staleOwnerPids -join ',') listener_count=$($deadOwnerRows.Count) attempts=$drainAttempts elapsed_ms=$drainElapsedMs bind_probe=success"
        Write-SupervisorEvent 'dead_owner_bind_drained' @{
            generation = $generation
            stale_owner_pids = $staleOwnerPids
            listener_count = $deadOwnerRows.Count
            listener_creation_times = $staleCreationTimes
            attempts = $drainAttempts
            elapsed_ms = $drainElapsedMs
            bind_probe_ok = $true
        }
        Write-SupervisorState -State 'dead_owner_bind_drained' -Generation $generation -ChildPid $null -ExitCode $null -Message "Kernel release of $($deadOwnerRows.Count) dead-owner TCP listener row(s) for pid(s) $($staleOwnerPids -join ',') was verified by an exclusive bind probe after ${drainElapsedMs}ms."
    }

    $generation += 1
    $token = Read-SynapseToken
    $env:SYNAPSE_BEARER_TOKEN = $token
    $env:SYNAPSE_LOG_DIR = $DaemonLogDir

    # Capture child stderr to a per-generation rotating file. Rust writes its
    # crash diagnostics here before __fastfail/abort (e.g. "memory allocation of
    # N bytes failed", "thread '<name>' has overflowed its stack", or a panic
    # message). Without this redirect those lines are lost and a 0xC0000409 exit
    # is unattributable (see issue #1809). Keep the most recent 10 files.
    $stderrLog = Join-Path $DaemonLogDir ('daemon-stderr-gen{0}-{1}.log' -f $generation, (Get-Date -Format 'yyyyMMddHHmmss'))
    try {
        $staleStderr = @(Get-ChildItem -LiteralPath $DaemonLogDir -Filter 'daemon-stderr-gen*.log' -File -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending | Select-Object -Skip 9)
        foreach ($old in $staleStderr) { Remove-Item -LiteralPath $old.FullName -Force -ErrorAction SilentlyContinue }
    } catch { }

    $daemonArgumentTextForGeneration = "$DaemonArgumentText --parent-pid $PID"
    Write-LogLine "SYNAPSE_DAEMON_LAUNCH_START generation=$generation command=$ExePath $daemonArgumentTextForGeneration"
    Write-SupervisorEvent 'launch_start' @{ generation = $generation; exe_path = $ExePath; arguments = $daemonArgumentTextForGeneration; stderr_log = $stderrLog; parent_pid = $PID }
    Write-SupervisorState -State 'launching' -Generation $generation -ChildPid $null -ExitCode $null -Message 'Starting synapse-mcp daemon.'

    $startTime = Get-Date
    $process = Start-Process -FilePath $ExePath -ArgumentList $daemonArgumentTextForGeneration -WorkingDirectory (Split-Path -Parent $ExePath) -WindowStyle Hidden -RedirectStandardError $stderrLog -PassThru
    # #2090: touching .Handle caches the native process handle in the
    # System.Diagnostics.Process object. Without it, `Start-Process -PassThru`
    # does not keep a handle open, the kernel object is released when the child
    # exits, and `$process.ExitCode` reads back $null for EVERY exit. That made
    # the `$exitCode -eq 0` park below dead code since it was written: a daemon
    # that exited cleanly was indistinguishable from one that crashed, and the
    # supervisor relaunched it. It is load-bearing now, because the #2090
    # OS-shutdown drain exits 0 specifically to tell the supervisor "the machine
    # is going down, do not start another daemon".
    $daemonProcessHandle = $null
    try {
        $daemonProcessHandle = $process.Handle
    } catch {
        throw "SYNAPSE_DAEMON_LAUNCH_HANDLE_CACHE_FAILED generation=$generation pid=$($process.Id) error=$($_.Exception.Message) remediation=repair process-handle access; the supervisor cannot distinguish clean exit from crash without the exact retained handle"
    }
    $childInfo = Get-ProcessInfoForPid -ProcessId $process.Id
    if ($null -eq $childInfo) {
        throw "SYNAPSE_DAEMON_LAUNCH_IDENTITY_MISSING generation=$generation pid=$($process.Id) remediation=inspect the retained process handle and stderr; a child that disappeared before identity capture is not a running generation"
    }
    if (-not (Test-ExpectedDaemonProcess -ProcessInfo $childInfo -ExpectedParentPid $PID)) {
        throw "SYNAPSE_DAEMON_LAUNCH_IDENTITY_MISMATCH generation=$generation pid=$($process.Id) exe=$($childInfo.ExecutablePath) command=$(([string]$childInfo.CommandLine -replace '\s+', ' ').Trim()) remediation=inspect the generated daemon arguments; the supervisor will not monitor an unverified child"
    }
    $childCreationDate = [string]$childInfo.CreationDate
    if ([string]::IsNullOrWhiteSpace($childCreationDate)) {
        throw "SYNAPSE_DAEMON_LAUNCH_CREATION_TIME_MISSING generation=$generation pid=$($process.Id) remediation=repair Win32_Process identity reads; PID alone cannot identify a daemon generation"
    }
    Write-LogLine "SYNAPSE_DAEMON_LAUNCH_OK generation=$generation pid=$($process.Id) creation_date=$childCreationDate parent_pid=$PID stderr_log=$stderrLog exit_code_observable=true"
    Write-SupervisorEvent 'launch_ok' @{ generation = $generation; child_pid = $process.Id; child_creation_date = $childCreationDate; parent_pid = $PID; stderr_log = $stderrLog }
    Write-SupervisorState -State 'starting' -Generation $generation -ChildPid $process.Id -ExitCode $null -Message "Daemon exact child identity exists; waiting up to ${daemonStartupSeconds}s for exact listener ownership and authenticated health before publishing running."

    $startupDeadline = (Get-Date).AddSeconds($daemonStartupSeconds)
    $startupAttempts = 0
    while ($true) {
        $startupAttempts += 1
        $readiness = Test-ExactChildReady -ChildPid $process.Id -CreationDate $childCreationDate -Token $token -BindParts $bindParts
        if ($readiness.Ready -eq $true) {
            break
        }
        if ($readiness.Terminal -eq $true) {
            Write-LogLine "SYNAPSE_DAEMON_STARTING_EXITED generation=$generation pid=$($process.Id) creation_date=$childCreationDate attempts=$startupAttempts reason=$($readiness.Reason)"
            Write-SupervisorEvent 'starting_exited' @{ generation = $generation; child_pid = $process.Id; child_creation_date = $childCreationDate; attempts = $startupAttempts; reason = $readiness.Reason }
            break
        }
        if ((Get-Date) -ge $startupDeadline) {
            throw "SYNAPSE_DAEMON_STARTUP_TIMEOUT generation=$generation pid=$($process.Id) creation_date=$childCreationDate attempts=$startupAttempts timeout_seconds=$daemonStartupSeconds last_readiness=$($readiness.Reason) remediation=inspect daemon stderr, exact listener ownership, authenticated /health, and vault-open progress; no replacement is launched while this exact child remains alive"
        }
        if ($startupAttempts -eq 1 -or ($startupAttempts % 20) -eq 0) {
            $startupElapsedMs = [int64]((Get-Date) - $startTime).TotalMilliseconds
            Write-LogLine "SYNAPSE_DAEMON_STARTING generation=$generation pid=$($process.Id) creation_date=$childCreationDate attempts=$startupAttempts readiness=$($readiness.Reason)"
            Write-SupervisorEvent 'starting_progress' @{ generation = $generation; child_pid = $process.Id; child_creation_date = $childCreationDate; attempts = $startupAttempts; elapsed_ms = $startupElapsedMs; readiness = $readiness.Reason }
            Write-SupervisorState -State 'starting' -Generation $generation -ChildPid $process.Id -ExitCode $null -Message "Daemon exact child remains live; readiness=$($readiness.Reason) attempts=$startupAttempts elapsed_ms=$startupElapsedMs. No replacement is eligible."
        }
        Start-Sleep -Milliseconds $daemonStartupPollMilliseconds
    }
    if ($readiness.Ready -eq $true) {
        Write-LogLine "SYNAPSE_DAEMON_READY generation=$generation pid=$($process.Id) creation_date=$childCreationDate attempts=$startupAttempts evidence=$($readiness.Reason)"
        Write-SupervisorEvent 'ready' @{ generation = $generation; child_pid = $process.Id; child_creation_date = $childCreationDate; attempts = $startupAttempts; evidence = $readiness.Reason }
        Write-SupervisorState -State 'running' -Generation $generation -ChildPid $process.Id -ExitCode $null -Message 'Daemon exact PID/creation/image/arguments own the listener and authenticated health returned 200.'
    }

    # #2236: after launch identity has been verified, the retained native
    # process handle is the authoritative identity and lifetime SoT. Windows
    # keeps that exact process object alive while the handle is open and
    # signals it at termination. A post-exit Win32_Process row can transiently
    # remain enumerable with already-cleared image/command-line properties;
    # allowing that teardown view to override WaitForExit(true) stranded the
    # daemon with SYNAPSE_DAEMON_CHILD_IDENTITY_DRIFT after a graceful restart.
    # The parameterless wait synchronizes redirected stream completion and
    # makes ExitCode immediately observable from this same exact handle.
    $process.WaitForExit()
    $terminalIdentity = Get-ExactChildIdentityState -ChildPid $process.Id -CreationDate $childCreationDate
    Write-LogLine "SYNAPSE_DAEMON_EXACT_HANDLE_TERMINAL_PROVED generation=$generation pid=$($process.Id) creation_date=$childCreationDate post_exit_identity=$($terminalIdentity.Reason)"
    Write-SupervisorEvent 'exact_handle_terminal_proved' @{ generation = $generation; child_pid = $process.Id; child_creation_date = $childCreationDate; post_exit_identity = $terminalIdentity.Reason }
    $endTime = Get-Date
    $exitCode = $process.ExitCode
    if ($null -eq $exitCode) {
        throw "SYNAPSE_DAEMON_EXIT_CODE_UNOBSERVABLE generation=$generation pid=$($process.Id) creation_date=$childCreationDate handle_cached=$($null -ne $daemonProcessHandle) remediation=inspect the retained process handle; the supervisor cannot choose clean-stop versus restart without the physical exit code"
    }
    $runtimeMs = [int64](($endTime - $startTime).TotalMilliseconds)
    $stderrTail = ''
    try {
        if (Test-Path -LiteralPath $stderrLog -PathType Leaf) {
            $stderrTail = ((Get-Content -LiteralPath $stderrLog -Tail 40 -ErrorAction SilentlyContinue) -join ' | ').Trim()
        }
    } catch { }
    Write-LogLine "SYNAPSE_DAEMON_EXIT generation=$generation pid=$($process.Id) exit_code=$exitCode runtime_ms=$runtimeMs stderr_log=$stderrLog"
    if ($exitCode -ne 0 -and -not [string]::IsNullOrWhiteSpace($stderrTail)) {
        Write-LogLine "SYNAPSE_DAEMON_STDERR generation=$generation pid=$($process.Id) exit_code=$exitCode tail=$stderrTail"
    }
    Write-SupervisorEvent 'child_exit' @{ generation = $generation; child_pid = $process.Id; exit_code = $exitCode; runtime_ms = $runtimeMs; stderr_log = $stderrLog; stderr_tail = $stderrTail }
    Write-SupervisorState -State 'child_exited' -Generation $generation -ChildPid $process.Id -ExitCode $exitCode -Message "Daemon child exited after ${runtimeMs}ms."

    Stop-IfOperatorStopRequested -Phase 'post_child_exit' -Generation $generation -ChildPid $process.Id -ExitCode $exitCode
    Stop-IfSetupMaintenanceActive -Phase 'post_child_exit' -Generation $generation -ChildPid $process.Id -ExitCode $exitCode

    if ($exitCode -eq 0) {
        Write-LogLine "SYNAPSE_DAEMON_SUPERVISOR_STOP generation=$generation reason=daemon_exit_zero"
        Write-SupervisorEvent 'supervisor_stop' @{ generation = $generation; reason = 'daemon_exit_zero' }
        Write-SupervisorState -State 'stopped' -Generation $generation -ChildPid $process.Id -ExitCode 0 -Message 'Daemon exited cleanly; supervisor stopped.'
        exit 0
    }

    $delaySeconds = Register-RapidFailure -FailureTime $endTime
    Write-LogLine "SYNAPSE_DAEMON_RESTART_SCHEDULED generation=$generation exit_code=$exitCode delay_seconds=$delaySeconds rapid_failures=$($rapidFailures.Count)"
    Write-SupervisorEvent 'restart_scheduled' @{ generation = $generation; exit_code = $exitCode; delay_seconds = $delaySeconds; rapid_failures = $rapidFailures.Count }
    Write-SupervisorState -State 'restart_wait' -Generation $generation -ChildPid $process.Id -ExitCode $exitCode -Message "Restart scheduled in ${delaySeconds}s after non-zero child exit."
    Start-Sleep -Seconds $delaySeconds
    # #2083: authority can be revoked DURING the backoff sleep (up to 60s). Both
    # revocation records are re-read here so a stop that lands mid-backoff parks
    # this supervisor instead of letting it wake up and relaunch.
    Stop-IfOperatorStopRequested -Phase 'post_restart_backoff' -Generation $generation -ChildPid $process.Id -ExitCode $exitCode
    Stop-IfSetupMaintenanceActive -Phase 'post_restart_backoff' -Generation $generation -ChildPid $process.Id -ExitCode $exitCode
}
'@

    $supervisorScript = $supervisorScript.
        Replace('__EXE_PATH__', (Quote-PowerShellSingleQuotedString $ExePath)).
        Replace('__BIND__', (Quote-PowerShellSingleQuotedString $Bind)).
        Replace('__DB_PATH__', (Quote-PowerShellSingleQuotedString $DbPath)).
        Replace('__PROFILES_DIR__', (Quote-PowerShellSingleQuotedString $ProfilesDir)).
        Replace('__DAEMON_LOG_DIR__', (Quote-PowerShellSingleQuotedString $daemonLogDir)).
        Replace('__TOKEN_PATH__', (Quote-PowerShellSingleQuotedString $TokenPath)).
        Replace('__LAUNCHER_LOG__', (Quote-PowerShellSingleQuotedString $launcherLog)).
        Replace('__SUPERVISOR_STATE__', (Quote-PowerShellSingleQuotedString $supervisorState)).
        Replace('__SUPERVISOR_EVENTS__', (Quote-PowerShellSingleQuotedString $supervisorEvents)).
        Replace('__MAINTENANCE_LOCK_PATH__', (Quote-PowerShellSingleQuotedString $MaintenanceLockPath)).
        Replace('__SUPERVISOR_STOP_REQUEST_PATH__', (Quote-PowerShellSingleQuotedString $supervisorStopRequestPath)).
        Replace('__EXPECTED_CALYX_CONFIG_PATH__', (Quote-PowerShellSingleQuotedString $CalyxConfigPath)).
        Replace('__DAEMON_ARGUMENT_TEXT__', (Quote-PowerShellSingleQuotedString $daemonArgumentText)).
        Replace('__EXPECTED_ALLOWED_PERMISSIONS__', (Quote-PowerShellSingleQuotedString $allowedPermissionsArgument)).
        Replace('__EXPECTED_ENABLE_AUDIO__', (Quote-PowerShellSingleQuotedString ([string]$EnableAudio)))

    $supervisorScript | Set-Content -Path $supervisorPath -Encoding ascii

    $supervisorCommand = @(
        (Quote-WindowsCommandArgument $powerShellExe),
        '-NoProfile',
        '-ExecutionPolicy', 'Bypass',
        '-File', (Quote-WindowsCommandArgument $supervisorPath)
    ) -join ' '

    $wrapperScript = @'
Option Explicit
Dim shell, fso, launcherLog, supervisorCommand, exitCode

Set shell = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
launcherLog = __LAUNCHER_LOG__
supervisorCommand = __SUPERVISOR_COMMAND__

Sub LogLine(message)
  Dim logFile
  Set logFile = fso.OpenTextFile(launcherLog, 8, True)
  logFile.WriteLine Now & " " & message
  logFile.Close
End Sub

LogLine "SYNAPSE_DAEMON_WRAPPER_START command=" & supervisorCommand
exitCode = shell.Run(supervisorCommand, 0, True)
LogLine "SYNAPSE_DAEMON_WRAPPER_EXIT exit_code=" & exitCode
WScript.Quit exitCode
'@

    $wrapperScript = $wrapperScript.
        Replace('__LAUNCHER_LOG__', (Vbs-Literal $launcherLog)).
        Replace('__SUPERVISOR_COMMAND__', (Vbs-Literal $supervisorCommand))

    $wrapperScript | Set-Content -Path $OutputPath -Encoding ascii
}

function Ensure-SynapseSetupProcessJobType {
    if ('SynapseSetup.ProcessJob' -as [type]) { return }

    Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace SynapseSetup
{
    public static class ProcessJob
    {
        private const uint CREATE_SUSPENDED = 0x00000004;
        private const uint CREATE_NO_WINDOW = 0x08000000;
        private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
        private const int JobObjectExtendedLimitInformation = 9;
        private const int JobObjectBasicProcessIdList = 3;
        private const uint WAIT_OBJECT_0 = 0x00000000;
        private const uint WAIT_TIMEOUT = 0x00000102;
        private const uint WAIT_FAILED = 0xffffffff;
        private const uint WAIT_NOT_CALLED = 0xfffffffe;
        private const uint EXIT_TIMEOUT = 124;
        private const uint EXIT_ASSIGN_FAILED = 125;
        private const uint EXIT_RESUME_FAILED = 126;
        private const uint INFINITE = 0xffffffff;

        [StructLayout(LayoutKind.Sequential)]
        private struct IO_COUNTERS
        {
            public ulong ReadOperationCount;
            public ulong WriteOperationCount;
            public ulong OtherOperationCount;
            public ulong ReadTransferCount;
            public ulong WriteTransferCount;
            public ulong OtherTransferCount;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_BASIC_LIMIT_INFORMATION
        {
            public long PerProcessUserTimeLimit;
            public long PerJobUserTimeLimit;
            public uint LimitFlags;
            public UIntPtr MinimumWorkingSetSize;
            public UIntPtr MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint PriorityClass;
            public uint SchedulingClass;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
        {
            public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
            public IO_COUNTERS IoInfo;
            public UIntPtr ProcessMemoryLimit;
            public UIntPtr JobMemoryLimit;
            public UIntPtr PeakProcessMemoryUsed;
            public UIntPtr PeakJobMemoryUsed;
        }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        private struct STARTUPINFO
        {
            public uint cb;
            public string lpReserved;
            public string lpDesktop;
            public string lpTitle;
            public uint dwX;
            public uint dwY;
            public uint dwXSize;
            public uint dwYSize;
            public uint dwXCountChars;
            public uint dwYCountChars;
            public uint dwFillAttribute;
            public uint dwFlags;
            public ushort wShowWindow;
            public ushort cbReserved2;
            public IntPtr lpReserved2;
            public IntPtr hStdInput;
            public IntPtr hStdOutput;
            public IntPtr hStdError;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct PROCESS_INFORMATION
        {
            public IntPtr hProcess;
            public IntPtr hThread;
            public uint dwProcessId;
            public uint dwThreadId;
        }

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        private static extern IntPtr CreateJobObject(IntPtr lpJobAttributes, string lpName);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetInformationJobObject(
            IntPtr hJob,
            int jobObjectInfoClass,
            IntPtr lpJobObjectInfo,
            uint cbJobObjectInfoLength);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool QueryInformationJobObject(
            IntPtr hJob,
            int jobObjectInfoClass,
            IntPtr lpJobObjectInfo,
            uint cbJobObjectInfoLength,
            out uint lpReturnLength);

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        private static extern bool CreateProcess(
            string lpApplicationName,
            StringBuilder lpCommandLine,
            IntPtr lpProcessAttributes,
            IntPtr lpThreadAttributes,
            bool bInheritHandles,
            uint dwCreationFlags,
            IntPtr lpEnvironment,
            string lpCurrentDirectory,
            ref STARTUPINFO lpStartupInfo,
            out PROCESS_INFORMATION lpProcessInformation);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint ResumeThread(IntPtr hThread);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint WaitForSingleObject(IntPtr hHandle, uint dwMilliseconds);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetExitCodeProcess(IntPtr hProcess, out uint lpExitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool TerminateJobObject(IntPtr hJob, uint uExitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool TerminateProcess(IntPtr hProcess, uint uExitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool CloseHandle(IntPtr hObject);

        public static int Run(
            string applicationName,
            string commandLine,
            string workingDirectory,
            uint timeoutMilliseconds,
            out string failure,
            out string diagnosticsJson)
        {
            failure = "";
            diagnosticsJson = "";
            string completionKind = "not_started";
            string waitKind = "not_waited";
            uint wait = WAIT_NOT_CALLED;
            uint exitCode = 0xffffffff;
            uint cleanupWait = WAIT_NOT_CALLED;
            uint childPid = 0;
            bool jobCreated = false;
            bool processCreated = false;
            bool assignedToJob = false;
            bool resumed = false;
            bool timedOut = false;
            bool terminateJobCalled = false;
            bool terminateJobOk = false;
            string terminateJobError = "";
            IntPtr job = IntPtr.Zero;
            IntPtr limitPointer = IntPtr.Zero;
            PROCESS_INFORMATION processInfo = new PROCESS_INFORMATION();
            uint[] activeJobProcessIds = new uint[0];
            string activeJobProcessQueryError = "";
            try
            {
                job = CreateJobObject(IntPtr.Zero, null);
                if (job == IntPtr.Zero)
                {
                    failure = "PROCESS_JOB_CREATE_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    completionKind = "job_create_failed";
                    return 127;
                }
                jobCreated = true;

                JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                int limitSize = Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
                limitPointer = Marshal.AllocHGlobal(limitSize);
                Marshal.StructureToPtr(limits, limitPointer, false);
                if (!SetInformationJobObject(job, JobObjectExtendedLimitInformation, limitPointer, (uint)limitSize))
                {
                    failure = "PROCESS_JOB_LIMIT_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    completionKind = "job_limit_failed";
                    return 127;
                }

                STARTUPINFO startupInfo = new STARTUPINFO();
                startupInfo.cb = (uint)Marshal.SizeOf(typeof(STARTUPINFO));
                StringBuilder mutableCommandLine = new StringBuilder(commandLine);
                bool created = CreateProcess(
                    applicationName,
                    mutableCommandLine,
                    IntPtr.Zero,
                    IntPtr.Zero,
                    false,
                    CREATE_SUSPENDED | CREATE_NO_WINDOW,
                    IntPtr.Zero,
                    workingDirectory,
                    ref startupInfo,
                    out processInfo);
                if (!created)
                {
                    failure = "PROCESS_JOB_CREATE_PROCESS_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    completionKind = "process_create_failed";
                    return 127;
                }
                processCreated = true;
                childPid = processInfo.dwProcessId;

                if (!AssignProcessToJobObject(job, processInfo.hProcess))
                {
                    failure = "PROCESS_JOB_ASSIGN_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    completionKind = "job_assign_failed";
                    bool terminated = TerminateProcess(processInfo.hProcess, EXIT_ASSIGN_FAILED);
                    terminateJobCalled = false;
                    terminateJobOk = terminated;
                    if (!terminated)
                    {
                        terminateJobError = new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    }
                    return (int)EXIT_ASSIGN_FAILED;
                }
                assignedToJob = true;

                if (ResumeThread(processInfo.hThread) == 0xffffffff)
                {
                    failure = "PROCESS_JOB_RESUME_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    completionKind = "resume_failed";
                    terminateJobCalled = true;
                    terminateJobOk = TerminateJobObject(job, EXIT_RESUME_FAILED);
                    if (!terminateJobOk)
                    {
                        terminateJobError = new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    }
                    return (int)EXIT_RESUME_FAILED;
                }
                resumed = true;

                wait = WaitForSingleObject(
                    processInfo.hProcess,
                    timeoutMilliseconds == 0 ? INFINITE : timeoutMilliseconds);
                waitKind = WaitKind(wait);
                if (wait == WAIT_TIMEOUT)
                {
                    timedOut = true;
                    completionKind = "timeout";
                    terminateJobCalled = true;
                    terminateJobOk = TerminateJobObject(job, EXIT_TIMEOUT);
                    if (!terminateJobOk)
                    {
                        terminateJobError = new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    }
                    cleanupWait = WaitForSingleObject(processInfo.hProcess, 15000);
                    failure = "PROCESS_JOB_TIMEOUT: child process tree exceeded timeout_ms=" + timeoutMilliseconds;
                    return (int)EXIT_TIMEOUT;
                }
                if (wait == WAIT_FAILED)
                {
                    failure = "PROCESS_JOB_WAIT_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    completionKind = "wait_failed";
                    terminateJobCalled = true;
                    terminateJobOk = TerminateJobObject(job, 127);
                    if (!terminateJobOk)
                    {
                        terminateJobError = new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    }
                    return 127;
                }
                if (wait != WAIT_OBJECT_0)
                {
                    failure = "PROCESS_JOB_WAIT_UNEXPECTED: wait_result=" + wait;
                    completionKind = "wait_unexpected";
                    terminateJobCalled = true;
                    terminateJobOk = TerminateJobObject(job, 127);
                    if (!terminateJobOk)
                    {
                        terminateJobError = new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    }
                    return 127;
                }

                if (!GetExitCodeProcess(processInfo.hProcess, out exitCode))
                {
                    failure = "PROCESS_JOB_EXIT_CODE_FAILED: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
                    completionKind = "exit_code_failed";
                    return 127;
                }
                completionKind = "child_exit";
                return unchecked((int)exitCode);
            }
            finally
            {
                if (job != IntPtr.Zero)
                {
                    try
                    {
                        activeJobProcessIds = ActiveProcessIds(job);
                    }
                    catch (Exception error)
                    {
                        activeJobProcessQueryError = error.Message;
                    }
                }
                diagnosticsJson = BuildDiagnosticsJson(
                    applicationName,
                    commandLine,
                    workingDirectory,
                    timeoutMilliseconds,
                    jobCreated,
                    processCreated,
                    childPid,
                    assignedToJob,
                    resumed,
                    wait,
                    waitKind,
                    timedOut,
                    exitCode,
                    completionKind,
                    terminateJobCalled,
                    terminateJobOk,
                    terminateJobError,
                    cleanupWait,
                    activeJobProcessIds,
                    activeJobProcessQueryError,
                    failure);
                if (limitPointer != IntPtr.Zero)
                {
                    Marshal.FreeHGlobal(limitPointer);
                }
                if (processInfo.hThread != IntPtr.Zero)
                {
                    CloseHandle(processInfo.hThread);
                }
                if (processInfo.hProcess != IntPtr.Zero)
                {
                    CloseHandle(processInfo.hProcess);
                }
                if (job != IntPtr.Zero)
                {
                    CloseHandle(job);
                }
            }
        }

        private static string WaitKind(uint wait)
        {
            if (wait == WAIT_NOT_CALLED) return "not_called";
            if (wait == WAIT_OBJECT_0) return "WAIT_OBJECT_0";
            if (wait == WAIT_TIMEOUT) return "WAIT_TIMEOUT";
            if (wait == WAIT_FAILED) return "WAIT_FAILED";
            return "unexpected_" + wait.ToString();
        }

        private static uint[] ActiveProcessIds(IntPtr job)
        {
            const int capacity = 4096;
            int headerBytes = sizeof(uint) * 2;
            int bufferBytes = headerBytes + (IntPtr.Size * capacity);
            IntPtr buffer = Marshal.AllocHGlobal(bufferBytes);
            try
            {
                uint returned;
                if (!QueryInformationJobObject(
                    job,
                    JobObjectBasicProcessIdList,
                    buffer,
                    (uint)bufferBytes,
                    out returned))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                uint count = unchecked((uint)Marshal.ReadInt32(buffer, sizeof(uint)));
                if (count > capacity)
                {
                    throw new InvalidOperationException("job process list exceeded diagnostic capacity");
                }
                uint[] processIds = new uint[count];
                for (int index = 0; index < count; index++)
                {
                    IntPtr offset = IntPtr.Add(buffer, headerBytes + (index * IntPtr.Size));
                    processIds[index] = unchecked((uint)(IntPtr.Size == 8
                        ? Marshal.ReadInt64(offset)
                        : Marshal.ReadInt32(offset)));
                }
                return processIds;
            }
            finally
            {
                Marshal.FreeHGlobal(buffer);
            }
        }

        private static string JsonEscape(string value)
        {
            if (value == null) return "";
            StringBuilder escaped = new StringBuilder();
            foreach (char c in value)
            {
                switch (c)
                {
                    case '\\': escaped.Append("\\\\"); break;
                    case '"': escaped.Append("\\\""); break;
                    case '\b': escaped.Append("\\b"); break;
                    case '\f': escaped.Append("\\f"); break;
                    case '\n': escaped.Append("\\n"); break;
                    case '\r': escaped.Append("\\r"); break;
                    case '\t': escaped.Append("\\t"); break;
                    default:
                        if (c < 0x20)
                        {
                            escaped.Append("\\u");
                            escaped.Append(((int)c).ToString("x4"));
                        }
                        else
                        {
                            escaped.Append(c);
                        }
                        break;
                }
            }
            return escaped.ToString();
        }

        private static string BuildDiagnosticsJson(
            string applicationName,
            string commandLine,
            string workingDirectory,
            uint timeoutMilliseconds,
            bool jobCreated,
            bool processCreated,
            uint childPid,
            bool assignedToJob,
            bool resumed,
            uint waitResult,
            string waitKind,
            bool timedOut,
            uint exitCode,
            string completionKind,
            bool terminateJobCalled,
            bool terminateJobOk,
            string terminateJobError,
            uint cleanupWait,
            uint[] activeJobProcessIds,
            string activeJobProcessQueryError,
            string failure)
        {
            int signedExitCode = unchecked((int)exitCode);
            StringBuilder json = new StringBuilder();
            json.Append("{");
            json.Append("\"schema\":\"synapse_process_job_result/v1\"");
            json.Append(",\"application_name\":\"").Append(JsonEscape(applicationName)).Append("\"");
            json.Append(",\"command_line\":\"").Append(JsonEscape(commandLine)).Append("\"");
            json.Append(",\"working_directory\":\"").Append(JsonEscape(workingDirectory)).Append("\"");
            json.Append(",\"timeout_ms\":").Append(timeoutMilliseconds);
            json.Append(",\"job_created\":").Append(jobCreated ? "true" : "false");
            json.Append(",\"process_created\":").Append(processCreated ? "true" : "false");
            json.Append(",\"child_pid\":").Append(childPid == 0 ? "null" : childPid.ToString());
            json.Append(",\"assigned_to_job\":").Append(assignedToJob ? "true" : "false");
            json.Append(",\"resumed\":").Append(resumed ? "true" : "false");
            json.Append(",\"wait_result\":").Append(waitResult);
            json.Append(",\"wait_kind\":\"").Append(JsonEscape(waitKind)).Append("\"");
            json.Append(",\"timed_out\":").Append(timedOut ? "true" : "false");
            json.Append(",\"exit_code_unsigned\":").Append(exitCode);
            json.Append(",\"exit_code_signed\":").Append(signedExitCode);
            json.Append(",\"exit_code_hex\":\"0x").Append(exitCode.ToString("X8")).Append("\"");
            json.Append(",\"completion_kind\":\"").Append(JsonEscape(completionKind)).Append("\"");
            json.Append(",\"terminate_job_called\":").Append(terminateJobCalled ? "true" : "false");
            json.Append(",\"terminate_job_ok\":").Append(terminateJobOk ? "true" : "false");
            json.Append(",\"terminate_job_error\":\"").Append(JsonEscape(terminateJobError)).Append("\"");
            json.Append(",\"cleanup_wait_result\":").Append(cleanupWait);
            json.Append(",\"cleanup_wait_kind\":\"").Append(JsonEscape(WaitKind(cleanupWait))).Append("\"");
            json.Append(",\"job_active_process_ids_after\":[");
            for (int index = 0; index < activeJobProcessIds.Length; index++)
            {
                if (index != 0) json.Append(",");
                json.Append(activeJobProcessIds[index]);
            }
            json.Append("]");
            json.Append(",\"job_active_process_query_error\":\"")
                .Append(JsonEscape(activeJobProcessQueryError)).Append("\"");
            json.Append(",\"failure\":\"").Append(JsonEscape(failure)).Append("\"");
            json.Append("}");
            return json.ToString();
        }
    }
}
'@ | Out-Null
}

function Invoke-SynapseProcessInKillOnCloseJob {
    param(
        [Parameter(Mandatory=$true)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [Parameter(Mandatory=$true)][string]$WorkingDirectory,
        [Parameter(Mandatory=$true)][int]$TimeoutMinutes,
        [string]$LogPath,
        [System.Management.Automation.PSReference]$Diagnostics
    )

    Ensure-SynapseSetupProcessJobType
    if (-not (Test-Path $FilePath)) { Die "Process job target missing: $FilePath" }
    if (-not (Test-Path $WorkingDirectory)) { Die "Process job working directory missing: $WorkingDirectory" }

    $argumentText = (($ArgumentList | ForEach-Object { Quote-WindowsCommandArgument $_ }) -join ' ').Trim()
    $targetCommand = (Quote-WindowsCommandArgument $FilePath)
    if (-not [string]::IsNullOrWhiteSpace($argumentText)) {
        $targetCommand = "$targetCommand $argumentText"
    }

    $applicationPath = $FilePath
    $commandLine = $targetCommand
    if (-not [string]::IsNullOrWhiteSpace($LogPath)) {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $LogPath) | Out-Null
        if (Test-Path $LogPath) { Remove-Item $LogPath -Force }
        $cmdPath = Join-Path $env:SystemRoot 'System32\cmd.exe'
        $redirectCommand = "$targetCommand > $(Quote-WindowsCommandArgument $LogPath) 2>&1"
        $applicationPath = $cmdPath
        $commandLine = "$(Quote-WindowsCommandArgument $cmdPath) /d /s /c `"$redirectCommand`""
    }

    $timeoutMilliseconds = [uint32]([math]::Min([int64]$TimeoutMinutes * 60 * 1000, [uint32]::MaxValue))
    $failure = ''
    $processJobDiagnosticsJson = ''
    $startedAt = (Get-Date).ToUniversalTime().ToString('o')
    $compilerProcessesBefore = @(Get-SynapseBuildToolProcessSnapshot)
    $exitCode = [SynapseSetup.ProcessJob]::Run(
        $applicationPath,
        $commandLine,
        $WorkingDirectory,
        $timeoutMilliseconds,
        [ref]$failure,
        [ref]$processJobDiagnosticsJson)
    $completedAt = (Get-Date).ToUniversalTime().ToString('o')
    $compilerProcessesAfter = @(Get-SynapseBuildToolProcessSnapshot)
    $processJobDiagnostics = $null
    if (-not [string]::IsNullOrWhiteSpace($processJobDiagnosticsJson)) {
        try {
            $processJobDiagnostics = $processJobDiagnosticsJson | ConvertFrom-Json -ErrorAction Stop
        } catch {
            $processJobDiagnostics = [pscustomobject]@{
                schema = 'synapse_process_job_result_parse_failed/v1'
                raw = $processJobDiagnosticsJson
                parse_error = $_.Exception.Message
            }
        }
    }
    $childProcessAfter = $null
    if ($processJobDiagnostics -and $processJobDiagnostics.child_pid) {
        $childPid = [int]$processJobDiagnostics.child_pid
        $childProcessAfter = Get-CimInstance Win32_Process -Filter "ProcessId=$childPid" -ErrorAction SilentlyContinue |
            Select-Object ProcessId, ParentProcessId, Name, ExecutablePath, CommandLine
    }
    $jobActiveProcessIds = if ($processJobDiagnostics -and $processJobDiagnostics.job_active_process_ids_after) {
        @($processJobDiagnostics.job_active_process_ids_after | ForEach-Object { [uint32]$_ })
    } else {
        @()
    }
    $jobOwnedBuildToolsAfter = @($compilerProcessesAfter | Where-Object {
        $jobActiveProcessIds -contains [uint32]$_.ProcessId
    })
    $unrelatedBuildToolsAfter = @($compilerProcessesAfter | Where-Object {
        $jobActiveProcessIds -notcontains [uint32]$_.ProcessId
    })
    $diagnosticObject = [ordered]@{
        schema = 'synapse_setup_process_job_invocation/v2'
        command = $targetCommand
        application_path = $applicationPath
        working_directory = $WorkingDirectory
        timeout_minutes = $TimeoutMinutes
        timeout_ms = $timeoutMilliseconds
        log_path = $LogPath
        started_at_utc = $startedAt
        completed_at_utc = $completedAt
        exit_code = $exitCode
        failure = $failure
        process_job = $processJobDiagnostics
        child_process_after = $childProcessAfter
        global_build_tool_processes_before = $compilerProcessesBefore
        global_build_tool_processes_after = $compilerProcessesAfter
        job_owned_build_tool_processes_after = $jobOwnedBuildToolsAfter
        unrelated_build_tool_processes_after = $unrelatedBuildToolsAfter
        cleanup_result = [ordered]@{
            process_table_after_read = $true
            child_process_alive_after = ($null -ne $childProcessAfter)
            job_owned_build_tool_process_count_after = @($jobOwnedBuildToolsAfter).Count
            unrelated_build_tool_process_count_after = @($unrelatedBuildToolsAfter).Count
        }
    }
    if ($PSBoundParameters.ContainsKey('Diagnostics')) {
        $Diagnostics.Value = [pscustomobject]$diagnosticObject
    }
    return $exitCode
}

function Get-SynapseBuildToolProcessSnapshot {
    @(Get-CimInstance Win32_Process -Filter "Name='cargo.exe' OR Name='rustc.exe'" -ErrorAction SilentlyContinue |
        Sort-Object ProcessId |
        Select-Object ProcessId, ParentProcessId, Name, ExecutablePath, CommandLine)
}

# Windows keeps a running executable image locked for write/delete, so cargo can
# never replace its own link output while a process is running FROM the cargo
# target tree (rust-lang/cargo#12485, #11544). That is an environment/ownership
# fault, not a source defect, and it is fully knowable before a ~20 minute
# release build starts. This enumerates every live process whose image resides
# under the resolved target directory so both the preflight and the failure
# classifier can name the exact PID and image path (#1865).
function Get-SynapseBuildOutputImageHolders {
    param([Parameter(Mandatory=$true)][string]$TargetDir)

    $readback = [ordered]@{
        schema = 'synapse_setup_build_output_image_holders/v1'
        target_dir = $null
        process_table_read = $false
        process_table_error = $null
        holders = @()
        unreadable_path_processes = @()
    }
    try {
        $targetFull = Get-SynapseFullPathForScopeCheck -Path $TargetDir
    } catch {
        $readback.target_dir = $TargetDir
        $readback.process_table_error = "target_dir_unresolvable: $($_.Exception.Message)"
        return [pscustomobject]$readback
    }
    $readback.target_dir = $targetFull
    $prefix = $targetFull + [System.IO.Path]::DirectorySeparatorChar

    $processes = $null
    try {
        $processes = @(Get-CimInstance Win32_Process -ErrorAction Stop |
            Select-Object ProcessId, ParentProcessId, Name, ExecutablePath, CommandLine, CreationDate)
        $readback.process_table_read = $true
    } catch {
        $readback.process_table_error = $_.Exception.Message
        return [pscustomobject]$readback
    }

    $holders = @()
    $unreadable = @()
    foreach ($process in $processes) {
        $exe = [string]$process.ExecutablePath
        if ([string]::IsNullOrWhiteSpace($exe)) {
            # A process whose image path cannot be read is not evidence of
            # absence. Record it so the operator sees the exact gap instead of a
            # silent "no holders" claim.
            $unreadable += [pscustomobject]@{
                pid = [int]$process.ProcessId
                name = [string]$process.Name
            }
            continue
        }
        $exeFull = $null
        try { $exeFull = [System.IO.Path]::GetFullPath($exe) } catch { $exeFull = $exe }
        if (-not $exeFull.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) { continue }
        $createdUtc = $null
        try {
            if ($process.CreationDate) { $createdUtc = ([datetime]$process.CreationDate).ToUniversalTime().ToString('o') }
        } catch { $createdUtc = $null }
        $holders += [pscustomobject]@{
            pid = [int]$process.ProcessId
            parent_pid = [int]$process.ParentProcessId
            name = [string]$process.Name
            executable_path = $exeFull
            command_line = (([string]$process.CommandLine -replace '\s+', ' ').Trim())
            created_at_utc = $createdUtc
        }
    }
    $readback.holders = @($holders | Sort-Object pid)
    $readback.unreadable_path_processes = @($unreadable | Sort-Object pid)
    return [pscustomobject]$readback
}

function Format-SynapseBuildOutputImageHolders {
    param([Parameter(Mandatory=$true)]$Readback)

    if (@($Readback.holders).Count -eq 0) { return '<none>' }
    return (@($Readback.holders | ForEach-Object {
        'pid={0} image={1} cmd={2}' -f $_.pid, $_.executable_path, $_.command_line
    }) -join ' | ')
}

# Cargo emits its own orchestration failures with the same `error:` prefix rustc
# uses for real diagnostics. Treating every `^error:` line as a compiler
# diagnostic is what made a locked link output report compiler_error=true with a
# remediation pointing at compiler errors that do not exist (#1865). Cargo's
# authoritative discriminator is `--message-format=json` (`reason` =
# `compiler-message`), which the human-readable build log deliberately does not
# use, so classify by shape instead: cargo orchestration failures are always
# `error: failed to <filesystem/network verb>` or `error: could not <verb>`, and
# never carry a rustc error code or a source span.
#
# `error: could not compile <crate>` belongs here too, and its absence is what
# made #1975 misreport a crashed compiler as a source defect. That line is
# ALWAYS cargo's wrapper around a rustc invocation that failed -- it is emitted
# whether rustc printed diagnostics or died of a segfault, and it never carries
# a source span. rustc's own diagnostics are separate `error[E....]:` / `error:`
# lines, so excluding the wrapper can never hide a genuine compile error: a real
# one still contributes its own line.
$script:SynapseCargoOrchestrationErrorPatterns = @(
    '(?i)^error:\s+failed to (remove|write|copy|rename|hardlink|link|create|open|read|move)\b',
    '(?i)^error:\s+could not (remove|create|write|open|read|delete)\b',
    '(?i)^error:\s+could not compile\b',
    '(?i)^error:\s+failed to (get|download|fetch|sync|update|load|select)\b',
    '(?i)^error:\s+the lock file needs to be updated',
    '(?i)^error:\s+no such command'
)

# A build tool that CRASHED is not a build that found errors. Cargo reports a
# crashed child as
#   process didn't exit successfully: `<tool> <args>` (exit code: 0xc0000005, STATUS_ACCESS_VIOLATION)
# where the exit code is an NTSTATUS value rather than a small integer. That is
# a subprocess crash signature, and it is mechanically distinguishable from a
# diagnostic: rustc/lld emitted no error, the operating system killed them.
#
# The distinction is the whole point of #1975. Four archived deploy failures
# reported `compiler_error=true` with the remediation "repair the compiler error
# lines", against a build in which every crate compiled and zero diagnostics were
# emitted -- sending the operator to hunt for source errors that did not exist,
# after an hour-long build, four times.
$script:SynapseToolchainCrashPatterns = @(
    "(?i)process didn't exit successfully:.*\(exit code:\s*0x[0-9a-f]{8}",
    '(?i)\(exit code:\s*0x[0-9a-f]{8},\s*STATUS_[A-Z_]+\s*\)',
    '(?i)\bSTATUS_ACCESS_VIOLATION\b',
    '(?i)\bSTATUS_STACK_OVERFLOW\b',
    '(?i)\bSTATUS_STACK_BUFFER_OVERRUN\b',
    '(?i)\bSTATUS_HEAP_CORRUPTION\b',
    '(?i)\bSTATUS_IN_PAGE_ERROR\b',
    '(?i)\bSTATUS_ILLEGAL_INSTRUCTION\b'
)

# `error: linking with `link.exe` failed` is rustc's wrapper around a separate
# tool's failure. It is never a source diagnostic, so "repair the compiler error
# lines" is the wrong remediation for it; it gets its own classification.
$script:SynapseLinkerFailurePatterns = @(
    '(?i)^error:\s+linking with .* failed',
    '(?i)\bLNK\d{4}\b',
    '(?i)^\s*=?\s*note:\s*rust-lld:\s*error:',
    '(?i)^rust-lld:\s*error:'
)

# A locked build output is always an environment/ownership fault. `os error 5`
# on the link output, MSVC LNK1104, and rust-lld output-write failures are the
# concrete Windows shapes of "something is holding the file we must replace".
$script:SynapseBuildOutputLockedPatterns = @(
    '(?i)^error:\s+failed to (remove|write|copy|rename|hardlink|link) ',
    '(?i)Access is denied\.\s*\(os error 5\)',
    '(?i)\bLNK1104\b',
    '(?i)rust-lld:\s*error:\s*failed to (write|open) output',
    '(?i)\(os error 32\)'
)

function Test-SynapseLogLineMatchesAny {
    param(
        [Parameter(Mandatory=$true)][string]$Line,
        [Parameter(Mandatory=$true)][string[]]$Patterns
    )
    foreach ($pattern in $Patterns) {
        if ($Line -match $pattern) { return $true }
    }
    return $false
}

function Get-SynapseBuildLogSignal {
    param([string]$Path)

    $signal = [ordered]@{
        path = $Path
        exists = $false
        has_compiler_error = $false
        compiler_error_matches = @()
        has_output_locked_error = $false
        output_locked_matches = @()
        output_locked_paths = @()
        cargo_orchestration_error_matches = @()
        has_linker_failure = $false
        linker_failure_matches = @()
        has_toolchain_crash = $false
        toolchain_crash_matches = @()
        toolchain_crash_status = $null
        toolchain_crash_exit_code = $null
        toolchain_crash_tool = $null
        tail_80 = ''
    }
    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path)) {
        return [pscustomobject]$signal
    }
    $signal.exists = $true
    $signal.tail_80 = (Get-Content -LiteralPath $Path -Tail 80 -ErrorAction SilentlyContinue) -join "`n"

    # `process didn't exit successfully` is in the candidate set because the
    # crash signature lives on THAT line, not on the `error:` line above it.
    # Before #1975 it was never scanned at all, so the one fact that identifies a
    # toolchain crash was invisible to this classifier.
    $candidates = @(Select-String -LiteralPath $Path -Pattern "(?i)(^error(\[.*\])?:|fatal error|could not compile|failed to run custom build command|panicked at|LNK\d{4}|Access is denied\.\s*\(os error 5\)|\(os error 32\)|rust-lld:\s*error:|process didn't exit successfully|STATUS_[A-Z_]{4,})" -ErrorAction SilentlyContinue |
        Select-Object LineNumber, Line)

    $compilerMatches = @()
    $orchestrationMatches = @()
    $lockedMatches = @()
    $linkerMatches = @()
    $crashMatches = @()
    $lockedPaths = @()
    foreach ($candidate in $candidates) {
        $line = [string]$candidate.Line
        $isLocked = Test-SynapseLogLineMatchesAny -Line $line -Patterns $script:SynapseBuildOutputLockedPatterns
        $isOrchestration = Test-SynapseLogLineMatchesAny -Line $line -Patterns $script:SynapseCargoOrchestrationErrorPatterns
        $isLinker = Test-SynapseLogLineMatchesAny -Line $line -Patterns $script:SynapseLinkerFailurePatterns
        $isCrash = Test-SynapseLogLineMatchesAny -Line $line -Patterns $script:SynapseToolchainCrashPatterns
        if ($isCrash) {
            $crashMatches += $candidate
            # Name the tool that died and the status it died of, so the
            # remediation can be specific instead of generic.
            $statusMatch = [regex]::Match($line, '(?i)\b(STATUS_[A-Z_]+)\b')
            if ($statusMatch.Success -and -not $signal.toolchain_crash_status) {
                $signal.toolchain_crash_status = $statusMatch.Groups[1].Value
            }
            $codeMatch = [regex]::Match($line, '(?i)exit code:\s*(0x[0-9a-f]{8})')
            if ($codeMatch.Success -and -not $signal.toolchain_crash_exit_code) {
                $signal.toolchain_crash_exit_code = $codeMatch.Groups[1].Value
            }
            $toolMatch = [regex]::Match($line, '(?i)`([^`]*?([A-Za-z0-9_.\-]+\.exe))')
            if ($toolMatch.Success -and -not $signal.toolchain_crash_tool) {
                $signal.toolchain_crash_tool = $toolMatch.Groups[2].Value
            }
        }
        if ($isLocked) {
            $lockedMatches += $candidate
            # Tools quote the offending path differently: cargo uses backticks,
            # MSVC LINK uses single quotes, rust-lld uses double quotes.
            foreach ($quotePattern in @('`(?<path>[^`]+)`', "'(?<path>[^']+)'", '"(?<path>[^"]+)"')) {
                $pathMatch = [regex]::Match($line, $quotePattern)
                if ($pathMatch.Success) { $lockedPaths += $pathMatch.Groups['path'].Value; break }
            }
        }
        if ($isLinker) { $linkerMatches += $candidate }
        if ($isOrchestration) { $orchestrationMatches += $candidate }
        # A line only counts as a compiler diagnostic when it is none of the
        # non-source failure shapes above. Anything else reproduces the #1865
        # misdirection where an ownership fault reads as compiler_error=true,
        # and the #1975 one where a crashed compiler did.
        if ($isOrchestration -or $isLocked -or $isLinker -or $isCrash) { continue }
        $compilerMatches += $candidate
    }

    $signal.compiler_error_matches = @($compilerMatches | Select-Object -First 20)
    $signal.has_compiler_error = (@($compilerMatches).Count -gt 0)
    $signal.output_locked_matches = @($lockedMatches | Select-Object -First 20)
    $signal.has_output_locked_error = (@($lockedMatches).Count -gt 0)
    $signal.output_locked_paths = @($lockedPaths | Select-Object -Unique)
    $signal.cargo_orchestration_error_matches = @($orchestrationMatches | Select-Object -First 20)
    $signal.linker_failure_matches = @($linkerMatches | Select-Object -First 20)
    $signal.has_linker_failure = (@($linkerMatches).Count -gt 0)
    $signal.toolchain_crash_matches = @($crashMatches | Select-Object -First 20)
    $signal.has_toolchain_crash = (@($crashMatches).Count -gt 0)
    return [pscustomobject]$signal
}

function Get-SynapseArtifactReadback {
    param([Parameter(Mandatory=$true)][string]$Path)

    $readback = [ordered]@{
        path = $Path
        exists = $false
        length_bytes = $null
        sha256 = $null
        exclusive_open = 'not_checked'
        exclusive_open_error = $null
    }
    if (-not (Test-Path -LiteralPath $Path)) {
        $readback.exclusive_open = 'missing'
        return [pscustomobject]$readback
    }
    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    $readback.exists = $true
    $readback.length_bytes = $item.Length
    try {
        $readback.sha256 = Get-SynapseFileSha256 -Path $Path
    } catch {
        $readback.sha256 = $null
        $readback.exclusive_open_error = "hash_failed: $($_.Exception.Message)"
    }
    try {
        $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::None)
        try {
            $readback.exclusive_open = 'ok'
        } finally {
            $stream.Dispose()
        }
    } catch {
        $readback.exclusive_open = 'locked_or_unreadable'
        $readback.exclusive_open_error = $_.Exception.Message
    }
    return [pscustomobject]$readback
}

function Test-SynapseStatusAccessViolationExit {
    param([AllowNull()]$Job)

    if (-not $Job) { return $false }
    $hex = [string]$Job.exit_code_hex
    if ($hex -ieq '0xC0000005') { return $true }
    try {
        if ([uint32]$Job.exit_code_unsigned -eq [uint32]3221225477) { return $true }
    } catch {}
    try {
        if ([int32]$Job.exit_code_signed -eq -1073741819) { return $true }
    } catch {}
    return $false
}

function Get-SynapseWerCrashReadback {
    param([AllowNull()][string]$SinceUtc)

    $folder = Join-Path $env:LOCALAPPDATA 'CrashDumps'
    $since = $null
    if (-not [string]::IsNullOrWhiteSpace($SinceUtc)) {
        try {
            $since = ([DateTimeOffset]::Parse($SinceUtc)).UtcDateTime.AddMinutes(-2)
        } catch {
            $since = $null
        }
    }

    $readback = [ordered]@{
        schema = 'synapse_setup_wer_crash_readback/v1'
        folder = $folder
        folder_exists = $false
        since_utc = if ($since) { $since.ToString('o') } else { $null }
        process_names = @('cargo.exe','rustc.exe','rust-lld.exe','lld-link.exe','link.exe')
        localdump_registry_readable = $false
        localdump_registry_error = $null
        localdump_registry = @()
        recent_dump_count = 0
        recent_dumps = @()
    }

    $registryPaths = @(
        'HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps',
        'HKLM:\SOFTWARE\Wow6432Node\Microsoft\Windows\Windows Error Reporting\LocalDumps'
    )
    $registryRows = @()
    foreach ($registryPath in $registryPaths) {
        try {
            if (Test-Path -LiteralPath $registryPath) {
                $root = Get-ItemProperty -LiteralPath $registryPath -ErrorAction Stop
                $registryRows += [pscustomobject]@{
                    path = $registryPath
                    dump_folder = $root.DumpFolder
                    dump_count = $root.DumpCount
                    dump_type = $root.DumpType
                    custom_dump_flags = $root.CustomDumpFlags
                }
                foreach ($name in $readback.process_names) {
                    $childPath = Join-Path $registryPath $name
                    if (Test-Path -LiteralPath $childPath) {
                        $child = Get-ItemProperty -LiteralPath $childPath -ErrorAction Stop
                        $registryRows += [pscustomobject]@{
                            path = $childPath
                            dump_folder = $child.DumpFolder
                            dump_count = $child.DumpCount
                            dump_type = $child.DumpType
                            custom_dump_flags = $child.CustomDumpFlags
                        }
                    }
                }
            }
            $readback.localdump_registry_readable = $true
        } catch {
            $readback.localdump_registry_error = $_.Exception.Message
        }
    }
    $readback.localdump_registry = @($registryRows)

    if (Test-Path -LiteralPath $folder) {
        $readback.folder_exists = $true
        $names = @($readback.process_names | ForEach-Object { $_.ToLowerInvariant() })
        $dumps = @(Get-ChildItem -LiteralPath $folder -File -ErrorAction SilentlyContinue |
            Where-Object {
                $lowerName = $_.Name.ToLowerInvariant()
                $matchesName = $false
                foreach ($name in $names) {
                    if ($lowerName.StartsWith($name.ToLowerInvariant())) {
                        $matchesName = $true
                        break
                    }
                }
                if (-not $matchesName) {
                    $false
                } elseif ($since) {
                    $_.LastWriteTimeUtc -ge $since
                } else {
                    $true
                }
            } |
            Sort-Object LastWriteTimeUtc -Descending |
            Select-Object -First 20 FullName, Name, Length, @{Name='LastWriteTimeUtc';Expression={$_.LastWriteTimeUtc.ToString('o')}})
        $readback.recent_dump_count = $dumps.Count
        $readback.recent_dumps = @($dumps)
    }

    return [pscustomobject]$readback
}

function Get-SynapseRustToolchainReadback {
    param([Parameter(Mandatory=$true)][string]$CargoPath)

    $commands = @('cargo','rustc','rust-lld','lld-link','link')
    $resolved = @()
    foreach ($command in $commands) {
        $row = [ordered]@{
            command = $command
            source = $null
            version = $null
            error = $null
        }
        try {
            $cmd = Get-Command $command -ErrorAction Stop
            $row.source = $cmd.Source
            if ($command -eq 'cargo') {
                $row.version = (& $CargoPath --version 2>&1 | Select-Object -First 1) -join ''
            } elseif ($command -eq 'rustc') {
                $row.version = (& $cmd.Source -vV 2>&1 | Select-Object -First 8) -join "`n"
            } else {
                $row.version = (& $cmd.Source --version 2>&1 | Select-Object -First 3) -join "`n"
            }
        } catch {
            $row.error = $_.Exception.Message
        }
        $resolved += [pscustomobject]$row
    }

    return [pscustomobject]@{
        schema = 'synapse_setup_rust_toolchain_readback/v1'
        cargo_path = $CargoPath
        commands = @($resolved)
    }
}

function Set-SynapseReleaseBuildCompilerEnvironment {
    # 64 MiB, raised from 8 MiB on 2026-08-03 (#1975).
    #
    # The previous value was 8 MiB, which is EXACTLY rustc's own built-in
    # default -- so this function set the variable to the value it already had
    # and changed nothing. It looked like a mitigation was in place for the
    # STATUS_ACCESS_VIOLATION crashes while none was.
    #
    # `std::thread` reads RUST_MIN_STACK for the default stack size of every
    # thread rustc spawns, including the LLVM codegen workers that run the
    # ThinLTO stage the crash logs place the fault in. Raising it costs nothing
    # at runtime -- it changes no codegen flag and does not touch the shipped
    # binary, only how much address space rustc's own worker threads reserve.
    #
    # THE STACK HYPOTHESIS IS NOW FALSIFIED (#2029). It was recorded here as "a
    # HYPOTHESIS UNDER TEST" on 2026-08-03; it failed its own test twice:
    #   2026-08-04  crash recurred, archive ...20260804T004949861Z-pid27796,
    #               diagnostics record rust_min_stack=67108864, jobs=8
    #   2026-08-06  crash recurred, archive ...20260806T174951173Z-pid46444,
    #               same 64 MiB stack, jobs=32, 72.8 GB physical free
    # The mechanism was sound -- ThinLTO workers are plain std threads, so they
    # DO honour RUST_MIN_STACK (on Windows std's default is only 2 MiB against
    # the frontend's 8 MiB, so this closed a real 4x asymmetry) -- but sound
    # mechanism is not evidence, and the evidence says no.
    #
    # This function is KEPT anyway, and deliberately: it costs nothing (it sets
    # no codegen flag and does not touch the shipped binary, only how much
    # address space rustc's own worker threads reserve), and leaving it in place
    # holds the variable constant so the arm now under test is not confounded by
    # re-introducing a second change. Do not read its survival as an endorsement.
    #
    # The arm that replaced it is the release profile's `lto = "thin"` -> false,
    # taken 2026-08-07 in the root Cargo.toml, where the full evidence table and
    # the falsification criterion are recorded. Do not add a fourth knob here
    # until that one has been settled.
    #
    # Either way the failure is now named correctly by
    # SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASHED instead of being reported as a
    # compiler error, so the evidence accrues on #1975/#2029 rather than being
    # lost.
    $minimumRustStack = 64 * 1024 * 1024
    $existing = $env:RUST_MIN_STACK
    $effective = $minimumRustStack
    $source = 'synapse_setup_default'
    if (-not [string]::IsNullOrWhiteSpace($existing)) {
        $parsed = 0L
        if ([Int64]::TryParse($existing, [ref]$parsed) -and $parsed -ge $minimumRustStack) {
            $effective = $parsed
            $source = 'preexisting_env'
        } else {
            $source = 'raised_by_synapse_setup'
        }
    }
    $env:RUST_MIN_STACK = [string]$effective
    return [pscustomobject]@{
        schema = 'synapse_setup_release_build_compiler_environment/v1'
        rust_min_stack = $env:RUST_MIN_STACK
        rust_min_stack_source = $source
        rust_min_stack_minimum = $minimumRustStack
    }
}

function Get-SynapseReleaseBuildToolchainCrashRetryBudget {
    # How many EXTRA release-build attempts setup makes after rustc physically
    # crashes (#1975 ask 3).
    #
    # This is not a fallback and it is not a retry-until-green loop. It fires on
    # exactly one classification -- SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASHED, a
    # named tool that died on an NTSTATUS having emitted zero source diagnostics
    # -- and on nothing else. A real compiler error, a link failure, a locked
    # output, or a timeout is deterministic and is still fatal on the first
    # attempt, because retrying any of those would be exactly the "cover a break
    # with a rerun" failure this codebase refuses.
    #
    # It is bounded, and every crashed attempt still writes its own immutable
    # diagnostics archive, so a retried deploy accrues MORE evidence than a
    # human-retried one rather than less. If the budget is exhausted the deploy
    # dies with the whole per-attempt history attached.
    #
    # Budget of 2 (3 attempts total). The observed crash rate is ~4 in 7
    # (#1975), so at p(crash) = 0.57 three independent attempts leave a
    # 0.57^3 = 18.6% chance of exhausting the budget -- high enough that the
    # exhaustion path is a real path that must report properly, and low enough
    # that most deploys land unattended. A retry re-runs only the final crate:
    # the measured successful retry took 14m54s against a 1.5-2h cold build.
    $default = 2
    $raw = $env:SYNAPSE_SETUP_TOOLCHAIN_CRASH_RETRIES
    if ([string]::IsNullOrWhiteSpace($raw)) {
        return [pscustomobject]@{ budget = $default; source = 'synapse_setup_default' }
    }
    $parsed = 0
    if (-not [int]::TryParse($raw, [ref]$parsed) -or $parsed -lt 0 -or $parsed -gt 10) {
        Die ("SYNAPSE_SETUP_TOOLCHAIN_CRASH_RETRY_BUDGET_INVALID value={0} remediation=SYNAPSE_SETUP_TOOLCHAIN_CRASH_RETRIES must be an integer in [0,10]; unset it to use the default of {1}" -f `
            $raw, $default)
    }
    return [pscustomobject]@{ budget = $parsed; source = 'env_override' }
}

function Get-SynapseReleaseBuildFailureKind {
    param(
        [Parameter(Mandatory=$true)]$Diagnostics,
        [Parameter(Mandatory=$true)]$LogSignal,
        [Parameter(Mandatory=$true)]$ArtifactReadback,
        [AllowNull()]$OutputImageHolders
    )

    $job = $Diagnostics.process_job
    # Ranked above the compiler-error branch on purpose: a locked link output is
    # an ownership fault whose fix is "stop the process holding the output", and
    # cargo reports it through the same `error:` prefix as a real diagnostic
    # (#1865).
    if ($LogSignal.has_output_locked_error) {
        $lockedPath = if (@($LogSignal.output_locked_paths).Count -gt 0) { @($LogSignal.output_locked_paths)[0] } else { '<unnamed>' }
        $holderText = if ($OutputImageHolders) { Format-SynapseBuildOutputImageHolders -Readback $OutputImageHolders } else { '<not_enumerated>' }
        return [pscustomobject]@{
            code = 'SYNAPSE_RELEASE_BUILD_OUTPUT_LOCKED'
            remediation = ("the build output could not be replaced because it is locked by a live process; locked_path=$lockedPath live_images_under_target_dir=$holderText; stop the exact process holding the build output (a daemon started straight out of target\release locks its own image on Windows) and rerun setup. This is NOT a compiler error; there is nothing to repair in the source.")
        }
    }
    if ($job -and $job.completion_kind -eq 'timeout') {
        return [pscustomobject]@{
            code = 'SYNAPSE_RELEASE_BUILD_TIMEOUT'
            remediation = 'increase BuildTimeoutMinutes only after verifying job_owned_build_tool_processes_after are still making progress, or inspect setup-build.log for a stuck compiler/linker; unrelated_build_tool_processes_after are context only and never cleanup targets'
        }
    }
    if ($job -and -not [string]::IsNullOrWhiteSpace([string]$job.failure) -and $job.completion_kind -ne 'child_exit') {
        return [pscustomobject]@{
            code = 'SYNAPSE_RELEASE_BUILD_PROCESS_JOB_FAILED'
            remediation = 'inspect process_job.failure, wait_kind, terminate_job_ok, and cleanup_wait_kind; repair the Windows process/job-object failure before rerunning setup'
        }
    }
    if ($ArtifactReadback.exclusive_open -eq 'locked_or_unreadable') {
        return [pscustomobject]@{
            code = 'SYNAPSE_RELEASE_BUILD_ARTIFACT_LOCKED'
            remediation = 'inspect the process table for a build or scanner process holding the release artifact; do not close protected terminal/IDE/WSL host processes'
        }
    }
    # Ranked above the compiler-error branch on purpose (#1975): a build tool
    # killed by the operating system emitted no diagnostic, so "repair the
    # compiler error lines" names a fault that does not exist. The crash is
    # detected at the job level (cargo itself died) OR in the log (cargo
    # survived and reported a crashed grandchild -- which is the shape that
    # actually occurs here, because rustc is the process that dies).
    $jobCrashed = ($job -and (Test-SynapseStatusAccessViolationExit -Job $job))
    if ($jobCrashed -or $LogSignal.has_toolchain_crash) {
        $status = if ($LogSignal.toolchain_crash_status) { $LogSignal.toolchain_crash_status } else { 'STATUS_ACCESS_VIOLATION' }
        $hex = if ($LogSignal.toolchain_crash_exit_code) { $LogSignal.toolchain_crash_exit_code } elseif ($job) { [string]$job.exit_code_hex } else { '<unknown>' }
        $tool = if ($LogSignal.toolchain_crash_tool) { $LogSignal.toolchain_crash_tool } elseif ($jobCrashed) { 'cargo.exe' } else { '<unnamed tool>' }
        $diagCount = @($LogSignal.compiler_error_matches).Count
        $diagNote = if ($diagCount -gt 0) {
            "The log ALSO carries $diagCount source-diagnostic line(s) (see compiler_error_matches); read those too, but they did not cause this exit."
        } else {
            'ZERO source-diagnostic lines were emitted, so there is nothing in the source to repair.'
        }
        return [pscustomobject]@{
            code = 'SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASHED'
            remediation = ("$tool was killed by the operating system with $status (exit=$hex). This is a build-TOOL crash, not a compile error: the tool never reported a problem with the code, the OS terminated it. $diagNote " +
                'Remediation, in order: (1) rerun setup -- this crash is probabilistic on this host and every crate other than the final one is already cached, so a retry costs a fraction of a full build; ' +
                '(2) if it repeats, read toolchain_crash_matches in setup-build.log to see which tool and which stage died, and check compiler_environment.rust_min_stack in the diagnostics -- a rustc codegen worker running out of stack surfaces exactly like this on Windows; ' +
                '(3) treat a change in the crashing stage, tool, or status as new evidence and record it on the tracking issue rather than assuming it is the same fault. Do NOT go looking for compiler error lines.')
        }
    }
    if ($LogSignal.has_compiler_error) {
        $first = @($LogSignal.compiler_error_matches | ForEach-Object { "line $($_.LineNumber): $($_.Line)" } | Select-Object -First 5) -join ' | '
        return [pscustomobject]@{
            code = 'SYNAPSE_RELEASE_BUILD_COMPILER_FAILED'
            remediation = ("repair the compiler error lines recorded in setup-build.log before rerunning setup. The classified diagnostics are: $first")
        }
    }
    # Ranked after the compiler branch: a genuine source defect can also fail the
    # link, and the source diagnostic is the more actionable of the two.
    if ($LogSignal.has_linker_failure) {
        return [pscustomobject]@{
            code = 'SYNAPSE_RELEASE_BUILD_LINKER_FAILED'
            remediation = 'the linker failed with no rustc source diagnostic; inspect the linker_failure_matches lines in setup-build.log (missing symbol, missing native library, or unreadable input) and repair the link inputs or toolchain. There are no compiler error lines to repair.'
        }
    }
    if ($job -and [int]$job.exit_code_signed -eq -1) {
        return [pscustomobject]@{
            code = 'SYNAPSE_RELEASE_BUILD_CHILD_EXIT_NO_COMPILER_ERROR'
            remediation = 'child process exited -1 without compiler diagnostics; inspect process_job child_pid/wait_kind/job_active_process_ids_after, job_owned_build_tool_processes_after, artifact_readback, and Windows host logs; unrelated_build_tool_processes_after are context only'
        }
    }
    return [pscustomobject]@{
        code = 'SYNAPSE_RELEASE_BUILD_CHILD_EXIT'
        remediation = 'child process exited nonzero; inspect setup-build.log, process_job, job_owned_build_tool_processes_after, and artifact_readback; unrelated_build_tool_processes_after are context only'
    }
}

function Get-SynapseCargoVersionFailureKind {
    param([Parameter(Mandatory=$true)]$Diagnostics)

    $job = $Diagnostics.process_job
    if ($job -and $job.completion_kind -eq 'timeout') {
        return [pscustomobject]@{
            code = 'SYNAPSE_CARGO_VERSION_TIMEOUT'
            remediation = 'cargo --version did not return inside the setup preflight timeout; inspect child_pid, wait_kind, terminate_job_ok, and process table before rerunning setup'
        }
    }
    if ($job -and -not [string]::IsNullOrWhiteSpace([string]$job.failure) -and $job.completion_kind -ne 'child_exit') {
        return [pscustomobject]@{
            code = 'SYNAPSE_CARGO_VERSION_PROCESS_JOB_FAILED'
            remediation = 'repair the Windows process/job-object failure recorded in setup-cargo-version-diagnostics.json before rerunning setup'
        }
    }
    return [pscustomobject]@{
        code = 'SYNAPSE_CARGO_VERSION_FAILED'
        remediation = 'cargo --version exited nonzero; inspect setup-cargo-version.log and setup-cargo-version-diagnostics.json, then repair the Rust toolchain before rerunning setup'
    }
}

function Install-CodexSynapseTokenLoader {
    param(
        [Parameter(Mandatory=$true)][string]$CodexCommandPath,
        [Parameter(Mandatory=$true)][string]$TokenPath
    )

    $npmDir = Split-Path -Parent $CodexCommandPath
    if (-not $npmDir -or -not (Test-Path $npmDir)) {
        Die "Cannot resolve Codex launcher directory from '$CodexCommandPath'."
    }

    $ps1Path = Join-Path $npmDir 'codex.ps1'
    $cmdPath = Join-Path $npmDir 'codex.cmd'
    $shPath = Join-Path $npmDir 'codex'

    if (Test-Path $ps1Path) {
        $ps1 = @'
#!/usr/bin/env pwsh
$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent

# Synapse MCP token loader: begin
$synapseConfigPath = Join-Path $env:USERPROFILE '.codex\config.toml'
$synapseTokenPath = Join-Path $env:APPDATA 'synapse\token.txt'
$synapseHasConfig = $false
if (Test-Path $synapseConfigPath) {
  try {
    $synapseHasConfig = ((Get-Content -Raw $synapseConfigPath) -match '(?m)^\[mcp_servers\.synapse\]')
  } catch {
    Write-Error "SYNAPSE_CODEX_CONFIG_UNREADABLE path=$synapseConfigPath remediation=repair Codex config permissions or rerun scripts\synapse-setup.ps1"
    exit 1
  }
}
if ($synapseHasConfig) {
  if (-not (Test-Path $synapseTokenPath)) {
    Write-Error "SYNAPSE_CODEX_TOKEN_MISSING path=$synapseTokenPath remediation=run scripts\synapse-setup.ps1 to generate the bearer token"
    exit 1
  }
  $synapseTokenRaw = Get-Content -Raw $synapseTokenPath
  $synapseToken = if ($null -eq $synapseTokenRaw) { '' } else { $synapseTokenRaw.Trim() }
  if ([string]::IsNullOrWhiteSpace($synapseToken)) {
    Write-Error "SYNAPSE_CODEX_TOKEN_EMPTY path=$synapseTokenPath remediation=delete the empty token and rerun scripts\synapse-setup.ps1"
    exit 1
  }
  if ($env:SYNAPSE_BEARER_TOKEN -ne $synapseToken) {
    $env:SYNAPSE_BEARER_TOKEN = $synapseToken
  }
  $synapseToolSurfacePath = Join-Path $env:APPDATA 'synapse\codex-tool-surface.json'
  if (-not (Test-Path $synapseToolSurfacePath)) {
    Write-Error "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_MISSING path=$synapseToolSurfacePath remediation=run scripts\synapse-setup.ps1 to write the current daemon tools/list fingerprint before starting Codex"
    exit 1
  }
  try {
    $synapseToolSurface = Get-Content -Raw $synapseToolSurfacePath | ConvertFrom-Json
  } catch {
    Write-Error "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_UNREADABLE path=$synapseToolSurfacePath error=$($_.Exception.Message) remediation=repair the snapshot file or rerun scripts\synapse-setup.ps1"
    exit 1
  }
  $synapseToolSurfaceHash = [string]$synapseToolSurface.tool_surface_sha256
  if ([string]::IsNullOrWhiteSpace($synapseToolSurfaceHash)) {
    Write-Error "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID path=$synapseToolSurfacePath remediation=delete the invalid snapshot and rerun scripts\synapse-setup.ps1"
    exit 1
  }
  $synapseStartSnapshotDir = Join-Path $env:LOCALAPPDATA 'synapse\codex-start-snapshots'
  $synapseStartSnapshotPath = Join-Path $synapseStartSnapshotDir ("codex-tool-surface-{0}-{1}.json" -f $PID, [Guid]::NewGuid().ToString('N'))
  try {
    New-Item -ItemType Directory -Force -Path $synapseStartSnapshotDir | Out-Null
    Copy-Item -LiteralPath $synapseToolSurfacePath -Destination $synapseStartSnapshotPath -Force
  } catch {
    Write-Error "SYNAPSE_CODEX_TOOL_SURFACE_START_SNAPSHOT_FAILED path=$synapseStartSnapshotPath error=$($_.Exception.Message) remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-start-snapshots before starting Codex"
    exit 1
  }
  $env:SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START = $synapseToolSurfaceHash
  $env:SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START = [string]$synapseToolSurface.tool_count
  $env:SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START = $synapseStartSnapshotPath
}
Remove-Variable synapseConfigPath,synapseTokenPath,synapseHasConfig -ErrorAction SilentlyContinue
Remove-Variable synapseTokenRaw,synapseToken -ErrorAction SilentlyContinue
Remove-Variable synapseToolSurfacePath,synapseToolSurface,synapseToolSurfaceHash -ErrorAction SilentlyContinue
Remove-Variable synapseStartSnapshotDir,synapseStartSnapshotPath -ErrorAction SilentlyContinue
# Synapse MCP token loader: end

$exe=""
if ($PSVersionTable.PSVersion -lt "6.0" -or $IsWindows) {
  # Fix case when both the Windows and Linux builds of Node
  # are installed in the same directory
  $exe=".exe"
}
$ret=0
if (Test-Path "$basedir/node$exe") {
  # Support pipeline input
  if ($MyInvocation.ExpectingInput) {
    $input | & "$basedir/node$exe"  "$basedir/node_modules/@openai/codex/bin/codex.js" $args
  } else {
    & "$basedir/node$exe"  "$basedir/node_modules/@openai/codex/bin/codex.js" $args
  }
  $ret=$LASTEXITCODE
} else {
  # Support pipeline input
  if ($MyInvocation.ExpectingInput) {
    $input | & "node$exe"  "$basedir/node_modules/@openai/codex/bin/codex.js" $args
  } else {
    & "node$exe"  "$basedir/node_modules/@openai/codex/bin/codex.js" $args
  }
  $ret=$LASTEXITCODE
}
exit $ret
'@
        Copy-Item $ps1Path "$ps1Path.synapse-bak" -Force
        Set-Content -Path $ps1Path -Value $ps1 -Encoding utf8
        Info "Installed Synapse token loader in Codex PowerShell launcher: $ps1Path"
    } else {
        Info "WARN: Codex PowerShell launcher not found at $ps1Path; cannot install ps1 token loader."
    }

    if (Test-Path $cmdPath) {
        $cmd = @'
@ECHO off
GOTO start
:find_dp0
SET dp0=%~dp0
EXIT /b
:start
SETLOCAL EnableExtensions EnableDelayedExpansion
CALL :find_dp0

REM Synapse MCP token loader: begin
SET "_synapse_cfg=%USERPROFILE%\.codex\config.toml"
SET "_synapse_tok=%APPDATA%\synapse\token.txt"
SET "_synapse_surface=%APPDATA%\synapse\codex-tool-surface.json"
SET "_synapse_has_cfg="
IF EXIST "%_synapse_cfg%" (
  %SystemRoot%\System32\findstr.exe /R /C:"^\[mcp_servers\.synapse\]" "%_synapse_cfg%" >NUL 2>NUL
  IF NOT ERRORLEVEL 1 SET "_synapse_has_cfg=1"
)
IF DEFINED _synapse_has_cfg (
  IF NOT EXIST "%_synapse_tok%" (
    ECHO SYNAPSE_CODEX_TOKEN_MISSING path=%_synapse_tok% remediation=run scripts\synapse-setup.ps1 to generate the bearer token 1>&2
    EXIT /B 1
  )
  SET /P _synapse_file_token=<"%_synapse_tok%"
  IF NOT DEFINED _synapse_file_token (
    ECHO SYNAPSE_CODEX_TOKEN_EMPTY path=%_synapse_tok% remediation=delete the empty token and rerun scripts\synapse-setup.ps1 1>&2
    EXIT /B 1
  )
  IF NOT "%SYNAPSE_BEARER_TOKEN%"=="!_synapse_file_token!" SET "SYNAPSE_BEARER_TOKEN=!_synapse_file_token!"
  IF NOT EXIST "%_synapse_surface%" (
    ECHO SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_MISSING path=%_synapse_surface% remediation=run scripts\synapse-setup.ps1 to write the current daemon tools/list fingerprint before starting Codex 1>&2
    EXIT /B 1
  )
  SET "_synapse_surface_hash="
  SET "_synapse_surface_count="
  FOR /F "tokens=1,2 delims=;" %%A IN ('%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe -NoLogo -NoProfile -NonInteractive -Command "$j = Get-Content -LiteralPath $env:_synapse_surface -Raw | ConvertFrom-Json; $h = [string]$j.tool_surface_sha256; $c = [int]$j.tool_count; if ($h -notmatch '^[0-9a-fA-F]{64}$' -or $c -lt 1) { exit 1 }; [Console]::Out.Write(('{0};{1}' -f $h.ToLowerInvariant(), $c))"') DO (
    SET "_synapse_surface_hash=%%A"
    SET "_synapse_surface_count=%%B"
  )
  IF NOT DEFINED _synapse_surface_hash (
    ECHO SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID path=%_synapse_surface% remediation=delete the invalid snapshot and rerun scripts\synapse-setup.ps1 1>&2
    EXIT /B 1
  )
  IF NOT DEFINED _synapse_surface_count (
    ECHO SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID path=%_synapse_surface% remediation=delete the invalid snapshot and rerun scripts\synapse-setup.ps1 1>&2
    EXIT /B 1
  )
  SET "_synapse_start_dir=%LOCALAPPDATA%\synapse\codex-start-snapshots"
  SET "_synapse_start_surface=!_synapse_start_dir!\codex-tool-surface-!RANDOM!-!RANDOM!.json"
  IF NOT EXIST "!_synapse_start_dir!" MD "!_synapse_start_dir!" >NUL 2>NUL
  IF NOT EXIST "!_synapse_start_dir!" (
    ECHO SYNAPSE_CODEX_TOOL_SURFACE_START_SNAPSHOT_FAILED path=!_synapse_start_surface! remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-start-snapshots before starting Codex 1>&2
    EXIT /B 1
  )
  COPY /Y "%_synapse_surface%" "!_synapse_start_surface!" >NUL
  IF ERRORLEVEL 1 (
    ECHO SYNAPSE_CODEX_TOOL_SURFACE_START_SNAPSHOT_FAILED path=!_synapse_start_surface! remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-start-snapshots before starting Codex 1>&2
    EXIT /B 1
  )
  SET "SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START=!_synapse_surface_hash!"
  SET "SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START=!_synapse_surface_count!"
  SET "SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START=!_synapse_start_surface!"
)
SET "_synapse_cfg="
SET "_synapse_tok="
SET "_synapse_surface="
SET "_synapse_has_cfg="
SET "_synapse_file_token="
SET "_synapse_surface_hash="
SET "_synapse_surface_count="
SET "_synapse_start_dir="
SET "_synapse_start_surface="
REM Synapse MCP token loader: end

IF EXIST "%dp0%\node.exe" (
  SET "_prog=%dp0%\node.exe"
) ELSE (
  SET "_prog=node"
  SET PATHEXT=%PATHEXT:;.JS;=;%
)

endLocal & SET "SYNAPSE_BEARER_TOKEN=%SYNAPSE_BEARER_TOKEN%" & SET "SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START=%SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START%" & SET "SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START=%SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START%" & SET "SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START=%SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START%" & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\node_modules\@openai\codex\bin\codex.js" %*
'@
        Copy-Item $cmdPath "$cmdPath.synapse-bak" -Force
        Set-Content -Path $cmdPath -Value $cmd -Encoding ascii
        Info "Installed Synapse token loader in Codex CMD launcher: $cmdPath"
    } else {
        Info "WARN: Codex CMD launcher not found at $cmdPath; cannot install cmd token loader."
    }

    if (Test-Path $shPath) {
        $sh = @'
#!/bin/sh
basedir=$(dirname "$(echo "$0" | sed -e 's,\\,/,g')")

# Synapse MCP token loader: begin
synapse_cfg="$USERPROFILE/.codex/config.toml"
synapse_tok="$APPDATA/synapse/token.txt"
case `uname` in
    *CYGWIN*|*MINGW*|*MSYS*)
        if command -v cygpath > /dev/null 2>&1; then
            synapse_cfg=$(cygpath -u "$synapse_cfg")
            synapse_tok=$(cygpath -u "$synapse_tok")
        fi
    ;;
esac
if [ -f "$synapse_cfg" ] && grep -Eq '^\[mcp_servers\.synapse\]' "$synapse_cfg"; then
    if [ ! -r "$synapse_tok" ]; then
        printf '%s\n' "SYNAPSE_CODEX_TOKEN_MISSING path=$synapse_tok remediation=run scripts/synapse-setup.ps1 to generate the bearer token" >&2
        exit 1
    fi
    synapse_file_token=$(tr -d '\r\n' < "$synapse_tok")
    if [ -z "$synapse_file_token" ]; then
        printf '%s\n' "SYNAPSE_CODEX_TOKEN_EMPTY path=$synapse_tok remediation=delete the empty token and rerun scripts/synapse-setup.ps1" >&2
        exit 1
    fi
    if [ "${SYNAPSE_BEARER_TOKEN:-}" != "$synapse_file_token" ]; then
        SYNAPSE_BEARER_TOKEN="$synapse_file_token"
        export SYNAPSE_BEARER_TOKEN
    fi
    synapse_surface="$APPDATA/synapse/codex-tool-surface.json"
    if [ ! -r "$synapse_surface" ]; then
        printf '%s\n' "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_MISSING path=$synapse_surface remediation=run scripts/synapse-setup.ps1 to write the current daemon tools/list fingerprint before starting Codex" >&2
        exit 1
    fi
    synapse_surface_hash=$(sed -n 's/.*"tool_surface_sha256"[[:space:]]*:[[:space:]]*"\([0-9a-fA-F][0-9a-fA-F]*\)".*/\1/p' "$synapse_surface" | head -n 1)
    synapse_surface_count=$(sed -n 's/.*"tool_count"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$synapse_surface" | head -n 1)
    if [ -z "$synapse_surface_hash" ]; then
        printf '%s\n' "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_INVALID path=$synapse_surface remediation=delete the invalid snapshot and rerun scripts/synapse-setup.ps1" >&2
        exit 1
    fi
    synapse_start_dir="$LOCALAPPDATA/synapse/codex-start-snapshots"
    synapse_start_surface="$synapse_start_dir/codex-tool-surface-$$-$(date +%s).json"
    if ! mkdir -p "$synapse_start_dir" || ! cp "$synapse_surface" "$synapse_start_surface"; then
        printf '%s\n' "SYNAPSE_CODEX_TOOL_SURFACE_START_SNAPSHOT_FAILED path=$synapse_start_surface remediation=repair permissions on $synapse_start_dir before starting Codex" >&2
        exit 1
    fi
    SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START="$synapse_surface_hash"
    SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START="$synapse_surface_count"
    SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START="$synapse_start_surface"
    export SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START
fi
unset synapse_cfg synapse_tok synapse_file_token synapse_surface synapse_surface_hash synapse_surface_count synapse_start_dir synapse_start_surface
# Synapse MCP token loader: end

case `uname` in
    *CYGWIN*|*MINGW*|*MSYS*)
        if command -v cygpath > /dev/null 2>&1; then
            basedir=`cygpath -w "$basedir"`
        fi
    ;;
esac

if [ -x "$basedir/node" ]; then
  exec "$basedir/node"  "$basedir/node_modules/@openai/codex/bin/codex.js" "$@"
else
  exec node  "$basedir/node_modules/@openai/codex/bin/codex.js" "$@"
fi
'@
        Copy-Item $shPath "$shPath.synapse-bak" -Force
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText($shPath, ($sh -replace "`r?`n", "`n"), $utf8NoBom)
        Info "Installed Synapse token loader in Codex shell launcher: $shPath"
    } else {
        Info "WARN: Codex shell launcher not found at $shPath; cannot install shell token loader."
    }

    $loaderTokenRaw = if (Test-Path $TokenPath) { Get-Content -Raw $TokenPath } else { $null }
    $loaderToken = if ($null -eq $loaderTokenRaw) { '' } else { $loaderTokenRaw.Trim() }
    if ((Test-Path $TokenPath) -and [string]::IsNullOrWhiteSpace($loaderToken)) {
        Die "Installed Codex token loaders, but token at $TokenPath is empty."
    }
}

function Test-CodexSynapseHttpConfig {
    param(
        [Parameter(Mandatory=$true)][string]$ConfigPath,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][int]$StartupTimeoutSec
    )

    $body = Get-CodexSynapseConfigBody -ConfigPath $ConfigPath
    if ($null -eq $body) {
        return $false
    }
    $bindUrlRegex = [regex]::Escape("http://$Bind/mcp")
    $startupTimeoutRegex = [regex]::Escape([string]$StartupTimeoutSec)
    $startupTimeoutMatches = [regex]::Matches(
        $body,
        "(?m)^\s*startup_timeout_sec\s*=\s*$startupTimeoutRegex(?:\.0+)?\s*$"
    )
    return ($body -match "url\s*=\s*`"$bindUrlRegex`"" -and
        $body -match 'bearer_token_env_var\s*=\s*"SYNAPSE_BEARER_TOKEN"' -and
        $body -match '(?m)^\s*required\s*=\s*true\s*$' -and
        $body -match '(?m)^\s*default_tools_approval_mode\s*=\s*"approve"\s*$' -and
        $startupTimeoutMatches.Count -eq 1)
}

function Test-CodexSynapseHttpTransportConfig {
    param(
        [Parameter(Mandatory=$true)][string]$ConfigPath,
        [Parameter(Mandatory=$true)][string]$Bind
    )

    $body = Get-CodexSynapseConfigBody -ConfigPath $ConfigPath
    if ($null -eq $body) {
        return $false
    }
    $bindUrlRegex = [regex]::Escape("http://$Bind/mcp")
    return ($body -match "url\s*=\s*`"$bindUrlRegex`"" -and
        $body -match 'bearer_token_env_var\s*=\s*"SYNAPSE_BEARER_TOKEN"')
}

function Get-CodexSynapseConfigBody {
    param(
        [Parameter(Mandatory=$true)][string]$ConfigPath
    )

    if (-not (Test-Path $ConfigPath)) {
        return $null
    }
    try {
        $content = Get-Content -Raw $ConfigPath
    } catch {
        return $null
    }
    $section = [regex]::Match(
        $content,
        '(?ms)^\[mcp_servers\.synapse\]\s*(?<body>.*?)(?=^\[|\z)'
    )
    if (-not $section.Success) {
        return $null
    }
    return [string]$section.Groups['body'].Value
}

function Set-CodexSynapseClientPolicy {
    param(
        [Parameter(Mandatory=$true)][string]$ConfigPath,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][int]$StartupTimeoutSec
    )

    $configDir = Split-Path -Parent $ConfigPath
    if (-not (Test-Path $configDir)) {
        [System.IO.Directory]::CreateDirectory($configDir) | Out-Null
    }

    $content = ''
    if (Test-Path $ConfigPath) {
        $content = Get-Content -Raw $ConfigPath
    }

    $desiredLines = @(
        ('url = "http://{0}/mcp"' -f $Bind),
        'bearer_token_env_var = "SYNAPSE_BEARER_TOKEN"',
        'required = true',
        'default_tools_approval_mode = "approve"',
        ('startup_timeout_sec = {0}' -f $StartupTimeoutSec)
    )
    $sectionRegex = '(?ms)^\[mcp_servers\.synapse\]\s*(?<body>.*?)(?=^\[|\z)'
    $section = [regex]::Match($content, $sectionRegex)

    if ($section.Success) {
        $body = [string]$section.Groups['body'].Value
        $preserved = @()
        foreach ($line in ($body -split "`r?`n")) {
            if ($line -match '^\s*(url|bearer_token_env_var|required|default_tools_approval_mode|startup_timeout_sec)\s*=') {
                continue
            }
            if ([string]::IsNullOrWhiteSpace($line) -and $preserved.Count -eq 0) {
                continue
            }
            $preserved += $line
        }
        while ($preserved.Count -gt 0 -and [string]::IsNullOrWhiteSpace($preserved[$preserved.Count - 1])) {
            if ($preserved.Count -eq 1) {
                $preserved = @()
            } else {
                $preserved = @($preserved[0..($preserved.Count - 2)])
            }
        }
        $newSectionLines = @('[mcp_servers.synapse]') + $desiredLines
        if ($preserved.Count -gt 0) {
            $newSectionLines += $preserved
        }
        $newSection = ($newSectionLines -join "`r`n") + "`r`n"
        $content = $content.Substring(0, $section.Index) + $newSection + $content.Substring($section.Index + $section.Length)
    } else {
        if (-not [string]::IsNullOrEmpty($content) -and -not $content.EndsWith("`n")) {
            $content += "`r`n"
        }
        if (-not [string]::IsNullOrWhiteSpace($content)) {
            $content += "`r`n"
        }
        $content += ((@('[mcp_servers.synapse]') + $desiredLines) -join "`r`n") + "`r`n"
    }

    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($ConfigPath, $content, $utf8NoBom)
}

function Test-SynapseMcpExecutableLeafName {
    param([AllowNull()][string]$Name)
    return (-not [string]::IsNullOrWhiteSpace($Name) -and $Name -match '(?i)^synapse-mcp(?:-[0-9a-f]{64})?\.exe$')
}

function Get-SynapseMcpProcessSnapshot {
    @(Get-CimInstance Win32_Process -Filter "Name LIKE 'synapse-mcp%.exe'" -ErrorAction SilentlyContinue |
        Where-Object { Test-SynapseMcpExecutableLeafName -Name $_.Name } |
        Sort-Object ProcessId |
        Select-Object ProcessId, ParentProcessId, Name, ExecutablePath, CommandLine)
}

function Format-SynapseMcpProcessSnapshot {
    param([object[]]$Snapshot)
    if (-not $Snapshot -or $Snapshot.Count -eq 0) {
        return '<none>'
    }
    return (($Snapshot | ForEach-Object {
        $matchRules = if ($_.PSObject.Properties.Name -contains 'DeployTargetRules') { $_.DeployTargetRules } else { '<unclassified>' }
        $bindArg = if ($_.PSObject.Properties.Name -contains 'DeployTargetBindArg') { $_.DeployTargetBindArg } else { '<unclassified>' }
        $dbArg = if ($_.PSObject.Properties.Name -contains 'DeployTargetDbArg') { $_.DeployTargetDbArg } else { '<unclassified>' }
        "pid=$($_.ProcessId) ppid=$($_.ParentProcessId) path=$($_.ExecutablePath) target_match=$matchRules bind_arg=$bindArg db_arg=$dbArg cmd=$($_.CommandLine)"
    }) -join "`n")
}

function Normalize-SynapseSetupPathForCompare {
    param([string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) { return '' }
    try {
        $full = [System.IO.Path]::GetFullPath($Path)
    } catch {
        $full = $Path.Trim()
    }
    return $full.TrimEnd([char[]]@([char]92, [char]47))
}

function Get-SynapseCommandLineArgumentValue {
    param(
        [string]$CommandLine,
        [Parameter(Mandatory=$true)][string]$Name
    )
    if ([string]::IsNullOrWhiteSpace($CommandLine)) { return $null }
    $escapedName = [regex]::Escape($Name)
    $pattern = "(?i)(?:^|\s)$escapedName(?:\s+|=)(?:""(?<quoted>[^""]*)""|(?<bare>\S+))"
    $match = [regex]::Match($CommandLine, $pattern)
    if (-not $match.Success) { return $null }
    if ($match.Groups['quoted'].Success) { return $match.Groups['quoted'].Value }
    return $match.Groups['bare'].Value
}

function Get-SynapseMcpDeployTargetMatch {
    param(
        [Parameter(Mandatory=$true)]$Process,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath
    )

    $rules = @()
    $bindArg = Get-SynapseCommandLineArgumentValue -CommandLine $Process.CommandLine -Name '--bind'
    if (-not [string]::IsNullOrWhiteSpace($bindArg) -and $bindArg.Trim() -ieq $Bind) {
        $rules += "bind=$Bind"
    }

    $expectedDb = Normalize-SynapseSetupPathForCompare -Path $DbPath
    $dbArg = Get-SynapseCommandLineArgumentValue -CommandLine $Process.CommandLine -Name '--db'
    $actualDb = Normalize-SynapseSetupPathForCompare -Path $dbArg
    if (-not [string]::IsNullOrWhiteSpace($actualDb) -and $actualDb -ieq $expectedDb) {
        $rules += "db=$expectedDb"
    }

    [pscustomobject]@{
        IsMatch = ($rules.Count -gt 0)
        Rules = $rules
        BindArg = if ($null -eq $bindArg) { '<missing>' } else { $bindArg }
        DbArg = if ($null -eq $dbArg) { '<missing>' } else { $dbArg }
        ExpectedDb = $expectedDb
    }
}

function Add-SynapseMcpDeployTargetMetadata {
    param(
        [Parameter(Mandatory=$true)]$Process,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath
    )

    $match = Get-SynapseMcpDeployTargetMatch -Process $Process -Bind $Bind -DbPath $DbPath
    $rules = if ($match.Rules.Count -gt 0) { $match.Rules -join ',' } else { '<none>' }
    $Process | Add-Member -NotePropertyName DeployTargetMatched -NotePropertyValue $match.IsMatch -Force
    $Process | Add-Member -NotePropertyName DeployTargetRules -NotePropertyValue $rules -Force
    $Process | Add-Member -NotePropertyName DeployTargetBindArg -NotePropertyValue $match.BindArg -Force
    $Process | Add-Member -NotePropertyName DeployTargetDbArg -NotePropertyValue $match.DbArg -Force
    return $Process
}

function Select-SynapseMcpDeployTargetProcesses {
    param(
        [object[]]$Snapshot,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [switch]$Invert
    )

    @($Snapshot | ForEach-Object {
        $process = Add-SynapseMcpDeployTargetMetadata -Process $_ -Bind $Bind -DbPath $DbPath
        if ($Invert) {
            if (-not $process.DeployTargetMatched) { $process }
        } else {
            if ($process.DeployTargetMatched) { $process }
        }
    })
}

function Get-SynapseLiveDaemonArgumentDrift {
    param(
        [object[]]$Snapshot,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$ExpectedExePath,
        [Parameter(Mandatory=$true)][string]$ExpectedSha256,
        [bool]$EnableAudio,
        [AllowNull()][string]$AllowedPermissions,
        [AllowNull()][string]$CalyxConfigPath
    )

    $expectedAllowed = Normalize-SynapseAllowedPermissionsArgument -Value $AllowedPermissions
    $expectedCalyxConfig = Normalize-SynapseSetupPathForCompare -Path $CalyxConfigPath
    $expectedPath = Normalize-SynapseSetupPathForCompare -Path $ExpectedExePath
    $targets = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $Snapshot -Bind $Bind -DbPath $DbPath)
    $drifts = @()
    foreach ($target in $targets) {
        $actualAllowedRaw = Get-SynapseCommandLineArgumentValue `
            -CommandLine $target.CommandLine `
            -Name '--allowed-permissions'
        $actualAllowed = Normalize-SynapseAllowedPermissionsArgument -Value $actualAllowedRaw
        $actualEnableAudio = ([string]$target.CommandLine) -match '(?i)(?:^|\s)--enable-audio(?:\s|$)'
        $actualCalyxConfig = Normalize-SynapseSetupPathForCompare -Path (Get-SynapseCommandLineArgumentValue -CommandLine $target.CommandLine -Name '--calyx-config')
        $actualPath = Normalize-SynapseSetupPathForCompare -Path ([string]$target.ExecutablePath)
        $actualSha256 = '<not-read>'
        $hashError = $null
        if (-not [string]::IsNullOrWhiteSpace($actualPath) -and (Test-Path -LiteralPath $actualPath -PathType Leaf)) {
            try {
                $actualSha256 = Get-SynapseFileSha256 -Path $actualPath
            } catch {
                $hashError = ($_.Exception.Message -replace '\s+', ' ').Trim()
                $actualSha256 = '<read-failed>'
            }
        } else {
            $actualSha256 = '<missing>'
        }
        $pathDrift = ($actualPath -ine $expectedPath)
        $hashDrift = ($actualSha256 -ine $ExpectedSha256)
        $permissionDrift = ($actualAllowed -ne $expectedAllowed)
        $audioDrift = ($actualEnableAudio -ne $EnableAudio)
        $calyxConfigDrift = ($actualCalyxConfig -ine $expectedCalyxConfig)
        if ($pathDrift -or $hashDrift -or $permissionDrift -or $audioDrift -or $calyxConfigDrift) {
            $drifts += [pscustomobject]@{
                pid = $target.ProcessId
                expected_executable_path = $expectedPath
                actual_executable_path = if ([string]::IsNullOrWhiteSpace($actualPath)) { '<missing>' } else { $actualPath }
                expected_executable_sha256 = $ExpectedSha256
                actual_executable_sha256 = $actualSha256
                executable_hash_error = if ($hashError) { $hashError } else { '<none>' }
                expected_allowed_permissions = if ([string]::IsNullOrWhiteSpace($expectedAllowed)) { '<default-read-only>' } else { $expectedAllowed }
                actual_allowed_permissions = if ([string]::IsNullOrWhiteSpace($actualAllowed)) { '<default-read-only>' } else { $actualAllowed }
                expected_enable_audio = $EnableAudio
                actual_enable_audio = $actualEnableAudio
                expected_calyx_config_path = if ([string]::IsNullOrWhiteSpace($expectedCalyxConfig)) { '<defaults>' } else { $expectedCalyxConfig }
                actual_calyx_config_path = if ([string]::IsNullOrWhiteSpace($actualCalyxConfig)) { '<defaults>' } else { $actualCalyxConfig }
                command_line = $target.CommandLine
            }
        }
    }

    [pscustomobject]@{
        HasDrift = ($drifts.Count -gt 0)
        TargetCount = $targets.Count
        DesiredAllowedPermissions = if ([string]::IsNullOrWhiteSpace($expectedAllowed)) { '<default-read-only>' } else { $expectedAllowed }
        DesiredEnableAudio = $EnableAudio
        DesiredCalyxConfigPath = if ([string]::IsNullOrWhiteSpace($expectedCalyxConfig)) { '<defaults>' } else { $expectedCalyxConfig }
        Drifts = $drifts
    }
}

function Get-SynapseBindEndpoint {
    param([Parameter(Mandatory=$true)][string]$Bind)

    $lastColon = $Bind.LastIndexOf(':')
    if ($lastColon -lt 1 -or $lastColon -eq ($Bind.Length - 1)) {
        Die "SYNAPSE_BIND_PARSE_FAILED bind=$Bind remediation=use host:port, for example 127.0.0.1:7700"
    }

    $address = $Bind.Substring(0, $lastColon)
    $portText = $Bind.Substring($lastColon + 1)
    $port = 0
    if (-not [int]::TryParse($portText, [ref]$port) -or $port -lt 1 -or $port -gt 65535) {
        Die "SYNAPSE_BIND_PARSE_FAILED bind=$Bind port=$portText remediation=use a TCP port from 1 through 65535"
    }

    [pscustomobject]@{ Address = $address; Port = $port }
}

function Test-SynapseBindAvailable {
    param(
        [Parameter(Mandatory=$true)][string]$Bind
    )

    $endpoint = Get-SynapseBindEndpoint -Bind $Bind
    $listener = $null
    try {
        $ipAddress = [System.Net.IPAddress]::Parse($endpoint.Address)
        $listener = [System.Net.Sockets.TcpListener]::new($ipAddress, [int]$endpoint.Port)
        $listener.Start()
        return [pscustomobject]@{ Ok = $true; Error = $null }
    } catch {
        return [pscustomobject]@{ Ok = $false; Error = $_.Exception.Message }
    } finally {
        if ($null -ne $listener) {
            try { $listener.Stop() } catch { }
        }
    }
}

function Get-SynapseTcpClientSnapshot {
    param([Parameter(Mandatory=$true)][string]$Bind)

    $endpoint = Get-SynapseBindEndpoint -Bind $Bind
    $allTcp = @(Get-NetTCPConnection -ErrorAction SilentlyContinue)
    $serverConnections = @($allTcp |
        Where-Object {
            $_.LocalAddress -eq $endpoint.Address -and
            $_.LocalPort -eq $endpoint.Port -and
            "$($_.State)" -ne 'Listen'
        } |
        Sort-Object LocalPort, RemotePort, OwningProcess)

    foreach ($connection in $serverConnections) {
        $peer = @($allTcp | Where-Object {
            $_.LocalAddress -eq $connection.RemoteAddress -and
            $_.LocalPort -eq $connection.RemotePort -and
            $_.RemoteAddress -eq $connection.LocalAddress -and
            $_.RemotePort -eq $connection.LocalPort
        } | Select-Object -First 1)
        $peerOwnerPid = if ($peer.Count -gt 0) { [int]$peer[0].OwningProcess } else { 0 }
        $peerOwner = if ($peerOwnerPid -gt 0) {
            Get-CimInstance Win32_Process -Filter "ProcessId=$peerOwnerPid" -ErrorAction SilentlyContinue
        } else {
            $null
        }
        $peerOwnerExists = ($null -ne $peerOwner)
        [pscustomobject]@{
            State = $connection.State
            LocalAddress = $connection.LocalAddress
            LocalPort = $connection.LocalPort
            RemoteAddress = $connection.RemoteAddress
            RemotePort = $connection.RemotePort
            OwningProcess = $connection.OwningProcess
            OwnerName = (Get-Process -Id $connection.OwningProcess -ErrorAction SilentlyContinue).ProcessName
            OwnerCommandLine = (Get-CimInstance Win32_Process -Filter "ProcessId=$($connection.OwningProcess)" -ErrorAction SilentlyContinue).CommandLine
            PeerOwningProcess = $peerOwnerPid
            PeerOwnerExists = $peerOwnerExists
            PeerOwnerName = $peerOwner.Name
            PeerOwnerCommandLine = $peerOwner.CommandLine
            HasLivePeer = $peerOwnerExists
        }
    }
}

function Get-SynapseTcpBindListenerSnapshot {
    param([Parameter(Mandatory=$true)][string]$Bind)

    $endpoint = Get-SynapseBindEndpoint -Bind $Bind
    $listeners = @(Get-NetTCPConnection -LocalAddress $endpoint.Address -LocalPort $endpoint.Port -State Listen -ErrorAction SilentlyContinue |
        Sort-Object LocalAddress, LocalPort, OwningProcess)

    foreach ($listener in $listeners) {
        $owner = if ($listener.OwningProcess -gt 0) {
            Get-CimInstance Win32_Process -Filter "ProcessId=$($listener.OwningProcess)" -ErrorAction SilentlyContinue
        } else {
            $null
        }
        [pscustomobject]@{
            LocalAddress = $listener.LocalAddress
            LocalPort = $listener.LocalPort
            State = $listener.State
            OwningProcess = $listener.OwningProcess
            CreationTime = $listener.CreationTime
            OwnerExists = ($null -ne $owner)
            OwnerName = $owner.Name
            OwnerCommandLine = $owner.CommandLine
        }
    }
}

function Get-SynapseInstalledDaemonIdentityReadback {
    param(
        [Parameter(Mandatory=$true)][int]$HealthPid,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [Parameter(Mandatory=$true)][string]$ExpectedExePath,
        [Parameter(Mandatory=$true)][string]$ExpectedSha256,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [AllowNull()][string]$AllowedPermissions,
        [AllowNull()][string]$CalyxConfigPath
    )

    $failures = [System.Collections.Generic.List[string]]::new()
    $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
    if ($listeners.Count -ne 1) {
        $failures.Add("listener_count expected=1 actual=$($listeners.Count)")
    }
    $listenerPid = if ($listeners.Count -eq 1) { [int]$listeners[0].OwningProcess } else { 0 }
    if ($listenerPid -ne $HealthPid) {
        $failures.Add("listener_pid expected=$HealthPid actual=$listenerPid")
    }

    $process = Get-CimInstance Win32_Process -Filter "ProcessId=$HealthPid" -ErrorAction SilentlyContinue
    if ($null -eq $process) {
        $failures.Add("health_process_missing pid=$HealthPid")
    }

    $expectedPath = Normalize-SynapseSetupPathForCompare -Path $ExpectedExePath
    $actualPath = if ($null -eq $process) { '' } else { Normalize-SynapseSetupPathForCompare -Path ([string]$process.ExecutablePath) }
    if ($actualPath -ine $expectedPath) {
        $failures.Add("executable_path expected=$expectedPath actual=$(if ($actualPath) { $actualPath } else { '<missing>' })")
    }

    $commandLine = if ($null -eq $process) { '' } else { [string]$process.CommandLine }
    $actualMode = Get-SynapseCommandLineArgumentValue -CommandLine $commandLine -Name '--mode'
    $actualBind = Get-SynapseCommandLineArgumentValue -CommandLine $commandLine -Name '--bind'
    $actualDb = Normalize-SynapseSetupPathForCompare -Path (Get-SynapseCommandLineArgumentValue -CommandLine $commandLine -Name '--db')
    $actualProfiles = Normalize-SynapseSetupPathForCompare -Path (Get-SynapseCommandLineArgumentValue -CommandLine $commandLine -Name '--profile-dir')
    $actualCalyxConfig = Normalize-SynapseSetupPathForCompare -Path (Get-SynapseCommandLineArgumentValue -CommandLine $commandLine -Name '--calyx-config')
    $expectedDb = Normalize-SynapseSetupPathForCompare -Path $DbPath
    $expectedProfiles = Normalize-SynapseSetupPathForCompare -Path $ProfilesDir
    $expectedCalyxConfig = Normalize-SynapseSetupPathForCompare -Path $CalyxConfigPath
    if ($actualMode -ine 'http') { $failures.Add("mode expected=http actual=$(if ($actualMode) { $actualMode } else { '<missing>' })") }
    if ($actualBind -ine $Bind) { $failures.Add("bind expected=$Bind actual=$(if ($actualBind) { $actualBind } else { '<missing>' })") }
    if ($actualDb -ine $expectedDb) { $failures.Add("db expected=$expectedDb actual=$(if ($actualDb) { $actualDb } else { '<missing>' })") }
    if ($actualProfiles -ine $expectedProfiles) { $failures.Add("profiles_dir expected=$expectedProfiles actual=$(if ($actualProfiles) { $actualProfiles } else { '<missing>' })") }
    if ($actualCalyxConfig -ine $expectedCalyxConfig) { $failures.Add("calyx_config expected=$(if ($expectedCalyxConfig) { $expectedCalyxConfig } else { '<defaults>' }) actual=$(if ($actualCalyxConfig) { $actualCalyxConfig } else { '<defaults>' })") }
    $expectedAllowed = Normalize-SynapseAllowedPermissionsArgument -Value $AllowedPermissions
    $actualAllowed = Normalize-SynapseAllowedPermissionsArgument -Value (Get-SynapseCommandLineArgumentValue -CommandLine $commandLine -Name '--allowed-permissions')
    if ($actualAllowed -cne $expectedAllowed) {
        $failures.Add("allowed_permissions expected=$(if ($expectedAllowed) { $expectedAllowed } else { '<default-read-only>' }) actual=$(if ($actualAllowed) { $actualAllowed } else { '<default-read-only>' })")
    }

    $actualSha256 = if (Test-Path -LiteralPath $ExpectedExePath -PathType Leaf) { Get-SynapseFileSha256 -Path $ExpectedExePath } else { '<missing>' }
    if ($actualSha256 -ine $ExpectedSha256) {
        $failures.Add("executable_sha256 expected=$ExpectedSha256 actual=$actualSha256")
    }

    $supervisorStatePath = Join-Path $LogDir 'daemon-supervisor-current.json'
    $supervisorState = $null
    $supervisorStateReadError = $null
    $supervisorSettleStartedAt = Get-Date
    $supervisorSettleDeadline = $supervisorSettleStartedAt.AddSeconds(30)
    $supervisorSettleReads = 0
    # `/health` becomes reachable just before the supervisor publishes its
    # final `running` readback. That small ordering window is not an identity
    # mismatch: the listener, binary hash, arguments, and child PID already
    # prove which daemon answered. Wait for the authoritative supervisor state
    # to settle, but never wait through a foreign child PID or a terminal state.
    while ($true) {
        $supervisorSettleReads++
        $supervisorState = $null
        $supervisorStateReadError = $null
        try {
            $supervisorState = (Get-Content -Raw -LiteralPath $supervisorStatePath -ErrorAction Stop) | ConvertFrom-Json
        } catch {
            $supervisorStateReadError = (($_.Exception.Message -replace '\s+', ' ').Trim())
        }
        $observedSupervisorStatus = if ($null -eq $supervisorState) { '<missing>' } else { [string]$supervisorState.state }
        $observedSupervisorChildPid = if ($null -eq $supervisorState -or $null -eq $supervisorState.child_pid) { 0 } else { [int]$supervisorState.child_pid }
        $supervisorStillPublishing = (
            ($null -eq $supervisorState -or $observedSupervisorStatus -eq 'starting') -and
            $observedSupervisorChildPid -in @(0, $HealthPid) -and
            (Get-Date) -lt $supervisorSettleDeadline)
        if (-not $supervisorStillPublishing) {
            break
        }
        Start-Sleep -Milliseconds 250
    }
    if ($null -eq $supervisorState) {
        $failures.Add("supervisor_state_unreadable path=$supervisorStatePath error=$(if ($supervisorStateReadError) { $supervisorStateReadError } else { '<unknown>' })")
    }
    $supervisorStatus = if ($null -eq $supervisorState) { '<missing>' } else { [string]$supervisorState.state }
    $supervisorChildPid = if ($null -eq $supervisorState -or $null -eq $supervisorState.child_pid) { 0 } else { [int]$supervisorState.child_pid }
    if ($supervisorStatus -notin @('running', 'adopted_existing')) {
        $failures.Add("supervisor_state expected=running_or_adopted_existing actual=$supervisorStatus")
    }
    if ($supervisorChildPid -ne $HealthPid) {
        $failures.Add("supervisor_child_pid expected=$HealthPid actual=$supervisorChildPid")
    }

    [pscustomobject]@{
        Ok = ($failures.Count -eq 0)
        Detail = if ($failures.Count -eq 0) { 'identity_verified' } else { $failures -join '; ' }
        HealthPid = $HealthPid
        ListenerPid = $listenerPid
        ExecutablePath = if ($actualPath) { $actualPath } else { '<missing>' }
        ExecutableSha256 = $actualSha256
        CommandLine = if ($commandLine) { ($commandLine -replace '\s+', ' ').Trim() } else { '<missing>' }
        SupervisorStatePath = $supervisorStatePath
        SupervisorState = $supervisorStatus
        SupervisorChildPid = $supervisorChildPid
        SupervisorSettleReads = $supervisorSettleReads
        SupervisorSettleWaitMs = [int][Math]::Round(((Get-Date) - $supervisorSettleStartedAt).TotalMilliseconds)
    }
}

function Format-SynapseTcpClientSnapshot {
    param([object[]]$Snapshot)
    if (-not $Snapshot -or $Snapshot.Count -eq 0) {
        return '<none>'
    }
    return (($Snapshot | ForEach-Object {
        "state=$($_.State) local=$($_.LocalAddress):$($_.LocalPort) remote=$($_.RemoteAddress):$($_.RemotePort) owner_pid=$($_.OwningProcess) owner=$($_.OwnerName) peer_pid=$($_.PeerOwningProcess) peer_exists=$($_.PeerOwnerExists) peer=$($_.PeerOwnerName) has_live_peer=$($_.HasLivePeer) peer_cmd=$($_.PeerOwnerCommandLine)"
    }) -join "`n")
}

function Format-SynapseTcpBindListenerSnapshot {
    param([object[]]$Snapshot)
    if (-not $Snapshot -or $Snapshot.Count -eq 0) {
        return '<none>'
    }
    return (($Snapshot | ForEach-Object {
        "state=$($_.State) local=$($_.LocalAddress):$($_.LocalPort) owner_pid=$($_.OwningProcess) owner_exists=$($_.OwnerExists) owner=$($_.OwnerName) created=$($_.CreationTime) owner_cmd=$($_.OwnerCommandLine)"
    }) -join "`n")
}

function Get-SynapseDaemonStartupLogSignal {
    param(
        [Parameter(Mandatory=$true)][string]$LogDir,
        [AllowNull()][datetime]$SinceUtc = $null,
        [int]$TailLines = 1600,
        [int]$MaxMatches = 24
    )

    $patterns = @(
        'MCP_CLI_PARSED',
        'MCP_DAEMON_SINGLE_INSTANCE_ACQUIRED',
        'MCP_DAEMON_SHELL_JOB_STORE_LOCK_ACQUIRED',
        'MCP_DAEMON_LIFECYCLE_READY',
        'M4_SHELL_JOB_STARTUP_CORRUPT_RECOVERY',
        'M4_SHELL_JOB_REAP_STARTUP',
        'MCP_DAEMON_STORAGE_AND_CALYX_OPEN_START',
        'SYNAPSE_CALYX_VAULT_OPEN_START',
        'SYNAPSE_CALYX_VAULT_LOCK_ACQUIRED',
        'SYNAPSE_CALYX_ASTER_OPEN_START',
        'CALYX_ASTER_RECOVERY_START',
        'CALYX_ASTER_RECOVERY_MANIFEST_LOADED',
        'CALYX_ASTER_MANIFESTED_BATCHES_READ_START',
        'CALYX_ASTER_MANIFESTED_BATCHES_READ_PROGRESS',
        'CALYX_ASTER_MANIFESTED_BATCHES_READ_DONE',
        'CALYX_ASTER_RECOVERY_DONE',
        'CALYX_ASTER_ROUTER_OPEN_START',
        'CALYX_ASTER_ROUTER_LOAD_START',
        'CALYX_ASTER_ROUTER_LOAD_DISCOVERY_PROGRESS',
        'CALYX_ASTER_ROUTER_LOAD_CF_DONE',
        'CALYX_ASTER_SST_LOOKUP_BUILD_START',
        'CALYX_ASTER_SST_LOOKUP_BUILD_PROGRESS',
        'CALYX_ASTER_SST_LOOKUP_BUILD_DONE',
        'CALYX_ASTER_ROUTER_LOAD_DONE',
        'CALYX_ASTER_ROUTER_OPEN_DONE',
        'CALYX_ASTER_ROUTER_OPEN_FAILED',
        'SYNAPSE_CALYX_ASTER_OPEN_FAILED',
        'SYNAPSE_CALYX_MATH_BACKEND_SELECTED',
        'SYNAPSE_CALYX_VAULT_OPENED',
        'STORAGE_BACKEND_OPENED',
        'MCP_DAEMON_STORAGE_AND_CALYX_OPENED',
        'TIMELINE_RECORDER_STARTED',
        'MCP_DAEMON_ACTIVITY_RECORDER_STARTED',
        'MCP_HTTP_BIND_NORMAL',
        'MCP_HTTP_BIND_FAILED',
        'refusing to start'
    )
    $pattern = ($patterns | ForEach-Object { [regex]::Escape($_) }) -join '|'
    $paths = @(Get-ChildItem -LiteralPath $LogDir -Filter 'synapse.log*' -File -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 2 |
        Sort-Object LastWriteTime)
    $matches = @()
    foreach ($path in $paths) {
        $lines = @(Get-Content -LiteralPath $path.FullName -Tail $TailLines -ErrorAction SilentlyContinue |
            Select-String -Pattern $pattern -ErrorAction SilentlyContinue |
            Select-Object -Last $MaxMatches)
        foreach ($line in $lines) {
            $text = [string]$line.Line
            if ($SinceUtc) {
                $timestampMatch = [regex]::Match($text, '"timestamp"\s*:\s*"(?<timestamp>[^"]+)"')
                if (-not $timestampMatch.Success) {
                    continue
                }
                $lineTimestamp = [datetime]::MinValue
                if (-not [datetime]::TryParse(
                    $timestampMatch.Groups['timestamp'].Value,
                    [System.Globalization.CultureInfo]::InvariantCulture,
                    [System.Globalization.DateTimeStyles]::AssumeUniversal -bor [System.Globalization.DateTimeStyles]::AdjustToUniversal,
                    [ref]$lineTimestamp)) {
                    continue
                }
                if ($lineTimestamp.ToUniversalTime() -lt $SinceUtc.ToUniversalTime()) {
                    continue
                }
            }
            if ($text.Length -gt 900) {
                $text = $text.Substring(0, 900) + '...<truncated>'
            }
            $matches += [pscustomobject]@{
                path = $path.FullName
                line = $text
            }
        }
    }
    if ($matches.Count -gt $MaxMatches) {
        $matches = @($matches | Select-Object -Last $MaxMatches)
    }
    return [pscustomobject]@{
        schema = 'synapse_daemon_startup_log_signal/v1'
        log_dir = $LogDir
        since_utc = ($(if ($SinceUtc) { $SinceUtc.ToUniversalTime().ToString('o') } else { $null }))
        files_scanned = @($paths | Select-Object -ExpandProperty FullName)
        matches = @($matches)
    }
}

function Format-SynapseDaemonStartupLogSignal {
    param([AllowNull()]$Signal)

    if (-not $Signal -or -not $Signal.matches -or @($Signal.matches).Count -eq 0) {
        return '<none>'
    }
    return ((@($Signal.matches) | ForEach-Object {
        "path=$($_.path) line=$($_.line)"
    }) -join "`n")
}

function Ensure-SynapseFileSizeProbeType {
    <#
      Issue #1877: NTFS replicates a file's size into its directory entry as a
      performance tweak for directory enumeration, and since Vista that
      replication only happens when the LAST handle to the file object closes.
      Get-ChildItem/Get-Item read the directory entry, so any file a live writer
      holds open reports an arbitrarily stale size — on this host the current
      daemon log read 0 bytes from the directory entry while its handle reported
      1,400,283. Microsoft documents the remedy on FindFirstFile: "To be assured
      of getting the current NTFS file system file attributes, call the
      GetFileInformationByHandle function."

      The vault WAL is exactly such a file, and its byte total is what the
      pre-purge deletion record (#1875) quotes back to the operator deciding
      whether to destroy a vault. Understating that is the wrong direction to be
      wrong in, so sizes come from a handle, never from the directory entry.

      Reproduced deterministically on this host: with a writer holding a .wal
      open after 1,048,576 bytes of appends, `Get-ChildItem` reported 1,000
      bytes and the handle reported 1,049,576.

      LAST-WRITE TIME IS REPLICATED THE SAME WAY and goes stale the same way.
      That matters beyond cosmetics: Get-SynapseVaultRecoveryFingerprint uses
      the newest write timestamp as a startup-progress signal, so a stale one
      makes an advancing vault look frozen. GetFileInformationByHandle returns
      size and timestamps together and is what Microsoft names as the remedy, so
      both come from the one call.

      FILE_READ_ATTRIBUTES is the minimal access that satisfies it, and asking
      for no data access at all is what lets this probe succeed against files an
      exclusive writer holds open.
    #>
    if ('SynapseSetup.FileSizeProbe' -as [type]) { return }

    Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

namespace SynapseSetup
{
    public static class FileSizeProbe
    {
        private const uint FILE_READ_ATTRIBUTES = 0x0080;
        private const uint FILE_SHARE_READ = 0x00000001;
        private const uint FILE_SHARE_WRITE = 0x00000002;
        private const uint FILE_SHARE_DELETE = 0x00000004;
        private const uint OPEN_EXISTING = 3;
        private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateFileW(
            string lpFileName,
            uint dwDesiredAccess,
            uint dwShareMode,
            IntPtr lpSecurityAttributes,
            uint dwCreationDisposition,
            uint dwFlagsAndAttributes,
            IntPtr hTemplateFile);

        [StructLayout(LayoutKind.Sequential)]
        private struct FILETIME
        {
            public uint dwLowDateTime;
            public uint dwHighDateTime;
            public long ToTicks()
            {
                return (long)(((ulong)dwHighDateTime << 32) | (ulong)dwLowDateTime);
            }
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct BY_HANDLE_FILE_INFORMATION
        {
            public uint dwFileAttributes;
            public FILETIME ftCreationTime;
            public FILETIME ftLastAccessTime;
            public FILETIME ftLastWriteTime;
            public uint dwVolumeSerialNumber;
            public uint nFileSizeHigh;
            public uint nFileSizeLow;
            public uint nNumberOfLinks;
            public uint nFileIndexHigh;
            public uint nFileIndexLow;
        }

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetFileInformationByHandle(IntPtr hFile, out BY_HANDLE_FILE_INFORMATION lpFileInformation);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CloseHandle(IntPtr hObject);

        private static readonly IntPtr INVALID_HANDLE_VALUE = new IntPtr(-1);

        /// <summary>
        /// Returns { size_in_bytes, last_write_utc_filetime } read from the file
        /// object itself. Throws Win32Exception naming the path and win32 code.
        /// </summary>
        public static long[] Probe(string path)
        {
            IntPtr handle = CreateFileW(
                path,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                IntPtr.Zero,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                IntPtr.Zero);
            if (handle == INVALID_HANDLE_VALUE)
            {
                int err = Marshal.GetLastWin32Error();
                throw new Win32Exception(err,
                    "CreateFileW(FILE_READ_ATTRIBUTES) failed for '" + path + "': win32=" + err +
                    " " + new Win32Exception(err).Message);
            }
            try
            {
                BY_HANDLE_FILE_INFORMATION info;
                if (!GetFileInformationByHandle(handle, out info))
                {
                    int err = Marshal.GetLastWin32Error();
                    throw new Win32Exception(err,
                        "GetFileInformationByHandle failed for '" + path + "': win32=" + err +
                        " " + new Win32Exception(err).Message);
                }
                long size = (long)(((ulong)info.nFileSizeHigh << 32) | (ulong)info.nFileSizeLow);
                return new long[] { size, info.ftLastWriteTime.ToTicks() };
            }
            finally
            {
                CloseHandle(handle);
            }
        }

        public static long Length(string path)
        {
            return Probe(path)[0];
        }
    }
}
'@
}

function Get-SynapseAuthoritativeFileLength {
    <#
      Issue #1877: the length of a file as its own handle reports it, not as the
      directory entry cached it. Returns a structured result rather than
      throwing, so a caller summing many files can report exactly which ones it
      could not size instead of losing the whole total to one failure.
    #>
    param([Parameter(Mandatory=$true)][string]$Path)

    Ensure-SynapseFileSizeProbeType
    try {
        $probe = [SynapseSetup.FileSizeProbe]::Probe($Path)
        return [pscustomobject]@{
            path = $Path
            bytes = [int64]$probe[0]
            last_write_utc = [datetime]::FromFileTimeUtc([int64]$probe[1])
            source = 'handle:GetFileInformationByHandle'
            error = $null
        }
    } catch {
        return [pscustomobject]@{
            path = $Path
            bytes = $null
            last_write_utc = $null
            source = 'unavailable'
            error = (($_.Exception.Message) -replace '\s+', ' ').Trim()
        }
    }
}

function Measure-SynapseAuthoritativeFileBytes {
    <#
      Issue #1877: sums file sizes read from handles, and reports incompleteness
      loudly instead of silently substituting the stale directory-entry value.
      `complete=$false` means the returned `bytes` is a LOWER BOUND and the
      unsized files are named.
    #>
    param([AllowNull()][object[]]$Files)

    $result = [ordered]@{
        file_count = 0
        bytes = [int64]0
        byte_source = 'handle:GetFileInformationByHandle'
        sized_file_count = 0
        unsized_file_count = 0
        unsized_files = @()
        complete = $true
        largest = $null
        newest_write_utc_ticks = [int64]0
    }
    $items = @($Files | Where-Object { $null -ne $_ })
    if ($items.Count -eq 0) { return [pscustomobject]$result }

    $result.file_count = $items.Count
    $unsized = New-Object System.Collections.Generic.List[object]
    $largestBytes = [int64]-1
    foreach ($file in $items) {
        $probe = Get-SynapseAuthoritativeFileLength -Path $file.FullName
        if ($null -eq $probe.bytes) {
            $unsized.Add([ordered]@{ path = $probe.path; error = $probe.error }) | Out-Null
            continue
        }
        $result.sized_file_count++
        $result.bytes += [int64]$probe.bytes
        # The enumeration entry's LastWriteTimeUtc is replicated metadata and
        # goes stale exactly like the size does (#1877), so the timestamp comes
        # from the same handle read, not from $file.
        $writeTicks = [int64]$probe.last_write_utc.Ticks
        if ($writeTicks -gt $result.newest_write_utc_ticks) { $result.newest_write_utc_ticks = $writeTicks }
        if ([int64]$probe.bytes -gt $largestBytes) {
            $largestBytes = [int64]$probe.bytes
            $result.largest = [ordered]@{
                name = $file.Name
                bytes = [int64]$probe.bytes
                last_write_utc = $probe.last_write_utc.ToString('o')
                byte_source = 'handle:GetFileInformationByHandle'
            }
        }
    }
    if ($unsized.Count -gt 0) {
        $result.unsized_file_count = $unsized.Count
        # Cap the embedded detail so one broken directory cannot make the
        # snapshot unreadable; the count above is never capped.
        $result.unsized_files = @($unsized | Select-Object -First 8)
        $result.complete = $false
    }
    return [pscustomobject]$result
}

function Get-SynapseCalyxPhysicalSnapshot {
    param([Parameter(Mandatory=$true)][string]$Path)

    $snapshot = [ordered]@{
        schema = 'synapse_calyx_physical_snapshot/v2'
        path = $Path
        exists = $false
        current_pointer = $null
        current_manifest_path = $null
        current_manifest_seq = $null
        current_manifest_durable_seq = $null
        current_manifest_derived_content_seq = $null
        current_manifest_error = $null
        manifest_file_count = 0
        manifest_bytes = 0
        cf_file_count = 0
        cf_bytes = 0
        wal_segment_count = 0
        wal_bytes = 0
        largest_wal = $null
        # Issue #1877: byte totals come from file handles, never directory
        # entries. `bytes_complete=$false` means every *_bytes figure is a lower
        # bound and `unsized_files` names what could not be read.
        byte_source = 'handle:GetFileInformationByHandle'
        bytes_complete = $true
        unsized_file_count = 0
        unsized_files = @()
        lock_path = (Join-Path $Path 'vault.lock')
        lock_exists = $false
        pid_path = (Join-Path $Path 'vault.pid')
        pid_sidecar = $null
        pid_process_exists = $null
        pid_process_name = $null
        pid_process_exe = $null
        pid_process_command = $null
        error = $null
    }

    try {
        $snapshot.exists = Test-Path -LiteralPath $Path -PathType Container
        if (-not $snapshot.exists) {
            return [pscustomobject]$snapshot
        }

        $currentPath = Join-Path $Path 'CURRENT'
        if (Test-Path -LiteralPath $currentPath -PathType Leaf) {
            try {
                $snapshot.current_pointer = (Get-Content -LiteralPath $currentPath -Raw).Trim()
                if (-not [string]::IsNullOrWhiteSpace($snapshot.current_pointer)) {
                    $manifestPath = Join-Path $Path $snapshot.current_pointer
                    $snapshot.current_manifest_path = $manifestPath
                    if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
                        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
                        $snapshot.current_manifest_seq = $manifest.manifest_seq
                        $snapshot.current_manifest_durable_seq = $manifest.durable_seq
                        $snapshot.current_manifest_derived_content_seq = $manifest.derived_content_seq
                    } else {
                        $snapshot.current_manifest_error = "CURRENT target missing: $manifestPath"
                    }
                }
            } catch {
                $snapshot.current_manifest_error = $_.Exception.Message
            }
        }

        $unsized = New-Object System.Collections.Generic.List[object]

        $manifestFiles = @(Get-ChildItem -LiteralPath $Path -Filter 'manifest-*.json' -File -ErrorAction SilentlyContinue)
        if ($manifestFiles.Count -gt 0) {
            $manifestMeasure = Measure-SynapseAuthoritativeFileBytes -Files $manifestFiles
            $snapshot.manifest_file_count = $manifestFiles.Count
            $snapshot.manifest_bytes = $manifestMeasure.bytes
            if (-not $manifestMeasure.complete) {
                $snapshot.bytes_complete = $false
                $snapshot.unsized_file_count += $manifestMeasure.unsized_file_count
                foreach ($u in @($manifestMeasure.unsized_files)) { $unsized.Add($u) | Out-Null }
            }
        }

        $cfRoot = Join-Path $Path 'cf'
        if (Test-Path -LiteralPath $cfRoot -PathType Container) {
            $cfFiles = @(Get-ChildItem -LiteralPath $cfRoot -Recurse -File -ErrorAction SilentlyContinue)
            if ($cfFiles.Count -gt 0) {
                $cfMeasure = Measure-SynapseAuthoritativeFileBytes -Files $cfFiles
                $snapshot.cf_file_count = $cfFiles.Count
                $snapshot.cf_bytes = $cfMeasure.bytes
                if (-not $cfMeasure.complete) {
                    $snapshot.bytes_complete = $false
                    $snapshot.unsized_file_count += $cfMeasure.unsized_file_count
                    foreach ($u in @($cfMeasure.unsized_files)) { $unsized.Add($u) | Out-Null }
                }
            }
        }

        $walRoot = Join-Path $Path 'wal'
        if (Test-Path -LiteralPath $walRoot -PathType Container) {
            $walFiles = @(Get-ChildItem -LiteralPath $walRoot -Filter '*.wal' -File -ErrorAction SilentlyContinue)
            if ($walFiles.Count -gt 0) {
                # The WAL is the file the live daemon holds open and appends to
                # continuously, so it is the one whose directory entry is most
                # reliably wrong (#1877).
                $walMeasure = Measure-SynapseAuthoritativeFileBytes -Files $walFiles
                $snapshot.wal_segment_count = $walFiles.Count
                $snapshot.wal_bytes = $walMeasure.bytes
                $snapshot.largest_wal = $walMeasure.largest
                if (-not $walMeasure.complete) {
                    $snapshot.bytes_complete = $false
                    $snapshot.unsized_file_count += $walMeasure.unsized_file_count
                    foreach ($u in @($walMeasure.unsized_files)) { $unsized.Add($u) | Out-Null }
                }
            }
        }

        if ($unsized.Count -gt 0) {
            $snapshot.unsized_files = @($unsized | Select-Object -First 12)
        }

        $snapshot.lock_exists = Test-Path -LiteralPath $snapshot.lock_path -PathType Leaf
        if (Test-Path -LiteralPath $snapshot.pid_path -PathType Leaf) {
            try {
                $pidText = (Get-Content -LiteralPath $snapshot.pid_path -Raw).Trim()
                $snapshot.pid_sidecar = $pidText
                $pidJson = $pidText | ConvertFrom-Json
                $sidecarPid = [int]$pidJson.pid
                $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$sidecarPid" -ErrorAction SilentlyContinue
                $snapshot.pid_process_exists = ($null -ne $proc)
                if ($proc) {
                    $snapshot.pid_process_name = $proc.Name
                    $snapshot.pid_process_exe = $proc.ExecutablePath
                    $snapshot.pid_process_command = $proc.CommandLine
                }
            } catch {
                $snapshot.pid_sidecar = "read_or_decode_failed: $($_.Exception.Message)"
            }
        }
    } catch {
        $snapshot.error = $_.Exception.Message
    }
    return [pscustomobject]$snapshot
}

function Write-SynapseVaultDeletionRecord {
    <#
      Records what a vault held immediately before setup deletes it, into a file
      OUTSIDE the vault directory so the record survives the deletion, and
      refuses to delete a populated vault without an explicit second flag.

      Issue #1875: `-Purge` removed a vault holding ~1.56M durable sequences and
      left no evidence anywhere that it had ever existed. Every marker of the
      vault's existence lived inside the directory being deleted, so the next
      open was an ordinary successful open of an empty directory. The vault has
      no automatic backup, so the deletion has to be both deliberate and
      recorded, and the record has to live where the delete cannot reach it.
    #>
    param(
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$Reason,
        [switch]$Confirmed
    )

    if (-not (Test-Path -LiteralPath $DbPath -PathType Container)) {
        Info "Vault deletion record skipped: no vault directory at $DbPath"
        return
    }

    $snapshot = Get-SynapseCalyxPhysicalSnapshot -Path $DbPath
    $vaultId = $null
    $identityPath = Join-Path $DbPath 'vault-identity.json'
    if (Test-Path -LiteralPath $identityPath -PathType Leaf) {
        try { $vaultId = ((Get-Content -LiteralPath $identityPath -Raw) | ConvertFrom-Json).vault_id }
        catch { $vaultId = "read_or_decode_failed: $($_.Exception.Message)" }
    }

    $durableSeq = 0
    if ($null -ne $snapshot.current_manifest_durable_seq) {
        $durableSeq = [int64]$snapshot.current_manifest_durable_seq
    }
    $populated = ($durableSeq -gt 0) -or ($snapshot.cf_file_count -gt 0) -or ($snapshot.manifest_file_count -gt 1)

    # Issue #1877: byte totals are read from handles, so they are current rather
    # than the stale directory-entry values NTFS caches for open files. If any
    # file could not be sized the totals are a LOWER BOUND, and the operator
    # deciding whether to destroy this vault has to be told that explicitly.
    $bytesQualifier = 'exact'
    if (-not $snapshot.bytes_complete) {
        $bytesQualifier = "lower_bound_unsized_files=$($snapshot.unsized_file_count)"
        Warn ("Vault byte totals are incomplete: $($snapshot.unsized_file_count) file(s) could not be sized " +
              "from a handle, so cf_bytes/wal_bytes below understate what is on disk. " +
              "First failures: " + (($snapshot.unsized_files | ForEach-Object { "$($_.path) ($($_.error))" }) -join '; '))
    }

    if ($populated -and -not $Confirmed) {
        Die (@(
            "SYNAPSE_SETUP_VAULT_PURGE_REFUSED",
            ("vault_dir=$DbPath vault_id=$vaultId durable_seq=$durableSeq " +
             "manifest_files=$($snapshot.manifest_file_count) cf_files=$($snapshot.cf_file_count) " +
             "cf_bytes=$($snapshot.cf_bytes) wal_segments=$($snapshot.wal_segment_count) " +
             "wal_bytes=$($snapshot.wal_bytes) byte_source=$($snapshot.byte_source) bytes=$bytesQualifier"),
            'source_of_truth=physical vault directory contents read immediately before deletion',
            ('remediation=this vault holds durable captured history and there is no automatic backup ' +
             '(issue #1687). Take a backup first, then rerun with -ConfirmVaultDestruction to delete ' +
             'it deliberately. -Purge alone will not destroy a populated vault.')
        ) -join ' ')
    }

    $parent = Split-Path -Parent $DbPath
    $leaf = Split-Path -Leaf $DbPath
    if ([string]::IsNullOrWhiteSpace($parent)) { $parent = '.' }
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
    $recordPath = Join-Path $parent ("{0}.deleted-{1}.json" -f $leaf, $stamp)

    $record = [ordered]@{
        schema = 'synapse_vault_deletion_record/v1'
        recorded_utc = (Get-Date).ToUniversalTime().ToString('o')
        reason = $Reason
        confirmed = [bool]$Confirmed
        vault_dir = $DbPath
        vault_id = $vaultId
        deleted_by_pid = $PID
        deleted_by_user = "$env:USERDOMAIN\$env:USERNAME"
        physical_snapshot = $snapshot
    }

    try {
        ($record | ConvertTo-Json -Depth 12) | Set-Content -LiteralPath $recordPath -Encoding UTF8
    } catch {
        Die (@(
            "SYNAPSE_SETUP_VAULT_DELETION_RECORD_WRITE_FAILED",
            "record_path=$recordPath error=$($_.Exception.Message)",
            'source_of_truth=deletion record file next to the vault directory',
            ('remediation=setup refuses to delete a vault it cannot record. Fix the write failure at ' +
             'the path above, then retry.')
        ) -join ' ')
    }

    if (-not (Test-Path -LiteralPath $recordPath -PathType Leaf)) {
        Die "SYNAPSE_SETUP_VAULT_DELETION_RECORD_ABSENT record_path=$recordPath remediation=the deletion record did not exist after writing it; refusing to delete the vault"
    }

    Info ("Vault deletion recorded at $recordPath (vault_id=$vaultId durable_seq=$durableSeq " +
          "cf_files=$($snapshot.cf_file_count) cf_bytes=$($snapshot.cf_bytes) wal_bytes=$($snapshot.wal_bytes) " +
          "byte_source=$($snapshot.byte_source) bytes=$bytesQualifier)")
}

function Format-SynapseCalyxPhysicalSnapshot {
    param([AllowNull()]$Snapshot)
    if ($null -eq $Snapshot) {
        return '<none>'
    }
    return ($Snapshot | ConvertTo-Json -Compress -Depth 8)
}

function Format-SynapseDaemonStartupPhysicalState {
    param(
        [Parameter(Mandatory=$true)][string]$DbPath,
        [string]$CalyxVaultPath
    )

    $dbSnapshot = Get-SynapseCalyxPhysicalSnapshot -Path $DbPath
    $vaultPath = $CalyxVaultPath
    if ([string]::IsNullOrWhiteSpace($vaultPath)) {
        $vaultPath = $DbPath
    }
    $vaultSnapshot = Get-SynapseCalyxPhysicalSnapshot -Path $vaultPath
    return ("storage_db={0}`ncalyx_vault={1}" -f `
        (Format-SynapseCalyxPhysicalSnapshot -Snapshot $dbSnapshot),
        (Format-SynapseCalyxPhysicalSnapshot -Snapshot $vaultSnapshot))
}

function Get-SynapseDaemonStartupProgressKey {
    param([AllowNull()]$Signal)

    if (-not $Signal -or -not $Signal.matches -or @($Signal.matches).Count -eq 0) {
        return $null
    }
    $last = @($Signal.matches)[@($Signal.matches).Count - 1]
    return "$($last.path)|$($last.line)"
}

function Test-SynapseInstallHealthProgressSignalCanExtend {
    param([AllowNull()][string]$ProgressKey)

    if ([string]::IsNullOrWhiteSpace($ProgressKey)) {
        return $false
    }
    $extendablePatterns = @(
        'MCP_DAEMON_SINGLE_INSTANCE_ACQUIRED',
        'MCP_DAEMON_SHELL_JOB_STORE_LOCK_ACQUIRED',
        'MCP_DAEMON_LIFECYCLE_READY',
        'M4_SHELL_JOB_STARTUP_CORRUPT_RECOVERY',
        'M4_SHELL_JOB_REAP_STARTUP',
        'MCP_DAEMON_STORAGE_AND_CALYX_OPEN_START',
        'SYNAPSE_CALYX_VAULT_OPEN_START',
        'SYNAPSE_CALYX_VAULT_LOCK_ACQUIRED',
        'SYNAPSE_CALYX_ASTER_OPEN_START',
        'CALYX_ASTER_RECOVERY_START',
        'CALYX_ASTER_RECOVERY_MANIFEST_LOADED',
        'CALYX_ASTER_MANIFESTED_BATCHES_READ_START',
        'CALYX_ASTER_MANIFESTED_BATCHES_READ_PROGRESS',
        'CALYX_ASTER_MANIFESTED_BATCHES_READ_DONE',
        'CALYX_ASTER_RECOVERY_DONE',
        'CALYX_ASTER_ROUTER_OPEN_START',
        'CALYX_ASTER_ROUTER_LOAD_START',
        'CALYX_ASTER_ROUTER_LOAD_DISCOVERY_PROGRESS',
        'CALYX_ASTER_ROUTER_LOAD_CF_DONE',
        'CALYX_ASTER_SST_LOOKUP_BUILD_START',
        'CALYX_ASTER_SST_LOOKUP_BUILD_PROGRESS',
        'CALYX_ASTER_SST_LOOKUP_BUILD_DONE',
        'CALYX_ASTER_ROUTER_LOAD_DONE',
        'CALYX_ASTER_ROUTER_OPEN_DONE',
        'SYNAPSE_CALYX_MATH_BACKEND_SELECTED',
        'SYNAPSE_CALYX_VAULT_OPENED',
        'STORAGE_BACKEND_OPENED',
        'MCP_DAEMON_STORAGE_AND_CALYX_OPENED',
        'TIMELINE_RECORDER_STARTED',
        'MCP_DAEMON_ACTIVITY_RECORDER_STARTED',
        'MCP_HTTP_BIND_NORMAL'
    )
    foreach ($pattern in $extendablePatterns) {
        if ($ProgressKey.Contains($pattern)) {
            return $true
        }
    }
    return $false
}

# ---------------------------------------------------------------------------
# Startup progress watchdog (#1816)
#
# A daemon open that replays a Calyx WAL backlog is legitimately SILENT for many
# minutes: the measured 2026-07-23 candidate emitted CALYX_ASTER_RECOVERY_START
# at 01:08:32Z and its next startup log line (CALYX_ASTER_RECOVERY_MANIFEST_
# LOADED) only at 01:25:58Z - a 17m26s gap with no log evidence at all. A gate
# that can only see log lines therefore cannot tell "working hard" from "hung",
# and a fixed wall-clock budget kills healthy candidates.
#
# So progress is sampled from four independent physical sources and ANY of them
# advancing counts as forward progress:
#   1. startup log phase advanced (and is not a terminal failure phase)
#   2. candidate process CPU time advanced by a meaningful amount
#   3. candidate process disk I/O transfer counters advanced by >= 1 MiB
#   4. vault physical state changed (CURRENT pointer / manifest / cf / wal files)
# A hung daemon burns no CPU, moves no bytes, writes no logs and mutates no
# files, so it stalls out and is failed closed. Absence of progress is failure;
# absence of evidence about progress is also failure, with its own verdict.
# ---------------------------------------------------------------------------

function Get-SynapseDaemonStartupPhaseCode {
    param([AllowNull()][string]$ProgressKey)

    if ([string]::IsNullOrWhiteSpace($ProgressKey)) {
        return '<none>'
    }
    $codeMatches = [regex]::Matches($ProgressKey, '"code"\s*:\s*"(?<code>[A-Za-z0-9_]+)"')
    if ($codeMatches.Count -gt 0) {
        return $codeMatches[$codeMatches.Count - 1].Groups['code'].Value
    }
    return '<uncoded-line>'
}

function Get-SynapseDaemonStartupTerminalFailureLine {
    param(
        [AllowNull()]$Signal,
        [AllowNull()][string]$DbPath,
        [AllowNull()][string]$Bind
    )

    if (-not $Signal -or -not $Signal.matches -or @($Signal.matches).Count -eq 0) {
        return $null
    }
    # Only final daemon-startup transaction failures are terminal. Nested
    # storage/Calyx operations deliberately emit their own ERROR before the
    # startup transaction decides whether that condition is recoverable. In
    # particular, STORAGE_CALYX_ROUTER_ONLY_ROWS_ADOPTION_START handles an
    # initial SYNAPSE_CALYX_ASTER_OPEN_FAILED and then proves a full-MVCC
    # reopen. Treating that nested line as terminal killed the same healthy PID
    # before it reached MCP_HTTP_BIND_NORMAL (#1816).
    #
    # Bind/runtime failures that occur without either final storage code are
    # still fail-closed: the owned daemon exits and the watchdog reports
    # daemon_not_running (or daemon_restart_loop) from the process/supervisor
    # Source of Truth. Do not infer transaction termination from log severity.
    $terminalCodes = @(
        'STORAGE_OR_CALYX_OPEN_START_FAILED',
        'STORAGE_LOCK_CONTENDED'
    )
    # Terminal lines are only honoured when they can be attributed to THIS
    # daemon (db path or bind appears in the line). synapse.log is shared by
    # candidate-preflight daemons, and a misattributed terminal line would kill
    # a healthy candidate - exactly the failure mode being fixed.
    $attribution = @()
    if (-not [string]::IsNullOrWhiteSpace($DbPath)) {
        $attribution += $DbPath
        $attribution += ([string]$DbPath).Replace('\', '\\')
    }
    if (-not [string]::IsNullOrWhiteSpace($Bind)) {
        $attribution += $Bind
    }
    if ($attribution.Count -eq 0) {
        return $null
    }
    foreach ($entry in @($Signal.matches)) {
        $text = [string]$entry.line
        foreach ($code in $terminalCodes) {
            if ($text.IndexOf($code, [System.StringComparison]::Ordinal) -lt 0) { continue }
            foreach ($needle in $attribution) {
                if ($text.IndexOf($needle, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
                    return $text
                }
            }
        }
    }
    return $null
}

function Get-SynapseDaemonProcessWorkCounterSample {
    param([AllowNull()][object[]]$Processes)

    $ids = @(@($Processes) | Where-Object { $null -ne $_ } | ForEach-Object { [int]$_.ProcessId } | Sort-Object)
    $sample = [ordered]@{
        Readable = $false
        Error = $null
        ProcessIds = $ids
        CpuTicks = [double]0
        IoBytes = [double]0
        Detail = '<none>'
    }
    if ($ids.Count -eq 0) {
        $sample.Detail = 'no_candidate_process'
        return [pscustomobject]$sample
    }
    try {
        $rows = @(Get-CimInstance Win32_Process -Filter "Name LIKE 'synapse-mcp%.exe'" -ErrorAction Stop |
            Where-Object { $ids -contains [int]$_.ProcessId } |
            Select-Object ProcessId, KernelModeTime, UserModeTime, ReadTransferCount, WriteTransferCount, OtherTransferCount)
        if ($rows.Count -eq 0) {
            $sample.Detail = 'candidate_process_exited_between_snapshots'
            return [pscustomobject]$sample
        }
        $details = @()
        foreach ($row in $rows) {
            $cpuTicks = ([double]$row.KernelModeTime) + ([double]$row.UserModeTime)
            $ioBytes = ([double]$row.ReadTransferCount) + ([double]$row.WriteTransferCount) + ([double]$row.OtherTransferCount)
            $sample.CpuTicks = $sample.CpuTicks + $cpuTicks
            $sample.IoBytes = $sample.IoBytes + $ioBytes
            $details += ("pid={0} cpu_ms={1} io_bytes={2}" -f [int]$row.ProcessId, [int64]($cpuTicks / 10000), [int64]$ioBytes)
        }
        $sample.Readable = $true
        $sample.Detail = ($details -join ' ')
    } catch {
        $sample.Error = (($_.Exception.Message) -replace '\s+', ' ').Trim()
        $sample.Detail = "counter_read_failed: $($sample.Error)"
    }
    return [pscustomobject]$sample
}

function Get-SynapseVaultRecoveryFingerprint {
    param([AllowNull()][string]$Path)

    $fingerprint = [ordered]@{
        Readable = $false
        Path = $Path
        Key = $null
        Detail = '<none>'
        Error = $null
    }
    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path -PathType Container)) {
        $fingerprint.Detail = 'vault_path_missing'
        return [pscustomobject]$fingerprint
    }
    try {
        $currentPointer = ''
        $currentPath = Join-Path $Path 'CURRENT'
        if (Test-Path -LiteralPath $currentPath -PathType Leaf) {
            $currentPointer = ((Get-Content -LiteralPath $currentPath -Raw -ErrorAction Stop) -replace '\s+', '')
        }
        # Issue #1877: this fingerprint is the stall detector — it decides whether
        # a starting daemon is making physical progress. Directory-entry sizes go
        # stale precisely on the file that proves progress (the WAL the daemon
        # holds open), which makes a growing vault look frozen. Size from handles.
        $newestTicks = [int64]0
        $manifestFiles = @(Get-ChildItem -LiteralPath $Path -Filter 'manifest-*.json' -File -ErrorAction SilentlyContinue)
        $manifestBytes = [double]0
        if ($manifestFiles.Count -gt 0) {
            $manifestMeasure = Measure-SynapseAuthoritativeFileBytes -Files $manifestFiles
            $manifestBytes = [double]$manifestMeasure.bytes
            if ($manifestMeasure.newest_write_utc_ticks -gt $newestTicks) { $newestTicks = $manifestMeasure.newest_write_utc_ticks }
        }
        $cfCount = 0
        $cfBytes = [double]0
        $cfRoot = Join-Path $Path 'cf'
        if (Test-Path -LiteralPath $cfRoot -PathType Container) {
            $cfFiles = @(Get-ChildItem -LiteralPath $cfRoot -Recurse -File -ErrorAction SilentlyContinue)
            if ($cfFiles.Count -gt 0) {
                $cfMeasure = Measure-SynapseAuthoritativeFileBytes -Files $cfFiles
                $cfCount = $cfFiles.Count
                $cfBytes = [double]$cfMeasure.bytes
                if ($cfMeasure.newest_write_utc_ticks -gt $newestTicks) { $newestTicks = $cfMeasure.newest_write_utc_ticks }
            }
        }
        $walCount = 0
        $walBytes = [double]0
        $walRoot = Join-Path $Path 'wal'
        if (Test-Path -LiteralPath $walRoot -PathType Container) {
            $walFiles = @(Get-ChildItem -LiteralPath $walRoot -Filter '*.wal' -File -ErrorAction SilentlyContinue)
            if ($walFiles.Count -gt 0) {
                $walMeasure = Measure-SynapseAuthoritativeFileBytes -Files $walFiles
                $walCount = $walFiles.Count
                $walBytes = [double]$walMeasure.bytes
                if ($walMeasure.newest_write_utc_ticks -gt $newestTicks) { $newestTicks = $walMeasure.newest_write_utc_ticks }
            }
        }
        $fingerprint.Key = ("current={0} manifests={1}/{2} cf={3}/{4} wal={5}/{6} newest_write_ticks={7}" -f `
            $(if ([string]::IsNullOrWhiteSpace($currentPointer)) { '<none>' } else { $currentPointer }),
            $manifestFiles.Count,
            [int64]$manifestBytes,
            $cfCount,
            [int64]$cfBytes,
            $walCount,
            [int64]$walBytes,
            $newestTicks)
        $fingerprint.Readable = $true
        $fingerprint.Detail = $fingerprint.Key
    } catch {
        $fingerprint.Error = (($_.Exception.Message) -replace '\s+', ' ').Trim()
        $fingerprint.Detail = "vault_fingerprint_failed: $($fingerprint.Error)"
    }
    return [pscustomobject]$fingerprint
}

function Get-SynapseDaemonStartupProgressSample {
    param(
        [AllowNull()]$StartupLogSignal,
        [AllowNull()][object[]]$CandidateProcesses,
        [AllowNull()][string]$VaultPath
    )

    $counters = Get-SynapseDaemonProcessWorkCounterSample -Processes $CandidateProcesses
    $vault = Get-SynapseVaultRecoveryFingerprint -Path $VaultPath
    $logKey = Get-SynapseDaemonStartupProgressKey -Signal $StartupLogSignal
    return [pscustomobject]@{
        SampledAt = (Get-Date)
        LogKey = $logKey
        LogPhase = (Get-SynapseDaemonStartupPhaseCode -ProgressKey $logKey)
        ProcessIds = @($counters.ProcessIds)
        ProcessCount = @($counters.ProcessIds).Count
        CountersReadable = $counters.Readable
        CpuTicks = $counters.CpuTicks
        IoBytes = $counters.IoBytes
        CounterDetail = $counters.Detail
        VaultReadable = $vault.Readable
        VaultKey = $vault.Key
        VaultDetail = $vault.Detail
        Observable = ($counters.Readable -or $vault.Readable -or (-not [string]::IsNullOrWhiteSpace($logKey)))
    }
}

function Compare-SynapseDaemonStartupProgressSample {
    param(
        [AllowNull()]$Previous,
        [Parameter(Mandatory=$true)]$Current,
        [double]$MinCpuTicksDelta = 2500000,
        [double]$MinIoBytesDelta = 1048576
    )

    if ($null -eq $Previous) {
        return [pscustomobject]@{
            Advanced = $false
            PidsChanged = $false
            Reasons = @('baseline_sample')
            Detail = 'baseline_sample'
        }
    }
    $previousIds = (@($Previous.ProcessIds) -join ',')
    $currentIds = (@($Current.ProcessIds) -join ',')
    $pidsChanged = ($previousIds -ne $currentIds)
    $reasons = @()
    if ((-not [string]::IsNullOrWhiteSpace($Current.LogKey)) -and
        ($Current.LogKey -ne $Previous.LogKey) -and
        (Test-SynapseInstallHealthProgressSignalCanExtend -ProgressKey $Current.LogKey)) {
        $reasons += ("startup_log_phase_advanced={0}" -f $Current.LogPhase)
    }
    if ($Current.CountersReadable -and $Previous.CountersReadable -and -not $pidsChanged) {
        $cpuDelta = [double]$Current.CpuTicks - [double]$Previous.CpuTicks
        $ioDelta = [double]$Current.IoBytes - [double]$Previous.IoBytes
        if ($cpuDelta -ge $MinCpuTicksDelta) {
            $reasons += ("cpu_time_advanced_ms={0}" -f [int64]($cpuDelta / 10000))
        }
        if ($ioDelta -ge $MinIoBytesDelta) {
            $reasons += ("io_bytes_advanced={0}" -f [int64]$ioDelta)
        }
    }
    if ($Current.VaultReadable -and $Previous.VaultReadable -and ($Current.VaultKey -ne $Previous.VaultKey)) {
        $reasons += 'vault_physical_state_advanced'
    }
    return [pscustomobject]@{
        Advanced = ($reasons.Count -gt 0)
        PidsChanged = $pidsChanged
        Reasons = @($reasons)
        Detail = $(if ($reasons.Count -gt 0) { $reasons -join ',' } else { 'no_forward_progress' })
    }
}

function New-SynapseDaemonStartupWatchdog {
    param(
        [Parameter(Mandatory=$true)][ValidateSet('candidate','install','rollback')][string]$Phase,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [AllowNull()][string]$VaultPath,
        [Parameter(Mandatory=$true)][int]$BaseTimeoutSeconds,
        [Parameter(Mandatory=$true)][int]$MaxSeconds,
        [Parameter(Mandatory=$true)][int]$StallSeconds,
        [int]$SampleIntervalSeconds = 30
    )

    $startedAt = Get-Date
    $effectiveMax = [Math]::Max($MaxSeconds, $BaseTimeoutSeconds)
    $effectiveVault = $VaultPath
    if ([string]::IsNullOrWhiteSpace($effectiveVault)) { $effectiveVault = $DbPath }
    return [pscustomobject]@{
        Phase = $Phase
        LogDir = $LogDir
        Bind = $Bind
        DbPath = $DbPath
        VaultPath = $effectiveVault
        BaseTimeoutSeconds = $BaseTimeoutSeconds
        MaxSeconds = $effectiveMax
        StallSeconds = $StallSeconds
        SampleIntervalSeconds = $SampleIntervalSeconds
        StartedAt = $startedAt
        SinceUtc = $startedAt.ToUniversalTime().AddSeconds(-5)
        NextSampleAt = $startedAt
        SampleCount = 0
        PreviousSample = $null
        LastSample = $null
        LastProgressAt = $startedAt
        LastProgressReason = 'watchdog_armed'
        ProgressEventCount = 0
        EverSawProcess = $false
        ConsecutiveNoProcessSamples = 0
        RestartCount = 0
        TerminalFailureLine = $null
        Verdict = 'continue'
    }
}

function Format-SynapseDaemonStartupWatchdogState {
    param([Parameter(Mandatory=$true)]$Watchdog)

    $now = Get-Date
    $sample = $Watchdog.LastSample
    return ("watchdog_phase={0} verdict={1} elapsed_s={2} base_timeout_s={3} absolute_cap_s={4} stall_window_s={5} samples={6} progress_events={7} last_progress_at={8} last_progress_age_s={9} last_progress_reason={10} process_count={11} pids={12} ever_saw_process={13} restart_count={14} counters_readable={15} counter_detail={16} vault_readable={17} vault_key={18} startup_log_phase={19} terminal_failure_line={20}" -f `
        $Watchdog.Phase,
        $Watchdog.Verdict,
        [int](($now - $Watchdog.StartedAt).TotalSeconds),
        $Watchdog.BaseTimeoutSeconds,
        $Watchdog.MaxSeconds,
        $Watchdog.StallSeconds,
        $Watchdog.SampleCount,
        $Watchdog.ProgressEventCount,
        $Watchdog.LastProgressAt.ToString('o'),
        [int](($now - $Watchdog.LastProgressAt).TotalSeconds),
        $Watchdog.LastProgressReason,
        $(if ($sample) { $sample.ProcessCount } else { '<unsampled>' }),
        $(if ($sample -and @($sample.ProcessIds).Count -gt 0) { (@($sample.ProcessIds) -join ',') } else { '<none>' }),
        $Watchdog.EverSawProcess,
        $Watchdog.RestartCount,
        $(if ($sample) { $sample.CountersReadable } else { '<unsampled>' }),
        $(if ($sample) { $sample.CounterDetail } else { '<unsampled>' }),
        $(if ($sample) { $sample.VaultReadable } else { '<unsampled>' }),
        $(if ($sample -and $sample.VaultKey) { $sample.VaultKey } else { '<none>' }),
        $(if ($sample) { $sample.LogPhase } else { '<unsampled>' }),
        $(if ([string]::IsNullOrWhiteSpace($Watchdog.TerminalFailureLine)) { '<none>' } else { $Watchdog.TerminalFailureLine }))
}

function Get-SynapseDaemonStartupWatchdogRemediation {
    param(
        [Parameter(Mandatory=$true)][string]$Verdict,
        [Parameter(Mandatory=$true)][string]$Phase
    )

    switch ($Verdict) {
        'terminal_failure' {
            return "the $Phase daemon logged a terminal startup failure attributed to this bind/db; the quoted terminal_failure_line names the exact cause (storage/Calyx open, vault lock holder, or HTTP bind). Fix that cause - waiting longer cannot help."
        }
        'daemon_not_running' {
            return "no $Phase daemon process for this bind/db was alive across two consecutive 30s samples after it had been seen running; the daemon exited instead of finishing startup. Inspect daemon-stderr-gen*.log for the exit and the launcher log for relaunch attempts."
        }
        'daemon_restart_loop' {
            return "the $Phase daemon process id changed repeatedly during the startup gate, i.e. it is crash-looping under the supervisor rather than opening. Inspect daemon-stderr-gen*.log for the crash and daemon-supervisor-events.jsonl for the relaunch cadence."
        }
        'stalled' {
            return "the $Phase daemon process is alive but made NO forward progress for the whole stall window: no new startup log phase, no CPU time burned, no disk bytes transferred and no vault file mutation. That is a hang, not a slow open. Capture a stack dump of the listed pid(s) and inspect the vault lock holder; do NOT raise the budget."
        }
        'absolute_cap' {
            return "the $Phase daemon was STILL MAKING FORWARD PROGRESS when the absolute ceiling expired, so this is a budget verdict and not a dead daemon. Let the daemon finish its open, then rerun setup; if this vault legitimately needs longer, rerun with a larger -InstallHealthMaxSeconds justified by the recorded last_progress_reason."
        }
        'progress_unobservable' {
            return "setup could not READ any progress evidence for the $Phase daemon (Win32_Process counters unreadable, vault directory unreadable and no startup log line). Setup fails closed rather than granting an unjustified extension. Fix the observability first: confirm the vault path is readable and that this session can query Win32_Process for synapse-mcp.exe."
        }
        'daemon_identity_mismatch' {
            return "the $Phase daemon answered /health but is not the binary/arguments/supervisor child setup just installed; the quoted terminal_identity_failure names each mismatching field. A foreign daemon owns the bind - stop it and rerun setup."
        }
        'manual_probe_rejected_ready_daemon' {
            return "-ManualInstallHealthRollbackProbe deliberately rejected a critical-ready daemon to exercise the rollback path; this is the requested probe outcome, not a daemon defect."
        }
        'manual_probe_bridge_ack_absolute_cap' {
            return "-ManualInstallHealthRollbackProbe with pause mode require_active_ack never saw an active Chrome bridge host before the absolute ceiling; attach a real bridge host before rerunning the probe."
        }
        default {
            return "unclassified $Phase startup watchdog verdict '$Verdict'; treat as a setup defect and report it with the full watchdog block."
        }
    }
}

function Update-SynapseDaemonStartupWatchdog {
    param([Parameter(Mandatory=$true)]$Watchdog)

    $now = Get-Date
    if ($now -ge $Watchdog.NextSampleAt) {
        $signal = Get-SynapseDaemonStartupLogSignal -LogDir $Watchdog.LogDir -SinceUtc $Watchdog.SinceUtc
        $processSnapshot = @(Get-SynapseMcpProcessSnapshot)
        $targets = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $processSnapshot -Bind $Watchdog.Bind -DbPath $Watchdog.DbPath)
        $sample = Get-SynapseDaemonStartupProgressSample `
            -StartupLogSignal $signal `
            -CandidateProcesses $targets `
            -VaultPath $Watchdog.VaultPath
        $Watchdog.SampleCount = $Watchdog.SampleCount + 1
        $comparison = Compare-SynapseDaemonStartupProgressSample -Previous $Watchdog.PreviousSample -Current $sample
        if ($sample.ProcessCount -gt 0) {
            $Watchdog.EverSawProcess = $true
            $Watchdog.ConsecutiveNoProcessSamples = 0
        } else {
            $Watchdog.ConsecutiveNoProcessSamples = $Watchdog.ConsecutiveNoProcessSamples + 1
        }
        if ($comparison.PidsChanged -and $Watchdog.PreviousSample -and $Watchdog.PreviousSample.ProcessCount -gt 0 -and $sample.ProcessCount -gt 0) {
            $Watchdog.RestartCount = $Watchdog.RestartCount + 1
            Info ("WARN: SYNAPSE_STARTUP_WATCHDOG_PROCESS_IDENTITY_CHANGED phase={0} restart_count={1} previous_pids={2} current_pids={3} remediation=a daemon restart during the startup gate means the daemon exited; two restarts fail the gate closed" -f `
                $Watchdog.Phase,
                $Watchdog.RestartCount,
                (@($Watchdog.PreviousSample.ProcessIds) -join ','),
                (@($sample.ProcessIds) -join ','))
        }
        if ($comparison.Advanced) {
            $Watchdog.LastProgressAt = $sample.SampledAt
            $Watchdog.LastProgressReason = $comparison.Detail
            $Watchdog.ProgressEventCount = $Watchdog.ProgressEventCount + 1
        }
        $terminalLine = Get-SynapseDaemonStartupTerminalFailureLine -Signal $signal -DbPath $Watchdog.DbPath -Bind $Watchdog.Bind
        if ($terminalLine) {
            $Watchdog.TerminalFailureLine = $terminalLine
        }
        $Watchdog.PreviousSample = $sample
        $Watchdog.LastSample = $sample
        $Watchdog.NextSampleAt = $now.AddSeconds($Watchdog.SampleIntervalSeconds)
        Info ("SYNAPSE_STARTUP_WATCHDOG_SAMPLE phase={0} sample={1} elapsed_s={2} progress={3} reasons={4} base_remaining_s={5} absolute_remaining_s={6} stall_remaining_s={7} process_count={8} pids={9} log_phase={10} counters={11} vault={12}" -f `
            $Watchdog.Phase,
            $Watchdog.SampleCount,
            [int](($now - $Watchdog.StartedAt).TotalSeconds),
            $comparison.Advanced,
            $comparison.Detail,
            [int]([Math]::Max(0, $Watchdog.BaseTimeoutSeconds - ($now - $Watchdog.StartedAt).TotalSeconds)),
            [int]([Math]::Max(0, $Watchdog.MaxSeconds - ($now - $Watchdog.StartedAt).TotalSeconds)),
            [int]([Math]::Max(0, $Watchdog.StallSeconds - ($now - $Watchdog.LastProgressAt).TotalSeconds)),
            $sample.ProcessCount,
            $(if (@($sample.ProcessIds).Count -gt 0) { (@($sample.ProcessIds) -join ',') } else { '<none>' }),
            $sample.LogPhase,
            $sample.CounterDetail,
            $(if ($sample.VaultKey) { $sample.VaultKey } else { '<unreadable>' }))
    }

    $elapsedSeconds = ($now - $Watchdog.StartedAt).TotalSeconds
    $stallAgeSeconds = ($now - $Watchdog.LastProgressAt).TotalSeconds
    $verdict = 'continue'
    if (-not [string]::IsNullOrWhiteSpace($Watchdog.TerminalFailureLine)) {
        $verdict = 'terminal_failure'
    } elseif ($Watchdog.EverSawProcess -and $Watchdog.ConsecutiveNoProcessSamples -ge 2) {
        $verdict = 'daemon_not_running'
    } elseif ($Watchdog.RestartCount -ge 2) {
        $verdict = 'daemon_restart_loop'
    } elseif ($elapsedSeconds -lt $Watchdog.BaseTimeoutSeconds) {
        $verdict = 'continue'
    } elseif ($null -ne $Watchdog.LastSample -and -not $Watchdog.LastSample.Observable) {
        $verdict = 'progress_unobservable'
    } elseif ($elapsedSeconds -ge $Watchdog.MaxSeconds) {
        $verdict = 'absolute_cap'
    } elseif ($stallAgeSeconds -ge $Watchdog.StallSeconds) {
        $verdict = 'stalled'
    }
    $Watchdog.Verdict = $verdict
    return [pscustomobject]@{
        Continue = ($verdict -eq 'continue')
        Verdict = $verdict
        ElapsedSeconds = [int]$elapsedSeconds
        StallAgeSeconds = [int]$stallAgeSeconds
        MadeProgress = ($Watchdog.ProgressEventCount -gt 0)
    }
}

# ---------------------------------------------------------------------------
# Rollback verdict facts (#1816)
#
# "The rollback daemon did not answer /health inside my window" is NOT the same
# fact as "the rollback failed". On 2026-07-23 setup printed
# SYNAPSE_INSTALL_HEALTH_FAILED_ROLLBACK_FAILED while the previous binary was
# byte-for-byte restored and running under the supervisor - it was simply still
# replaying the vault, and answered /health 9 minutes after setup gave up. The
# operator therefore debugged the wrong binary for a full FSV cycle. The verdict
# below is derived from PHYSICAL facts (restored bytes hash, live process
# running those bytes, supervisor restart authority, forward progress) and only
# says ROLLBACK_FAILED when one of those physical facts is actually missing.
# ---------------------------------------------------------------------------

function Get-SynapseRollbackPhysicalFacts {
    param(
        [Parameter(Mandatory=$true)][string]$ExePath,
        [Parameter(Mandatory=$true)][string]$ExpectedSha256,
        [AllowNull()][string]$BackupPath,
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$SupervisorPath,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [AllowNull()]$Watchdog,
        [switch]$DaemonHealthy
    )

    $installedSha256 = '<missing>'
    if (Test-Path -LiteralPath $ExePath -PathType Leaf) {
        try {
            $installedSha256 = Get-SynapseFileSha256 -Path $ExePath
        } catch {
            $installedSha256 = "<hash_failed: $((($_.Exception.Message) -replace '\s+', ' ').Trim())>"
        }
    }
    $binaryRestored = ($installedSha256 -ieq $ExpectedSha256)

    $taskState = '<unregistered>'
    try {
        $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        if ($task) { $taskState = [string]$task.State }
    } catch {
        $taskState = "<task_query_failed: $((($_.Exception.Message) -replace '\s+', ' ').Trim())>"
    }

    $supervisorProcesses = @()
    try {
        $supervisorProcesses = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $SupervisorPath)
    } catch {
        $supervisorProcesses = @()
    }
    $supervisorStatePath = Join-Path $LogDir 'daemon-supervisor-current.json'
    $supervisorStateText = '<unreadable>'
    $supervisorChildPid = 0
    try {
        $supervisorStateJson = (Get-Content -Raw -LiteralPath $supervisorStatePath -ErrorAction Stop) | ConvertFrom-Json
        $supervisorStateText = [string]$supervisorStateJson.state
        if ($null -ne $supervisorStateJson.child_pid) { $supervisorChildPid = [int]$supervisorStateJson.child_pid }
    } catch {
        $supervisorStateText = '<unreadable>'
    }
    $supervisorPresent = (@($supervisorProcesses).Count -gt 0) -or ($taskState -eq 'Running')

    $daemonProcesses = @(Select-SynapseMcpDeployTargetProcesses `
        -Snapshot @(Get-SynapseMcpProcessSnapshot) `
        -Bind $Bind `
        -DbPath $DbPath)
    $daemonDetails = @()
    $daemonRunningRestoredBytes = $false
    foreach ($daemonProcess in $daemonProcesses) {
        $processExePath = [string]$daemonProcess.ExecutablePath
        $processSha256 = '<unreadable>'
        if (-not [string]::IsNullOrWhiteSpace($processExePath) -and (Test-Path -LiteralPath $processExePath -PathType Leaf)) {
            try {
                $processSha256 = Get-SynapseFileSha256 -Path $processExePath
            } catch {
                $processSha256 = '<hash_failed>'
            }
        }
        if ($processSha256 -ieq $ExpectedSha256) { $daemonRunningRestoredBytes = $true }
        $daemonDetails += ("pid={0} path={1} sha256={2} matches_rollback_bytes={3}" -f `
            $daemonProcess.ProcessId,
            $(if ([string]::IsNullOrWhiteSpace($processExePath)) { '<unknown>' } else { $processExePath }),
            $processSha256,
            ($processSha256 -ieq $ExpectedSha256))
    }
    $daemonRunning = (@($daemonProcesses).Count -gt 0)

    $watchdogVerdict = '<not-run>'
    $watchdogMadeProgress = $false
    $watchdogPhase = '<unknown>'
    $lastProgressAgeSeconds = -1
    $lastProgressReason = '<none>'
    if ($Watchdog) {
        $watchdogVerdict = [string]$Watchdog.Verdict
        $watchdogMadeProgress = ($Watchdog.ProgressEventCount -gt 0)
        $lastProgressReason = [string]$Watchdog.LastProgressReason
        $lastProgressAgeSeconds = [int](((Get-Date) - $Watchdog.LastProgressAt).TotalSeconds)
        if ($Watchdog.LastSample) { $watchdogPhase = [string]$Watchdog.LastSample.LogPhase }
    }

    if ($DaemonHealthy) {
        $verdict = 'rolled_back_daemon_healthy'
    } elseif (-not $binaryRestored) {
        $verdict = 'rollback_failed_binary_not_restored'
    } elseif (-not $daemonRunning) {
        $verdict = 'rollback_failed_daemon_not_running'
    } elseif (-not $daemonRunningRestoredBytes) {
        $verdict = 'rollback_failed_daemon_running_foreign_bytes'
    } elseif ($watchdogVerdict -in @('stalled', 'terminal_failure', 'daemon_restart_loop', 'progress_unobservable')) {
        $verdict = 'rollback_failed_daemon_not_progressing'
    } else {
        $verdict = 'rolled_back_daemon_recovering'
    }

    return [pscustomobject]@{
        Verdict = $verdict
        RollbackSucceeded = ($verdict -eq 'rolled_back_daemon_healthy' -or $verdict -eq 'rolled_back_daemon_recovering')
        ExePath = $ExePath
        BackupPath = $(if ([string]::IsNullOrWhiteSpace($BackupPath)) { '<none>' } else { $BackupPath })
        ExpectedSha256 = $ExpectedSha256
        InstalledSha256 = $installedSha256
        BinaryRestored = $binaryRestored
        TaskName = $TaskName
        TaskState = $taskState
        SupervisorPath = $SupervisorPath
        SupervisorProcessCount = @($supervisorProcesses).Count
        SupervisorState = $supervisorStateText
        SupervisorChildPid = $supervisorChildPid
        SupervisorPresent = $supervisorPresent
        DaemonRunning = $daemonRunning
        DaemonRunningRestoredBytes = $daemonRunningRestoredBytes
        DaemonDetail = $(if ($daemonDetails.Count -gt 0) { $daemonDetails -join ' | ' } else { '<none>' })
        WatchdogVerdict = $watchdogVerdict
        WatchdogMadeProgress = $watchdogMadeProgress
        StartupLogPhase = $watchdogPhase
        LastProgressReason = $lastProgressReason
        LastProgressAgeSeconds = $lastProgressAgeSeconds
    }
}

function Format-SynapseRollbackPhysicalFacts {
    param([Parameter(Mandatory=$true)]$Facts)

    return ("rollback_verdict={0} rollback_succeeded={1} binary_restored={2} install_path={3} expected_sha256={4} installed_sha256={5} backup={6} task={7} task_state={8} supervisor_present={9} supervisor_process_count={10} supervisor_state={11} supervisor_child_pid={12} daemon_running={13} daemon_running_restored_bytes={14} daemon_processes=[{15}] startup_watchdog_verdict={16} startup_log_phase={17} made_forward_progress={18} last_progress_reason={19} last_progress_age_s={20}" -f `
        $Facts.Verdict,
        $Facts.RollbackSucceeded,
        $Facts.BinaryRestored,
        $Facts.ExePath,
        $Facts.ExpectedSha256,
        $Facts.InstalledSha256,
        $Facts.BackupPath,
        $Facts.TaskName,
        $Facts.TaskState,
        $Facts.SupervisorPresent,
        $Facts.SupervisorProcessCount,
        $Facts.SupervisorState,
        $Facts.SupervisorChildPid,
        $Facts.DaemonRunning,
        $Facts.DaemonRunningRestoredBytes,
        $Facts.DaemonDetail,
        $Facts.WatchdogVerdict,
        $Facts.StartupLogPhase,
        $Facts.WatchdogMadeProgress,
        $Facts.LastProgressReason,
        $Facts.LastProgressAgeSeconds)
}

function Get-SynapseRollbackVerdictRemediation {
    param([Parameter(Mandatory=$true)]$Facts)

    switch ($Facts.Verdict) {
        'rollback_failed_binary_not_restored' {
            return "ROLLBACK FAILED: $($Facts.ExePath) does NOT contain the previous bytes (expected $($Facts.ExpectedSha256), found $($Facts.InstalledSha256)). Restore it by hand from $($Facts.BackupPath), verify the sha256, then start task $($Facts.TaskName) before doing anything else."
        }
        'rollback_failed_daemon_not_running' {
            return "ROLLBACK FAILED: the previous bytes were restored to $($Facts.ExePath) but NO daemon process for this bind/db is running them, so nothing will serve /mcp. Start task $($Facts.TaskName) and inspect daemon-stderr-gen*.log plus daemon-launcher.log for why the relaunch died."
        }
        'rollback_failed_daemon_running_foreign_bytes' {
            return "ROLLBACK FAILED: a daemon for this bind/db is running, but none of its processes execute the restored bytes ($($Facts.ExpectedSha256)). Identify the listed process paths, stop the foreign daemon, and restart the supervisor task so the restored binary owns the bind."
        }
        'rollback_failed_daemon_not_progressing' {
            return "ROLLBACK FAILED: the restored binary is installed and running, but its startup made no forward progress (watchdog verdict $($Facts.WatchdogVerdict), last progress $($Facts.LastProgressAgeSeconds)s ago at phase $($Facts.StartupLogPhase)). The previous daemon is hung, not slow - capture a stack dump of the listed pid(s) and inspect the vault lock holder before restarting."
        }
        'rolled_back_daemon_recovering' {
            return "ROLLBACK SUCCEEDED - the candidate binary is NOT installed. $($Facts.ExePath) holds the previous bytes ($($Facts.ExpectedSha256), hash-verified), a daemon process is running those exact bytes, supervisor restart authority is present=$($Facts.SupervisorPresent), and the daemon is STILL OPENING its Calyx vault (phase $($Facts.StartupLogPhase), forward progress $($Facts.LastProgressAgeSeconds)s ago: $($Facts.LastProgressReason)). Do NOT debug the candidate binary - it was never left installed. Wait for /health to answer, then diagnose the CANDIDATE from its own startup logs before retrying the deploy."
        }
        'rolled_back_daemon_healthy' {
            return "ROLLBACK SUCCEEDED and the previous daemon is serving /health again; diagnose the candidate from its own startup logs before retrying the deploy."
        }
        default {
            return "unclassified rollback verdict '$($Facts.Verdict)'; treat as a setup defect and report it with the full rollback fact block."
        }
    }
}

function Get-SynapseProtectedProcessNames {
    return @(
        'cmd.exe',
        'powershell.exe',
        'pwsh.exe',
        'WindowsTerminal.exe',
        'OpenConsole.exe',
        'conhost.exe',
        'wsl.exe',
        'wslhost.exe',
        'Code.exe',
        'codex.exe',
        'claude.exe',
        'node.exe'
    )
}

function Get-SynapseTcpClientPeerCloseDecision {
    param(
        [Parameter(Mandatory=$true)]$TcpClient
    )

    if (-not $TcpClient.HasLivePeer -or [int]$TcpClient.PeerOwningProcess -le 0) {
        return [pscustomobject]@{
            CanClose = $false
            Reason = 'no_live_peer'
            PeerProcess = $null
            Kind = $null
        }
    }

    $peerPid = [int]$TcpClient.PeerOwningProcess
    $peerProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$peerPid" -ErrorAction SilentlyContinue
    if (-not $peerProcess) {
        return [pscustomobject]@{
            CanClose = $false
            Reason = 'peer_exited'
            PeerProcess = $null
            Kind = $null
        }
    }

    $protectedNames = Get-SynapseProtectedProcessNames
    if ($protectedNames -contains $peerProcess.Name) {
        return [pscustomobject]@{
            CanClose = $false
            Reason = "protected_process:$($peerProcess.Name)"
            PeerProcess = $peerProcess
            Kind = $null
        }
    }

    $commandLine = [string]$peerProcess.CommandLine
    $isChromeNetworkService = (
        $peerProcess.Name -ieq 'chrome.exe' -and
        $commandLine -match '(?i)--type=utility' -and
        $commandLine -match '(?i)--utility-sub-type=network\.mojom\.NetworkService'
    )
    if ($isChromeNetworkService) {
        return [pscustomobject]@{
            CanClose = $true
            Reason = 'exact_chrome_network_service_peer'
            PeerProcess = $peerProcess
            Kind = 'chrome_network_service'
        }
    }

    return [pscustomobject]@{
        CanClose = $false
        Reason = "unowned_peer:$($peerProcess.Name)"
        PeerProcess = $peerProcess
        Kind = $null
    }
}

function Stop-SynapseStaleBindClientPeersForMaintenance {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][object[]]$TcpClients
    )

    $liveClients = @($TcpClients | Where-Object { $_.HasLivePeer -and [int]$_.PeerOwningProcess -gt 0 })
    if ($liveClients.Count -eq 0) {
        return [pscustomobject]@{
            ClosedCount = 0
            ClosedPeerPids = @()
            RefusedPeerPids = @()
        }
    }

    $decisions = @($liveClients | ForEach-Object {
        $decision = Get-SynapseTcpClientPeerCloseDecision -TcpClient $_
        [pscustomobject]@{
            TcpClient = $_
            CanClose = $decision.CanClose
            Reason = $decision.Reason
            PeerProcess = $decision.PeerProcess
            Kind = $decision.Kind
        }
    })
    $closable = @($decisions | Where-Object { $_.CanClose })
    $refused = @($decisions | Where-Object { -not $_.CanClose })
    $refusedPeerPids = @($refused | ForEach-Object { [int]$_.TcpClient.PeerOwningProcess } | Where-Object { $_ -gt 0 } | Sort-Object -Unique)

    if ($refused.Count -gt 0) {
        $refusedDetail = (($refused | ForEach-Object {
            $peer = $_.PeerProcess
            $peerPid = if ($peer) { [int]$peer.ProcessId } else { [int]$_.TcpClient.PeerOwningProcess }
            $peerName = if ($peer) { $peer.Name } else { '<missing>' }
            $peerCommandLine = if ($peer) { $peer.CommandLine } else { '<missing>' }
            "peer_pid=$peerPid peer=$peerName reason=$($_.Reason) tcp=local:$($_.TcpClient.LocalAddress):$($_.TcpClient.LocalPort)->remote:$($_.TcpClient.RemoteAddress):$($_.TcpClient.RemotePort) peer_cmd=$peerCommandLine"
        }) -join "`n")
        Info ("FORCE_RESTART: SYNAPSE_FORCE_RESTART_TCP_PEER_CLOSE_REFUSED reason={0} bind={1} refused_count={2}`nrefused:`n{3}`nremediation=setup only closes exact known non-terminal client peers that are safe to restart, such as Chrome NetworkService. Protected terminal/IDE/WSL/Codex/Claude/Node peers are left running and the bind must release naturally or setup fails closed." -f `
            $Reason,
            $Bind,
            $refused.Count,
            $refusedDetail)
    }

    if ($closable.Count -eq 0) {
        return [pscustomobject]@{
            ClosedCount = 0
            ClosedPeerPids = @()
            RefusedPeerPids = $refusedPeerPids
        }
    }

    $closedPeerPids = @()
    foreach ($peerGroup in ($closable | Group-Object { [int]$_.PeerProcess.ProcessId })) {
        $first = $peerGroup.Group[0]
        $peer = $first.PeerProcess
        $peerPid = [int]$peer.ProcessId
        $peerCommandLine = [string]$peer.CommandLine
        $tcpDetail = (($peerGroup.Group | ForEach-Object {
            "local:$($_.TcpClient.LocalAddress):$($_.TcpClient.LocalPort)->remote:$($_.TcpClient.RemoteAddress):$($_.TcpClient.RemotePort)"
        }) -join ',')
        Info ("FORCE_RESTART: SYNAPSE_FORCE_RESTART_CLOSE_TCP_PEER reason={0} bind={1} peer_pid={2} peer={3} kind={4} tcp={5} peer_cmd={6}`nremediation=Windows kept a dead-owner Synapse listener row because this exact live client peer still owned a socket to the stopped daemon. Closing this exact Chrome NetworkService process lets Chrome restart networking without closing the browser profile, then setup separately re-probes the bind before starting a daemon." -f `
            $Reason,
            $Bind,
            $peerPid,
            $peer.Name,
            $first.Kind,
            $tcpDetail,
            $peerCommandLine)
        try {
            Stop-Process -Id $peerPid -Force -ErrorAction Stop
            $closedPeerPids += $peerPid
        } catch {
            Die ("SYNAPSE_FORCE_RESTART_TCP_PEER_CLOSE_FAILED reason={0} bind={1} peer_pid={2} peer={3} error={4} remediation=setup could not close the exact live client peer that is holding the dead-owner daemon socket; inspect the peer process and rerun setup after it exits" -f `
                $Reason,
                $Bind,
                $peerPid,
                $peer.Name,
                $_.Exception.Message)
        }
    }

    Start-Sleep -Seconds 2
    foreach ($closedPid in $closedPeerPids) {
        $after = Get-CimInstance Win32_Process -Filter "ProcessId=$closedPid" -ErrorAction SilentlyContinue
        if ($after) {
            Die ("SYNAPSE_FORCE_RESTART_TCP_PEER_STILL_RUNNING reason={0} bind={1} peer_pid={2} peer={3} command_line={4} remediation=exact client peer did not exit after Stop-Process; inspect it before retrying setup" -f `
                $Reason,
                $Bind,
                $closedPid,
                $after.Name,
                $after.CommandLine)
        }
    }

    Info ("FORCE_RESTART: SYNAPSE_FORCE_RESTART_TCP_PEER_CLOSE_VERIFIED reason={0} bind={1} closed_peer_pids={2}" -f `
        $Reason,
        $Bind,
        ($closedPeerPids -join ','))
    return [pscustomobject]@{
        ClosedCount = $closedPeerPids.Count
        ClosedPeerPids = @($closedPeerPids)
        RefusedPeerPids = $refusedPeerPids
    }
}

function Wait-SynapseBindReleased {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [int]$TimeoutSeconds = 15,
        [switch]$ForceRestart
    )

    if ($script:SynapseBindPostExitContinuationRequired) {
        $detail = $script:SynapseBindPostExitContinuationDetail
        $detailReason = if ($detail -and $detail.reason) { [string]$detail.reason } else { '<unknown>' }
        Info ("SYNAPSE_BIND_POST_EXIT_CONTINUATION_ALREADY_REQUIRED reason={0} bind={1} original_reason={2} remediation=the verified daemon bytes will be installed and a post-exit continuation will reacquire the maintenance lock after this setup process exits; skipping duplicate bind-drain wait inside the same process." -f `
            $Reason,
            $Bind,
            $detailReason)
        return
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $lastDeadOwnerLog = [DateTime]::MinValue
    $lastSafePeerCloseDeferredLog = [DateTime]::MinValue
    $closedForceRestartPeerPids = @{}
    $refusedForceRestartPeerPids = @{}
    $deferredForceRestartPeerPids = @{}
    $maxForceRestartPeerClosePids = 5
    do {
        Assert-SynapseChromeBridgeMaintenancePauseBudget -Reason $Reason -Bind $Bind -Phase 'initial_bind_wait'
        $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        $probe = Test-SynapseBindAvailable -Bind $Bind
        if ($listeners.Count -eq 0 -and $probe.Ok) {
            Info "Synapse bind release verified reason=$Reason bind=$Bind listener_count=0 bind_probe=ok"
            return
        }
        $liveListeners = @($listeners | Where-Object { $_.OwnerExists })
        $staleListeners = @($listeners | Where-Object { -not $_.OwnerExists })
        if ($liveListeners.Count -eq 0 -and $staleListeners.Count -gt 0 -and $probe.Ok) {
            Info ("Synapse bind release accepted stale dead-owner listener rows reason={0} bind={1} timeout_s={2} stale_listener_count={3} bind_probe=ok`nstale_listeners:`n{4}`nremediation=Windows can report LISTEN rows briefly after the owning process exits; setup verified a new listener can bind before continuing." -f `
                $Reason,
                $Bind,
                $TimeoutSeconds,
                $staleListeners.Count,
                (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners))
            return
        }
        if ($liveListeners.Count -eq 0 -and -not $probe.Ok -and (((Get-Date) - $lastDeadOwnerLog).TotalSeconds -ge 10)) {
            $lastDeadOwnerLog = Get-Date
            Info ("Synapse bind release waiting on Windows dead-owner TCP drain reason={0} bind={1} listener_count={2} stale_listener_count={3} bind_probe_error={4}`nstale_listeners:`n{5}`nremediation=setup will not reuse or steal the port; it waits until a normal bind probe proves the address is actually reusable." -f `
                $Reason,
                $Bind,
                $listeners.Count,
                $staleListeners.Count,
                $probe.Error,
                (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners))
        }
        if ($ForceRestart -and $liveListeners.Count -eq 0 -and $staleListeners.Count -gt 0 -and -not $probe.Ok) {
            $tcpClients = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
            $liveTcpClients = @($tcpClients | Where-Object { $_.HasLivePeer -and [int]$_.PeerOwningProcess -gt 0 })
            $unclosableLiveTcpClients = @($liveTcpClients | Where-Object {
                $decision = Get-SynapseTcpClientPeerCloseDecision -TcpClient $_
                -not $decision.CanClose
            })
            if ($unclosableLiveTcpClients.Count -gt 0) {
                if (((Get-Date) - $lastSafePeerCloseDeferredLog).TotalSeconds -ge 10) {
                    $lastSafePeerCloseDeferredLog = Get-Date
                    Info ("Synapse bind release safe-peer close deferred reason={0} bind={1} unclosable_live_peer_count={2} live_peer_count={3}`ntcp_clients:`n{4}`nremediation=setup will not churn restartable peers while protected/unowned clients still hold the dead-owner daemon socket; it waits for those clients to release naturally, then may close exact safe peers if needed." -f `
                        $Reason,
                        $Bind,
                        $unclosableLiveTcpClients.Count,
                        $liveTcpClients.Count,
                        (Format-SynapseTcpClientSnapshot -Snapshot $liveTcpClients))
                }
            } else {
            $newLiveTcpClients = @($liveTcpClients | Where-Object {
                $peerPidKey = [string][int]$_.PeerOwningProcess
                -not $closedForceRestartPeerPids.ContainsKey($peerPidKey) -and -not $refusedForceRestartPeerPids.ContainsKey($peerPidKey) -and -not $deferredForceRestartPeerPids.ContainsKey($peerPidKey)
            })
            if ($newLiveTcpClients.Count -gt 0) {
                $newPeerPids = @($newLiveTcpClients | ForEach-Object { [int]$_.PeerOwningProcess } | Sort-Object -Unique)
                $newClosablePeerPids = @($newLiveTcpClients | Where-Object {
                    $decision = Get-SynapseTcpClientPeerCloseDecision -TcpClient $_
                    $decision.CanClose
                } | ForEach-Object { [int]$_.PeerOwningProcess } | Sort-Object -Unique)
                if (($closedForceRestartPeerPids.Count + $newClosablePeerPids.Count) -gt $maxForceRestartPeerClosePids) {
                    foreach ($peerPid in @($newClosablePeerPids)) {
                        $deferredForceRestartPeerPids[[string]$peerPid] = $true
                    }
                    Info ("SYNAPSE_FORCE_RESTART_TCP_PEER_CLOSE_LIMIT_REACHED_WAITING reason={0} bind={1} max_peer_pids={2} already_closed_peer_pids={3} deferred_peer_pids={4}`ntcp_clients:`n{5}`nremediation=Chrome NetworkService can restart faster than Windows releases the stopped daemon socket. Setup stops closing additional safe peers at the hard cap and continues the bounded dead-owner bind drain; final success still requires a normal bind probe, and final failure reports the remaining physical TCP/process SoT." -f `
                        $Reason,
                        $Bind,
                        $maxForceRestartPeerClosePids,
                        (($closedForceRestartPeerPids.Keys | Sort-Object) -join ','),
                        ($newClosablePeerPids -join ','),
                        (Format-SynapseTcpClientSnapshot -Snapshot $liveTcpClients))
                    continue
                }
                $peerClose = Stop-SynapseStaleBindClientPeersForMaintenance -Reason $Reason -Bind $Bind -TcpClients $newLiveTcpClients
                foreach ($peerPid in @($peerClose.ClosedPeerPids)) {
                    $closedForceRestartPeerPids[[string]$peerPid] = $true
                }
                foreach ($peerPid in @($peerClose.RefusedPeerPids)) {
                    $refusedForceRestartPeerPids[[string]$peerPid] = $true
                }
                if ([int]$peerClose.ClosedCount -gt 0) {
                    continue
                }
            }
            }
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)

    $extendedDeadline = (Get-Date).AddSeconds([Math]::Max(300, $TimeoutSeconds))
    $enteredExtendedDrain = $false
    while ((Get-Date) -lt $extendedDeadline) {
        Assert-SynapseChromeBridgeMaintenancePauseBudget -Reason $Reason -Bind $Bind -Phase 'extended_dead_owner_drain'
        $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        $probe = Test-SynapseBindAvailable -Bind $Bind
        $tcpClients = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
        $processes = @(Get-SynapseMcpProcessSnapshot)
        $liveListeners = @($listeners | Where-Object { $_.OwnerExists })
        $staleListeners = @($listeners | Where-Object { -not $_.OwnerExists })
        if ($listeners.Count -eq 0 -and $probe.Ok) {
            Info ("Synapse bind release verified after dead-owner drain reason={0} bind={1} initial_timeout_s={2} listener_count=0 bind_probe=ok tcp_client_count={3} process_count={4}" -f `
                $Reason,
                $Bind,
                $TimeoutSeconds,
                $tcpClients.Count,
                $processes.Count)
            return
        }
        if ($liveListeners.Count -eq 0 -and $staleListeners.Count -gt 0 -and $probe.Ok) {
            Info ("Synapse bind release accepted stale dead-owner listener rows after drain reason={0} bind={1} initial_timeout_s={2} stale_listener_count={3} tcp_client_count={4} process_count={5} bind_probe=ok`nstale_listeners:`n{6}`ntcp_clients:`n{7}`nremediation=Windows still reports stale LISTEN rows, but setup separately proved a normal listener can bind." -f `
                $Reason,
                $Bind,
                $TimeoutSeconds,
                $staleListeners.Count,
                $tcpClients.Count,
                $processes.Count,
                (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners),
                (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients))
            return
        }
        if ($liveListeners.Count -gt 0 -or $processes.Count -gt 0) {
            break
        }
        if ($ForceRestart -and $staleListeners.Count -gt 0 -and -not $probe.Ok -and $tcpClients.Count -gt 0) {
            $liveTcpClients = @($tcpClients | Where-Object { $_.HasLivePeer -and [int]$_.PeerOwningProcess -gt 0 })
            if ($liveTcpClients.Count -eq 0) {
                if (-not $enteredExtendedDrain -or (((Get-Date) - $lastDeadOwnerLog).TotalSeconds -ge 10)) {
                    Info ("Synapse bind release live-peer close not attempted yet reason={0} bind={1} tcp_client_count={2} live_peer_count=0 remediation=setup will keep waiting; TIME_WAIT or ownerless rows do not consume the exact live-peer close attempt." -f `
                        $Reason,
                        $Bind,
                        $tcpClients.Count)
                }
            } else {
                $unclosableLiveTcpClients = @($liveTcpClients | Where-Object {
                    $decision = Get-SynapseTcpClientPeerCloseDecision -TcpClient $_
                    -not $decision.CanClose
                })
                if ($unclosableLiveTcpClients.Count -gt 0) {
                    $codexPinnedTcpClients = @(Get-SynapseCodexPeerRows -TcpClients $unclosableLiveTcpClients)
                    if ($codexPinnedTcpClients.Count -gt 0 -and $processes.Count -eq 0 -and $staleListeners.Count -gt 0 -and -not $probe.Ok) {
                        $codexPeerPids = @($codexPinnedTcpClients |
                            ForEach-Object { [int]$_.PeerOwningProcess } |
                            Sort-Object -Unique)
                        $script:SynapseBindPostExitContinuationRequired = $true
                        $script:SynapseBindPostExitContinuationDetail = [ordered]@{
                            schema = 'synapse_bind_post_exit_continuation_required/v1'
                            reason = $Reason
                            bind = $Bind
                            timeout_s = $TimeoutSeconds
                            phase = 'protected_codex_dead_owner_bind'
                            stale_listener_count = $staleListeners.Count
                            codex_peer_pids = @($codexPeerPids)
                            bind_probe_ok = $probe.Ok
                            bind_probe_error = $probe.Error
                            observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
                            stale_listeners = @($staleListeners)
                            tcp_clients = @($tcpClients)
                            protected_codex_tcp_clients = @($codexPinnedTcpClients)
                            processes = @($processes)
                            remediation = 'the stopped daemon has no live owner, but Windows still exposes a dead-owner listener pinned by protected Codex MCP peers inside this setup process; install the verified bytes, then start a post-exit continuation after this runner exits instead of killing Codex or terminal/IDE/WSL hosts'
                        }
                        Info ("SYNAPSE_BIND_POST_EXIT_CONTINUATION_REQUIRED reason={0} bind={1} phase=protected_codex_dead_owner_bind codex_peer_pids={2} stale_listener_count={3} bind_probe_error={4}`nstale_listeners:`n{5}`ntcp_clients:`n{6}`nremediation=setup will install the verified daemon bytes and then start a hidden post-exit continuation after the current runner exits; it will not kill protected Codex, terminal, IDE, or WSL host processes." -f `
                            $Reason,
                            $Bind,
                            ($codexPeerPids -join ','),
                            $staleListeners.Count,
                            $probe.Error,
                            (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners),
                            (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients))
                        return
                    }
                    if (-not $enteredExtendedDrain -or (((Get-Date) - $lastSafePeerCloseDeferredLog).TotalSeconds -ge 10)) {
                        $lastSafePeerCloseDeferredLog = Get-Date
                        Info ("Synapse bind release safe-peer close deferred reason={0} bind={1} unclosable_live_peer_count={2} live_peer_count={3}`ntcp_clients:`n{4}`nremediation=setup will not churn restartable peers while protected/unowned clients still hold the dead-owner daemon socket; it waits for those clients to release naturally, then may close exact safe peers if needed." -f `
                            $Reason,
                            $Bind,
                            $unclosableLiveTcpClients.Count,
                            $liveTcpClients.Count,
                            (Format-SynapseTcpClientSnapshot -Snapshot $liveTcpClients))
                    }
                } else {
                $newLiveTcpClients = @($liveTcpClients | Where-Object {
                    $peerPidKey = [string][int]$_.PeerOwningProcess
                    -not $closedForceRestartPeerPids.ContainsKey($peerPidKey) -and -not $refusedForceRestartPeerPids.ContainsKey($peerPidKey) -and -not $deferredForceRestartPeerPids.ContainsKey($peerPidKey)
                })
                if ($newLiveTcpClients.Count -eq 0) {
                    if (-not $enteredExtendedDrain -or (((Get-Date) - $lastDeadOwnerLog).TotalSeconds -ge 10)) {
                        Info ("Synapse bind release live-peer close already classified reason={0} bind={1} live_peer_count={2} closed_peer_pids={3} refused_peer_pids={4} remediation=setup will keep waiting for Windows to release the socket after exact safe-peer close attempts and protected-peer refusals already completed." -f `
                            $Reason,
                            $Bind,
                            $liveTcpClients.Count,
                            (($closedForceRestartPeerPids.Keys | Sort-Object) -join ','),
                            (($refusedForceRestartPeerPids.Keys | Sort-Object) -join ',') + "$(if ($deferredForceRestartPeerPids.Count -gt 0) { '; deferred_peer_pids=' + (($deferredForceRestartPeerPids.Keys | Sort-Object) -join ',') } else { '' })")
                    }
                } else {
                    $newPeerPids = @($newLiveTcpClients | ForEach-Object { [int]$_.PeerOwningProcess } | Sort-Object -Unique)
                    $newClosablePeerPids = @($newLiveTcpClients | Where-Object {
                        $decision = Get-SynapseTcpClientPeerCloseDecision -TcpClient $_
                        $decision.CanClose
                    } | ForEach-Object { [int]$_.PeerOwningProcess } | Sort-Object -Unique)
                    if (($closedForceRestartPeerPids.Count + $newClosablePeerPids.Count) -gt $maxForceRestartPeerClosePids) {
                        foreach ($peerPid in @($newClosablePeerPids)) {
                            $deferredForceRestartPeerPids[[string]$peerPid] = $true
                        }
                        Info ("SYNAPSE_FORCE_RESTART_TCP_PEER_CLOSE_LIMIT_REACHED_WAITING reason={0} bind={1} max_peer_pids={2} already_closed_peer_pids={3} deferred_peer_pids={4}`ntcp_clients:`n{5}`nremediation=Chrome NetworkService can restart faster than Windows releases the stopped daemon socket. Setup stops closing additional safe peers at the hard cap and continues the bounded dead-owner bind drain; final success still requires a normal bind probe, and final failure reports the remaining physical TCP/process SoT." -f `
                            $Reason,
                            $Bind,
                            $maxForceRestartPeerClosePids,
                            (($closedForceRestartPeerPids.Keys | Sort-Object) -join ','),
                            ($newClosablePeerPids -join ','),
                            (Format-SynapseTcpClientSnapshot -Snapshot $liveTcpClients))
                        continue
                    }
                    $peerClose = Stop-SynapseStaleBindClientPeersForMaintenance -Reason $Reason -Bind $Bind -TcpClients $newLiveTcpClients
                    foreach ($peerPid in @($peerClose.ClosedPeerPids)) {
                        $closedForceRestartPeerPids[[string]$peerPid] = $true
                    }
                    foreach ($peerPid in @($peerClose.RefusedPeerPids)) {
                        $refusedForceRestartPeerPids[[string]$peerPid] = $true
                    }
                    if ([int]$peerClose.ClosedCount -gt 0) {
                        continue
                    }
                }
                }
            }
        }
        if (-not $enteredExtendedDrain -or (((Get-Date) - $lastDeadOwnerLog).TotalSeconds -ge 10)) {
            $enteredExtendedDrain = $true
            $lastDeadOwnerLog = Get-Date
            Info ("Synapse bind release extended dead-owner drain reason={0} bind={1} initial_timeout_s={2} listener_count={3} stale_listener_count={4} tcp_client_count={5} process_count={6} bind_probe_ok={7} bind_probe_error={8}`nstale_listeners:`n{9}`ntcp_clients:`n{10}`nremediation=setup is waiting for Windows to release dead-owner TCP rows; it will continue only after a normal bind probe succeeds." -f `
                $Reason,
                $Bind,
                $TimeoutSeconds,
                $listeners.Count,
                $staleListeners.Count,
                $tcpClients.Count,
                $processes.Count,
                $probe.Ok,
                $probe.Error,
                (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners),
                (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients))
        }
        Start-Sleep -Seconds 1
    }

    $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
    $probe = Test-SynapseBindAvailable -Bind $Bind
    $tcpClients = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
    $processes = @(Get-SynapseMcpProcessSnapshot)
    $liveListeners = @($listeners | Where-Object { $_.OwnerExists })
    $staleListeners = @($listeners | Where-Object { -not $_.OwnerExists })
    if ($liveListeners.Count -eq 0 -and $staleListeners.Count -gt 0 -and $probe.Ok) {
        Info ("Synapse bind release accepted stale dead-owner listener rows reason={0} bind={1} timeout_s={2} stale_listener_count={3} tcp_client_count={4} process_count={5} bind_probe=ok`nstale_listeners:`n{6}`ntcp_clients:`n{7}`nremediation=Windows can report LISTEN rows briefly after the owning process exits; setup verified a new listener can bind before continuing." -f `
            $Reason,
            $Bind,
            $TimeoutSeconds,
            $staleListeners.Count,
            $tcpClients.Count,
            $processes.Count,
            (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners),
            (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients))
        return
    }
    if ($ForceRestart -and $liveListeners.Count -eq 0 -and $staleListeners.Count -gt 0 -and $processes.Count -eq 0 -and $tcpClients.Count -eq 0 -and -not $probe.Ok) {
        Info ("SYNAPSE_BIND_FINAL_DEAD_OWNER_SETTLE reason={0} bind={1} settle_s={2} stale_listener_count={3} bind_probe_error={4}`nstale_listeners:`n{5}`nremediation=no live daemon process and no TCP peer client remain; setup performs one bounded final kernel-state settle/readback before fatal so a disappearing Windows dead-owner row is not mistaken for a live owner." -f `
            $Reason,
            $Bind,
            $SynapseBindFinalDeadOwnerSettleSeconds,
            $staleListeners.Count,
            $probe.Error,
            (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners))
        $settleDeadline = (Get-Date).AddSeconds($SynapseBindFinalDeadOwnerSettleSeconds)
        while ((Get-Date) -lt $settleDeadline) {
            Assert-SynapseChromeBridgeMaintenancePauseBudget -Reason $Reason -Bind $Bind -Phase 'final_dead_owner_settle'
            Start-Sleep -Milliseconds 250
            $settleListeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
            $settleProbe = Test-SynapseBindAvailable -Bind $Bind
            $settleTcpClients = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
            $settleProcesses = @(Get-SynapseMcpProcessSnapshot)
            $settleLiveListeners = @($settleListeners | Where-Object { $_.OwnerExists })
            $settleStaleListeners = @($settleListeners | Where-Object { -not $_.OwnerExists })
            if ($settleListeners.Count -eq 0 -and $settleProbe.Ok) {
                Info ("Synapse bind release verified after final dead-owner settle reason={0} bind={1} listener_count=0 bind_probe=ok tcp_client_count={2} process_count={3}" -f `
                    $Reason,
                    $Bind,
                    $settleTcpClients.Count,
                    $settleProcesses.Count)
                return
            }
            if ($settleLiveListeners.Count -eq 0 -and $settleStaleListeners.Count -gt 0 -and $settleProbe.Ok) {
                Info ("Synapse bind release accepted stale dead-owner listener rows after final settle reason={0} bind={1} stale_listener_count={2} tcp_client_count={3} process_count={4} bind_probe=ok`nstale_listeners:`n{5}`ntcp_clients:`n{6}`nremediation=Windows still reports stale LISTEN rows, but setup separately proved a normal listener can bind." -f `
                    $Reason,
                    $Bind,
                    $settleStaleListeners.Count,
                    $settleTcpClients.Count,
                    $settleProcesses.Count,
                    (Format-SynapseTcpBindListenerSnapshot -Snapshot $settleStaleListeners),
                    (Format-SynapseTcpClientSnapshot -Snapshot $settleTcpClients))
                return
            }
            if ($settleLiveListeners.Count -gt 0 -or $settleProcesses.Count -gt 0 -or $settleTcpClients.Count -gt 0) {
                Info ("SYNAPSE_BIND_FINAL_DEAD_OWNER_SETTLE_ABORTED reason={0} bind={1} live_listener_count={2} tcp_client_count={3} process_count={4} bind_probe_ok={5}`nlive_listeners:`n{6}`ntcp_clients:`n{7}`nprocesses:`n{8}`nremediation=a live owner/client appeared during final settle; setup will fail closed with the current physical SoT instead of assuming the stale-row case." -f `
                    $Reason,
                    $Bind,
                    $settleLiveListeners.Count,
                    $settleTcpClients.Count,
                    $settleProcesses.Count,
                    $settleProbe.Ok,
                    (Format-SynapseTcpBindListenerSnapshot -Snapshot $settleLiveListeners),
                    (Format-SynapseTcpClientSnapshot -Snapshot $settleTcpClients),
                    (Format-SynapseMcpProcessSnapshot -Snapshot $settleProcesses))
                break
            }
        }
        $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        $probe = Test-SynapseBindAvailable -Bind $Bind
        $tcpClients = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
        $processes = @(Get-SynapseMcpProcessSnapshot)
        $liveListeners = @($listeners | Where-Object { $_.OwnerExists })
        $staleListeners = @($listeners | Where-Object { -not $_.OwnerExists })
        if ($liveListeners.Count -eq 0 -and $staleListeners.Count -gt 0 -and $processes.Count -eq 0 -and $tcpClients.Count -eq 0 -and -not $probe.Ok) {
            $script:SynapseBindPostExitContinuationRequired = $true
            $script:SynapseBindPostExitContinuationDetail = [ordered]@{
                schema = 'synapse_bind_post_exit_continuation_required/v1'
                reason = $Reason
                bind = $Bind
                timeout_s = $TimeoutSeconds
                stale_listener_count = $staleListeners.Count
                bind_probe_ok = $probe.Ok
                bind_probe_error = $probe.Error
                observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
                stale_listeners = @($staleListeners)
                tcp_clients = @($tcpClients)
                processes = @($processes)
                remediation = 'the stopped daemon has no live owner and no TCP peers, but Windows keeps the dead-owner listener unavailable inside this setup process; install the verified bytes, then start a post-exit continuation that waits for this runner to release process-scoped/kernel state before daemon start'
            }
            Info ("SYNAPSE_BIND_POST_EXIT_CONTINUATION_REQUIRED reason={0} bind={1} stale_listener_count={2} bind_probe_error={3}`nstale_listeners:`n{4}`nremediation=setup will install the verified daemon bytes and then start a hidden post-exit continuation instead of starting a daemon while the bind probe still fails." -f `
                $Reason,
                $Bind,
                $staleListeners.Count,
                $probe.Error,
                (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners))
            return
        }
    }
    if ($ForceRestart -and $liveListeners.Count -eq 0 -and $staleListeners.Count -gt 0 -and $processes.Count -eq 0 -and -not $probe.Ok) {
        $liveTcpClients = @($tcpClients | Where-Object { $_.HasLivePeer -and [int]$_.PeerOwningProcess -gt 0 })
        $codexPinnedTcpClients = @(Get-SynapseCodexPeerRows -TcpClients $liveTcpClients)
        if ($codexPinnedTcpClients.Count -gt 0) {
            $codexPeerPids = @($codexPinnedTcpClients |
                ForEach-Object { [int]$_.PeerOwningProcess } |
                Sort-Object -Unique)
            $script:SynapseBindPostExitContinuationRequired = $true
            $script:SynapseBindPostExitContinuationDetail = [ordered]@{
                schema = 'synapse_bind_post_exit_continuation_required/v1'
                reason = $Reason
                bind = $Bind
                timeout_s = $TimeoutSeconds
                phase = 'protected_codex_dead_owner_bind_final'
                stale_listener_count = $staleListeners.Count
                codex_peer_pids = @($codexPeerPids)
                bind_probe_ok = $probe.Ok
                bind_probe_error = $probe.Error
                observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
                stale_listeners = @($staleListeners)
                tcp_clients = @($tcpClients)
                protected_codex_tcp_clients = @($codexPinnedTcpClients)
                processes = @($processes)
                remediation = 'the stopped daemon has no live owner, but a protected Codex MCP peer appeared only at the final dead-owner readback; install the verified bytes, then start a post-exit continuation after this runner exits instead of killing Codex or terminal/IDE/WSL hosts'
            }
            Info ("SYNAPSE_BIND_POST_EXIT_CONTINUATION_REQUIRED reason={0} bind={1} phase=protected_codex_dead_owner_bind_final codex_peer_pids={2} stale_listener_count={3} bind_probe_error={4}`nstale_listeners:`n{5}`ntcp_clients:`n{6}`nremediation=setup will install the verified daemon bytes and then start a hidden post-exit continuation after the current runner exits; it will not kill protected Codex, terminal, IDE, or WSL host processes." -f `
                $Reason,
                $Bind,
                ($codexPeerPids -join ','),
                $staleListeners.Count,
                $probe.Error,
                (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners),
                (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients))
            return
        }
    }
    Die ("SYNAPSE_BIND_STILL_LISTENING reason={0} bind={1} timeout_s={2} listener_count={3} live_listener_count={4} stale_listener_count={5} process_count={6} bind_probe_ok={7} bind_probe_error={8}`nlive_listeners:`n{9}`nstale_listeners:`n{10}`ntcp_clients:`n{11}`nprocesses:`n{12}`nremediation=the configured HTTP bind is still occupied after daemon shutdown or Windows has not released dead-owner TCP rows after the extended drain. Do not start another daemon or switch ports. Close/restart the exact live MCP client peer listed here if it owns the remaining connection, or restart the current Codex process when it is the peer; never close terminal/IDE/WSL processes globally." -f `
        $Reason,
        $Bind,
        $TimeoutSeconds,
        $listeners.Count,
        $liveListeners.Count,
        $staleListeners.Count,
        $processes.Count,
        $probe.Ok,
        $probe.Error,
        (Format-SynapseTcpBindListenerSnapshot -Snapshot $liveListeners),
        (Format-SynapseTcpBindListenerSnapshot -Snapshot $staleListeners),
        (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients),
        (Format-SynapseMcpProcessSnapshot -Snapshot $processes))
}

function Read-SynapseSetupTokenForRestartGuard {
    param([Parameter(Mandatory=$true)][string]$TokenPath)

    if (-not (Test-Path -LiteralPath $TokenPath)) {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_RESTART_GUARD_TOKEN_MISSING'; Token = $null; Detail = "path=$TokenPath" }
    }

    try {
        $raw = Get-Content -Raw -LiteralPath $TokenPath
        $token = if ($null -eq $raw) { '' } else { $raw.Trim() }
    } catch {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_RESTART_GUARD_TOKEN_READ_FAILED'; Token = $null; Detail = "path=$TokenPath error=$($_.Exception.Message)" }
    }

    if ($token.Length -lt 16) {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_RESTART_GUARD_TOKEN_INVALID'; Token = $null; Detail = "path=$TokenPath length=$($token.Length)" }
    }

    [pscustomobject]@{ Ok = $true; Code = 'OK'; Token = $token; Detail = "path=$TokenPath length=$($token.Length)" }
}

function Read-SynapseHealthForRestartGuard {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [ValidateRange(1, 300)][int]$TimeoutSec = 4
    )

    try {
        $health = Invoke-RestMethod -Uri "http://$Bind/health" -Headers @{ Authorization = "Bearer $Token" } -TimeoutSec $TimeoutSec
        [pscustomobject]@{ Ok = $true; Health = $health; Error = $null; TimeoutSec = $TimeoutSec }
    } catch {
        [pscustomobject]@{ Ok = $false; Health = $null; Error = $_.Exception.Message; TimeoutSec = $TimeoutSec }
    }
}

function ConvertTo-SynapseCanonicalValue {
    param([AllowNull()][object]$Value)

    if ($null -eq $Value) {
        return $null
    }

    if ($Value -is [System.Collections.IDictionary]) {
        $ordered = [ordered]@{}
        foreach ($key in @($Value.Keys | Sort-Object { [string]$_ })) {
            $ordered[[string]$key] = ConvertTo-SynapseCanonicalValue -Value $Value[$key]
        }
        return $ordered
    }

    if ($Value -is [System.Management.Automation.PSCustomObject]) {
        $ordered = [ordered]@{}
        foreach ($prop in @($Value.PSObject.Properties | Sort-Object Name)) {
            $ordered[$prop.Name] = ConvertTo-SynapseCanonicalValue -Value $prop.Value
        }
        return $ordered
    }

    if ($Value -is [System.Collections.IEnumerable] -and $Value -isnot [string]) {
        $items = New-Object System.Collections.ArrayList
        foreach ($item in $Value) {
            [void]$items.Add((ConvertTo-SynapseCanonicalValue -Value $item))
        }
        return ,($items.ToArray())
    }

    return $Value
}

function Get-SynapseCanonicalJson {
    param([AllowNull()][object]$Value)

    $canonical = ConvertTo-SynapseCanonicalValue -Value $Value
    return ($canonical | ConvertTo-Json -Depth 100 -Compress)
}

function Get-SynapseSha256Hex {
    param([Parameter(Mandatory=$true)][string]$Text)

    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
        return (($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString('x2') }) -join '')
    } finally {
        $sha.Dispose()
    }
}

function Get-SynapseObjectPropertyValue {
    param(
        [AllowNull()]$Object,
        [Parameter(Mandatory=$true)][string[]]$Names
    )

    if ($null -eq $Object) {
        return $null
    }
    foreach ($name in $Names) {
        $property = $Object.PSObject.Properties[$name]
        if ($property) {
            return $property.Value
        }
    }
    return $null
}

function Format-SynapseHealthSubsystemStatuses {
    param([AllowNull()]$Health)

    $subsystems = Get-SynapseObjectPropertyValue -Object $Health -Names @('subsystems')
    if ($null -eq $subsystems) {
        return '<missing>'
    }

    $statuses = @()
    foreach ($prop in @($subsystems.PSObject.Properties | Sort-Object Name)) {
        $status = [string](Get-SynapseObjectPropertyValue -Object $prop.Value -Names @('status'))
        if ([string]::IsNullOrWhiteSpace($status)) {
            $status = '<missing>'
        }
        $statuses += ("{0}={1}" -f $prop.Name, $status)
    }
    if ($statuses.Count -eq 0) {
        return '<none>'
    }
    return ($statuses -join ',')
}

function Test-SynapseHealthCriticalSubsystemsReady {
    param([AllowNull()]$Health)

    $subsystems = Get-SynapseObjectPropertyValue -Object $Health -Names @('subsystems')
    if ($null -eq $subsystems) {
        return [pscustomobject]@{
            Ok = $false
            Detail = 'health.subsystems missing'
        }
    }

    $criticalNames = @(
        'action',
        'daemon_drain',
        'daemon_lifecycle',
        'facade_contract',
        'http',
        'perception',
        'public_tool_registry',
        'storage'
    )
    $missing = @()
    $bad = @()
    foreach ($name in $criticalNames) {
        $node = Get-SynapseObjectPropertyValue -Object $subsystems -Names @($name)
        if ($null -eq $node) {
            $missing += $name
            continue
        }
        $status = [string](Get-SynapseObjectPropertyValue -Object $node -Names @('status'))
        $readyStatuses = if ($name -eq 'storage') { @('ok','maintenance') } else { @('ok') }
        if ($status -notin $readyStatuses) {
            if ([string]::IsNullOrWhiteSpace($status)) {
                $status = '<missing>'
            }
            $bad += ("{0}={1}" -f $name, $status)
        }
    }

    if ($missing.Count -gt 0 -or $bad.Count -gt 0) {
        return [pscustomobject]@{
            Ok = $false
            Detail = ("missing={0} bad={1}" -f (Format-SynapseLimitedList -Items $missing), (Format-SynapseLimitedList -Items $bad))
        }
    }

    return [pscustomobject]@{
        Ok = $true
        Detail = 'critical_subsystems_ok'
    }
}

function Read-SynapseMcpSseJsonResponse {
    param(
        [Parameter(Mandatory=$true)][string]$Content,
        [Parameter(Mandatory=$true)][string]$Operation,
        [int]$ExpectedId = 0
    )

    $trimmed = $Content.Trim()
    if ($trimmed.StartsWith('{')) {
        $message = $trimmed | ConvertFrom-Json
    } else {
        $normalized = ($Content -replace "`r`n", "`n") -replace "`r", "`n"
        $message = $null
        foreach ($frame in @($normalized -split "`n`n")) {
            $dataLines = @()
            foreach ($line in @($frame -split "`n")) {
                if ($line.StartsWith('data:')) {
                    $dataLines += $line.Substring(5).TrimStart()
                }
            }
            $data = ($dataLines -join "`n").Trim()
            if ($data.StartsWith('{')) {
                $message = $data | ConvertFrom-Json
                break
            }
        }
        if ($null -eq $message) {
            $prefix = if ($Content.Length -gt 240) { $Content.Substring(0, 240) } else { $Content }
            Die "SYNAPSE_MCP_SSE_PARSE_FAILED operation=$Operation content_prefix=$prefix remediation=streamable HTTP returned no JSON data frame; inspect daemon logs and MCP transport compatibility"
        }
    }

    if ($ExpectedId -ne 0 -and [int]$message.id -ne $ExpectedId) {
        Die "SYNAPSE_MCP_JSONRPC_ID_MISMATCH operation=$Operation expected_id=$ExpectedId actual_id=$($message.id) remediation=the daemon returned an unexpected JSON-RPC response; inspect streamable HTTP session handling"
    }
    if ($null -ne $message.error) {
        $errorJson = $message.error | ConvertTo-Json -Compress -Depth 8
        Die "SYNAPSE_MCP_JSONRPC_ERROR operation=$Operation error=$errorJson remediation=repair the daemon MCP endpoint before accepting setup"
    }

    return $message
}

function Get-SynapseWebResponseUtf8Content {
    param([Parameter(Mandatory=$true)]$Response)

    $streamProperty = $Response.PSObject.Properties['RawContentStream']
    if ($streamProperty -and $null -ne $streamProperty.Value) {
        $stream = $streamProperty.Value
        if ($stream.CanSeek) {
            $stream.Position = 0
        }
        $encoding = [System.Text.UTF8Encoding]::new($false, $true)
        $reader = [System.IO.StreamReader]::new($stream, $encoding, $true, 4096, $true)
        try {
            return $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
            if ($stream.CanSeek) {
                $stream.Position = 0
            }
        }
    }

    $content = $Response.Content
    if ($content -is [byte[]]) {
        $encoding = [System.Text.UTF8Encoding]::new($false, $true)
        return $encoding.GetString($content)
    }
    return [string]$content
}

function Invoke-SynapseMcpHttpPost {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][string]$Method,
        [Parameter(Mandatory=$true)]$Params,
        [int]$Id = 0,
        [string]$SessionId,
        [int]$TimeoutSec = $script:SynapseSetupMcpRequestTimeoutSec
    )

    $headers = @{
        Authorization = "Bearer $Token"
        Accept = 'application/json, text/event-stream'
    }
    if (-not [string]::IsNullOrWhiteSpace($SessionId)) {
        $headers['Mcp-Session-Id'] = $SessionId
        $headers['MCP-Protocol-Version'] = $script:SynapseMcpProtocolVersion
    }

    $request = [ordered]@{
        jsonrpc = '2.0'
        method = $Method
        params = $Params
    }
    if ($Id -ne 0) {
        $request['id'] = $Id
    }
    $body = $request | ConvertTo-Json -Depth 30 -Compress

    try {
        $response = Invoke-WebRequest `
            -Uri "http://$Bind/mcp" `
            -Method Post `
            -Headers $headers `
            -ContentType 'application/json' `
            -Body $body `
            -TimeoutSec $TimeoutSec `
            -UseBasicParsing `
            -ErrorAction Stop
        return [pscustomobject]@{
            Content = Get-SynapseWebResponseUtf8Content -Response $response
            Headers = $response.Headers
            StatusCode = $response.StatusCode
        }
    } catch {
        $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        $tcpClients = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
        $processes = @(Get-SynapseMcpProcessSnapshot)
        Die ("SYNAPSE_MCP_TOOL_SURFACE_READ_FAILED stage={0} bind={1} timeout_s={2} session_id={3} error={4}`nlisteners:`n{5}`ntcp_clients:`n{6}`nprocesses:`n{7}`nremediation=repair streamable HTTP MCP before accepting setup. If process/socket SoT is healthy, inspect MCP session lifecycle/storage logs for slow initialize/tools/list/tool-call handling and raise the bounded setup MCP budget only with measured evidence." -f `
            $Method,
            $Bind,
            $TimeoutSec,
            ($(if ([string]::IsNullOrWhiteSpace($SessionId)) { '<none>' } else { $SessionId })),
            $_.Exception.Message,
            (Format-SynapseTcpBindListenerSnapshot -Snapshot $listeners),
            (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients),
            (Format-SynapseMcpProcessSnapshot -Snapshot $processes))
    }
}

function Close-SynapseMcpSetupSession {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][string]$SessionId,
        [switch]$Required
    )

    $headers = @{
        Authorization = "Bearer $Token"
        Accept = 'application/json, text/event-stream'
        'Mcp-Session-Id' = $SessionId
        'MCP-Protocol-Version' = $script:SynapseMcpProtocolVersion
    }
    $timeoutSec = $script:SynapseMcpSessionDeleteTimeoutSec

    try {
        Invoke-WebRequest -Uri "http://$Bind/mcp" -Method Delete -Headers $headers -TimeoutSec $timeoutSec -UseBasicParsing -ErrorAction Stop | Out-Null
    } catch {
        $diagnostic = "SYNAPSE_MCP_TOOL_SURFACE_SESSION_DELETE_FAILED bind=$Bind session_id=$SessionId timeout_sec=$timeoutSec error=$($_.Exception.Message) remediation=inspect health active_sessions plus MCP_SESSION_TEARDOWN_COMPLETED/MCP_HTTP_SESSION_LIFECYCLE_CLEANUP logs and candidate process/socket SoT"
        if ($Required) {
            Die $diagnostic
        }
        Info "WARN: $diagnostic"
    }
}

function Invoke-SynapseSetupMcpTool {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][string]$Name,
        [Parameter(Mandatory=$true)]$Arguments,
        [string]$Profile,
        [string]$ProfileReason,
        [switch]$AcquireForegroundLease,
        [int]$TimeoutSec = $script:SynapseSetupMcpRequestTimeoutSec
    )

    $sessionId = $null
    $mcpReadSucceeded = $false
    $leaseAcquired = $false
    try {
        $initParams = [ordered]@{
            protocolVersion = $script:SynapseMcpProtocolVersion
            capabilities = @{}
            clientInfo = [ordered]@{ name = 'synapse-setup'; version = '0' }
        }
        $initResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -Method 'initialize' -Params $initParams -Id 1
        $sessionId = @($initResponse.Headers['Mcp-Session-Id'])[0]
        if ([string]::IsNullOrWhiteSpace($sessionId)) {
            Die "SYNAPSE_MCP_TOOL_SESSION_MISSING bind=$Bind tool=$Name remediation=streamable HTTP initialize did not return Mcp-Session-Id"
        }
        $initMessage = Read-SynapseMcpSseJsonResponse -Content $initResponse.Content -Operation 'initialize' -ExpectedId 1
        if ($null -eq $initMessage.result -or $null -eq $initMessage.result.capabilities) {
            Die "SYNAPSE_MCP_TOOL_INITIALIZE_INVALID bind=$Bind session_id=$sessionId tool=$Name remediation=daemon initialize response is missing capabilities"
        }

        Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'notifications/initialized' -Params @{} | Out-Null

        $requestId = 2
        $toolsResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/list' -Params @{} -Id $requestId -TimeoutSec $TimeoutSec
        $toolsMessage = Read-SynapseMcpSseJsonResponse -Content $toolsResponse.Content -Operation 'tools/list setup session' -ExpectedId $requestId
        $toolNames = @($toolsMessage.result.tools | ForEach-Object { [string]$_.name })
        if ($toolNames -notcontains $Name) {
            $visible = if ($toolNames.Count -eq 0) { '<none>' } else { $toolNames -join ',' }
            Die "SYNAPSE_MCP_SETUP_TOOL_NOT_VISIBLE bind=$Bind session_id=$sessionId requested_tool=$Name visible_tools=$visible remediation=setup may only call public facade tools visible through tools/list; route hidden implementation tools through their public facade/profile path"
        }
        $requestId++
        if ($AcquireForegroundLease) {
            if ($Profile -notin @('break_glass', 'full_capability')) {
                Die "SYNAPSE_MCP_SETUP_LEASE_PROFILE_INVALID bind=$Bind session_id=$sessionId tool=$Name requested_profile=$(if ([string]::IsNullOrWhiteSpace($Profile)) { '<none>' } else { $Profile }) remediation=AcquireForegroundLease is only valid for the two maintenance-authorized profiles break_glass/full_capability"
            }
            if ($toolNames -notcontains 'act') {
                $visible = if ($toolNames.Count -eq 0) { '<none>' } else { $toolNames -join ',' }
                Die "SYNAPSE_MCP_SETUP_ACT_TOOL_NOT_VISIBLE bind=$Bind session_id=$sessionId tool=$Name visible_tools=$visible remediation=an audited maintenance operation requires the public act facade so setup can acquire and independently read back the foreground input lease"
            }
            $leaseTtlMs = [Math]::Min(300000, [Math]::Max(100, ([int64]$TimeoutSec * 1000L)))
            $leaseCallParams = @{
                name = 'act'
                arguments = [ordered]@{
                    operation = 'lease_acquire'
                    ttl_ms = $leaseTtlMs
                }
            }
            $leaseResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/call' -Params $leaseCallParams -Id $requestId -TimeoutSec $TimeoutSec
            $leaseMessage = Read-SynapseMcpSseJsonResponse -Content $leaseResponse.Content -Operation 'tools/call act lease_acquire' -ExpectedId $requestId
            if ($leaseMessage.result.isError -eq $true) {
                $leaseErrorText = @($leaseMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
                Die "SYNAPSE_MCP_SETUP_LEASE_ACQUIRE_ERROR bind=$Bind session_id=$sessionId tool=$Name error=$leaseErrorText remediation=repair the candidate's public act/foreground-lease authority path before accepting setup"
            }
            $leaseText = @($leaseMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
            try {
                $leaseJson = $leaseText | ConvertFrom-Json -ErrorAction Stop
            } catch {
                Die "SYNAPSE_MCP_SETUP_LEASE_ACQUIRE_JSON_INVALID bind=$Bind session_id=$sessionId tool=$Name error=$($_.Exception.Message) remediation=act operation=lease_acquire must return its physical lease readback as JSON"
            }
            $lease = $leaseJson.lease
            if ($null -eq $lease -or $lease.held -ne $true -or $lease.is_owner -ne $true -or [string]$lease.owner_session_id -ne $sessionId -or [string]$lease.this_session_id -ne $sessionId) {
                $leaseReadback = if ($null -eq $lease) { '<missing>' } else { $lease | ConvertTo-Json -Depth 8 -Compress }
                Die "SYNAPSE_MCP_SETUP_LEASE_ACQUIRE_READBACK_INVALID bind=$Bind session_id=$sessionId tool=$Name lease=$leaseReadback remediation=the public act facade did not independently read back this exact MCP session as the physical foreground-lease owner; refuse maintenance authority"
            }
            $leaseAcquired = $true
            Info "Setup MCP session foreground lease acquired and read back session_id=$sessionId tool=$Name ttl_ms=$leaseTtlMs outcome=$($lease.outcome)"
            $requestId++
        }
        if (-not [string]::IsNullOrWhiteSpace($Profile)) {
            if ($toolNames -notcontains 'profile') {
                $visible = if ($toolNames.Count -eq 0) { '<none>' } else { $toolNames -join ',' }
                Die "SYNAPSE_MCP_SETUP_PROFILE_TOOL_NOT_VISIBLE bind=$Bind session_id=$sessionId requested_profile=$Profile visible_tools=$visible remediation=setup profile escalation requires the public profile facade in tools/list"
            }
            if ([string]::IsNullOrWhiteSpace($ProfileReason)) {
                Die "SYNAPSE_MCP_PROFILE_REASON_MISSING bind=$Bind session_id=$sessionId tool=$Name requested_profile=$Profile remediation=setup profile escalation requires an explicit reason for audit readback"
            }
            $profileArgs = [ordered]@{
                operation = 'set'
                profile = $Profile
                confirm_break_glass = $true
                reason = $ProfileReason
            }
            $profileCallParams = @{ name = 'profile'; arguments = $profileArgs }
            $profileResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/call' -Params $profileCallParams -Id $requestId -TimeoutSec $TimeoutSec
            $profileMessage = Read-SynapseMcpSseJsonResponse -Content $profileResponse.Content -Operation "tools/call profile set $Profile" -ExpectedId $requestId
            if ($profileMessage.result.isError -eq $true) {
                $profileErrorText = @($profileMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
                Die "SYNAPSE_MCP_PROFILE_SET_ERROR bind=$Bind session_id=$sessionId requested_profile=$Profile tool=$Name error=$profileErrorText remediation=repair the setup MCP profile policy path before accepting setup"
            }
            Info "Setup MCP session profile set session_id=$sessionId profile=$Profile reason=$ProfileReason"
            $requestId++
        }
        $callParams = @{ name = $Name; arguments = $Arguments }
        $callResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/call' -Params $callParams -Id $requestId -TimeoutSec $TimeoutSec
        $callMessage = Read-SynapseMcpSseJsonResponse -Content $callResponse.Content -Operation "tools/call $Name" -ExpectedId $requestId
        if ($callMessage.result.isError -eq $true) {
            $errorText = @($callMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
            Die "SYNAPSE_MCP_TOOL_CALL_ERROR bind=$Bind session_id=$sessionId tool=$Name error=$errorText remediation=repair the live daemon/bridge before accepting setup"
        }
        $text = @($callMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
        $json = $null
        if (-not [string]::IsNullOrWhiteSpace($text)) {
            try {
                $json = $text | ConvertFrom-Json
            } catch {
                $json = $null
            }
        }
        $result = [pscustomobject]@{
            SessionId = $sessionId
            Message = $callMessage
            Text = $text
            Json = $json
        }
        if ($leaseAcquired) {
            $requestId++
            $restoreProfileParams = @{
                name = 'profile'
                arguments = [ordered]@{
                    operation = 'set'
                    profile = 'normal_agent'
                    confirm_break_glass = $false
                    reason = "synapse-setup completed audited maintenance tool $Name; restore least authority before session teardown"
                }
            }
            $restoreProfileResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/call' -Params $restoreProfileParams -Id $requestId -TimeoutSec $TimeoutSec
            $restoreProfileMessage = Read-SynapseMcpSseJsonResponse -Content $restoreProfileResponse.Content -Operation 'tools/call profile restore normal_agent' -ExpectedId $requestId
            if ($restoreProfileMessage.result.isError -eq $true) {
                $restoreProfileError = @($restoreProfileMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
                Die "SYNAPSE_MCP_SETUP_PROFILE_RESTORE_ERROR bind=$Bind session_id=$sessionId tool=$Name error=$restoreProfileError remediation=the maintenance operation completed but session authority could not be restored; candidate validation must fail and destroy this isolated daemon"
            }
            $requestId++
            $releaseCallParams = @{ name = 'act'; arguments = @{ operation = 'lease_release' } }
            $releaseResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/call' -Params $releaseCallParams -Id $requestId -TimeoutSec $TimeoutSec
            $releaseMessage = Read-SynapseMcpSseJsonResponse -Content $releaseResponse.Content -Operation 'tools/call act lease_release' -ExpectedId $requestId
            if ($releaseMessage.result.isError -eq $true) {
                $releaseError = @($releaseMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
                Die "SYNAPSE_MCP_SETUP_LEASE_RELEASE_ERROR bind=$Bind session_id=$sessionId tool=$Name error=$releaseError remediation=the maintenance operation completed but its foreground lease could not be released; candidate validation must fail and destroy this isolated daemon"
            }
            $releaseText = @($releaseMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
            try {
                $releaseJson = $releaseText | ConvertFrom-Json -ErrorAction Stop
            } catch {
                Die "SYNAPSE_MCP_SETUP_LEASE_RELEASE_JSON_INVALID bind=$Bind session_id=$sessionId tool=$Name error=$($_.Exception.Message) remediation=act operation=lease_release must return its physical post-release readback as JSON"
            }
            $releasedLease = $releaseJson.lease
            if ($null -eq $releasedLease -or $releasedLease.held -eq $true -or $releasedLease.is_owner -eq $true -or [string]$releasedLease.owner_session_id -eq $sessionId) {
                $releaseReadback = if ($null -eq $releasedLease) { '<missing>' } else { $releasedLease | ConvertTo-Json -Depth 8 -Compress }
                Die "SYNAPSE_MCP_SETUP_LEASE_RELEASE_READBACK_INVALID bind=$Bind session_id=$sessionId tool=$Name lease=$releaseReadback remediation=the public act facade did not independently prove the foreground lease absent after the maintenance operation; candidate validation must fail"
            }
            $leaseAcquired = $false
            Info "Setup MCP session profile restored and foreground lease release read back session_id=$sessionId tool=$Name outcome=$($releasedLease.outcome) held=$($releasedLease.held)"
        }
        $mcpReadSucceeded = $true
        return $result
    } finally {
        if (-not [string]::IsNullOrWhiteSpace($sessionId)) {
            Close-SynapseMcpSetupSession -Bind $Bind -Token $Token -SessionId $sessionId -Required:$mcpReadSucceeded
        }
    }
}

# --- #2031 -----------------------------------------------------------------
# Which /health facts the Chrome bridge installer actually depends on.
#
# Deliberately NOT included: chrome_bridge status. An absent, stale, or
# unavailable bridge host is exactly why setup is about to run the reload, so
# demanding a clean chrome_bridge here would deadlock the repair path. The one
# chrome_bridge condition that does block the reload is a state lock that
# health itself refuses to wait behind: while that lock is busy or poisoned
# the subsystem carries no nested host record, and the installer cannot read
# the "before" host identity it must diff the replacement host against.
function Get-SynapseDaemonBridgeReloadUnreadySubsystems {
    param([Parameter(Mandatory=$true)]$Health)

    $unready = New-Object System.Collections.Generic.List[string]
    # The daemon-wide readiness contract setup already enforces at install
    # health (http, facade_contract, public_tool_registry, daemon_lifecycle,
    # action, daemon_drain, perception, storage). A cold daemon reports e.g.
    # public_tool_registry=pending_facades here while the public tool surface
    # is still registering, which is exactly "not ready for traffic yet".
    $critical = Test-SynapseHealthCriticalSubsystemsReady -Health $Health
    if (-not $critical.Ok) {
        [void]$unready.Add("critical_subsystems=[$($critical.Detail)]")
    }
    $subsystems = Get-SynapseObjectPropertyValue -Object $Health -Names @('subsystems')
    if ($null -eq $subsystems) {
        [void]$unready.Add('subsystems=missing')
        return $unready.ToArray()
    }
    $bridge = Get-SynapseObjectPropertyValue -Object $subsystems -Names @('chrome_bridge')
    if ($null -eq $bridge) {
        [void]$unready.Add('chrome_bridge=missing')
    } else {
        $bridgeDetail = [string](Get-SynapseObjectPropertyValue -Object $bridge -Names @('detail'))
        if ($bridgeDetail -match 'chrome_bridge_state_lock_busy') {
            [void]$unready.Add('chrome_bridge=state_lock_busy')
        } elseif ($bridgeDetail -match 'chrome_bridge_state_lock_poisoned') {
            [void]$unready.Add('chrome_bridge=state_lock_poisoned')
        }
    }
    if (@(Get-SynapseObjectPropertyValue -Object $Health -Names @('tool_names')) -notcontains 'browser_debugger') {
        [void]$unready.Add('tool_surface=browser_debugger_absent')
    }
    # Emitted unwrapped so the caller's @() sees an empty collection when the
    # daemon is ready. A `,`-wrapped return would make an empty result read as
    # a one-element array and the gate could never pass.
    return $unready.ToArray()
}

# --- #2031 -----------------------------------------------------------------
# Post-handoff daemon readiness gate for every Chrome bridge installer entry.
#
# install-synapse-chrome-debugger.ps1 -- run directly by setup for the UI
# repair path, and spawned by the daemon itself for browser_debugger
# reload_bridge -- reads authenticated /health with a small fixed per-request
# budget before it will touch Chrome. A daemon that setup has only just handed
# off is already accepting connections while it is still finishing cold-start
# work, so that read timed out and the reload failed closed with
# SYNAPSE_CHROME_BRIDGE_DAEMON_HEALTH_BEFORE_RELOAD_FAILED even though an
# independent authenticated /health seconds later succeeded (#2031).
#
# "Port is open" and "one /health eventually answered" are liveness facts.
# This gate is startup-probe shaped in the Kubernetes sense: the same check
# the consumer performs, given an explicitly bounded runway with a generous
# failure budget, polled with backoff, and gated on consecutive successes
# (a successThreshold) so a single lucky sample cannot be mistaken for a
# settled daemon. It proves the named subsystems the reload depends on, it
# never sleeps and hopes, it never retries past its deadline, and on expiry it
# dies naming the exact unready subsystem plus the probe evidence.
function Assert-SynapseDaemonBridgeReloadReadiness {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][int]$ExpectedDaemonPid,
        [ValidateRange(0, 900000)][int]$TimeoutMs = 0
    )

    if ($TimeoutMs -le 0) {
        $TimeoutMs = $SynapseChromeBridgeReloadReadinessTimeoutMs
    }
    $probeTimeoutSec = $SynapseChromeBridgeInstallerHealthProbeTimeoutSec
    $requiredOk = $SynapseChromeBridgeReloadReadinessSuccessThreshold
    $startedMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $deadlineMs = [int64]$startedMs + [int64]$TimeoutMs
    $attempt = 0
    $consecutiveOk = 0
    $bestConsecutiveOk = 0
    $slowestOkProbeMs = 0
    $lastError = $null
    $lastUnready = @('no_probe_completed')
    $backoffMs = $SynapseChromeBridgeReloadReadinessMinBackoffMs
    $readyHealth = $null

    while ($true) {
        $attempt += 1
        $probeWatch = [System.Diagnostics.Stopwatch]::StartNew()
        $healthRead = Read-SynapseHealthForRestartGuard -Bind $Bind -Token $Token -TimeoutSec $probeTimeoutSec
        $probeWatch.Stop()
        $probeMs = [int]$probeWatch.Elapsed.TotalMilliseconds
        if (-not $healthRead.Ok) {
            $lastError = [string]$healthRead.Error
            # A credential rejection is not a startup race: the daemon answered
            # and refused these bytes. No amount of waiting repairs it.
            if ($lastError -match '(?i)\(401\)|\(403\)|Unauthorized|Forbidden') {
                Die "SYNAPSE_CHROME_BRIDGE_RELOAD_READINESS_UNAUTHORIZED bind=$Bind reason=$Reason expected_pid=$ExpectedDaemonPid attempt=$attempt probe_timeout_s=$probeTimeoutSec error=$lastError remediation=the post-handoff daemon answered authenticated /health with a credential rejection, so the Chrome bridge installer can never prove pre-reload host identity with this token; repair the setup bearer token and the daemon that loaded it instead of waiting on startup timing"
            }
            $consecutiveOk = 0
            $lastUnready = @("health_request_failed=$lastError")
        } else {
            $health = $healthRead.Health
            $actualPid = [int]$health.pid
            if ($ExpectedDaemonPid -gt 0 -and $actualPid -ne $ExpectedDaemonPid) {
                # Waiting cannot reconcile a different daemon; the identity
                # setup verified during handoff is gone.
                Die "SYNAPSE_CHROME_BRIDGE_RELOAD_READINESS_DAEMON_PID_DRIFT bind=$Bind reason=$Reason expected_pid=$ExpectedDaemonPid actual_pid=$actualPid attempt=$attempt elapsed_ms=$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() - $startedMs) remediation=the daemon serving /health is not the one setup handed off to; do not reload the Chrome bridge against an unverified daemon, re-run setup and inspect the supervisor/scheduled-task restart authority"
            }
            $lastError = $null
            $lastUnready = @(Get-SynapseDaemonBridgeReloadUnreadySubsystems -Health $health)
            if ($lastUnready.Count -eq 0) {
                $consecutiveOk += 1
                $readyHealth = $health
                if ($probeMs -gt $slowestOkProbeMs) { $slowestOkProbeMs = $probeMs }
                if ($consecutiveOk -gt $bestConsecutiveOk) { $bestConsecutiveOk = $consecutiveOk }
                if ($consecutiveOk -ge $requiredOk) {
                    $elapsedMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() - $startedMs
                    Info ("Daemon proven ready for Chrome bridge installer reason={0} bind={1} pid={2} attempts={3} consecutive_ok={4} probe_timeout_s={5} slowest_ok_probe_ms={6} elapsed_ms={7}" -f `
                        $Reason, $Bind, $ExpectedDaemonPid, $attempt, $consecutiveOk, $probeTimeoutSec, $slowestOkProbeMs, $elapsedMs)
                    return [pscustomobject]@{
                        Health = $readyHealth
                        Attempts = $attempt
                        ConsecutiveOk = $consecutiveOk
                        ProbeTimeoutSec = $probeTimeoutSec
                        SlowestOkProbeMs = $slowestOkProbeMs
                        ElapsedMs = $elapsedMs
                    }
                }
            } else {
                $consecutiveOk = 0
            }
        }

        $nowMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        if ($nowMs -ge $deadlineMs) {
            break
        }
        # Consecutive samples are spaced, not bursted, so successThreshold
        # measures a daemon that stays servable rather than one lucky window.
        $waitMs = if ($consecutiveOk -gt 0) { $SynapseChromeBridgeReloadReadinessSuccessSpacingMs } else { $backoffMs }
        $remainingMs = [int][Math]::Max(0, [int64]$deadlineMs - [int64]$nowMs)
        $sleepMs = [Math]::Min([int]$waitMs, $remainingMs)
        if ($sleepMs -gt 0) {
            Start-Sleep -Milliseconds $sleepMs
        }
        if ($consecutiveOk -eq 0) {
            $backoffMs = [Math]::Min([int]$SynapseChromeBridgeReloadReadinessMaxBackoffMs, [int]$backoffMs * 2)
        }
    }

    $elapsedMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() - $startedMs
    $unreadyText = if ($lastUnready.Count -eq 0) { '<none>' } else { $lastUnready -join ',' }
    Die ("SYNAPSE_CHROME_BRIDGE_RELOAD_READINESS_TIMEOUT bind={0} reason={1} expected_pid={2} unready_subsystems={3} attempts={4} consecutive_ok={5} best_consecutive_ok={6} required_consecutive_ok={7} probe_timeout_s={8} slowest_ok_probe_ms={9} elapsed_ms={10} timeout_ms={11} last_health_error={12} remediation=setup refuses to invoke the Chrome bridge installer against a daemon that has not proven it answers authenticated /health inside the installer's own {8}s per-request budget with the named subsystems ready. The unready_subsystems field is the exact blocking signal: repair that subsystem (or the cold-start work still holding /health) and rerun setup. Setup never reloads the bridge on an unproven daemon and never widens the installer's probe budget to hide it." -f `
        $Bind,
        $Reason,
        $ExpectedDaemonPid,
        $unreadyText,
        $attempt,
        $consecutiveOk,
        $bestConsecutiveOk,
        $requiredOk,
        $probeTimeoutSec,
        $slowestOkProbeMs,
        $elapsedMs,
        $TimeoutMs,
        ($(if ([string]::IsNullOrWhiteSpace($lastError)) { '<none>' } else { $lastError })))
}

function Assert-SynapseChromeBridgeLiveAfterSetup {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)]$Health,
        [Parameter(Mandatory=$true)][string]$ChromeBridgeInstallerPath,
        [Parameter(Mandatory=$true)][string]$ChromeNativeHostExePath
    )

    $chromeBridge = $Health.subsystems.chrome_bridge
    $status = [string]$chromeBridge.status
    $detail = [string]$chromeBridge.detail
    $isClean = $status -eq 'ok' -and $detail -match 'extension_stale=false' -and $detail -match 'reloadSelf'
    if ($isClean) {
        Info "Chrome bridge OK after daemon start: stale=false capability=reloadSelf"
        return $Health
    }

    $currentHealth = $Health
    $nowMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $postStartWaitMs = $SynapseChromeBridgeDefaultPostStartWaitMs
    if ($null -ne $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs) {
        $postStartWaitMs = [Math]::Max(
            [int64]$postStartWaitMs,
            [int64]$SynapseChromeBridgeMaintenancePostStartWaitMs)
        $postStartWaitMs = [Math]::Min([int64]$postStartWaitMs, [int64]$SynapseChromeBridgeMaxPostStartWaitMs)
    }
    $deadlineMs = [int64]$nowMs + [int64]$postStartWaitMs
    $attempt = 0
    $lastWaitHealthError = $null
    while ([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() -lt $deadlineMs) {
        if ($detail -notmatch 'no_active_chrome_bridge_host') {
            break
        }
        $attempt += 1
        $currentMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        $pauseRemainingMs = 0
        if ($null -ne $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs) {
            $pauseRemainingMs = [Math]::Max(0, [int64]$script:SynapseChromeBridgeMaintenancePauseUntilUnixMs - [int64]$currentMs)
        }
        $waitRemainingMs = [Math]::Max(0, [int64]$deadlineMs - [int64]$currentMs)
        Info ("Chrome bridge host absent after daemon start; waiting for alarmReconnect readback attempt={0} pause_until_unix_ms={1} pause_remaining_ms={2} resume_probe_after_unix_ms={3} wait_remaining_ms={4}" -f `
            $attempt,
            ($(if ($null -eq $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs) { '<none>' } else { $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs })),
            $pauseRemainingMs,
            ($(if ($null -eq $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs) { '<none>' } else { $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs })),
            $waitRemainingMs)
        Start-Sleep -Seconds 2
        $healthTimeoutSec = [Math]::Min(10, [Math]::Max(4, [int][Math]::Ceiling($waitRemainingMs / 1000)))
        $healthRead = Read-SynapseHealthForRestartGuard -Bind $Bind -Token $Token -TimeoutSec $healthTimeoutSec
        if (-not $healthRead.Ok) {
            $lastWaitHealthError = $healthRead.Error
            Info "WARN: Chrome bridge wait health read failed attempt=$attempt timeout_s=$($healthRead.TimeoutSec) wait_remaining_ms=$waitRemainingMs error=$lastWaitHealthError"
            continue
        }
        $currentHealth = $healthRead.Health
        $chromeBridge = $currentHealth.subsystems.chrome_bridge
        $status = [string]$chromeBridge.status
        $detail = [string]$chromeBridge.detail
        $isClean = $status -eq 'ok' -and $detail -match 'extension_stale=false' -and $detail -match 'reloadSelf'
        if ($isClean) {
            Info "Chrome bridge OK after daemon start wait: stale=false capability=reloadSelf"
            return $currentHealth
        }
    }
    if ($detail -match 'no_active_chrome_bridge_host') {
        if ($null -ne $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs) {
            $timeoutMs = [Math]::Max(0, [int64][DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() - [int64]$nowMs)
            Die ("SYNAPSE_CHROME_BRIDGE_ALARM_RECONNECT_TIMEOUT status={0} detail={1} waited_ms={2} pause_until_unix_ms={3} resume_probe_after_unix_ms={4} last_health_error={5} remediation=setup requested a maintenance reconnect pause before daemon restart and then waited for the installed MV3 alarmReconnect path to observe the replacement daemon and re-register. It did not. Inspect the Chrome extension service-worker console for maintenance resume probe logs and daemon /health chrome_bridge detail; setup refuses to hide the reconnect failure with foreground UI reload." -f `
                $status,
                $detail,
                $timeoutMs,
                $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs,
                ($(if ($null -eq $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs) { '<none>' } else { $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs })),
                ($(if ([string]::IsNullOrWhiteSpace($lastWaitHealthError)) { '<none>' } else { $lastWaitHealthError })))
        }
        Die "SYNAPSE_CHROME_BACKGROUND_RELOAD_HOST_UNAVAILABLE status=$status detail=$detail waited_ms=$([Math]::Max(0, [int64][DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() - [int64]$nowMs)) last_health_error=$(if ([string]::IsNullOrWhiteSpace($lastWaitHealthError)) { '<none>' } else { $lastWaitHealthError }) remediation=the installed alarmReconnect lifecycle did not restore an authenticated bridge host; setup failed before activating, navigating, restoring, minimizing, unminimizing, clicking, typing into, or otherwise altering any human Chrome window. Restore the installed bridge through a natural Chrome restart or an explicit operator-authorized install, then rerun setup"
    }

    Info "WARN: Chrome bridge not clean after daemon start; requesting in-place browser_debugger.reload_bridge through the new live MCP daemon. status=$status detail=$detail"
    # #2031: reload_bridge makes the daemon spawn the Chrome bridge installer,
    # which immediately reads authenticated /health back off this same daemon
    # under a small per-request budget. Prove that budget is already met, and
    # that the browser_debugger facade is registered, before asking for it.
    [void](Assert-SynapseDaemonBridgeReloadReadiness `
        -Bind $Bind `
        -Token $Token `
        -Reason 'chrome_bridge_reload_bridge' `
        -ExpectedDaemonPid ([int]$currentHealth.pid))
    $reloadArgs = [ordered]@{
        operation = 'reload_bridge'
        reload_bridge = [ordered]@{ wait_timeout_ms = 30000 }
    }
    $reload = Invoke-SynapseSetupMcpTool `
        -Bind $Bind `
        -Token $Token `
        -Name 'browser_debugger' `
        -Arguments $reloadArgs `
        -Profile 'browser_debugger' `
        -ProfileReason 'synapse-setup Chrome bridge post-start reload through public browser_debugger facade' `
        -TimeoutSec 45
    $reloadReadback = if ($reload.Json -and $reload.Json.reload_bridge) { $reload.Json.reload_bridge } else { $null }
    $afterBuild = if ($reloadReadback -and $reloadReadback.after) { [string]$reloadReadback.after.extension_build_id } else { 'unknown' }
    Info "Chrome bridge reload completed through public browser_debugger facade after_build_id=$afterBuild"

    $postReloadDeadlineMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() + [int64]45000
    $postReloadAttempt = 0
    $lastPostReloadHealthError = $null
    $afterHealth = $null
    do {
        $postReloadAttempt += 1
        $waitRemainingMs = [Math]::Max(0, [int64]$postReloadDeadlineMs - [int64][DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())
        $healthTimeoutSec = [Math]::Min(10, [Math]::Max(4, [int][Math]::Ceiling($waitRemainingMs / 1000)))
        $healthRead = Read-SynapseHealthForRestartGuard -Bind $Bind -Token $Token -TimeoutSec $healthTimeoutSec
        if ($healthRead.Ok) {
            $afterHealth = $healthRead.Health
            break
        }
        $lastPostReloadHealthError = $healthRead.Error
        Info "WARN: Chrome bridge post-reload health read failed attempt=$postReloadAttempt timeout_s=$($healthRead.TimeoutSec) wait_remaining_ms=$waitRemainingMs error=$lastPostReloadHealthError"
        Start-Sleep -Seconds 2
    } while ([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() -lt $postReloadDeadlineMs)
    if ($null -eq $afterHealth) {
        Die "SYNAPSE_CHROME_BRIDGE_POST_RELOAD_HEALTH_FAILED bind=$Bind attempts=$postReloadAttempt last_error=$(if ([string]::IsNullOrWhiteSpace($lastPostReloadHealthError)) { '<none>' } else { $lastPostReloadHealthError }) remediation=daemon was live before bridge reload but /health did not return before the bounded post-reload deadline"
    }
    $afterBridge = $afterHealth.subsystems.chrome_bridge
    $afterStatus = [string]$afterBridge.status
    $afterDetail = [string]$afterBridge.detail
    if ($afterStatus -ne 'ok' -or $afterDetail -notmatch 'extension_stale=false' -or $afterDetail -notmatch 'reloadSelf') {
        Die "SYNAPSE_CHROME_BRIDGE_STALE_AFTER_SETUP_RELOAD status=$afterStatus detail=$afterDetail remediation=setup requires the already-open Chrome profile to load the bundled bridge build; run scripts\\install-synapse-chrome-debugger.ps1 from the interactive desktop and keep normal bridge commands failed closed until health is clean"
    }
    Info "Chrome bridge OK after setup reload: stale=false capability=reloadSelf"
    return $afterHealth
}

function Read-SynapseDaemonToolSurface {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)]$Health
    )

    $sessionId = $null
    $mcpReadSucceeded = $false
    try {
        $initParams = [ordered]@{
            protocolVersion = $script:SynapseMcpProtocolVersion
            capabilities = @{}
            clientInfo = [ordered]@{ name = 'synapse-setup'; version = '0' }
        }
        $initResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -Method 'initialize' -Params $initParams -Id 1
        $sessionId = @($initResponse.Headers['Mcp-Session-Id'])[0]
        if ([string]::IsNullOrWhiteSpace($sessionId)) {
            Die "SYNAPSE_MCP_TOOL_SURFACE_SESSION_MISSING bind=$Bind remediation=streamable HTTP initialize did not return Mcp-Session-Id; repair daemon transport"
        }
        $initMessage = Read-SynapseMcpSseJsonResponse -Content $initResponse.Content -Operation 'initialize' -ExpectedId 1
        if ($null -eq $initMessage.result -or $null -eq $initMessage.result.capabilities) {
            Die "SYNAPSE_MCP_INITIALIZE_RESULT_INVALID bind=$Bind session_id=$sessionId remediation=daemon initialize response is missing capabilities"
        }

        Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'notifications/initialized' -Params @{} | Out-Null

        $tools = @()
        $cursor = $null
        $requestId = 2
        do {
            $listParams = @{}
            if (-not [string]::IsNullOrWhiteSpace($cursor)) {
                $listParams['cursor'] = $cursor
            }
            $listResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/list' -Params $listParams -Id $requestId
            $listMessage = Read-SynapseMcpSseJsonResponse -Content $listResponse.Content -Operation 'tools/list' -ExpectedId $requestId
            if ($null -eq $listMessage.result -or $null -eq $listMessage.result.tools) {
                Die "SYNAPSE_MCP_TOOLS_LIST_RESULT_INVALID bind=$Bind session_id=$sessionId request_id=$requestId remediation=tools/list did not return a tools array"
            }
            $tools += @($listMessage.result.tools)
            $cursor = [string]$listMessage.result.nextCursor
            $requestId += 1
        } while (-not [string]::IsNullOrWhiteSpace($cursor))

        $sortedTools = @($tools | Sort-Object name)
        $toolNames = @($sortedTools | ForEach-Object { [string]$_.name })

        $healthCallParams = @{ name = 'health'; arguments = @{} }
        $healthCallResponse = Invoke-SynapseMcpHttpPost -Bind $Bind -Token $Token -SessionId $sessionId -Method 'tools/call' -Params $healthCallParams -Id $requestId
        $healthCallMessage = Read-SynapseMcpSseJsonResponse -Content $healthCallResponse.Content -Operation 'tools/call health' -ExpectedId $requestId
        $healthText = @($healthCallMessage.result.content | Where-Object { [string]$_.type -eq 'text' } | Select-Object -First 1).text
        if ([string]::IsNullOrWhiteSpace($healthText)) {
            Die "SYNAPSE_MCP_HEALTH_TOOL_RESULT_INVALID bind=$Bind session_id=$sessionId request_id=$requestId remediation=health tools/call did not return JSON text content"
        }
        try {
            $sessionHealth = $healthText | ConvertFrom-Json
        } catch {
            Die "SYNAPSE_MCP_HEALTH_TOOL_JSON_INVALID bind=$Bind session_id=$sessionId request_id=$requestId error=$($_.Exception.Message) remediation=health tools/call returned non-JSON text"
        }
        $runtimeHash = [string]$sessionHealth.tool_surface_sha256
        $runtimeToolCount = try { [int]$sessionHealth.tool_count } catch { -1 }
        $runtimeNames = @($sessionHealth.tool_names | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object)
        $toolNamesSorted = @($toolNames | Sort-Object)
        $runtimeNamesJoined = ($runtimeNames -join "`n")
        $toolNamesJoined = ($toolNamesSorted -join "`n")
        if ([string]::IsNullOrWhiteSpace($runtimeHash) -or $runtimeToolCount -ne $toolNames.Count -or $runtimeNamesJoined -ne $toolNamesJoined) {
            Die ("SYNAPSE_MCP_HEALTH_TOOL_SURFACE_MISMATCH bind={0} session_id={1} tools_list_count={2} health_count={3} health_hash={4} tools_list_only={5} health_only={6} remediation=repair health/tool-list fingerprint agreement before writing Codex snapshot" -f `
                $Bind,
                $sessionId,
                $toolNames.Count,
                $runtimeToolCount,
                $runtimeHash,
                (Format-SynapseLimitedList -Items @($toolNamesSorted | Where-Object { $runtimeNames -notcontains $_ })),
                (Format-SynapseLimitedList -Items @($runtimeNames | Where-Object { $toolNamesSorted -notcontains $_ })))
        }

        $toolSchemas = @($sortedTools | ForEach-Object {
            $tool = $_
            $inputSchema = Get-SynapseObjectPropertyValue -Object $tool -Names @('inputSchema', 'input_schema')
            $outputSchema = Get-SynapseObjectPropertyValue -Object $tool -Names @('outputSchema', 'output_schema')
            $toolCanonical = Get-SynapseCanonicalJson -Value $tool
            [ordered]@{
                name = [string]$tool.name
                description = [string]$tool.description
                input_schema = $inputSchema
                input_schema_sha256 = Get-SynapseSha256Hex -Text (Get-SynapseCanonicalJson -Value $inputSchema)
                output_schema = $outputSchema
                output_schema_sha256 = if ($null -eq $outputSchema) { $null } else { Get-SynapseSha256Hex -Text (Get-SynapseCanonicalJson -Value $outputSchema) }
                tool_sha256 = Get-SynapseSha256Hex -Text $toolCanonical
            }
        })
        $canonical = Get-SynapseCanonicalJson -Value ([ordered]@{
            mcp_surface = 'tools/list'
            tools = $sortedTools
        })
        $setupCanonicalHash = Get-SynapseSha256Hex -Text $canonical
        $daemonPid = try { [int]$Health.pid } catch { $null }

        $surface = [pscustomobject]([ordered]@{
            schema = 2
            created_at_utc = [DateTime]::UtcNow.ToString('o')
            bind = $Bind
            daemon_pid = $daemonPid
            tool_count = $toolNames.Count
            tool_surface_sha256 = $runtimeHash
            tool_surface_sha256_source = 'mcp_health_tool'
            tool_surface_setup_canonical_sha256 = $setupCanonicalHash
            tool_names = $toolNames
            tool_schemas = $toolSchemas
        })
        $mcpReadSucceeded = $true
        return $surface
    } finally {
        if (-not [string]::IsNullOrWhiteSpace($sessionId)) {
            Close-SynapseMcpSetupSession -Bind $Bind -Token $Token -SessionId $sessionId -Required:$mcpReadSucceeded
        }
    }
}

function Get-SynapseFileSha256 {
    param([Parameter(Mandatory=$true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        Die "SYNAPSE_FILE_HASH_MISSING path=$Path remediation=build or install the daemon binary before hashing it"
    }
    try {
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try {
            $share = [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
            # [System.IO.File]::Open() hands back a FileStream with the .NET
            # default 4 KiB private buffer, so hashing the ~254 MB daemon binary
            # cost ~65,000 read syscalls. This function is the single most-called
            # expensive primitive in setup: every artifact, backup, staged copy,
            # runtime companion, and installed-identity readback goes through it,
            # roughly ten full passes over that binary per run.
            #
            # Measured on this host (AMD 9950X3D, NVMe), 254 MB binary:
            #   4 KiB (old): 770 ms cold / ~223 ms warm
            #   1 MiB (new): 149 ms cold / ~140 ms warm
            # A buffer sweep put the optimum plateau at 64 KiB-4 MiB (all within
            # noise of each other); 1 MiB is what dotnet/runtime's FileStream
            # guidance recommends for files in the 100 MB-7 GB range, and
            # PowerShell made the same fix to Get-FileHash itself in PR #20881
            # after finding the 4 KiB default indefensible for large files.
            #
            # This is a pure read-path change: every caller still physically
            # re-reads and re-hashes all bytes, so no readback is weakened or
            # memoized away. Only the syscall count drops.
            $bufferSize = 1048576
            $stream = New-Object System.IO.FileStream(
                $Path,
                [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::Read,
                $share,
                $bufferSize,
                [System.IO.FileOptions]::SequentialScan)
            try {
                $hash = $sha.ComputeHash($stream)
            } finally {
                $stream.Dispose()
            }
        } finally {
            $sha.Dispose()
        }
        # Byte-for-byte identical to the previous `ForEach-Object { 'X2' } -join`
        # (uppercase, unseparated) but without allocating a pipeline object per
        # digest byte: 87 ms vs 888 ms over 20,000 digests.
        return [BitConverter]::ToString($hash).Replace('-', '')
    } catch {
        Die "SYNAPSE_FILE_HASH_FAILED path=$Path error=$($_.Exception.Message) remediation=verify the file exists, is readable by this user, and is not protected by an exclusive writer before retrying setup"
    }
}

function Get-SynapseFullPathForScopeCheck {
    param([Parameter(Mandatory=$true)][string]$Path)
    return [System.IO.Path]::GetFullPath($Path).TrimEnd(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar)
}

function Assert-SynapseProfileRelativePathSafe {
    param(
        [Parameter(Mandatory=$true)][string]$RelativePath,
        [Parameter(Mandatory=$true)][string]$Context
    )

    if ([string]::IsNullOrWhiteSpace($RelativePath)) {
        Die "SYNAPSE_PROFILE_RELATIVE_PATH_EMPTY context=$Context remediation=bundled profile manifests must use non-empty relative paths"
    }
    if ($RelativePath -match '^[A-Za-z]:|^\\\\') {
        Die "SYNAPSE_PROFILE_RELATIVE_PATH_ABSOLUTE context=$Context relative_path=$RelativePath remediation=bundled profile manifests must not contain absolute paths"
    }
    $normalized = $RelativePath.Replace('\', '/')
    foreach ($segment in @($normalized.Split('/'))) {
        if ([string]::IsNullOrWhiteSpace($segment) -or $segment -eq '.' -or $segment -eq '..') {
            Die "SYNAPSE_PROFILE_RELATIVE_PATH_UNSAFE context=$Context relative_path=$RelativePath segment=$segment remediation=bundled profile manifests must not contain empty, current-directory, or parent-directory segments"
        }
    }
    return $normalized
}

function Join-SynapseProfileRelativePath {
    param(
        [Parameter(Mandatory=$true)][string]$BaseDir,
        [Parameter(Mandatory=$true)][string]$RelativePath,
        [Parameter(Mandatory=$true)][string]$Context
    )

    $normalized = Assert-SynapseProfileRelativePathSafe -RelativePath $RelativePath -Context $Context
    $baseFull = Get-SynapseFullPathForScopeCheck -Path $BaseDir
    $baseWithSep = $baseFull + [System.IO.Path]::DirectorySeparatorChar
    $nativeRelative = $normalized.Replace('/', [string][System.IO.Path]::DirectorySeparatorChar)
    $candidate = [System.IO.Path]::GetFullPath((Join-Path $baseFull $nativeRelative))
    if (-not $candidate.StartsWith($baseWithSep, [System.StringComparison]::OrdinalIgnoreCase)) {
        Die "SYNAPSE_PROFILE_RELATIVE_PATH_SCOPE_ESCAPE context=$Context base=$baseFull relative_path=$RelativePath resolved=$candidate remediation=bundled profile reconciliation refuses paths that escape the profile directory"
    }
    return $candidate
}

function Get-SynapseProfileRelativePath {
    param(
        [Parameter(Mandatory=$true)][string]$BaseDir,
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$Context
    )

    $baseFull = Get-SynapseFullPathForScopeCheck -Path $BaseDir
    $baseWithSep = $baseFull + [System.IO.Path]::DirectorySeparatorChar
    $full = [System.IO.Path]::GetFullPath($Path)
    if (-not $full.StartsWith($baseWithSep, [System.StringComparison]::OrdinalIgnoreCase)) {
        Die "SYNAPSE_PROFILE_PATH_SCOPE_ESCAPE context=$Context base=$baseFull path=$full remediation=bundled profile reconciliation refuses to classify files outside the profile root"
    }
    return (Assert-SynapseProfileRelativePathSafe -RelativePath ($full.Substring($baseWithSep.Length).Replace('\', '/')) -Context $Context)
}

function Test-SynapseSetupProfileMetadataRelativePath {
    param([Parameter(Mandatory=$true)][string]$RelativePath)

    $normalized = $RelativePath.Replace('\', '/')
    return (
        $normalized -ieq $script:SynapseBundledProfilesManifestFileName -or
        $normalized.StartsWith("$($script:SynapseBundledProfilesQuarantineDirName)/", [System.StringComparison]::OrdinalIgnoreCase) -or
        $normalized.StartsWith("$($script:SynapseBundledProfilesRollbackDirName)/", [System.StringComparison]::OrdinalIgnoreCase))
}

function New-SynapseCaseInsensitiveMap {
    return (New-Object System.Collections.Hashtable ([System.StringComparer]::OrdinalIgnoreCase))
}

function Read-SynapseProfileTomlId {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$RelativePath
    )

    $reader = $null
    try {
        $reader = [System.IO.File]::OpenText($Path)
        while ($null -ne ($line = $reader.ReadLine())) {
            if ($line -match '^\s*#') { continue }
            if ($line -match '^\s*\[') { break }
            if ($line -match '^\s*id\s*=\s*"([^"]+)"') {
                return $Matches[1]
            }
        }
    } catch {
        Die "SYNAPSE_PROFILE_ID_READ_FAILED path=$Path relative_path=$RelativePath error=$($_.Exception.Message) remediation=verify the bundled profile TOML is readable before setup can deploy it"
    } finally {
        if ($null -ne $reader) {
            $reader.Dispose()
        }
    }
    return ''
}

function Get-SynapseBundledProfileSourceEntries {
    param([Parameter(Mandatory=$true)][string]$SourceProfilesDir)

    if (-not (Test-Path -LiteralPath $SourceProfilesDir -PathType Container)) {
        Die "SYNAPSE_BUNDLED_PROFILES_SOURCE_MISSING path=$SourceProfilesDir remediation=provide the repository bundled profile directory before setup can reconcile installed profiles"
    }

    $entries = @()
    foreach ($file in @(Get-ChildItem -LiteralPath $SourceProfilesDir -File -Recurse -ErrorAction Stop)) {
        $relative = Get-SynapseProfileRelativePath -BaseDir $SourceProfilesDir -Path $file.FullName -Context 'source_bundled_profiles'
        if (Test-SynapseSetupProfileMetadataRelativePath -RelativePath $relative) { continue }
        $isTopLevelProfile = ($relative -notmatch '/' -and $relative.EndsWith('.toml', [System.StringComparison]::OrdinalIgnoreCase))
        $entries += [pscustomobject]@{
            relative_path = $relative
            source_path = $file.FullName
            sha256 = Get-SynapseFileSha256 -Path $file.FullName
            length = [int64]$file.Length
            last_write_time_utc = $file.LastWriteTimeUtc.ToString('o')
            profile_file = [bool]$isTopLevelProfile
        }
    }

    $entries = @($entries | Sort-Object relative_path)
    if ($entries.Count -lt 1) {
        Die "SYNAPSE_BUNDLED_PROFILES_SOURCE_EMPTY path=$SourceProfilesDir remediation=repository bundled profile source must contain at least one file"
    }

    $pathMap = New-SynapseCaseInsensitiveMap
    foreach ($entry in $entries) {
        if ($pathMap.ContainsKey($entry.relative_path)) {
            Die "SYNAPSE_BUNDLED_PROFILES_DUPLICATE_RELATIVE_PATH path=$SourceProfilesDir relative_path=$($entry.relative_path) remediation=remove case-colliding bundled profile files before setup can deploy them"
        }
        $pathMap[$entry.relative_path] = $entry
    }

    $profileEntries = @($entries | Where-Object { $_.profile_file })
    if ($profileEntries.Count -lt 1) {
        Die "SYNAPSE_BUNDLED_PROFILES_SOURCE_NO_TOML path=$SourceProfilesDir remediation=profile-dependent tools need at least one top-level bundled .toml profile"
    }

    $ids = @()
    foreach ($entry in $profileEntries) {
        $id = Read-SynapseProfileTomlId -Path $entry.source_path -RelativePath $entry.relative_path
        if ([string]::IsNullOrWhiteSpace($id)) {
            Die "SYNAPSE_BUNDLED_PROFILE_ID_MISSING path=$($entry.source_path) relative_path=$($entry.relative_path) remediation=bundled profile TOML must declare a top-level id before any table"
        }
        $ids += [pscustomobject]@{ id = $id; relative_path = $entry.relative_path }
    }
    $duplicateIds = @($ids | Group-Object id | Where-Object { $_.Count -gt 1 })
    if ($duplicateIds.Count -gt 0) {
        $detail = (($duplicateIds | ForEach-Object {
            "$($_.Name):$((@($_.Group | ForEach-Object { $_.relative_path }) -join ','))"
        }) -join ';')
        Die "SYNAPSE_BUNDLED_PROFILE_DUPLICATE_ID path=$SourceProfilesDir duplicates=$detail remediation=profile ids must be unique before setup can deploy a coherent bundled set"
    }

    return $entries
}

function Read-SynapseBundledProfilesManifest {
    param([Parameter(Mandatory=$true)][string]$ManifestPath)

    if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
        return $null
    }
    try {
        $manifest = Get-Content -Raw -LiteralPath $ManifestPath | ConvertFrom-Json -ErrorAction Stop
    } catch {
        Die "SYNAPSE_BUNDLED_PROFILES_MANIFEST_INVALID_JSON path=$ManifestPath error=$($_.Exception.Message) remediation=manifest exists but is unreadable; inspect it before setup can safely decide which deployed profiles it owns"
    }

    if ([int]$manifest.schema_version -ne 1) {
        Die "SYNAPSE_BUNDLED_PROFILES_MANIFEST_UNSUPPORTED path=$ManifestPath schema_version=$($manifest.schema_version) remediation=setup only understands bundled profile manifest schema_version=1"
    }
    if ([string]$manifest.owner -ne 'synapse-setup:bundled-profiles') {
        Die "SYNAPSE_BUNDLED_PROFILES_MANIFEST_OWNER_MISMATCH path=$ManifestPath owner=$($manifest.owner) remediation=setup refuses to use a manifest it did not create"
    }

    $seen = New-SynapseCaseInsensitiveMap
    foreach ($file in @($manifest.files)) {
        $relative = Assert-SynapseProfileRelativePathSafe -RelativePath ([string]$file.relative_path) -Context 'previous_bundled_profiles_manifest'
        if ($seen.ContainsKey($relative)) {
            Die "SYNAPSE_BUNDLED_PROFILES_MANIFEST_DUPLICATE_PATH path=$ManifestPath relative_path=$relative remediation=repair duplicate manifest entries before setup can safely prune retired bundled profiles"
        }
        if ([string]::IsNullOrWhiteSpace([string]$file.sha256) -or ([string]$file.sha256) -notmatch '^[0-9A-Fa-f]{64}$') {
            Die "SYNAPSE_BUNDLED_PROFILES_MANIFEST_BAD_HASH path=$ManifestPath relative_path=$relative sha256=$($file.sha256) remediation=manifest file entries must carry valid SHA-256 hashes"
        }
        $seen[$relative] = $true
    }

    return $manifest
}

function Convert-SynapseEntriesToMap {
    param([Parameter(Mandatory=$true)]$Entries)

    $map = New-SynapseCaseInsensitiveMap
    foreach ($entry in @($Entries)) {
        $relative = Assert-SynapseProfileRelativePathSafe -RelativePath ([string]$entry.relative_path) -Context 'profile_entry_map'
        $map[$relative] = $entry
    }
    return $map
}

function Copy-SynapseProfileFileWithHashReadback {
    param(
        [Parameter(Mandatory=$true)][string]$Source,
        [Parameter(Mandatory=$true)][string]$Destination,
        [Parameter(Mandatory=$true)][string]$ExpectedSha256,
        [Parameter(Mandatory=$true)][string]$Context
    )

    $parent = Split-Path -Parent $Destination
    if (-not [string]::IsNullOrWhiteSpace($parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
    $actual = Get-SynapseFileSha256 -Path $Destination
    if ($actual -ne $ExpectedSha256) {
        Die "SYNAPSE_PROFILE_COPY_HASH_MISMATCH context=$Context source=$Source destination=$Destination expected_sha256=$ExpectedSha256 actual_sha256=$actual remediation=copied profile bytes changed during copy; setup refuses to install an incoherent bundled profile set"
    }
}

function New-SynapseBundledProfilesManifestObject {
    param(
        [Parameter(Mandatory=$true)][string]$SourceProfilesDir,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [Parameter(Mandatory=$true)]$Entries,
        [Parameter(Mandatory=$true)]$Quarantined,
        [Parameter(Mandatory=$true)]$PreservedCustomProfiles,
        [Parameter(Mandatory=$true)]$PreservedLegacyRetiredHashMismatches
    )

    $fileRecords = @($Entries | ForEach-Object {
        [ordered]@{
            relative_path = [string]$_.relative_path
            sha256 = [string]$_.sha256
            length = [int64]$_.length
            profile_file = [bool]$_.profile_file
        }
    })
    $fingerprintInput = [ordered]@{
        schema_version = 1
        files = $fileRecords
    }
    $sourceManifestSha256 = Get-SynapseSha256Hex -Text (Get-SynapseCanonicalJson -Value $fingerprintInput)

    return [ordered]@{
        schema_version = 1
        owner = 'synapse-setup:bundled-profiles'
        generated_at_utc = (Get-Date).ToUniversalTime().ToString('o')
        source_profiles_dir = [System.IO.Path]::GetFullPath($SourceProfilesDir)
        deployed_profiles_dir = [System.IO.Path]::GetFullPath($ProfilesDir)
        source_manifest_sha256 = $sourceManifestSha256
        bundled_file_count = $fileRecords.Count
        bundled_profile_count = @($fileRecords | Where-Object { $_.profile_file }).Count
        files = $fileRecords
        quarantined_retired = @($Quarantined)
        preserved_custom_profiles = @($PreservedCustomProfiles)
        preserved_legacy_retired_hash_mismatches = @($PreservedLegacyRetiredHashMismatches)
    }
}

function Install-SynapseBundledProfiles {
    param(
        [Parameter(Mandatory=$true)][string]$SourceProfilesDir,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [Parameter(Mandatory=$true)][string]$LogDir
    )

    New-Item -ItemType Directory -Force -Path $ProfilesDir | Out-Null
    $manifestPath = Join-Path $ProfilesDir $script:SynapseBundledProfilesManifestFileName
    $sourceEntries = @(Get-SynapseBundledProfileSourceEntries -SourceProfilesDir $SourceProfilesDir)
    $sourceMap = Convert-SynapseEntriesToMap -Entries $sourceEntries
    $previousManifest = Read-SynapseBundledProfilesManifest -ManifestPath $manifestPath
    $previousMap = New-SynapseCaseInsensitiveMap
    if ($previousManifest) {
        $previousMap = Convert-SynapseEntriesToMap -Entries @($previousManifest.files)
    }

    $stageRoot = New-SynapseSetupRunDirectory -Root $LogDir -Purpose 'profile-stage'
    foreach ($entry in $sourceEntries) {
        $stagedPath = Join-SynapseProfileRelativePath -BaseDir $stageRoot -RelativePath $entry.relative_path -Context 'stage_bundled_profiles'
        Copy-SynapseProfileFileWithHashReadback -Source $entry.source_path -Destination $stagedPath -ExpectedSha256 $entry.sha256 -Context 'stage_bundled_profiles'
    }

    $retireActions = @()
    foreach ($previous in @($previousMap.Values)) {
        $relative = [string]$previous.relative_path
        if ($sourceMap.ContainsKey($relative)) { continue }
        $target = Join-SynapseProfileRelativePath -BaseDir $ProfilesDir -RelativePath $relative -Context 'retire_previous_bundled_profile'
        if (Test-Path -LiteralPath $target -PathType Leaf) {
            $retireActions += [pscustomobject]@{
                relative_path = $relative
                path = $target
                prior_sha256 = ([string]$previous.sha256).ToUpperInvariant()
                reason = 'previous_manifest_absent_from_source'
                require_hash_match = $false
            }
        }
    }

    foreach ($legacy in @($script:SynapseLegacyRetiredBundledProfiles)) {
        $relative = Assert-SynapseProfileRelativePathSafe -RelativePath ([string]$legacy.relative_path) -Context 'legacy_retired_bundled_profile'
        if ($sourceMap.ContainsKey($relative)) { continue }
        if (@($retireActions | Where-Object { $_.relative_path -ieq $relative }).Count -gt 0) { continue }
        $target = Join-SynapseProfileRelativePath -BaseDir $ProfilesDir -RelativePath $relative -Context 'retire_legacy_bundled_profile'
        if (Test-Path -LiteralPath $target -PathType Leaf) {
            $retireActions += [pscustomobject]@{
                relative_path = $relative
                path = $target
                prior_sha256 = ([string]$legacy.sha256).ToUpperInvariant()
                reason = "legacy_retired_seed:$($legacy.issue)"
                require_hash_match = $true
            }
        }
    }

    $rollbackRoot = $null
    $quarantineRoot = $null
    $copiedNew = @()
    $overwritten = @()
    $quarantined = @()
    $preservedLegacyMismatches = @()

    try {
        foreach ($entry in $sourceEntries) {
            $relative = [string]$entry.relative_path
            $stagedPath = Join-SynapseProfileRelativePath -BaseDir $stageRoot -RelativePath $relative -Context 'install_staged_bundled_profile'
            $targetPath = Join-SynapseProfileRelativePath -BaseDir $ProfilesDir -RelativePath $relative -Context 'install_bundled_profile'
            $targetExists = Test-Path -LiteralPath $targetPath -PathType Leaf
            if ($targetExists) {
                $targetHash = Get-SynapseFileSha256 -Path $targetPath
                if ($targetHash -eq $entry.sha256) {
                    continue
                }
                if ([string]::IsNullOrWhiteSpace($rollbackRoot)) {
                    $rollbackRoot = New-SynapseSetupRunDirectory -Root (Join-Path $ProfilesDir $script:SynapseBundledProfilesRollbackDirName) -Purpose 'profiles-rollback'
                }
                $backupPath = Join-SynapseProfileRelativePath -BaseDir $rollbackRoot -RelativePath $relative -Context 'backup_existing_profile'
                Copy-SynapseProfileFileWithHashReadback -Source $targetPath -Destination $backupPath -ExpectedSha256 $targetHash -Context 'backup_existing_profile'
                $overwritten += [pscustomobject]@{
                    relative_path = $relative
                    target_path = $targetPath
                    backup_path = $backupPath
                    original_sha256 = $targetHash
                }
            } else {
                $copiedNew += [pscustomobject]@{
                    relative_path = $relative
                    target_path = $targetPath
                }
            }
            Copy-SynapseProfileFileWithHashReadback -Source $stagedPath -Destination $targetPath -ExpectedSha256 $entry.sha256 -Context 'install_bundled_profile'
        }

        foreach ($action in $retireActions) {
            if (-not (Test-Path -LiteralPath $action.path -PathType Leaf)) { continue }
            $currentHash = Get-SynapseFileSha256 -Path $action.path
            if ($action.require_hash_match -and $currentHash -ne $action.prior_sha256) {
                $preservedLegacyMismatches += [ordered]@{
                    relative_path = $action.relative_path
                    path = $action.path
                    expected_sha256 = $action.prior_sha256
                    actual_sha256 = $currentHash
                    reason = 'legacy_retired_seed_hash_mismatch_preserved_as_custom'
                }
                continue
            }

            if ([string]::IsNullOrWhiteSpace($quarantineRoot)) {
                $quarantineRoot = New-SynapseSetupRunDirectory -Root (Join-Path $ProfilesDir $script:SynapseBundledProfilesQuarantineDirName) -Purpose 'retired'
            }
            $quarantinePath = Join-SynapseProfileRelativePath -BaseDir $quarantineRoot -RelativePath $action.relative_path -Context 'quarantine_retired_bundled_profile'
            $quarantineParent = Split-Path -Parent $quarantinePath
            if (-not [string]::IsNullOrWhiteSpace($quarantineParent)) {
                New-Item -ItemType Directory -Force -Path $quarantineParent | Out-Null
            }
            Move-Item -LiteralPath $action.path -Destination $quarantinePath -Force
            if (Test-Path -LiteralPath $action.path -PathType Leaf) {
                Die "SYNAPSE_RETIRED_BUNDLED_PROFILE_QUARANTINE_FAILED path=$($action.path) quarantine=$quarantinePath remediation=setup moved a retired bundled profile but it remains active in the watched profile directory"
            }
            $quarantineHash = Get-SynapseFileSha256 -Path $quarantinePath
            if ($quarantineHash -ne $currentHash) {
                Die "SYNAPSE_RETIRED_BUNDLED_PROFILE_QUARANTINE_HASH_MISMATCH path=$($action.path) quarantine=$quarantinePath expected_sha256=$currentHash actual_sha256=$quarantineHash remediation=quarantined retired bundled profile bytes changed during move"
            }
            $quarantineRecord = [ordered]@{
                relative_path = $action.relative_path
                original_path = $action.path
                quarantine_path = $quarantinePath
                sha256 = $currentHash
                reason = $action.reason
            }
            $quarantined += $quarantineRecord
        }

        $deployMismatches = @()
        foreach ($entry in $sourceEntries) {
            $targetPath = Join-SynapseProfileRelativePath -BaseDir $ProfilesDir -RelativePath $entry.relative_path -Context 'verify_installed_bundled_profile'
            if (-not (Test-Path -LiteralPath $targetPath -PathType Leaf)) {
                $deployMismatches += "missing:$($entry.relative_path)"
                continue
            }
            $targetHash = Get-SynapseFileSha256 -Path $targetPath
            if ($targetHash -ne $entry.sha256) {
                $deployMismatches += "hash_mismatch:$($entry.relative_path):expected=$($entry.sha256):actual=$targetHash"
            }
        }
        if ($deployMismatches.Count -gt 0) {
            Die "SYNAPSE_BUNDLED_PROFILES_DEPLOY_VERIFY_FAILED mismatches=$($deployMismatches -join ';') remediation=installed bundled profile set does not match the staged source manifest"
        }

        $staleOwnedStillActive = @()
        foreach ($previous in @($previousMap.Values)) {
            $relative = [string]$previous.relative_path
            if ($sourceMap.ContainsKey($relative)) { continue }
            $targetPath = Join-SynapseProfileRelativePath -BaseDir $ProfilesDir -RelativePath $relative -Context 'verify_retired_previous_bundled_profile'
            if (Test-Path -LiteralPath $targetPath -PathType Leaf) {
                $staleOwnedStillActive += $relative
            }
        }
        foreach ($legacy in @($script:SynapseLegacyRetiredBundledProfiles)) {
            $relative = [string]$legacy.relative_path
            if ($sourceMap.ContainsKey($relative)) { continue }
            $targetPath = Join-SynapseProfileRelativePath -BaseDir $ProfilesDir -RelativePath $relative -Context 'verify_retired_legacy_bundled_profile'
            if (Test-Path -LiteralPath $targetPath -PathType Leaf) {
                $targetHash = Get-SynapseFileSha256 -Path $targetPath
                if ($targetHash -eq ([string]$legacy.sha256).ToUpperInvariant()) {
                    $staleOwnedStillActive += $relative
                }
            }
        }
        if ($staleOwnedStillActive.Count -gt 0) {
            Die "SYNAPSE_RETIRED_BUNDLED_PROFILES_STILL_ACTIVE profiles=$($staleOwnedStillActive -join ',') remediation=retired setup-owned profile files must be absent from the watched top-level deployed profile directory"
        }

        $customProfiles = @()
        foreach ($profile in @(Get-ChildItem -LiteralPath $ProfilesDir -Filter *.toml -File -ErrorAction SilentlyContinue | Sort-Object Name)) {
            $relative = Get-SynapseProfileRelativePath -BaseDir $ProfilesDir -Path $profile.FullName -Context 'classify_custom_profile'
            if ($sourceMap.ContainsKey($relative)) { continue }
            $customProfiles += [ordered]@{
                relative_path = $relative
                path = $profile.FullName
                sha256 = Get-SynapseFileSha256 -Path $profile.FullName
            }
        }

        $manifest = New-SynapseBundledProfilesManifestObject `
            -SourceProfilesDir $SourceProfilesDir `
            -ProfilesDir $ProfilesDir `
            -Entries $sourceEntries `
            -Quarantined $quarantined `
            -PreservedCustomProfiles $customProfiles `
            -PreservedLegacyRetiredHashMismatches $preservedLegacyMismatches
        $manifestTempPath = Join-Path $ProfilesDir (".{0}.{1}.tmp" -f $script:SynapseBundledProfilesManifestFileName, [Guid]::NewGuid().ToString('N'))
        Write-SynapseUtf8NoBomFile -Path $manifestTempPath -Text (($manifest | ConvertTo-Json -Depth 40) + "`n")
        Move-Item -LiteralPath $manifestTempPath -Destination $manifestPath -Force
        $manifestReadback = Read-SynapseBundledProfilesManifest -ManifestPath $manifestPath
        if ([string]$manifestReadback.source_manifest_sha256 -ne [string]$manifest.source_manifest_sha256) {
            Die "SYNAPSE_BUNDLED_PROFILES_MANIFEST_READBACK_MISMATCH path=$manifestPath expected_sha256=$($manifest.source_manifest_sha256) actual_sha256=$($manifestReadback.source_manifest_sha256) remediation=setup wrote the bundled profile ownership manifest but read back different content"
        }

        Info ("Bundled profile reconciliation complete source_files={0} bundled_profiles={1} custom_profiles={2} quarantined_retired={3} legacy_hash_mismatch_preserved={4} manifest={5} source_manifest_sha256={6}" -f `
            $sourceEntries.Count,
            @($sourceEntries | Where-Object { $_.profile_file }).Count,
            $customProfiles.Count,
            $quarantined.Count,
            $preservedLegacyMismatches.Count,
            $manifestPath,
            $manifest.source_manifest_sha256)
        return [pscustomobject]@{
            ManifestPath = $manifestPath
            SourceManifestSha256 = [string]$manifest.source_manifest_sha256
            BundledFileCount = $sourceEntries.Count
            BundledProfileCount = @($sourceEntries | Where-Object { $_.profile_file }).Count
            CustomProfileCount = $customProfiles.Count
            QuarantinedRetiredCount = $quarantined.Count
        }
    } catch {
        $originalError = $_.Exception.Message
        $rollbackErrors = @()
        foreach ($newFile in @($copiedNew | Sort-Object relative_path -Descending)) {
            try {
                if (Test-Path -LiteralPath $newFile.target_path -PathType Leaf) {
                    Remove-Item -LiteralPath $newFile.target_path -Force
                }
            } catch {
                $rollbackErrors += "remove_new:$($newFile.relative_path):$($_.Exception.Message)"
            }
        }
        foreach ($backup in @($overwritten | Sort-Object relative_path)) {
            try {
                Copy-SynapseProfileFileWithHashReadback -Source $backup.backup_path -Destination $backup.target_path -ExpectedSha256 $backup.original_sha256 -Context 'rollback_existing_profile'
            } catch {
                $rollbackErrors += "restore_overwritten:$($backup.relative_path):$($_.Exception.Message)"
            }
        }
        foreach ($retired in @($quarantined | Sort-Object relative_path)) {
            try {
                if ((Test-Path -LiteralPath $retired.quarantine_path -PathType Leaf) -and -not (Test-Path -LiteralPath $retired.original_path -PathType Leaf)) {
                    $parent = Split-Path -Parent $retired.original_path
                    if (-not [string]::IsNullOrWhiteSpace($parent)) {
                        New-Item -ItemType Directory -Force -Path $parent | Out-Null
                    }
                    Move-Item -LiteralPath $retired.quarantine_path -Destination $retired.original_path -Force
                }
            } catch {
                $rollbackErrors += "restore_quarantined:$($retired.relative_path):$($_.Exception.Message)"
            }
        }
        if ($rollbackErrors.Count -gt 0) {
            Die "SYNAPSE_BUNDLED_PROFILES_RECONCILE_FAILED_ROLLBACK_FAILED original_error=[$originalError] rollback_errors=$($rollbackErrors -join ';') remediation=profile deployment failed and automatic rollback did not fully restore the prior deployed bytes; inspect $ProfilesDir before restarting the daemon"
        }
        Die "SYNAPSE_BUNDLED_PROFILES_RECONCILE_FAILED error=[$originalError] rollback=completed remediation=setup restored prior deployed profile bytes; fix the named profile deployment error and rerun setup"
    }
}

function New-SynapseSetupRunDirectory {
    param(
        [Parameter(Mandatory=$true)][string]$Root,
        [Parameter(Mandatory=$true)][string]$Purpose
    )

    $safePurpose = $Purpose -replace '[^A-Za-z0-9_.-]', '_'
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssfffZ')
    $path = Join-Path $Root "$safePurpose-$stamp-$PID"
    New-Item -ItemType Directory -Force -Path $path | Out-Null
    return $path
}

function Get-SynapseDaemonStagingDescriptor {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot
    )

    $rootFull = Get-SynapseFullPathForScopeCheck -Path $ExpectedRoot
    if (-not (Test-Path -LiteralPath $rootFull -PathType Container)) {
        throw "SYNAPSE_DAEMON_STAGING_ROOT_MISSING root=$rootFull remediation=do not delete anything; rerun setup so the exact setup-owned root can be inspected"
    }
    $rootItem = Get-Item -LiteralPath $rootFull -Force -ErrorAction Stop
    if ($rootItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "SYNAPSE_DAEMON_STAGING_ROOT_REPARSE_POINT root=$rootFull attributes=$($rootItem.Attributes) remediation=do not follow or delete this link; restore setup-staging as a physical directory"
    }

    $pathFull = Get-SynapseFullPathForScopeCheck -Path $Path
    $actualParent = Get-SynapseFullPathForScopeCheck -Path (Split-Path -Parent $pathFull)
    if (-not $actualParent.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "SYNAPSE_DAEMON_STAGING_SCOPE_INVALID path=$pathFull expected_parent=$rootFull actual_parent=$actualParent remediation=do not delete this path; inspect setup run-directory construction"
    }
    $leaf = Split-Path -Leaf $pathFull
    if ($leaf -notmatch '^daemon-binary-\d{8}T\d{9}Z-\d+$') {
        throw "SYNAPSE_DAEMON_STAGING_NAME_INVALID path=$pathFull leaf=$leaf remediation=do not delete this directory; only exact setup-owned daemon-binary timestamp/PID names are eligible"
    }
    if (-not (Test-Path -LiteralPath $pathFull -PathType Container)) {
        throw "SYNAPSE_DAEMON_STAGING_DIRECTORY_MISSING path=$pathFull remediation=refresh the setup-staging Source of Truth before retrying cleanup"
    }
    $directory = Get-Item -LiteralPath $pathFull -Force -ErrorAction Stop
    if ($directory.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "SYNAPSE_DAEMON_STAGING_DIRECTORY_REPARSE_POINT path=$pathFull attributes=$($directory.Attributes) remediation=do not follow or delete this link; inspect the setup-staging directory"
    }

    $items = @(Get-ChildItem -LiteralPath $pathFull -Force -ErrorAction Stop)
    if ($items.Count -gt 4) {
        throw "SYNAPSE_DAEMON_STAGING_ENTRY_BOUND_EXCEEDED path=$pathFull count=$($items.Count) max=4 remediation=do not delete this directory; inspect the unexpected setup-staging contents"
    }
    $invalid = @($items | Where-Object {
        $_.PSIsContainer -or
        ($_.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or
        $_.Name -notmatch '^(synapse-mcp-[0-9A-Fa-f]{64}\.exe|onnxruntime\.dll|onnxruntime_providers_shared\.dll|onnxruntime_providers_cuda\.dll)$'
    })
    if ($invalid.Count -gt 0) {
        $invalidState = ($invalid | ForEach-Object { "name=$($_.Name),container=$($_.PSIsContainer),attributes=$($_.Attributes)" }) -join ';'
        throw "SYNAPSE_DAEMON_STAGING_CONTENT_INVALID path=$pathFull invalid=[$invalidState] remediation=do not delete this directory; inspect why an unowned entry exists under setup-staging"
    }
    $executables = @($items | Where-Object { $_.Name -match '^synapse-mcp-[0-9A-Fa-f]{64}\.exe$' })
    if ($items.Count -gt 0 -and $executables.Count -ne 1) {
        throw "SYNAPSE_DAEMON_STAGING_EXECUTABLE_CARDINALITY_INVALID path=$pathFull item_count=$($items.Count) executable_count=$($executables.Count) remediation=do not delete this directory; setup-owned non-empty staging must contain exactly one hash-named daemon executable"
    }
    $bytes = ($items | Measure-Object -Property Length -Sum).Sum
    if ($null -eq $bytes) { $bytes = 0 }
    return [pscustomobject]@{
        Path = $pathFull
        ItemCount = $items.Count
        Bytes = [int64]$bytes
    }
}

function Get-SynapseWindowsMemoryReadback {
    $readback = [ordered]@{
        schema = 'synapse_setup_windows_memory_readback/v1'
        observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
        source_of_truth = 'Win32_OperatingSystem + Win32_PageFileUsage'
        total_physical_bytes = $null
        available_physical_bytes = $null
        commit_limit_bytes = $null
        commit_available_bytes = $null
        commit_charge_bytes = $null
        commit_percent = $null
        pagefiles = @()
        read_succeeded = $false
        error = $null
    }
    try {
        $os = Get-CimInstance Win32_OperatingSystem -ErrorAction Stop
        $totalPhysical = [uint64]$os.TotalVisibleMemorySize * [uint64]1024
        $availablePhysical = [uint64]$os.FreePhysicalMemory * [uint64]1024
        $commitLimit = [uint64]$os.TotalVirtualMemorySize * [uint64]1024
        $commitAvailable = [uint64]$os.FreeVirtualMemory * [uint64]1024
        $commitCharge = if ($commitLimit -ge $commitAvailable) {
            $commitLimit - $commitAvailable
        } else {
            [uint64]0
        }
        $readback.total_physical_bytes = $totalPhysical
        $readback.available_physical_bytes = $availablePhysical
        $readback.commit_limit_bytes = $commitLimit
        $readback.commit_available_bytes = $commitAvailable
        $readback.commit_charge_bytes = $commitCharge
        $readback.commit_percent = if ($commitLimit -gt 0) {
            [math]::Round(([double]$commitCharge / [double]$commitLimit) * 100, 2)
        } else {
            $null
        }
        $readback.pagefiles = @(Get-CimInstance Win32_PageFileUsage -ErrorAction Stop |
            Select-Object Name, AllocatedBaseSize, CurrentUsage, PeakUsage, TempPageFile)
        $readback.read_succeeded = $true
    } catch {
        $readback.error = $_.Exception.Message
    }
    return [pscustomobject]$readback
}

function Assert-SynapseDaemonStagingHasNoLiveExecutable {
    param([Parameter(Mandatory=$true)][string]$Path)

    $pathFull = Get-SynapseFullPathForScopeCheck -Path $Path
    $pathPrefix = $pathFull + [System.IO.Path]::DirectorySeparatorChar
    $processes = @(Get-CimInstance Win32_Process -ErrorAction Stop)
    foreach ($process in $processes) {
        if ([string]::IsNullOrWhiteSpace([string]$process.ExecutablePath)) {
            if ([string]$process.Name -like 'synapse-mcp*') {
                throw "SYNAPSE_DAEMON_STAGING_PROCESS_PATH_UNREADABLE pid=$($process.ProcessId) name=$($process.Name) staging_path=$pathFull remediation=do not delete staging while a Synapse executable path cannot be read; inspect this exact PID"
            }
            continue
        }
        $executableFull = [System.IO.Path]::GetFullPath([string]$process.ExecutablePath)
        if ($executableFull.StartsWith($pathPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "SYNAPSE_DAEMON_STAGING_EXECUTABLE_LIVE pid=$($process.ProcessId) executable=$executableFull staging_path=$pathFull remediation=stop only this exact setup-owned candidate process, verify it exited, then rerun setup cleanup"
        }
    }
}

function Remove-SynapseDaemonStagingDirectory {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot,
        [string]$Reason = 'setup_cleanup'
    )

    # Independently re-read the complete descriptor and process SoT at the
    # destructive boundary; callers may have supplied a stale enumeration.
    $descriptor = Get-SynapseDaemonStagingDescriptor -Path $Path -ExpectedRoot $ExpectedRoot
    Assert-SynapseDaemonStagingHasNoLiveExecutable -Path $descriptor.Path
    foreach ($file in @(Get-ChildItem -LiteralPath $descriptor.Path -File -Force -ErrorAction Stop)) {
        Remove-Item -LiteralPath $file.FullName -Force -ErrorAction Stop
        if (Test-Path -LiteralPath $file.FullName) {
            throw "SYNAPSE_DAEMON_STAGING_FILE_CLEANUP_UNVERIFIED path=$($file.FullName) reason=$Reason remediation=inspect filesystem permissions and handles; the exact setup-owned file still exists after Remove-Item"
        }
    }
    Remove-Item -LiteralPath $descriptor.Path -Force -ErrorAction Stop
    if (Test-Path -LiteralPath $descriptor.Path) {
        throw "SYNAPSE_DAEMON_STAGING_CLEANUP_UNVERIFIED path=$($descriptor.Path) reason=$Reason remediation=inspect filesystem permissions and handles; the exact directory still exists after Remove-Item"
    }
    Info "Daemon staging artifact removed reason=$Reason path=$($descriptor.Path) item_count=$($descriptor.ItemCount) bytes=$($descriptor.Bytes) readback_exists=false"
    return $descriptor
}

function Remove-SynapseStaleDaemonStagingArtifacts {
    param([Parameter(Mandatory=$true)][string]$LogDir)

    $stagingRoot = Get-SynapseFullPathForScopeCheck -Path (Join-Path $LogDir 'setup-staging')
    if (-not (Test-Path -LiteralPath $stagingRoot)) {
        Info "Daemon staging stale sweep root=$stagingRoot before_count=0 before_bytes=0 after_count=0 after_bytes=0"
        return
    }
    if (-not (Test-Path -LiteralPath $stagingRoot -PathType Container)) {
        throw "SYNAPSE_DAEMON_STAGING_ROOT_NOT_DIRECTORY root=$stagingRoot remediation=do not delete this path; restore setup-staging as a physical directory"
    }
    $rootItem = Get-Item -LiteralPath $stagingRoot -Force -ErrorAction Stop
    if ($rootItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "SYNAPSE_DAEMON_STAGING_ROOT_REPARSE_POINT root=$stagingRoot attributes=$($rootItem.Attributes) remediation=do not follow or delete this link; restore setup-staging as a physical directory"
    }
    $children = @(Get-ChildItem -LiteralPath $stagingRoot -Force -ErrorAction Stop)
    if ($children.Count -gt 4096) {
        throw "SYNAPSE_DAEMON_STAGING_DIRECTORY_BOUND_EXCEEDED root=$stagingRoot count=$($children.Count) max=4096 remediation=inspect the unexpectedly large setup-owned root before cleanup"
    }
    $nonDirectories = @($children | Where-Object { -not $_.PSIsContainer })
    if ($nonDirectories.Count -gt 0) {
        $names = ($nonDirectories | Select-Object -ExpandProperty Name) -join ','
        throw "SYNAPSE_DAEMON_STAGING_ROOT_CONTENT_INVALID root=$stagingRoot files=[$names] remediation=do not delete anything; only setup-owned run directories may exist directly under this root"
    }

    $validated = @()
    foreach ($child in @($children | Sort-Object Name)) {
        $validated += Get-SynapseDaemonStagingDescriptor -Path $child.FullName -ExpectedRoot $stagingRoot
    }
    $beforeBytes = ($validated | Measure-Object -Property Bytes -Sum).Sum
    if ($null -eq $beforeBytes) { $beforeBytes = 0 }
    foreach ($descriptor in $validated) {
        [void](Remove-SynapseDaemonStagingDirectory -Path $descriptor.Path -ExpectedRoot $stagingRoot -Reason 'startup_stale_sweep')
    }
    $after = @(Get-ChildItem -LiteralPath $stagingRoot -Force -ErrorAction Stop)
    $afterBytes = ($after | Where-Object { -not $_.PSIsContainer } | Measure-Object -Property Length -Sum).Sum
    if ($null -eq $afterBytes) { $afterBytes = 0 }
    if ($after.Count -ne 0 -or [int64]$afterBytes -ne 0) {
        throw "SYNAPSE_DAEMON_STAGING_SWEEP_READBACK_FAILED root=$stagingRoot before_count=$($validated.Count) before_bytes=$beforeBytes after_count=$($after.Count) after_bytes=$afterBytes remediation=inspect the exact remaining setup-staging entries before rerunning setup"
    }
    Info "Daemon staging stale sweep root=$stagingRoot before_count=$($validated.Count) before_bytes=$beforeBytes after_count=0 after_bytes=0"
}

function Remove-SynapseCurrentDaemonStagingArtifact {
    if ([string]::IsNullOrWhiteSpace([string]$script:SynapseCurrentDaemonStagingDirectory)) {
        return
    }
    $stagingRoot = Get-SynapseFullPathForScopeCheck -Path (Join-Path $LogDir 'setup-staging')
    [void](Remove-SynapseDaemonStagingDirectory -Path $script:SynapseCurrentDaemonStagingDirectory -ExpectedRoot $stagingRoot -Reason 'current_setup_run')
    $script:SynapseCurrentDaemonStagingDirectory = $null
}

function Get-SynapseAcquisitionTempArtifactDescriptor {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot
    )

    $rootFull = Get-SynapseFullPathForScopeCheck -Path $ExpectedRoot
    $pathFull = [System.IO.Path]::GetFullPath($Path)
    $rootPrefix = $rootFull + [System.IO.Path]::DirectorySeparatorChar
    if ($pathFull -eq $rootFull -or
        -not $pathFull.StartsWith($rootPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "SYNAPSE_ACQUISITION_TEMP_SCOPE_ESCAPE root=$rootFull path=$pathFull remediation=do not delete anything; acquisition cleanup accepts only descendants of the exact setup-owned root"
    }
    if (-not (Test-Path -LiteralPath $pathFull)) {
        return [pscustomobject][ordered]@{
            Path = $pathFull
            Exists = $false
            IsContainer = $false
            ItemCount = 0
            Bytes = [int64]0
        }
    }
    if (-not (Test-Path -LiteralPath $rootFull -PathType Container)) {
        throw "SYNAPSE_ACQUISITION_ROOT_NOT_DIRECTORY root=$rootFull path=$pathFull remediation=do not delete anything; restore the setup-owned acquisition root as a physical directory"
    }
    $ancestor = Get-Item -LiteralPath $rootFull -Force -ErrorAction Stop
    while ($true) {
        if ($ancestor.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            throw "SYNAPSE_ACQUISITION_TEMP_ANCESTOR_REPARSE_POINT root=$rootFull path=$pathFull ancestor=$($ancestor.FullName) attributes=$($ancestor.Attributes) remediation=do not follow or delete through this link; restore the setup-owned acquisition tree as physical directories"
        }
        if ($ancestor.FullName -eq $rootFull) { break }
        $ancestor = $ancestor.Parent
        if ($null -eq $ancestor) {
            throw "SYNAPSE_ACQUISITION_TEMP_ANCESTOR_READ_FAILED root=$rootFull path=$pathFull remediation=do not delete anything; the candidate ancestry did not reach its validated root"
        }
    }
    $parentPath = Split-Path -Parent $pathFull
    $parent = Get-Item -LiteralPath $parentPath -Force -ErrorAction Stop
    while ($null -ne $parent -and $parent.FullName -ne $rootFull) {
        if ($parent.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            throw "SYNAPSE_ACQUISITION_TEMP_ANCESTOR_REPARSE_POINT root=$rootFull path=$pathFull ancestor=$($parent.FullName) attributes=$($parent.Attributes) remediation=do not follow or delete through this link; restore the setup-owned acquisition tree as physical directories"
        }
        $parent = $parent.Parent
    }
    if ($null -eq $parent) {
        throw "SYNAPSE_ACQUISITION_TEMP_ANCESTOR_READ_FAILED root=$rootFull path=$pathFull remediation=do not delete anything; the candidate ancestry did not reach its validated root"
    }
    $item = Get-Item -LiteralPath $pathFull -Force -ErrorAction Stop
    if ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "SYNAPSE_ACQUISITION_TEMP_REPARSE_POINT root=$rootFull path=$pathFull attributes=$($item.Attributes) remediation=do not follow or delete this link; inspect the exact setup-owned acquisition root"
    }
    if ($item.PSIsContainer) {
        $footprint = Get-SynapseDirectoryFootprint -Path $pathFull
        if (-not $footprint.complete) {
            $errors = @($footprint.read_errors_sample) -join ' | '
            throw "SYNAPSE_ACQUISITION_TEMP_FOOTPRINT_INCOMPLETE root=$rootFull path=$pathFull read_error_count=$($footprint.read_error_count) errors=$errors remediation=repair access to the exact temp directory before setup retries cleanup"
        }
        return [pscustomobject][ordered]@{
            Path = $pathFull
            Exists = $true
            IsContainer = $true
            ItemCount = [int64]$footprint.file_count + 1
            Bytes = [int64]$footprint.byte_len
        }
    }
    return [pscustomobject][ordered]@{
        Path = $pathFull
        Exists = $true
        IsContainer = $false
        ItemCount = 1
        Bytes = [int64]$item.Length
    }
}

function Remove-SynapseAcquisitionTempArtifact {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    $descriptor = Get-SynapseAcquisitionTempArtifactDescriptor -Path $Path -ExpectedRoot $ExpectedRoot
    if (-not $descriptor.Exists) { return $descriptor }
    try {
        if ($descriptor.IsContainer) {
            Remove-Item -LiteralPath $descriptor.Path -Recurse -Force -ErrorAction Stop
        } else {
            Remove-Item -LiteralPath $descriptor.Path -Force -ErrorAction Stop
        }
    } catch {
        throw "SYNAPSE_ACQUISITION_TEMP_CLEANUP_FAILED reason=$Reason root=$ExpectedRoot path=$($descriptor.Path) item_count=$($descriptor.ItemCount) bytes=$($descriptor.Bytes) error=$($_.Exception.Message) remediation=close the exact process holding this setup-owned temp artifact, repair its permissions, and rerun setup"
    }
    if (Test-Path -LiteralPath $descriptor.Path) {
        throw "SYNAPSE_ACQUISITION_TEMP_CLEANUP_READBACK_FAILED reason=$Reason root=$ExpectedRoot path=$($descriptor.Path) item_count=$($descriptor.ItemCount) bytes=$($descriptor.Bytes) remediation=inspect the exact path; cleanup returned without removing its physical Source of Truth"
    }
    Info "Acquisition temp artifact removed reason=$Reason path=$($descriptor.Path) item_count=$($descriptor.ItemCount) bytes=$($descriptor.Bytes) readback_exists=false"
    return $descriptor
}

function Remove-SynapseStaleAcquisitionArtifacts {
    param([Parameter(Mandatory=$true)][string[]]$Roots)

    $observedAt = (Get-Date).ToUniversalTime().ToString('o')
    $candidates = @()
    $rootReadbacks = @()
    foreach ($requestedRoot in @($Roots | Sort-Object -Unique)) {
        $root = Get-SynapseFullPathForScopeCheck -Path $requestedRoot
        if (-not (Test-Path -LiteralPath $root)) {
            $rootReadbacks += [pscustomobject][ordered]@{ root = $root; exists = $false; enumerated_count = 0 }
            continue
        }
        if (-not (Test-Path -LiteralPath $root -PathType Container)) {
            throw "SYNAPSE_ACQUISITION_ROOT_NOT_DIRECTORY root=$root remediation=do not delete this path; restore the setup-owned acquisition root as a physical directory"
        }
        $rootItem = Get-Item -LiteralPath $root -Force -ErrorAction Stop
        if ($rootItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            throw "SYNAPSE_ACQUISITION_ROOT_REPARSE_POINT root=$root attributes=$($rootItem.Attributes) remediation=do not follow or delete this link; restore the setup-owned acquisition root as a physical directory"
        }
        $entries = @(Get-ChildItem -LiteralPath $root -Recurse -Force -ErrorAction Stop)
        if ($entries.Count -gt 65536) {
            throw "SYNAPSE_ACQUISITION_ROOT_BOUND_EXCEEDED root=$root count=$($entries.Count) max=65536 remediation=inspect the unexpectedly large setup-owned acquisition root before cleanup"
        }
        $rootReadbacks += [pscustomobject][ordered]@{ root = $root; exists = $true; enumerated_count = $entries.Count }
        foreach ($entry in $entries) {
            $ownerPidText = $null
            $kind = $null
            if ($entry.Name -match '\.download-(\d+)$') {
                if ($entry.PSIsContainer) {
                    throw "SYNAPSE_ACQUISITION_TEMP_SHAPE_INVALID root=$root path=$($entry.FullName) kind=download expected=file remediation=do not delete the unexpected directory; inspect how it was created"
                }
                $ownerPidText = $Matches[1]
                $kind = 'download'
            } elseif ($entry.Name -match '^extract-(\d+)$') {
                if (-not $entry.PSIsContainer) {
                    throw "SYNAPSE_ACQUISITION_TEMP_SHAPE_INVALID root=$root path=$($entry.FullName) kind=extract expected=directory remediation=do not delete the unexpected file; inspect how it was created"
                }
                $ownerPidText = $Matches[1]
                $kind = 'extract'
            } elseif ($entry.Name -match '^package-(\d+)\.zip$') {
                if ($entry.PSIsContainer) {
                    throw "SYNAPSE_ACQUISITION_TEMP_SHAPE_INVALID root=$root path=$($entry.FullName) kind=package_zip expected=file remediation=do not delete the unexpected directory; inspect how it was created"
                }
                $ownerPidText = $Matches[1]
                $kind = 'package_zip'
            } else {
                continue
            }
            $ownerPid = [int64]0
            if (-not [int64]::TryParse($ownerPidText, [ref]$ownerPid) -or
                $ownerPid -le 0 -or $ownerPid -gt [int]::MaxValue) {
                throw "SYNAPSE_ACQUISITION_TEMP_PID_INVALID root=$root path=$($entry.FullName) owner_pid=$ownerPidText remediation=do not delete the malformed setup temp artifact; inspect how it was created"
            }
            $descriptor = Get-SynapseAcquisitionTempArtifactDescriptor -Path $entry.FullName -ExpectedRoot $root
            $candidates += [pscustomobject][ordered]@{
                root = $root
                path = $descriptor.Path
                kind = $kind
                owner_pid = [int]$ownerPid
                item_count = $descriptor.ItemCount
                bytes = $descriptor.Bytes
            }
        }
    }

    $reaped = @()
    $retained = @()
    foreach ($candidate in @($candidates | Sort-Object { $_.path.Length } -Descending)) {
        $liveOwner = Get-Process -Id $candidate.owner_pid -ErrorAction SilentlyContinue
        if ($null -ne $liveOwner) {
            $retained += $candidate
            continue
        }
        $removed = Remove-SynapseAcquisitionTempArtifact `
            -Path $candidate.path `
            -ExpectedRoot $candidate.root `
            -Reason "stale_dead_owner_$($candidate.owner_pid)"
        if ($removed.Exists) { $reaped += $candidate }
    }

    foreach ($candidate in $reaped) {
        if (Test-Path -LiteralPath $candidate.path) {
            throw "SYNAPSE_ACQUISITION_STALE_SWEEP_READBACK_FAILED path=$($candidate.path) owner_pid=$($candidate.owner_pid) expected_exists=false remediation=inspect the exact setup-owned artifact that remained after cleanup"
        }
    }
    foreach ($candidate in $retained) {
        if (-not (Test-Path -LiteralPath $candidate.path)) {
            throw "SYNAPSE_ACQUISITION_LIVE_OWNER_ARTIFACT_CHANGED path=$($candidate.path) owner_pid=$($candidate.owner_pid) expected_exists=true remediation=a concurrent owner changed its temp artifact during setup; rerun after that acquisition finishes"
        }
    }
    $candidateBytes = ($candidates | Measure-Object -Property bytes -Sum).Sum
    if ($null -eq $candidateBytes) { $candidateBytes = 0 }
    $reclaimedBytes = ($reaped | Measure-Object -Property bytes -Sum).Sum
    if ($null -eq $reclaimedBytes) { $reclaimedBytes = 0 }
    $readback = [pscustomobject][ordered]@{
        schema = 'synapse_setup_acquisition_cleanup/v1'
        observed_at_utc = $observedAt
        completed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
        roots = @($rootReadbacks)
        candidate_count = $candidates.Count
        candidate_bytes = [int64]$candidateBytes
        reaped_count = $reaped.Count
        reclaimed_bytes = [int64]$reclaimedBytes
        retained_live_count = $retained.Count
        reaped = @($reaped)
        retained_live = @($retained)
    }
    Info "Acquisition stale sweep candidate_count=$($readback.candidate_count) candidate_bytes=$($readback.candidate_bytes) reaped_count=$($readback.reaped_count) reclaimed_bytes=$($readback.reclaimed_bytes) retained_live_count=$($readback.retained_live_count)"
    return $readback
}

function Get-SynapseOrtRuntimeCompanions {
    param([Parameter(Mandatory=$true)][string]$ExecutablePath)

    $directory = Split-Path -Parent $ExecutablePath
    $required = @('onnxruntime.dll', 'onnxruntime_providers_shared.dll', 'onnxruntime_providers_cuda.dll')
    $files = @()
    foreach ($name in $required) {
        $path = Join-Path $directory $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            Die "SYNAPSE_ORT_RUNTIME_COMPANION_MISSING executable=$ExecutablePath companion=$path remediation=the CUDA-enabled daemon is a runtime bundle; build or install the named ONNX Runtime provider DLL beside the executable"
        }
        $files += [pscustomobject]@{
            Name = $name
            Path = $path
            Sha256 = Get-SynapseFileSha256 -Path $path
        }
    }
    return @($files)
}

function Install-SynapsePinnedOrtGpuRuntime {
    param([Parameter(Mandatory=$true)][string]$Root)

    $version = '1.27.1'
    $packageSha256 = '07AD9D174F19BA47C13BF1989EE0C9A8D4832481E2EB51A16DD659A71162381F'
    $expectedFiles = [ordered]@{
        'onnxruntime.dll' = '75BBF0C47CB90E17F566DA46F50ECAAB4A0DA978E6D8C2B58FBB89708EA067DE'
        'onnxruntime_providers_shared.dll' = '3D8EF56BABC5D581153CB032DE6841CFBCC884533A3C74038CEB6DFB6CCB05AE'
        'onnxruntime_providers_cuda.dll' = '46766BA4A7F971A2D5F01569EA6CCFD501F5CF578F2257734CFD578DC28CAD2E'
    }
    $versionRoot = Join-Path $Root $version
    $packagePath = Join-Path $versionRoot "Microsoft.ML.OnnxRuntime.Gpu.Windows.$version.nupkg"
    $extractRoot = Join-Path $versionRoot 'package'
    $nativeDir = Join-Path $extractRoot 'runtimes\win-x64\native'
    New-Item -ItemType Directory -Force -Path $versionRoot | Out-Null
    if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
        $downloadPath = "$packagePath.download-$PID"
        try {
            try {
                Invoke-WebRequest -Uri "https://www.nuget.org/api/v2/package/Microsoft.ML.OnnxRuntime.Gpu.Windows/$version" -OutFile $downloadPath -ErrorAction Stop
            } catch {
                Die "SYNAPSE_ORT_RUNTIME_PACKAGE_DOWNLOAD_FAILED path=$downloadPath version=$version error=$($_.Exception.Message) remediation=verify network/TLS access to the authoritative Microsoft NuGet package endpoint and rerun setup"
            }
            $downloadHash = Get-SynapseFileSha256 -Path $downloadPath
            if ($downloadHash -ne $packageSha256) {
                Die "SYNAPSE_ORT_RUNTIME_PACKAGE_HASH_MISMATCH path=$downloadPath expected_sha256=$packageSha256 actual_sha256=$downloadHash remediation=do not install an unverified ONNX Runtime package; inspect the authoritative Microsoft NuGet release"
            }
            Move-Item -LiteralPath $downloadPath -Destination $packagePath -ErrorAction Stop
        } finally {
            [void](Remove-SynapseAcquisitionTempArtifact -Path $downloadPath -ExpectedRoot $Root -Reason 'current_ort_download')
        }
    }
    $packageReadback = Get-SynapseFileSha256 -Path $packagePath
    if ($packageReadback -ne $packageSha256) {
        Die "SYNAPSE_ORT_RUNTIME_PACKAGE_HASH_MISMATCH path=$packagePath expected_sha256=$packageSha256 actual_sha256=$packageReadback remediation=delete only this corrupt cached package and rerun setup to reacquire it from Microsoft NuGet"
    }
    if (-not (Test-Path -LiteralPath $nativeDir -PathType Container)) {
        $zipPath = Join-Path $versionRoot "package-$PID.zip"
        $extractAttempt = Join-Path $versionRoot "extract-$PID"
        try {
            try {
                Copy-Item -LiteralPath $packagePath -Destination $zipPath -Force -ErrorAction Stop
                New-Item -ItemType Directory -Force -Path $extractAttempt -ErrorAction Stop | Out-Null
                Expand-Archive -LiteralPath $zipPath -DestinationPath $extractAttempt -ErrorAction Stop
                Move-Item -LiteralPath $extractAttempt -Destination $extractRoot -ErrorAction Stop
            } catch {
                Die "SYNAPSE_ORT_RUNTIME_PACKAGE_EXTRACT_FAILED package=$packagePath zip=$zipPath extract_attempt=$extractAttempt destination=$extractRoot error=$($_.Exception.Message) remediation=verify free disk space and access to the exact runtime cache directory, then rerun setup"
            }
        } finally {
            [void](Remove-SynapseAcquisitionTempArtifact -Path $extractAttempt -ExpectedRoot $Root -Reason 'current_ort_extract')
            [void](Remove-SynapseAcquisitionTempArtifact -Path $zipPath -ExpectedRoot $Root -Reason 'current_ort_package_zip')
        }
    }
    foreach ($entry in $expectedFiles.GetEnumerator()) {
        $path = Join-Path $nativeDir $entry.Key
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            Die "SYNAPSE_ORT_RUNTIME_FILE_MISSING path=$path package=$packagePath remediation=the pinned Microsoft ONNX Runtime package did not contain its declared Windows x64 runtime bundle"
        }
        $actual = Get-SynapseFileSha256 -Path $path
        if ($actual -ne $entry.Value) {
            Die "SYNAPSE_ORT_RUNTIME_FILE_HASH_MISMATCH path=$path expected_sha256=$($entry.Value) actual_sha256=$actual remediation=the extracted runtime differs from the pinned Microsoft package; refuse the build and reacquire the exact package"
        }
    }
    Info "Pinned Microsoft ONNX Runtime GPU bundle verified version=$version package_sha256=$packageReadback native_dir=$nativeDir"
    return [pscustomobject]@{ Version = $version; NativeDir = $nativeDir; PackagePath = $packagePath; PackageSha256 = $packageReadback }
}

function Get-SynapseOptionalModelPin {
    <#
        Reads the committed pin for an optional embedded model and proves it
        agrees with the constant compiled into the daemon (#1863).

        The pin used to live in two independent hardcoded copies -- one here and
        one in registry.rs. Two copies of a supply-chain constant drift, and a
        drifted pin means the installer packages bytes the daemon will refuse at
        load time. The pin file is now the single authored value and this
        function fails closed if the Rust constant does not match it.
    #>
    param(
        [Parameter(Mandatory=$true)][string]$SourceDir,
        [Parameter(Mandatory=$true)][string]$PinRelativePath,
        [Parameter(Mandatory=$true)][string]$RustConstantName,
        [Parameter(Mandatory=$true)][string]$RustLengthConstantName,
        [Parameter(Mandatory=$true)][string]$RustSourceRelativePath
    )

    $pinPath = Join-Path $SourceDir $PinRelativePath
    if (-not (Test-Path -LiteralPath $pinPath -PathType Leaf)) {
        Die "SYNAPSE_OPTIONAL_MODEL_PIN_MISSING path=$pinPath remediation=restore the committed model pin file; it is the authoritative record of which bytes this checkout accepts"
    }
    try {
        $pin = Get-Content -LiteralPath $pinPath -Raw -Encoding UTF8 | ConvertFrom-Json
    } catch {
        Die "SYNAPSE_OPTIONAL_MODEL_PIN_UNREADABLE path=$pinPath error=$($_.Exception.Message) remediation=the pin file is not valid JSON; repair it from version control"
    }
    if ([string]::IsNullOrWhiteSpace([string]$pin.sha256) -or ([string]$pin.sha256).Length -ne 64) {
        Die "SYNAPSE_OPTIONAL_MODEL_PIN_INVALID path=$pinPath sha256=$($pin.sha256) remediation=sha256 must be exactly 64 hex characters"
    }
    if ([int64]$pin.length -le 0) {
        Die "SYNAPSE_OPTIONAL_MODEL_PIN_INVALID path=$pinPath length=$($pin.length) remediation=length must be a positive byte count"
    }
    $pinSha = ([string]$pin.sha256).ToUpperInvariant()

    $rustPath = Join-Path $SourceDir $RustSourceRelativePath
    if (-not (Test-Path -LiteralPath $rustPath -PathType Leaf)) {
        Die "SYNAPSE_OPTIONAL_MODEL_PIN_SOURCE_MISSING path=$rustPath remediation=the daemon model registry source is required to verify the pin agrees with the compiled constant"
    }
    $rustText = Get-Content -LiteralPath $rustPath -Raw -Encoding UTF8
    $match = [regex]::Match($rustText, "(?s)$([regex]::Escape($RustConstantName))\s*:\s*&str\s*=\s*`"sha256:([0-9a-fA-F]{64})`"")
    if (-not $match.Success) {
        Die "SYNAPSE_OPTIONAL_MODEL_PIN_CONSTANT_NOT_FOUND constant=$RustConstantName path=$rustPath remediation=the daemon constant could not be located; the installer refuses to package a model whose runtime pin it cannot read"
    }
    $rustSha = $match.Groups[1].Value.ToUpperInvariant()
    if ($rustSha -ne $pinSha) {
        Die "SYNAPSE_OPTIONAL_MODEL_PIN_DRIFT pin_path=$pinPath pin_sha256=$pinSha rust_path=$rustPath rust_constant=$RustConstantName rust_sha256=$rustSha remediation=the committed pin and the compiled daemon constant disagree; run scripts/build-whisper-e2e-onnx.ps1 which rewrites both, or reconcile them by hand before installing"
    }
    $lengthMatch = [regex]::Match($rustText, "$([regex]::Escape($RustLengthConstantName))\s*:\s*u64\s*=\s*([0-9_]+)")
    if (-not $lengthMatch.Success) {
        Die "SYNAPSE_OPTIONAL_MODEL_PIN_LENGTH_CONSTANT_NOT_FOUND constant=$RustLengthConstantName path=$rustPath remediation=the daemon length constant could not be located; the installer refuses to certify runtime health metadata it cannot verify"
    }
    $rustLength = [int64]($lengthMatch.Groups[1].Value.Replace('_', ''))
    if ($rustLength -ne [int64]$pin.length) {
        Die "SYNAPSE_OPTIONAL_MODEL_PIN_LENGTH_DRIFT pin_path=$pinPath pin_length=$($pin.length) rust_path=$rustPath rust_constant=$RustLengthConstantName rust_length=$rustLength remediation=run scripts/build-whisper-e2e-onnx.ps1 to repin hash and length atomically"
    }
    Info "Optional model pin verified id=$($pin.id) sha256=$pinSha length=$($pin.length) pin=$pinPath rust_constant=$RustConstantName"
    return [pscustomobject]@{
        Id = [string]$pin.id
        FileName = [string]$pin.filename
        Sha256 = $pinSha
        Length = [int64]$pin.length
        Recipe = [string]$pin.recipe
        OverrideEnv = [string]$pin.acquisition.override_env
        PinPath = $pinPath
    }
}

function Get-SynapseEmbeddedModelPins {
    <#
        The single authored description of every embedded model slot: what it
        is, whether the install may complete without it, and the exact bytes
        that count as legitimate. Both the packaging path and the -SkipBuild
        validation path read this, so they can never disagree (#1863).
    #>
    param([Parameter(Mandatory=$true)][string]$SourceDir)

    $whisperPin = Get-SynapseOptionalModelPin -SourceDir $SourceDir `
        -PinRelativePath 'models\whisper-tiny-int8.pin.json' `
        -RustConstantName 'WHISPER_TINY_INT8_ONNX_SHA256' `
        -RustLengthConstantName 'WHISPER_TINY_INT8_ONNX_LENGTH' `
        -RustSourceRelativePath 'crates\synapse-models\src\registry.rs'
    $extensionsPin = Get-SynapseOptionalModelPin -SourceDir $SourceDir `
        -PinRelativePath 'models\onnxruntime-extensions-whisper.pin.json' `
        -RustConstantName 'ORT_EXTENSIONS_WHISPER_SHA256' `
        -RustLengthConstantName 'ORT_EXTENSIONS_WHISPER_LENGTH' `
        -RustSourceRelativePath 'crates\synapse-models\src\registry.rs'
    $extensionsPath = Join-Path $env:LOCALAPPDATA 'synapse\models\ort-extensions\onnxruntime_extensions.dll'
    # ABSENT and WRONG are not the same fault, and only one of them may stop an
    # install.
    #
    # These custom operators are a prerequisite of exactly one OPTIONAL slot -
    # the end-to-end Whisper graph, `Required = $false`,
    # `Capability = audio_speech_to_text`. `Install-SynapsePinnedDetectionModels`
    # states the rule directly: "a disabled optional capability must never fail
    # the whole install (#1863)". Failing closed here inverted that - it made an
    # optional STT capability a hard gate on the entire deploy, so a host that
    # had never run the heavy Olive/PyTorch Whisper recipe could not install
    # Synapse at all. Observed 2026-08-06: setup died at
    # SYNAPSE_ORT_EXTENSIONS_LIBRARY_MISSING before reaching the build phase,
    # on a machine where every required model was present.
    #
    # So: absent -> the Whisper slot is unavailable and says so, and the install
    # continues with a recorded capability gap. Present-but-wrong-bytes stays
    # fatal, because that is a supply-chain integrity fault, not a gap.
    $whisperPrerequisiteMissing = $null
    if (-not (Test-Path -LiteralPath $extensionsPath -PathType Leaf)) {
        $whisperPrerequisiteMissing = "pinned ONNX Runtime Extensions library absent at $extensionsPath (pin=$($extensionsPin.PinPath)); the end-to-end Whisper graph cannot load its custom operators without it. Produce it with scripts\build-whisper-e2e-onnx.ps1"
        Warn "SYNAPSE_ORT_EXTENSIONS_LIBRARY_ABSENT path=$extensionsPath pin=$($extensionsPin.PinPath) effect=the optional audio_speech_to_text capability will be packaged as unavailable and the install continues remediation=run scripts\build-whisper-e2e-onnx.ps1 to enable end-to-end Whisper"
    } else {
        $extensionsActualLength = [int64](Get-Item -LiteralPath $extensionsPath).Length
        $extensionsActualSha = Get-SynapseFileSha256 -Path $extensionsPath
        if ($extensionsActualLength -ne $extensionsPin.Length -or $extensionsActualSha -ne $extensionsPin.Sha256) {
            Die "SYNAPSE_ORT_EXTENSIONS_LIBRARY_IDENTITY_MISMATCH path=$extensionsPath expected_sha256=$($extensionsPin.Sha256) actual_sha256=$extensionsActualSha expected_length=$($extensionsPin.Length) actual_length=$extensionsActualLength remediation=remove the mismatched file and regenerate it with scripts\build-whisper-e2e-onnx.ps1"
        }
        Info "Pinned ONNX Runtime Extensions library verified path=$extensionsPath sha256=$extensionsActualSha length=$extensionsActualLength"
    }

    # Candidate sources for the optional artifact, in precedence order. The
    # operator override comes first so a freshly produced artifact can be
    # packaged without moving files into a magic location.
    $whisperSources = @()
    if (-not [string]::IsNullOrWhiteSpace($env:SYNAPSE_WHISPER_ONNX_SOURCE)) {
        $whisperSources += $env:SYNAPSE_WHISPER_ONNX_SOURCE
    }
    $whisperSources += (Join-Path $env:LOCALAPPDATA 'synapse\models\whisper-tiny-int8.onnx')
    $whisperSources += (Join-Path $SourceDir 'models\whisper-tiny-int8.onnx')

    $models = @(
        [pscustomobject]@{
            Name = 'gpu'
            Required = $true
            FileName = 'rtdetr_v2_s_coco.onnx'
            Url = 'https://huggingface.co/onnx-community/rtdetr_v2_r18vd-ONNX/resolve/main/onnx/model.onnx'
            Sha256 = '583A236AC21C95A7FD94F284FC21485E42355BFEF82C27011BA78FBC09EE87E2'
            Length = [int64]81057510
            SourcePaths = @()
        },
        [pscustomobject]@{
            Name = 'cpu'
            Required = $true
            FileName = 'rtdetr_v2_s_coco_int8_cpu.onnx'
            Url = 'https://huggingface.co/onnx-community/rtdetr_v2_r18vd-ONNX/resolve/main/onnx/model_int8.onnx'
            Sha256 = 'FED736D2593CF2AB099F665EEEB6D315D909783EEA830F80A807FC1AC1C1B2EC'
            Length = [int64]20991219
            SourcePaths = @()
        },
        [pscustomobject]@{
            Name = 'whisper'
            Required = $false
            Capability = 'audio_speech_to_text'
            FileName = $whisperPin.FileName
            Url = $null
            Sha256 = $whisperPin.Sha256
            Length = $whisperPin.Length
            SourcePaths = @($whisperSources)
            Recipe = $whisperPin.Recipe
            OverrideEnv = $whisperPin.OverrideEnv
            PinPath = $whisperPin.PinPath
            # Non-null when a prerequisite of this optional slot is absent, so
            # the acquisition pass records the REAL cause of the gap instead of
            # reporting "no verified source artifact found" and sending the
            # operator looking for a missing .onnx that was never the problem.
            PrerequisiteMissing = $whisperPrerequisiteMissing
        }
    )
    return @($models)
}

function Install-SynapsePinnedDetectionModels {
    <#
        Acquires every embedded runtime model.

        Required models must be obtained or the build fails. Optional models
        (currently only the end-to-end STT graph) are acquired when a verified
        source is available and are otherwise reported as an explicit capability
        gap -- a disabled optional capability must never fail the whole install
        (#1863).
    #>
    param(
        [Parameter(Mandatory=$true)][string]$Root,
        [Parameter(Mandatory=$true)][string]$SourceDir
    )

    $models = @(Get-SynapseEmbeddedModelPins -SourceDir $SourceDir)
    New-Item -ItemType Directory -Force -Path $Root | Out-Null
    foreach ($model in $models) {
        # An optional slot whose PREREQUISITE is missing is unavailable no
        # matter what bytes are on disk, so acquiring the artifact would prove
        # nothing. Record the real cause and move on. A required slot in this
        # state is still fatal: nothing optional is being protected there.
        $prerequisiteMissing = [string]$model.PrerequisiteMissing
        if (-not [string]::IsNullOrWhiteSpace($prerequisiteMissing)) {
            if ($model.Required) {
                Die "SYNAPSE_EMBEDDED_MODEL_PREREQUISITE_MISSING model=$($model.Name) detail=$prerequisiteMissing remediation=a REQUIRED model slot cannot be packaged while its prerequisite is absent"
            }
            $model | Add-Member -NotePropertyName Path -NotePropertyValue $null -Force
            $model | Add-Member -NotePropertyName Present -NotePropertyValue $false -Force
            $model | Add-Member -NotePropertyName AbsenceReason -NotePropertyValue $prerequisiteMissing -Force
            Warn ("SYNAPSE_OPTIONAL_MODEL_ABSENT model={0} capability={1} reason=prerequisite_missing detail={2} recipe={3} effect=the install continues and this capability reports itself unavailable" -f `
                $model.Name, $model.Capability, $prerequisiteMissing, $model.Recipe)
            continue
        }
        $path = Join-Path $Root $model.FileName
        $valid = (Test-Path -LiteralPath $path -PathType Leaf) -and
            ((Get-Item -LiteralPath $path).Length -eq $model.Length) -and
            ((Get-SynapseFileSha256 -Path $path) -eq $model.Sha256)
        if (-not $valid) {
            $download = "$path.download-$PID"
            try {
                $acquired = $false
                $attempted = @()
                try {
                    if ($model.Url) {
                        Invoke-WebRequest -Uri $model.Url -OutFile $download -ErrorAction Stop
                        $acquired = $true
                        $attempted += $model.Url
                    } else {
                        foreach ($candidate in @($model.SourcePaths)) {
                            $attempted += $candidate
                            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                                Copy-Item -LiteralPath $candidate -Destination $download -Force -ErrorAction Stop
                                $acquired = $true
                                break
                            }
                        }
                    }
                } catch {
                    Die "SYNAPSE_EMBEDDED_MODEL_ACQUISITION_FAILED model=$($model.Name) path=$download attempted_sources=$($attempted -join ';') error=$($_.Exception.Message) remediation=repair access to the exact pinned source or its authoritative URL and rerun setup"
                }
                if (-not $acquired) {
                    if ($model.Required) {
                        Die "SYNAPSE_EMBEDDED_MODEL_SOURCE_MISSING model=$($model.Name) attempted_sources=$($attempted -join ';') remediation=a required embedded model could not be acquired; restore the pinned source artifact or network access to its pinned URL"
                    }
                    # Optional and absent: record the capability gap and continue.
                    # The install is still correct; the dependent capability is not
                    # available and says so.
                    $model | Add-Member -NotePropertyName Path -NotePropertyValue $null -Force
                    $model | Add-Member -NotePropertyName Present -NotePropertyValue $false -Force
                    $model | Add-Member -NotePropertyName AbsenceReason -NotePropertyValue (
                        "no verified source artifact found; searched: $($attempted -join '; ')"
                    ) -Force
                    Warn ("SYNAPSE_OPTIONAL_MODEL_ABSENT model={0} capability={1} expected_sha256={2} expected_length={3} searched={4} recipe={5} override_env={6} pin={7} effect=the install continues and this capability reports itself unavailable" -f `
                        $model.Name, $model.Capability, $model.Sha256, $model.Length, ($attempted -join ';'), $model.Recipe, $model.OverrideEnv, $model.PinPath)
                    continue
                }
                $actualLength = (Get-Item -LiteralPath $download).Length
                $actualHash = Get-SynapseFileSha256 -Path $download
                if ($actualLength -ne $model.Length -or $actualHash -ne $model.Sha256) {
                    Die "SYNAPSE_EMBEDDED_MODEL_DOWNLOAD_INVALID model=$($model.Name) path=$download expected_length=$($model.Length) actual_length=$actualLength expected_sha256=$($model.Sha256) actual_sha256=$actualHash remediation=refuse unverified model bytes; inspect the pinned upstream artifact"
                }
                Move-Item -LiteralPath $download -Destination $path -Force -ErrorAction Stop
            } finally {
                [void](Remove-SynapseAcquisitionTempArtifact -Path $download -ExpectedRoot $Root -Reason "current_model_$($model.Name)")
            }
        }
        $model | Add-Member -NotePropertyName Path -NotePropertyValue $path -Force
        $model | Add-Member -NotePropertyName Present -NotePropertyValue $true -Force
        Info "Pinned embedded runtime model verified model=$($model.Name) required=$($model.Required) path=$path length=$($model.Length) sha256=$($model.Sha256)"
    }
    return @($models)
}

# Slot order is positional and MUST match REGISTERED_MODELS in
# crates/synapse-models/src/registry.rs. The daemon reads the same table by
# index, so reordering here without reordering there would silently mis-map
# payloads; the daemon refuses a slot count that disagrees with its registry.
$script:SynapseEmbeddedModelSlotOrder = @('gpu', 'cpu', 'whisper')
$script:SynapseEmbeddedModelBundleMagicV1 = 'SYNMODEL_BUNDLE1'
$script:SynapseEmbeddedModelBundleMagicV2 = 'SYNMODEL_BUNDLE2'
$script:SynapseEmbeddedModelBundleFormatVersion = 2
# [int64 length][32-byte sha256] per slot.
$script:SynapseEmbeddedModelBundleSlotSize = 40
# [uint32 slot_count][uint32 format_version][16-byte magic].
$script:SynapseEmbeddedModelBundleFooterSize = 24

function Get-SynapseExecutableModelBundle {
    <#
        Decodes the self-describing model bundle appended to an executable.

        A slot whose length is 0 is a positive record of absence, which is what
        allows an optional model to be missing without making the bundle
        invalid. Returns $null when the executable carries no bundle at all
        (#1863).
    #>
    param([Parameter(Mandatory=$true)][string]$ExecutablePath)

    $magicV1 = [System.Text.Encoding]::ASCII.GetBytes($script:SynapseEmbeddedModelBundleMagicV1)
    $magicV2 = [System.Text.Encoding]::ASCII.GetBytes($script:SynapseEmbeddedModelBundleMagicV2)
    $slotCountExpected = @($script:SynapseEmbeddedModelSlotOrder).Count
    $tableSize = $slotCountExpected * $script:SynapseEmbeddedModelBundleSlotSize
    $trailerSize = $tableSize + $script:SynapseEmbeddedModelBundleFooterSize

    $stream = [System.IO.File]::Open($ExecutablePath, 'Open', 'Read', 'ReadWrite')
    try {
        if ($stream.Length -lt $script:SynapseEmbeddedModelBundleFooterSize) {
            return $null
        }
        $stream.Position = $stream.Length - 16
        $magic = New-Object byte[] 16
        if ($stream.Read($magic, 0, 16) -ne 16) {
            Die "SYNAPSE_EMBEDDED_MODEL_TRAILER_READ_FAILED path=$ExecutablePath remediation=inspect filesystem integrity; the executable trailer could not be read"
        }
        $isV1 = $true
        $isV2 = $true
        for ($index = 0; $index -lt 16; $index++) {
            if ($magic[$index] -ne $magicV1[$index]) { $isV1 = $false }
            if ($magic[$index] -ne $magicV2[$index]) { $isV2 = $false }
        }
        if ($isV1) {
            # A pre-#1863 binary. Report it precisely instead of letting it look
            # like an unpackaged executable, which would silently repackage from
            # an unknown base length and corrupt the image.
            Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_LEGACY_FORMAT path=$ExecutablePath format=$($script:SynapseEmbeddedModelBundleMagicV1) remediation=this executable was packaged by an installer whose bundle cannot express optional models; rebuild from source through this setup script rather than repackaging the existing binary"
        }
        if (-not $isV2) {
            return $null
        }
        if ($stream.Length -lt $trailerSize) {
            Die "SYNAPSE_EMBEDDED_MODEL_TRAILER_INVALID path=$ExecutablePath executable_length=$($stream.Length) required_trailer=$trailerSize remediation=the executable is smaller than its own bundle trailer; rebuild and repackage it"
        }
        $stream.Position = $stream.Length - $script:SynapseEmbeddedModelBundleFooterSize
        $footer = New-Object byte[] $script:SynapseEmbeddedModelBundleFooterSize
        if ($stream.Read($footer, 0, $script:SynapseEmbeddedModelBundleFooterSize) -ne $script:SynapseEmbeddedModelBundleFooterSize) {
            Die "SYNAPSE_EMBEDDED_MODEL_TRAILER_READ_FAILED path=$ExecutablePath remediation=inspect filesystem integrity; the executable bundle footer could not be read"
        }
        $slotCount = [BitConverter]::ToUInt32($footer, 0)
        $formatVersion = [BitConverter]::ToUInt32($footer, 4)
        if ($formatVersion -ne $script:SynapseEmbeddedModelBundleFormatVersion) {
            Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_FORMAT_UNSUPPORTED path=$ExecutablePath format_version=$formatVersion expected=$($script:SynapseEmbeddedModelBundleFormatVersion) remediation=the executable was packaged by a different installer version; rebuild from this checkout"
        }
        if ($slotCount -ne $slotCountExpected) {
            Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_SLOT_COUNT_MISMATCH path=$ExecutablePath slot_count=$slotCount expected=$slotCountExpected remediation=the executable and this installer disagree about the registered model set; rebuild from this checkout"
        }

        $stream.Position = $stream.Length - $trailerSize
        $table = New-Object byte[] $tableSize
        if ($stream.Read($table, 0, $tableSize) -ne $tableSize) {
            Die "SYNAPSE_EMBEDDED_MODEL_TRAILER_READ_FAILED path=$ExecutablePath remediation=inspect filesystem integrity; the executable slot table could not be read"
        }

        $slots = @()
        $payloadLength = [int64]0
        for ($index = 0; $index -lt $slotCount; $index++) {
            $base = $index * $script:SynapseEmbeddedModelBundleSlotSize
            $length = [BitConverter]::ToInt64($table, $base)
            if ($length -lt 0) {
                Die "SYNAPSE_EMBEDDED_MODEL_TRAILER_INVALID path=$ExecutablePath slot=$($script:SynapseEmbeddedModelSlotOrder[$index]) length=$length remediation=the executable advertises a negative model payload length; rebuild and repackage it"
            }
            $payloadLength += $length
            $digest = ''
            if ($length -gt 0) {
                $digestBytes = New-Object byte[] 32
                [Array]::Copy($table, $base + 8, $digestBytes, 0, 32)
                $digest = ($digestBytes | ForEach-Object { $_.ToString('X2') }) -join ''
            }
            $slots += [pscustomobject]@{
                Name = $script:SynapseEmbeddedModelSlotOrder[$index]
                Length = [int64]$length
                Sha256 = $digest
                Present = ($length -gt 0)
                Offset = [int64]0
            }
        }
        $baseLength = [int64]($stream.Length - $trailerSize - $payloadLength)
        if ($baseLength -lt 0) {
            Die "SYNAPSE_EMBEDDED_MODEL_TRAILER_INVALID path=$ExecutablePath executable_length=$($stream.Length) payload_length=$payloadLength trailer_length=$trailerSize remediation=the executable advertises impossible model payload lengths; rebuild and repackage it"
        }
        $offset = $baseLength
        foreach ($slot in $slots) {
            $slot.Offset = [int64]$offset
            $offset += $slot.Length
        }
        return [pscustomobject]@{
            BaseLength = $baseLength
            TotalLength = [int64]$stream.Length
            PayloadLength = $payloadLength
            Slots = @($slots)
        }
    } finally {
        $stream.Dispose()
    }
}

function Get-SynapseExecutableRangeSha256 {
    param(
        [Parameter(Mandatory=$true)][string]$ExecutablePath,
        [Parameter(Mandatory=$true)][int64]$Offset,
        [Parameter(Mandatory=$true)][int64]$Length
    )

    $stream = [System.IO.File]::Open($ExecutablePath, 'Open', 'Read', 'ReadWrite')
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $stream.Position = $Offset
        $remaining = $Length
        $buffer = New-Object byte[] (1024 * 1024)
        while ($remaining -gt 0) {
            $wanted = [int][Math]::Min($buffer.Length, $remaining)
            $read = $stream.Read($buffer, 0, $wanted)
            if ($read -le 0) {
                Die "SYNAPSE_EMBEDDED_MODEL_PAYLOAD_TRUNCATED path=$ExecutablePath offset=$Offset length=$Length remaining=$remaining remediation=rebuild and repackage the executable"
            }
            [void]$sha.TransformBlock($buffer, 0, $read, $null, 0)
            $remaining -= $read
        }
        [void]$sha.TransformFinalBlock([byte[]]::new(0), 0, 0)
        return ($sha.Hash | ForEach-Object { $_.ToString('X2') }) -join ''
    } finally {
        $sha.Dispose()
        $stream.Dispose()
    }
}

function Assert-SynapseExecutableModelBundle {
    <#
        Re-reads the packaged executable and proves every present slot hashes to
        what the slot table claims, that required models are present, and that
        absent slots are optional. Verification is against the bundle's own
        recorded digests plus the acquired models' verified digests -- no third
        hardcoded copy of the hashes (#1863).
    #>
    param(
        [Parameter(Mandatory=$true)][string]$ExecutablePath,
        [Parameter(Mandatory=$true)]$Models
    )

    $bundle = Get-SynapseExecutableModelBundle -ExecutablePath $ExecutablePath
    if (-not $bundle) {
        Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_MISSING path=$ExecutablePath remediation=build through synapse-setup.ps1 so the pinned models are packaged into the executable"
    }
    $summary = @()
    foreach ($slot in $bundle.Slots) {
        $model = @($Models | Where-Object { $_.Name -eq $slot.Name })[0]
        if (-not $model) {
            Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_SLOT_UNKNOWN path=$ExecutablePath slot=$($slot.Name) remediation=the packaged slot table names a model this installer does not know; rebuild from this checkout"
        }
        if (-not $slot.Present) {
            if ($model.Required) {
                Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_REQUIRED_SLOT_ABSENT path=$ExecutablePath slot=$($slot.Name) remediation=a required model was not packaged; the executable is unusable — reacquire the model and repackage"
            }
            if ($model.Present) {
                Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_SLOT_LOST path=$ExecutablePath slot=$($slot.Name) expected_length=$($model.Length) remediation=the model was acquired but is absent from the packaged bundle; the packaging step is defective"
            }
            $summary += "$($slot.Name)=absent"
            continue
        }
        if (-not $model.Present) {
            Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_SLOT_UNEXPECTED path=$ExecutablePath slot=$($slot.Name) packaged_length=$($slot.Length) remediation=the bundle contains a payload for a model that was never acquired; the packaging step is defective"
        }
        if ($slot.Length -ne $model.Length) {
            Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_INVALID path=$ExecutablePath slot=$($slot.Name) packaged_length=$($slot.Length) expected_length=$($model.Length) remediation=the executable does not contain the exact pinned model; rebuild and repackage it"
        }
        $actual = Get-SynapseExecutableRangeSha256 -ExecutablePath $ExecutablePath -Offset $slot.Offset -Length $slot.Length
        if ($actual -ne $model.Sha256 -or $actual -ne $slot.Sha256) {
            Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_INVALID path=$ExecutablePath slot=$($slot.Name) offset=$($slot.Offset) length=$($slot.Length) payload_sha256=$actual slot_table_sha256=$($slot.Sha256) expected_sha256=$($model.Sha256) remediation=the executable does not contain the exact pinned model; rebuild and repackage it"
        }
        $summary += "$($slot.Name)=$actual"
    }
    Info "Executable model bundle verified path=$ExecutablePath base_length=$($bundle.BaseLength) total_length=$($bundle.TotalLength) slots=$($summary -join ' ')"
    return $bundle
}

function Add-SynapseExecutableModelBundle {
    <#
        Appends the payloads and the slot table. Absent optional models occupy a
        zero-length slot with a zero digest, which is how the daemon learns the
        capability is genuinely not packaged rather than guessing (#1863).
    #>
    param(
        [Parameter(Mandatory=$true)][string]$ExecutablePath,
        [Parameter(Mandatory=$true)]$Models
    )

    $ordered = @()
    foreach ($name in $script:SynapseEmbeddedModelSlotOrder) {
        $model = @($Models | Where-Object { $_.Name -eq $name })[0]
        if (-not $model) {
            Die "SYNAPSE_EMBEDDED_MODEL_SELECTION_FAILED slot=$name remediation=the pinned model acquisition did not return an entry for every registered slot"
        }
        if ($model.Required -and -not $model.Present) {
            Die "SYNAPSE_EMBEDDED_MODEL_SELECTION_FAILED slot=$name remediation=a required model was not acquired; refusing to package an unusable executable"
        }
        $ordered += $model
    }

    $existing = Get-SynapseExecutableModelBundle -ExecutablePath $ExecutablePath
    if ($existing) {
        $stream = [System.IO.File]::Open($ExecutablePath, 'Open', 'Write', 'None')
        try { $stream.SetLength($existing.BaseLength) } finally { $stream.Dispose() }
    }
    $output = [System.IO.File]::Open($ExecutablePath, 'Append', 'Write', 'None')
    try {
        foreach ($model in $ordered) {
            if (-not $model.Present) { continue }
            $payload = [System.IO.File]::OpenRead($model.Path)
            try { $payload.CopyTo($output) } finally { $payload.Dispose() }
        }
        foreach ($model in $ordered) {
            if ($model.Present) {
                $output.Write([BitConverter]::GetBytes([int64]$model.Length), 0, 8)
                $digestBytes = [byte[]]::new(32)
                for ($index = 0; $index -lt 32; $index++) {
                    $digestBytes[$index] = [Convert]::ToByte($model.Sha256.Substring($index * 2, 2), 16)
                }
                $output.Write($digestBytes, 0, 32)
            } else {
                $output.Write([BitConverter]::GetBytes([int64]0), 0, 8)
                $output.Write([byte[]]::new(32), 0, 32)
            }
        }
        $output.Write([BitConverter]::GetBytes([uint32]@($script:SynapseEmbeddedModelSlotOrder).Count), 0, 4)
        $output.Write([BitConverter]::GetBytes([uint32]$script:SynapseEmbeddedModelBundleFormatVersion), 0, 4)
        $magic = [System.Text.Encoding]::ASCII.GetBytes($script:SynapseEmbeddedModelBundleMagicV2)
        $output.Write($magic, 0, $magic.Length)
        $output.Flush($true)
    } finally {
        $output.Dispose()
    }
    return Assert-SynapseExecutableModelBundle -ExecutablePath $ExecutablePath -Models $ordered
}

function New-SynapseStagedDaemonBinary {
    param(
        [Parameter(Mandatory=$true)][string]$BuiltPath,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$RuntimeDir
    )

    if (-not [string]::IsNullOrWhiteSpace([string]$script:SynapseCurrentDaemonStagingDirectory)) {
        Die "SYNAPSE_DAEMON_STAGING_ALREADY_ACTIVE path=$script:SynapseCurrentDaemonStagingDirectory remediation=clean the exact prior setup-owned staging directory before creating another candidate copy"
    }
    $stagingRoot = Join-Path $LogDir 'setup-staging'
    $stagingDir = New-SynapseSetupRunDirectory -Root $stagingRoot -Purpose 'daemon-binary'
    $script:SynapseCurrentDaemonStagingDirectory = Get-SynapseFullPathForScopeCheck -Path $stagingDir
    $builtHash = Get-SynapseFileSha256 -Path $BuiltPath
    $stagedPath = Join-Path $stagingDir "synapse-mcp-$builtHash.exe"
    Copy-Item -LiteralPath $BuiltPath -Destination $stagedPath -Force
    $stagedHash = Get-SynapseFileSha256 -Path $stagedPath
    if ($stagedHash -ne $builtHash) {
        Die "SYNAPSE_STAGED_BINARY_HASH_MISMATCH built=$BuiltPath staged=$stagedPath built_hash=$builtHash staged_hash=$stagedHash remediation=inspect disk/storage; refusing to install an unverified binary"
    }
    $runtimeFiles = @()
    foreach ($companion in @(Get-SynapseOrtRuntimeCompanions -ExecutablePath (Join-Path $RuntimeDir 'synapse-mcp.exe'))) {
        $stagedCompanion = Join-Path $stagingDir $companion.Name
        Copy-Item -LiteralPath $companion.Path -Destination $stagedCompanion -Force
        $stagedCompanionHash = Get-SynapseFileSha256 -Path $stagedCompanion
        if ($stagedCompanionHash -ne $companion.Sha256) {
            Die "SYNAPSE_STAGED_RUNTIME_COMPANION_HASH_MISMATCH source=$($companion.Path) staged=$stagedCompanion expected_sha256=$($companion.Sha256) actual_sha256=$stagedCompanionHash remediation=inspect disk/storage; refusing to install an incoherent ONNX Runtime bundle"
        }
        $runtimeFiles += [pscustomobject]@{ Name = $companion.Name; Path = $stagedCompanion; Sha256 = $stagedCompanionHash }
    }
    Info "Staged daemon binary path=$stagedPath sha256=$stagedHash"
    return [pscustomobject]@{
        Path = $stagedPath
        Directory = $script:SynapseCurrentDaemonStagingDirectory
        Sha256 = $stagedHash
        SourcePath = $BuiltPath
        RuntimeFiles = @($runtimeFiles)
    }
}

function New-SynapseCandidateBind {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Parse('127.0.0.1'), 0)
    try {
        $listener.Start()
        $port = [int]$listener.LocalEndpoint.Port
    } finally {
        $listener.Stop()
    }
    return "127.0.0.1:$port"
}

function Get-SynapseCandidateExitReadback {
    param([AllowNull()][System.Diagnostics.Process]$Process)

    if ($null -eq $Process) {
        return [pscustomobject]@{ HasExited = $null; ExitCodeSigned = $null; ExitCodeHex = $null }
    }
    try {
        $Process.Refresh()
        if (-not $Process.HasExited) {
            return [pscustomobject]@{ HasExited = $false; ExitCodeSigned = $null; ExitCodeHex = $null }
        }
        # Complete redirected-stream processing before reading retained exit metadata.
        $Process.WaitForExit()
        $signed = [int]$Process.ExitCode
        $unsigned = [BitConverter]::ToUInt32([BitConverter]::GetBytes($signed), 0)
        return [pscustomobject]@{
            HasExited = $true
            ExitCodeSigned = $signed
            ExitCodeHex = ('0x{0:X8}' -f $unsigned)
        }
    } catch {
        return [pscustomobject]@{
            HasExited = $null
            ExitCodeSigned = $null
            ExitCodeHex = $null
            Error = $_.Exception.Message
        }
    }
}

function Write-SynapseCandidateFailureEvidence {
    param(
        [Parameter(Mandatory=$true)][string]$CandidateRoot,
        [AllowNull()][System.Diagnostics.Process]$Process,
        [Parameter(Mandatory=$true)][string]$FailureMessage,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$ExecutablePath,
        [Parameter(Mandatory=$true)][string]$ExecutableSha256
    )

    $exit = Get-SynapseCandidateExitReadback -Process $Process
    $evidence = @()
    foreach ($file in @(Get-ChildItem -LiteralPath $CandidateRoot -File -Recurse -Force -ErrorAction Stop | Sort-Object FullName)) {
        $relative = [System.IO.Path]::GetRelativePath($CandidateRoot, $file.FullName)
        $evidence += [ordered]@{
            relative_path = $relative
            length = [int64]$file.Length
            sha256 = Get-SynapseFileSha256 -Path $file.FullName
        }
    }
    $manifestPath = Join-Path $CandidateRoot 'candidate-diagnostic.json'
    $manifest = [ordered]@{
        schema = 'synapse_candidate_failure/v1'
        retained_at_utc = [DateTime]::UtcNow.ToString('o')
        failure = $FailureMessage
        candidate_pid = if ($null -eq $Process) { $null } else { [int]$Process.Id }
        process_has_exited = $exit.HasExited
        exit_code_signed = $exit.ExitCodeSigned
        exit_code_hex = $exit.ExitCodeHex
        exit_read_error = if ($exit.PSObject.Properties.Name -contains 'Error') { $exit.Error } else { $null }
        bind = $Bind
        executable_path = $ExecutablePath
        executable_sha256 = $ExecutableSha256
        evidence = @($evidence)
        retention_policy = 'newest 5 verified candidate failure bundles'
    }
    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($manifestPath, (($manifest | ConvertTo-Json -Depth 10) + "`n"), $encoding)
    $manifestHash = Get-SynapseFileSha256 -Path $manifestPath
    return [pscustomobject]@{
        Path = $manifestPath
        Sha256 = $manifestHash
        Exit = $exit
        EvidenceCount = $evidence.Count
    }
}

function Get-SynapseCandidateArtifactDescriptor {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot
    )

    $rootFull = [System.IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\')
    $pathFull = [System.IO.Path]::GetFullPath($Path).TrimEnd('\')
    $parentFull = [System.IO.Path]::GetFullPath((Split-Path -Parent $pathFull)).TrimEnd('\')
    $leaf = Split-Path -Leaf $pathFull
    if (-not $parentFull.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase) -or
        $leaf -notmatch '^candidate-\d{8}T\d{9}Z-(\d+)$') {
        throw "SYNAPSE_CANDIDATE_ARTIFACT_SCOPE_INVALID path=$pathFull expected_parent=$rootFull actual_parent=$parentFull leaf=$leaf remediation=do not delete the path; only exact setup-created candidate directories are eligible"
    }
    $ownerPid = [int]$Matches[1]
    if (-not (Test-Path -LiteralPath $pathFull)) {
        return [pscustomobject]@{ Path = $pathFull; Exists = $false; OwnerPid = $ownerPid }
    }
    $item = Get-Item -LiteralPath $pathFull -Force -ErrorAction Stop
    if (-not $item.PSIsContainer -or ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        throw "SYNAPSE_CANDIDATE_ARTIFACT_TYPE_INVALID path=$pathFull is_container=$($item.PSIsContainer) attributes=$($item.Attributes) remediation=do not recurse into a file or reparse point; inspect the exact setup-candidates child"
    }
    return [pscustomobject]@{
        Path = $pathFull
        Exists = $true
        OwnerPid = $ownerPid
    }
}

function Assert-SynapseCandidateArtifactCleanupSafe {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot,
        [int]$CandidateProcessId = 0,
        [AllowEmptyString()][string]$Bind = ''
    )

    $descriptor = Get-SynapseCandidateArtifactDescriptor -Path $Path -ExpectedRoot $ExpectedRoot
    if (-not $descriptor.Exists) { return $descriptor }

    if ($CandidateProcessId -gt 0) {
        $candidateProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$CandidateProcessId" -ErrorAction SilentlyContinue
        if ($candidateProcess) {
            throw "SYNAPSE_CANDIDATE_ARTIFACT_PROCESS_LIVE path=$($descriptor.Path) candidate_pid=$CandidateProcessId actual_name=$($candidateProcess.Name) actual_path=$($candidateProcess.ExecutablePath) remediation=do not delete candidate storage while the recorded process identity is live"
        }
    }
    $pathReferences = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {
        $_.ProcessId -ne $PID -and
        -not [string]::IsNullOrWhiteSpace([string]$_.CommandLine) -and
        ([string]$_.CommandLine).IndexOf($descriptor.Path, [System.StringComparison]::OrdinalIgnoreCase) -ge 0
    })
    if ($pathReferences.Count -gt 0) {
        $referenceText = ($pathReferences | ForEach-Object { "pid=$($_.ProcessId),name=$($_.Name)" }) -join ';'
        throw "SYNAPSE_CANDIDATE_ARTIFACT_PROCESS_REFERENCE_LIVE path=$($descriptor.Path) references=$referenceText remediation=inspect these exact processes; do not delete storage referenced by a live command line"
    }
    if (-not [string]::IsNullOrWhiteSpace($Bind)) {
        $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        if ($listeners.Count -gt 0) {
            throw "SYNAPSE_CANDIDATE_ARTIFACT_BIND_LIVE path=$($descriptor.Path) candidate_pid=$CandidateProcessId bind=$Bind listeners=$(Format-SynapseTcpBindListenerSnapshot -Snapshot $listeners) remediation=do not delete candidate storage until its exact listener is absent"
        }
    }
    return $descriptor
}

function Get-SynapseCandidateCleanupNativeErrorCode {
    param([Parameter(Mandatory=$true)][System.Exception]$Exception)

    $cursor = $Exception
    $lastCode = 0
    while ($null -ne $cursor) {
        $code = ([int64]$cursor.HResult) -band 0xFFFF
        if ($code -in @(5, 32, 33)) { return [int]$code }
        if ($code -ne 0) { $lastCode = [int]$code }
        $cursor = $cursor.InnerException
    }
    return $lastCode
}

function Get-SynapseCandidateCleanupIntentPath {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot
    )

    $descriptor = Get-SynapseCandidateArtifactDescriptor -Path $Path -ExpectedRoot $ExpectedRoot
    $leaf = Split-Path -Leaf $descriptor.Path
    $suffix = $leaf.Substring('candidate-'.Length)
    return Join-Path ([System.IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\')) "candidate-cleanup-$suffix.json"
}

function Read-SynapseCandidateCleanupIntent {
    param(
        [Parameter(Mandatory=$true)][string]$IntentPath,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot
    )

    $rootFull = [System.IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\')
    $intentFull = [System.IO.Path]::GetFullPath($IntentPath)
    $parentFull = [System.IO.Path]::GetFullPath((Split-Path -Parent $intentFull)).TrimEnd('\')
    $leaf = Split-Path -Leaf $intentFull
    if (-not $parentFull.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase) -or
        $leaf -notmatch '^candidate-cleanup-(\d{8}T\d{9}Z-\d+)\.json$') {
        throw "SYNAPSE_CANDIDATE_CLEANUP_INTENT_SCOPE_INVALID path=$intentFull expected_parent=$rootFull actual_parent=$parentFull leaf=$leaf remediation=preserve and inspect the file; only exact setup-created cleanup intents are authoritative"
    }
    $candidatePath = Join-Path $rootFull "candidate-$($Matches[1])"
    $descriptor = Get-SynapseCandidateArtifactDescriptor -Path $candidatePath -ExpectedRoot $rootFull
    $item = Get-Item -LiteralPath $intentFull -Force -ErrorAction Stop
    if ($item.PSIsContainer -or ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        throw "SYNAPSE_CANDIDATE_CLEANUP_INTENT_TYPE_INVALID path=$intentFull attributes=$($item.Attributes) remediation=preserve and inspect the non-regular intent path; setup will not follow it"
    }
    try {
        $intent = Get-Content -LiteralPath $intentFull -Raw -Encoding UTF8 -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
    } catch {
        throw "SYNAPSE_CANDIDATE_CLEANUP_INTENT_UNREADABLE path=$intentFull error=$($_.Exception.Message) remediation=preserve the intent and candidate directory; repair the exact transaction journal before retrying setup"
    }
    $recordedRoot = [System.IO.Path]::GetFullPath([string]$intent.expected_root).TrimEnd('\')
    $recordedCandidate = [System.IO.Path]::GetFullPath([string]$intent.candidate_path).TrimEnd('\')
    if ([string]$intent.schema -ne 'synapse_candidate_cleanup_intent/v1' -or
        [string]$intent.state -ne 'pending' -or
        -not $recordedRoot.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase) -or
        -not $recordedCandidate.Equals($descriptor.Path, [System.StringComparison]::OrdinalIgnoreCase) -or
        [int]$intent.setup_owner_pid -ne $descriptor.OwnerPid -or
        [int]$intent.candidate_pid -le 0 -or
        [string]::IsNullOrWhiteSpace([string]$intent.bind) -or
        [string]::IsNullOrWhiteSpace([string]$intent.reason)) {
        throw "SYNAPSE_CANDIDATE_CLEANUP_INTENT_INVALID path=$intentFull candidate=$($descriptor.Path) schema=$($intent.schema) state=$($intent.state) setup_owner_pid=$($intent.setup_owner_pid) expected_setup_owner_pid=$($descriptor.OwnerPid) candidate_pid=$($intent.candidate_pid) bind=$($intent.bind) reason=$($intent.reason) remediation=preserve both paths and repair the exact cleanup transaction identity; setup refuses ambiguous recursive deletion"
    }
    return [pscustomobject]@{
        Path = $intentFull
        CandidatePath = $descriptor.Path
        SetupOwnerPid = [int]$intent.setup_owner_pid
        CandidatePid = [int]$intent.candidate_pid
        Bind = [string]$intent.bind
        Reason = [string]$intent.reason
    }
}

function Ensure-SynapseCandidateCleanupIntent {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot,
        [Parameter(Mandatory=$true)][int]$CandidateProcessId,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    if ($CandidateProcessId -le 0 -or [string]::IsNullOrWhiteSpace($Bind)) {
        throw "SYNAPSE_CANDIDATE_CLEANUP_IDENTITY_MISSING path=$Path candidate_pid=$CandidateProcessId bind=$Bind reason=$Reason remediation=cleanup requires the exact validated candidate PID and bind"
    }
    $descriptor = Get-SynapseCandidateArtifactDescriptor -Path $Path -ExpectedRoot $ExpectedRoot
    $intentPath = Get-SynapseCandidateCleanupIntentPath -Path $descriptor.Path -ExpectedRoot $ExpectedRoot
    if (-not (Test-Path -LiteralPath $intentPath)) {
        $record = [ordered]@{
            schema = 'synapse_candidate_cleanup_intent/v1'
            state = 'pending'
            expected_root = [System.IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\')
            candidate_path = $descriptor.Path
            setup_owner_pid = $descriptor.OwnerPid
            candidate_pid = $CandidateProcessId
            bind = $Bind
            reason = $Reason
            created_at_utc = [DateTime]::UtcNow.ToString('o')
        }
        $tempPath = "$intentPath.tmp-$PID-$([guid]::NewGuid().ToString('N'))"
        try {
            [System.IO.File]::WriteAllText($tempPath, (($record | ConvertTo-Json -Depth 8) + "`n"), [System.Text.UTF8Encoding]::new($false))
            Move-Item -LiteralPath $tempPath -Destination $intentPath -ErrorAction Stop
        } catch {
            try { Remove-Item -LiteralPath $tempPath -Force -ErrorAction SilentlyContinue } catch { }
            throw "SYNAPSE_CANDIDATE_CLEANUP_INTENT_WRITE_FAILED path=$intentPath candidate=$($descriptor.Path) error=$($_.Exception.Message) remediation=repair the setup-candidates directory; setup will not begin a recursive delete without a durable recovery intent"
        }
    }
    $intent = Read-SynapseCandidateCleanupIntent -IntentPath $intentPath -ExpectedRoot $ExpectedRoot
    if ($intent.CandidatePid -ne $CandidateProcessId -or
        $intent.Bind -ne $Bind -or
        $intent.Reason -ne $Reason) {
        throw "SYNAPSE_CANDIDATE_CLEANUP_INTENT_CONFLICT path=$intentPath candidate=$($intent.CandidatePath) recorded_pid=$($intent.CandidatePid) expected_pid=$CandidateProcessId recorded_bind=$($intent.Bind) expected_bind=$Bind recorded_reason=$($intent.Reason) expected_reason=$Reason remediation=preserve both paths; a different cleanup transaction already owns this candidate"
    }
    Info "Candidate cleanup intent verified path=$($intent.Path) candidate=$($intent.CandidatePath) setup_owner_pid=$($intent.SetupOwnerPid) candidate_pid=$($intent.CandidatePid) bind=$($intent.Bind) reason=$($intent.Reason)"
    return $intent
}

function Complete-SynapseCandidateCleanupIntent {
    param([Parameter(Mandatory=$true)]$Intent)

    if (Test-Path -LiteralPath $Intent.CandidatePath) {
        throw "SYNAPSE_CANDIDATE_CLEANUP_INTENT_EARLY_COMPLETE path=$($Intent.Path) candidate=$($Intent.CandidatePath) remediation=do not clear recovery authority while the candidate directory remains"
    }
    Remove-Item -LiteralPath $Intent.Path -Force -ErrorAction Stop
    if ((Test-Path -LiteralPath $Intent.Path) -or (Test-Path -LiteralPath $Intent.CandidatePath)) {
        throw "SYNAPSE_CANDIDATE_CLEANUP_INTENT_COMPLETION_UNVERIFIED path=$($Intent.Path) candidate=$($Intent.CandidatePath) remediation=inspect the exact intent and candidate paths; cleanup is incomplete"
    }
    Info "Candidate cleanup intent completion verified path=$($Intent.Path) candidate=$($Intent.CandidatePath) intent_exists=false candidate_exists=false"
}

function Remove-SynapseCandidateArtifact {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$ExpectedRoot,
        [int]$CandidateProcessId = 0,
        [AllowEmptyString()][string]$Bind = '',
        [ValidateRange(1, 60)][int]$TimeoutSeconds = 20,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    $initial = Get-SynapseCandidateArtifactDescriptor -Path $Path -ExpectedRoot $ExpectedRoot
    $intentPath = Get-SynapseCandidateCleanupIntentPath -Path $initial.Path -ExpectedRoot $ExpectedRoot
    if (-not $initial.Exists -and -not (Test-Path -LiteralPath $intentPath)) {
        Info "Candidate artifact absence verified reason=$Reason path=$($initial.Path) candidate_pid=$CandidateProcessId bind=$Bind attempts=0 elapsed_ms=0 readback_exists=false intent_exists=false"
        return $initial
    }
    $intent = if ($initial.Exists) {
        Ensure-SynapseCandidateCleanupIntent `
            -Path $initial.Path `
            -ExpectedRoot $ExpectedRoot `
            -CandidateProcessId $CandidateProcessId `
            -Bind $Bind `
            -Reason $Reason
    } else {
        Read-SynapseCandidateCleanupIntent -IntentPath $intentPath -ExpectedRoot $ExpectedRoot
    }
    if ($intent.CandidatePid -ne $CandidateProcessId -or
        $intent.Bind -ne $Bind -or
        $intent.Reason -ne $Reason) {
        throw "SYNAPSE_CANDIDATE_CLEANUP_INTENT_RESUME_CONFLICT path=$($intent.Path) candidate=$($intent.CandidatePath) recorded_pid=$($intent.CandidatePid) expected_pid=$CandidateProcessId recorded_bind=$($intent.Bind) expected_bind=$Bind recorded_reason=$($intent.Reason) expected_reason=$Reason remediation=resume the exact recorded cleanup transaction; do not replace its process/socket identity"
    }
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $attempt = 0
    $lastNativeError = 0
    $lastError = '<none>'
    while ($true) {
        $attempt++
        $descriptor = Assert-SynapseCandidateArtifactCleanupSafe `
            -Path $Path `
            -ExpectedRoot $ExpectedRoot `
            -CandidateProcessId $CandidateProcessId `
            -Bind $Bind
        if (-not $descriptor.Exists) {
            Complete-SynapseCandidateCleanupIntent -Intent $intent
            Info "Candidate artifact absence verified reason=$Reason path=$($descriptor.Path) candidate_pid=$CandidateProcessId bind=$Bind attempts=$attempt elapsed_ms=$($clock.ElapsedMilliseconds) readback_exists=false intent_exists=false"
            return $descriptor
        }

        try {
            Remove-Item -LiteralPath $descriptor.Path -Recurse -Force -ErrorAction Stop
            if (-not (Test-Path -LiteralPath $descriptor.Path)) {
                Complete-SynapseCandidateCleanupIntent -Intent $intent
                Info "Candidate artifact cleanup verified reason=$Reason path=$($descriptor.Path) candidate_pid=$CandidateProcessId bind=$Bind attempts=$attempt elapsed_ms=$($clock.ElapsedMilliseconds) readback_exists=false intent_exists=false"
                return $descriptor
            }
            $lastNativeError = 0
            $lastError = 'Remove-Item returned but the directory remains present'
        } catch {
            $lastNativeError = Get-SynapseCandidateCleanupNativeErrorCode -Exception $_.Exception
            $lastError = $_.Exception.Message
            if ($lastNativeError -notin @(5, 32, 33)) {
                throw "SYNAPSE_CANDIDATE_ARTIFACT_CLEANUP_FAILED reason=$Reason path=$($descriptor.Path) candidate_pid=$CandidateProcessId bind=$Bind attempt=$attempt elapsed_ms=$($clock.ElapsedMilliseconds) native_error=$lastNativeError error=$lastError remediation=inspect the exact non-transient filesystem failure; setup refuses to ignore or broaden candidate cleanup"
            }
        }

        if ($clock.Elapsed.TotalSeconds -ge $TimeoutSeconds) {
            throw "SYNAPSE_CANDIDATE_ARTIFACT_CLEANUP_TIMEOUT reason=$Reason path=$($descriptor.Path) candidate_pid=$CandidateProcessId bind=$Bind attempts=$attempt timeout_seconds=$TimeoutSeconds native_error=$lastNativeError error=$lastError remediation=identify the exact process holding this isolated candidate path, close only that owned handle, and rerun setup"
        }
        Info "Candidate artifact cleanup waiting reason=$Reason path=$($descriptor.Path) candidate_pid=$CandidateProcessId bind=$Bind attempt=$attempt elapsed_ms=$($clock.ElapsedMilliseconds) native_error=$lastNativeError error=$lastError"
        Start-Sleep -Milliseconds 250
    }
}

function Resume-SynapseCandidateCleanupIntents {
    param([Parameter(Mandatory=$true)][string]$Root)

    if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
        Info "Candidate cleanup intent recovery root=$Root intent_count=0 resumed_count=0 retained_count=0"
        return
    }
    $rootFull = [System.IO.Path]::GetFullPath($Root).TrimEnd('\')
    $resumed = 0
    $retained = 0
    $intents = @(Get-ChildItem -LiteralPath $rootFull -File -Force -Filter 'candidate-cleanup-*.json' -ErrorAction Stop | Sort-Object Name)
    foreach ($file in $intents) {
        $intent = Read-SynapseCandidateCleanupIntent -IntentPath $file.FullName -ExpectedRoot $rootFull
        if (Get-Process -Id $intent.SetupOwnerPid -ErrorAction SilentlyContinue) {
            $retained++
            Info "Candidate cleanup intent retained path=$($intent.Path) candidate=$($intent.CandidatePath) setup_owner_pid=$($intent.SetupOwnerPid) reason=setup_owner_pid_still_live"
            continue
        }
        [void](Remove-SynapseCandidateArtifact `
            -Path $intent.CandidatePath `
            -ExpectedRoot $rootFull `
            -CandidateProcessId $intent.CandidatePid `
            -Bind $intent.Bind `
            -Reason $intent.Reason)
        $resumed++
    }
    Info "Candidate cleanup intent recovery root=$rootFull intent_count=$($intents.Count) resumed_count=$resumed retained_count=$retained"
}

function Remove-SynapseStaleSuccessfulCandidateArtifacts {
    param([Parameter(Mandatory=$true)][string]$Root)

    if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
        Info "Stale successful candidate sweep root=$Root eligible_count=0 removed_count=0 retained_count=0"
        return
    }
    $rootFull = [System.IO.Path]::GetFullPath($Root).TrimEnd('\')
    $removed = 0
    $retained = 0
    $eligible = 0
    foreach ($child in @(Get-ChildItem -LiteralPath $rootFull -Directory -Force -ErrorAction Stop | Sort-Object Name)) {
        $descriptor = Get-SynapseCandidateArtifactDescriptor -Path $child.FullName -ExpectedRoot $rootFull
        $diagnosticPath = Join-Path $descriptor.Path 'candidate-diagnostic.json'
        if (Test-Path -LiteralPath $diagnosticPath -PathType Leaf) {
            $retained++
            continue
        }
        $healthPath = Join-Path $descriptor.Path 'candidate-health-after-bootstrap.json'
        if (-not (Test-Path -LiteralPath $healthPath -PathType Leaf)) {
            $retained++
            Info "WARN: SYNAPSE_CANDIDATE_STALE_STATE_AMBIGUOUS path=$($descriptor.Path) owner_pid=$($descriptor.OwnerPid) reason=validated_health_evidence_missing remediation=preserve and inspect this exact candidate directory; setup will not infer success"
            continue
        }
        try {
            $health = Get-Content -LiteralPath $healthPath -Raw -Encoding UTF8 -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
        } catch {
            throw "SYNAPSE_CANDIDATE_STALE_HEALTH_UNREADABLE path=$healthPath error=$($_.Exception.Message) remediation=preserve the exact candidate directory and inspect its post-bootstrap health evidence"
        }
        if ($health.ok -ne $true -or [int]$health.pid -le 0 -or [int]$health.tool_count -lt 1 -or [string]::IsNullOrWhiteSpace([string]$health.build)) {
            $retained++
            Info "WARN: SYNAPSE_CANDIDATE_STALE_STATE_AMBIGUOUS path=$($descriptor.Path) owner_pid=$($descriptor.OwnerPid) health_ok=$($health.ok) health_pid=$($health.pid) tool_count=$($health.tool_count) build=$($health.build) remediation=preserve and inspect this exact candidate directory; setup will not infer successful validation"
            continue
        }
        $recordedPidPath = Join-Path $descriptor.Path 'db\daemon.pid'
        $recordedPid = 0
        try {
            $recordedPidText = ([string](Get-Content -LiteralPath $recordedPidPath -Raw -Encoding UTF8 -ErrorAction Stop)).Trim()
        } catch {
            throw "SYNAPSE_CANDIDATE_STALE_PID_UNREADABLE path=$($descriptor.Path) pid_path=$recordedPidPath error=$($_.Exception.Message) remediation=preserve the exact candidate directory; stale cleanup requires its physical PID record"
        }
        if (-not [int]::TryParse($recordedPidText, [ref]$recordedPid) -or $recordedPid -ne [int]$health.pid) {
            throw "SYNAPSE_CANDIDATE_STALE_PID_MISMATCH path=$($descriptor.Path) health_pid=$($health.pid) recorded_pid=$recordedPid pid_path=$recordedPidPath remediation=preserve the directory; stale cleanup authority is inconsistent"
        }
        $recordedBind = [string]$health.subsystems.http.bind_addr
        if ([string]::IsNullOrWhiteSpace($recordedBind)) {
            throw "SYNAPSE_CANDIDATE_STALE_BIND_MISSING path=$($descriptor.Path) health_path=$healthPath candidate_pid=$recordedPid remediation=preserve the exact candidate directory; stale cleanup requires its validated HTTP bind"
        }
        if (Get-Process -Id $descriptor.OwnerPid -ErrorAction SilentlyContinue) {
            $retained++
            Info "Stale successful candidate retained path=$($descriptor.Path) owner_pid=$($descriptor.OwnerPid) candidate_pid=$recordedPid reason=setup_owner_pid_still_live"
            continue
        }
        $eligible++
        [void](Remove-SynapseCandidateArtifact `
            -Path $descriptor.Path `
            -ExpectedRoot $rootFull `
            -CandidateProcessId $recordedPid `
            -Bind $recordedBind `
            -Reason 'startup_stale_validated_candidate')
        $removed++
    }
    Info "Stale successful candidate sweep root=$rootFull eligible_count=$eligible removed_count=$removed retained_count=$retained"
}

function Remove-SynapseExpiredCandidateFailureEvidence {
    param(
        [Parameter(Mandatory=$true)][string]$Root,
        [ValidateRange(1, 20)][int]$Keep = 5
    )

    if (-not (Test-Path -LiteralPath $Root -PathType Container)) { return }
    $rootFull = [System.IO.Path]::GetFullPath($Root).TrimEnd('\')
    $verified = @(Get-ChildItem -LiteralPath $rootFull -Directory -Force -ErrorAction Stop |
        Where-Object {
            $_.Name -match '^candidate-\d{8}T\d{9}Z-\d+$' -and
            -not ($_.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -and
            (Test-Path -LiteralPath (Join-Path $_.FullName 'candidate-diagnostic.json') -PathType Leaf)
        } |
        Sort-Object LastWriteTimeUtc -Descending)
    foreach ($expired in @($verified | Select-Object -Skip $Keep)) {
        $parent = [System.IO.Path]::GetFullPath((Split-Path -Parent $expired.FullName)).TrimEnd('\')
        if (-not $parent.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase)) {
            Die "SYNAPSE_CANDIDATE_EVIDENCE_RETENTION_SCOPE_INVALID path=$($expired.FullName) expected_parent=$rootFull actual_parent=$parent remediation=do not delete anything; inspect candidate evidence paths"
        }
        Remove-Item -LiteralPath $expired.FullName -Recurse -Force -ErrorAction Stop
        if (Test-Path -LiteralPath $expired.FullName) {
            Die "SYNAPSE_CANDIDATE_EVIDENCE_RETENTION_CLEANUP_UNVERIFIED path=$($expired.FullName) remediation=inspect filesystem permissions; the expired verified failure bundle still exists"
        }
        Info "Expired candidate failure evidence removed path=$($expired.FullName) retention_keep=$Keep"
    }
}

function Stop-SynapseExactCandidateProcess {
    param(
        [Parameter(Mandatory=$true)][int]$ProcessId,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [string]$Reason = 'candidate_health'
    )

    $current = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction SilentlyContinue
    if (-not $current) {
        Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds 5
        return
    }

    $shutdown = Request-SynapseGracefulShutdown -Bind $Bind -Token $Token -ExpectedPids @($ProcessId) -Reason $Reason
    if ($shutdown.Ok) {
        $deadline = (Get-Date).AddSeconds(10)
        do {
            Start-Sleep -Milliseconds 250
            $current = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction SilentlyContinue
            if (-not $current) {
                Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds 5
                Info "Candidate daemon graceful shutdown verified pid=$ProcessId bind=$Bind"
                return
            }
        } while ((Get-Date) -lt $deadline)
    } else {
        Info "WARN: candidate graceful shutdown failed pid=$ProcessId bind=$Bind code=$($shutdown.Code) error=$($shutdown.Error); falling back to exact spawned PID stop"
    }

    $current = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction SilentlyContinue
    if ($current) {
        $exeLeaf = if ($current.ExecutablePath) { Split-Path -Leaf $current.ExecutablePath } else { '' }
        if (-not (Test-SynapseMcpExecutableLeafName -Name $current.Name) -and -not (Test-SynapseMcpExecutableLeafName -Name $exeLeaf)) {
            Die "SYNAPSE_CANDIDATE_STOP_TARGET_MISMATCH pid=$ProcessId actual_name=$($current.Name) actual_path=$($current.ExecutablePath) remediation=PID was reused before candidate cleanup; refusing to stop it"
        }
        Stop-Process -Id $ProcessId -Force -ErrorAction Stop
        Info "Candidate daemon exact spawned PID stop issued pid=$ProcessId bind=$Bind"
    }
    Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds 5
}

function Get-SynapseCandidateReplacementReservationId {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token
    )

    $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
    if ($listeners.Count -eq 0) { return $null }
    if ($listeners.Count -ne 1) {
        Die "SYNAPSE_GPU_REPLACEMENT_LISTENER_AMBIGUOUS bind=$Bind listeners=$(Format-SynapseTcpBindListenerSnapshot -Snapshot $listeners) remediation=repair the configured daemon listener before candidate validation"
    }
    $listener = $listeners[0]
    if (-not $listener.OwnerExists -or -not (Test-SynapseMcpExecutableLeafName -Name $listener.OwnerName)) {
        Die "SYNAPSE_GPU_REPLACEMENT_LISTENER_INVALID bind=$Bind pid=$($listener.OwningProcess) owner=$($listener.OwnerName) remediation=the configured bind must be owned by the exact live Synapse daemon before replacement admission"
    }

    $healthRead = Read-SynapseHealthForRestartGuard -Bind $Bind -Token $Token -TimeoutSec 10
    if (-not $healthRead.Ok) {
        Die "SYNAPSE_GPU_REPLACEMENT_HEALTH_UNREADABLE bind=$Bind live_pid=$($listener.OwningProcess) error=$($healthRead.Error) remediation=repair authenticated health for the exact live daemon before candidate validation"
    }
    $health = $healthRead.Health
    # #1890: identity and fitness are two different questions about the outgoing daemon and must
    # not share one gate. Identity (does the process answering /health hold the listen socket?) is
    # the real replacement hazard -- a leaked socket or a second daemon -- and still refuses.
    # Fitness (health.ok) is NOT a precondition for replacing the outgoing daemon: an unhealthy
    # daemon is the strongest reason to replace it, and gating on it deadlocks every deploy whose
    # whole purpose is to fix why health is false. The candidate's fitness is decided separately
    # in Test-SynapseCandidateDaemon, which is the process whose fitness is actually in question.
    $liveHealthPid = if ($null -eq $health.pid) { 0 } else { [int]$health.pid }
    if ($liveHealthPid -ne [int]$listener.OwningProcess) {
        Die "SYNAPSE_GPU_REPLACEMENT_HEALTH_IDENTITY_MISMATCH bind=$Bind listener_pid=$($listener.OwningProcess) health_pid=$(if ($null -eq $health.pid) { '<absent>' } else { $liveHealthPid }) failing_term=health_pid_ne_listener_pid remediation=the process answering authenticated /health is not the process holding the listen socket; repair the exact daemon process/socket identity (leaked listen socket or a second daemon) before candidate validation"
    }
    if ($health.ok -ne $true) {
        $liveSubsystemStatuses = @()
        if ($health.subsystems) {
            foreach ($prop in @($health.subsystems.PSObject.Properties | Sort-Object Name)) {
                $status = [string]$prop.Value.status
                if ($status -and $status -ne 'ok') { $liveSubsystemStatuses += ("{0}={1}" -f $prop.Name, $status) }
            }
        }
        $liveStatusText = if ($liveSubsystemStatuses.Count -gt 0) { $liveSubsystemStatuses -join ',' } else { '<none-not-ok>' }
        Info "SYNAPSE_GPU_REPLACEMENT_OUTGOING_DAEMON_UNHEALTHY bind=$Bind live_pid=$($listener.OwningProcess) health_ok=false identity_verified=true not_ok_subsystems=$liveStatusText note=replacement proceeds; process/socket/health identity is consistent and replacing an unhealthy daemon is the remedy, not a reason to refuse"
    }
    $calyxHealth = $health.subsystems.calyx_vault
    $selectedBackend = [string]$calyxHealth.calyx_math_backend
    $probeStatus = [string]$calyxHealth.calyx_math_probe_status
    $healthReservationId = [string]$calyxHealth.calyx_gpu_reservation_id
    # Same split for the Calyx subsystem: the datum this function needs from it is the selected math
    # backend (which decides whether a GPU reservation has to be handed off at all). That datum being
    # readable is the precondition; the subsystem's aggregate verdict is not. Every exactness check
    # the handoff actually depends on -- ledger row, reservation id shape, lease file -- still runs
    # below and still refuses under its own precise code.
    if ([string]::IsNullOrWhiteSpace($selectedBackend)) {
        Die "SYNAPSE_GPU_REPLACEMENT_CALYX_BACKEND_UNREADABLE bind=$Bind live_pid=$($listener.OwningProcess) calyx_status=$(if ([string]$calyxHealth.status) { [string]$calyxHealth.status } else { '<absent>' }) selected_backend=<absent> failing_term=calyx_math_backend_absent remediation=authenticated health must name the live daemon's Calyx math backend so the GPU reservation handoff can be decided; repair the live Calyx vault subsystem before candidate validation"
    }
    if ([string]$calyxHealth.status -ne 'ok') {
        Info "SYNAPSE_GPU_REPLACEMENT_OUTGOING_CALYX_NOT_OK bind=$Bind live_pid=$($listener.OwningProcess) calyx_status=$([string]$calyxHealth.status) selected_backend=$selectedBackend note=replacement proceeds; the backend needed for the reservation decision is readable and every reservation exactness check below still refuses under its own code"
    }

    $programData = [Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
    if ([string]::IsNullOrWhiteSpace($programData)) {
        Die 'SYNAPSE_GPU_REPLACEMENT_ROOT_UNAVAILABLE remediation=Windows CommonApplicationData is required to verify the host GPU reservation ledger'
    }
    $statePath = Join-Path $programData 'Calyx\gpu-reservations\device-0\reservations.json'
    $state = $null
    $statePresent = Test-Path -LiteralPath $statePath -PathType Leaf
    if ($statePresent) {
        try {
            $state = Get-Content -LiteralPath $statePath -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
        } catch {
            Die "SYNAPSE_GPU_REPLACEMENT_LEDGER_UNREADABLE path=$statePath error=$($_.Exception.Message) remediation=repair the host GPU reservation Source of Truth before candidate validation"
        }
        $stateProperties = @($state.PSObject.Properties.Name)
        if ([int]$state.schema_version -ne 1 -or [int]$state.device_index -ne 0 -or $stateProperties -notcontains 'reservations') {
            Die "SYNAPSE_GPU_REPLACEMENT_LEDGER_SCHEMA_INVALID path=$statePath schema_version=$(if ($null -eq $state.schema_version) { '<absent>' } else { [string]$state.schema_version }) device_index=$(if ($null -eq $state.device_index) { '<absent>' } else { [string]$state.device_index }) reservations_present=$($stateProperties -contains 'reservations') remediation=repair the device-0 host GPU reservation ledger schema before candidate validation"
        }
    }

    $pidRows = if ($null -eq $state) {
        @()
    } else {
        @($state.reservations | Where-Object { [int]$_.pid -eq [int]$listener.OwningProcess })
    }
    if ($selectedBackend -ine 'cuda') {
        if (-not [string]::IsNullOrWhiteSpace($healthReservationId) -or $pidRows.Count -ne 0) {
            Die "SYNAPSE_GPU_REPLACEMENT_NON_CUDA_STATE_CONFLICT path=$(if ($statePresent) { $statePath } else { '<absent>' }) live_pid=$($listener.OwningProcess) selected_backend=$selectedBackend health_reservation_id=$(if ($healthReservationId) { $healthReservationId } else { '<none>' }) ledger_pid_row_count=$($pidRows.Count) remediation=repair the mixed Calyx backend/GPU reservation state before candidate validation"
        }
        Info "Candidate replacement reservation not required bind=$Bind live_pid=$($listener.OwningProcess) selected_backend=$selectedBackend health_reservation_id=<none> ledger_pid_row_count=0 state_path=$(if ($statePresent) { $statePath } else { '<absent>' })"
        return $null
    }

    if (-not $statePresent) {
        Die "SYNAPSE_GPU_REPLACEMENT_LEDGER_MISSING path=$statePath live_pid=$($listener.OwningProcess) selected_backend=$selectedBackend health_reservation_id=$(if ($healthReservationId) { $healthReservationId } else { '<none>' }) remediation=repair the live CUDA daemon GPU reservation Source of Truth before candidate validation"
    }
    # #2239 made CUDA ownership demand-driven. `dormant` means no caller has
    # created a context or reservation yet; after a caller, the final release
    # destroys the context and publishes `dormant_verified` only after a
    # separate ledger read. A candidate therefore needs a replacement
    # reservation only while the outgoing daemon has an active lease. Preserve
    # every mixed-state refusal: only those two explicit idle lifecycle states
    # are accepted, and only when health plus the current physical ledger
    # independently prove there is nothing to hand off.
    if (@('dormant', 'dormant_verified') -icontains $probeStatus) {
        if (-not [string]::IsNullOrWhiteSpace($healthReservationId) -or $pidRows.Count -ne 0) {
            Die "SYNAPSE_GPU_REPLACEMENT_IDLE_STATE_CONFLICT path=$statePath live_pid=$($listener.OwningProcess) selected_backend=$selectedBackend probe_status=$probeStatus health_reservation_id=$(if ($healthReservationId) { $healthReservationId } else { '<none>' }) ledger_pid_row_count=$($pidRows.Count) remediation=dormant and dormant_verified require an absent health reservation identity and zero physical ledger rows for the exact live daemon"
        }
        try {
            $stateHash = (Get-FileHash -LiteralPath $statePath -Algorithm SHA256 -ErrorAction Stop).Hash
        } catch {
            Die "SYNAPSE_GPU_REPLACEMENT_LEDGER_HASH_FAILED path=$statePath live_pid=$($listener.OwningProcess) error=$($_.Exception.Message) remediation=repair physical read access to the host GPU reservation Source of Truth before candidate validation"
        }
        Info "SYNAPSE_GPU_REPLACEMENT_IDLE_VERIFIED bind=$Bind live_pid=$($listener.OwningProcess) selected_backend=$selectedBackend probe_status=$probeStatus health_reservation_id=<none> ledger_pid_row_count=0 ledger_reservation_count=$(@($state.reservations).Count) ledger_sha256=$stateHash state_path=$statePath note=no active CUDA context or host lease exists to hand off; candidate admission remains independently validated"
        return $null
    }
    if ($healthReservationId -notmatch '^[0-9a-fA-F]{32}$') {
        Die "SYNAPSE_GPU_REPLACEMENT_HEALTH_RESERVATION_INVALID path=$statePath live_pid=$($listener.OwningProcess) selected_backend=$selectedBackend probe_status=$(if ($probeStatus) { $probeStatus } else { '<absent>' }) health_reservation_id=$(if ($healthReservationId) { $healthReservationId } else { '<none>' }) remediation=repair the live CUDA daemon health reservation identity before candidate validation"
    }
    $matches = @($pidRows | Where-Object {
        [string]$_.owner -eq 'synapse-mcp' -and
        [string]$_.reservation_id -ieq $healthReservationId -and
        [string]::IsNullOrWhiteSpace([string]$_.replaces_reservation_id)
    })
    if ($pidRows.Count -ne 1 -or $matches.Count -ne 1) {
        Die "SYNAPSE_GPU_REPLACEMENT_RESERVATION_AMBIGUOUS path=$statePath live_pid=$($listener.OwningProcess) health_reservation_id=$healthReservationId ledger_pid_row_count=$($pidRows.Count) exact_match_count=$($matches.Count) remediation=the live CUDA daemon must own exactly one primary device-0 reservation matching authenticated health before candidate validation"
    }
    $row = $matches[0]
    if (-not (Test-Path -LiteralPath ([string]$row.lease_file) -PathType Leaf)) {
        Die "SYNAPSE_GPU_REPLACEMENT_RESERVATION_INVALID path=$statePath live_pid=$($listener.OwningProcess) reservation_id=$($row.reservation_id) lease=$($row.lease_file) remediation=repair the exact live reservation row/lease before candidate validation"
    }
    Info "Candidate replacement reservation verified bind=$Bind live_pid=$($listener.OwningProcess) reservation_id=$($row.reservation_id) requested_mib=$($row.requested_mib) state_path=$statePath"
    return [string]$row.reservation_id
}

function Write-SynapseCandidateJsonEvidence {
    param(
        [Parameter(Mandatory=$true)][string]$CandidateRoot,
        [Parameter(Mandatory=$true)][ValidatePattern('^candidate-[a-z0-9-]+\.json$')][string]$LeafName,
        [Parameter(Mandatory=$true)]$Value
    )

    $rootFull = [System.IO.Path]::GetFullPath($CandidateRoot).TrimEnd('\')
    $path = [System.IO.Path]::GetFullPath((Join-Path $CandidateRoot $LeafName))
    $parent = [System.IO.Path]::GetFullPath((Split-Path -Parent $path)).TrimEnd('\')
    if (-not $parent.Equals($rootFull, [System.StringComparison]::OrdinalIgnoreCase)) {
        Die "SYNAPSE_CANDIDATE_EVIDENCE_SCOPE_INVALID path=$path expected_parent=$rootFull actual_parent=$parent remediation=refuse to write candidate evidence outside the exact isolated candidate directory"
    }
    try {
        $json = $Value | ConvertTo-Json -Depth 30
        [System.IO.File]::WriteAllText($path, $json, [System.Text.UTF8Encoding]::new($false))
        $readback = Get-Content -Raw -LiteralPath $path -ErrorAction Stop | ConvertFrom-Json -Depth 30 -ErrorAction Stop
        $sha256 = Get-SynapseFileSha256 -Path $path
        $length = (Get-Item -LiteralPath $path -ErrorAction Stop).Length
    } catch {
        Die "SYNAPSE_CANDIDATE_EVIDENCE_WRITE_FAILED path=$path error=$($_.Exception.Message) remediation=repair the exact candidate evidence directory before setup can accept or reject this binary"
    }
    return [pscustomobject]@{
        Path = $path
        Sha256 = $sha256
        Length = $length
        Readback = $readback
    }
}

function Format-SynapseCandidateHealthFailures {
    param([Parameter(Mandatory=$true)]$Health)

    $failures = @()
    if ($Health.subsystems) {
        foreach ($prop in @($Health.subsystems.PSObject.Properties | Sort-Object Name)) {
            $status = [string]$prop.Value.status
            if ($status -eq 'error') {
                $failures += [ordered]@{
                    name = [string]$prop.Name
                    status = $status
                    detail = [string]$prop.Value.detail
                }
            }
        }
    }
    return @($failures)
}

function Test-SynapseCandidateDaemon {
    param(
        [Parameter(Mandatory=$true)][string]$CandidateExePath,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [bool]$EnableAudio,
        [AllowNull()][string]$AllowedPermissions,
        [AllowNull()][string]$CalyxConfigPath,
        [AllowNull()][string]$ReplacementReservationId = $null
    )

    if (-not (Test-Path -LiteralPath $CandidateExePath)) {
        Die "SYNAPSE_CANDIDATE_BINARY_MISSING path=$CandidateExePath remediation=build or provide a real synapse-mcp.exe before setup can touch the live daemon"
    }
    if (-not (Test-Path -LiteralPath $ProfilesDir)) {
        Die "SYNAPSE_CANDIDATE_PROFILES_MISSING path=$ProfilesDir remediation=build/deploy profiles before validating the daemon candidate"
    }
    $tokenRead = Read-SynapseSetupTokenForRestartGuard -TokenPath $TokenPath
    if (-not $tokenRead.Ok) {
        Die "$($tokenRead.Code) stage=candidate_health $($tokenRead.Detail) remediation=setup must have a valid bearer token before candidate health can be proven"
    }

    $candidateRoot = New-SynapseSetupRunDirectory -Root (Join-Path $LogDir 'setup-candidates') -Purpose 'candidate'
    $candidateDb = Join-Path $candidateRoot 'db'
    $candidateCalyxVault = $candidateDb
    $candidateShellJobRoot = Join-Path $candidateRoot 'shell-jobs'
    New-Item -ItemType Directory -Force -Path $candidateDb | Out-Null
    New-Item -ItemType Directory -Force -Path $candidateShellJobRoot | Out-Null
    $candidateBind = New-SynapseCandidateBind
    $candidateHash = Get-SynapseFileSha256 -Path $CandidateExePath
    $candidateStdout = Join-Path $candidateRoot 'candidate-stdout.log'
    $candidateStderr = Join-Path $candidateRoot 'candidate-stderr.log'
    $candidateCalyxConfig = if ([string]::IsNullOrWhiteSpace($CalyxConfigPath)) { '<defaults>' } else { $CalyxConfigPath }
    Info "Candidate daemon health preflight starting exe=$CandidateExePath sha256=$candidateHash bind=$candidateBind db=$candidateDb calyx_vault=$candidateCalyxVault calyx_config=$candidateCalyxConfig shell_job_root=$candidateShellJobRoot profiles=$ProfilesDir"

    $candidate = $null
    $health = $null
    $lastHealthError = $null
    $surface = $null
    $candidateSucceeded = $false
    $candidateFailureMessage = $null
    try {
        $previousShellJobRoot = Get-Item Env:SYNAPSE_SHELL_JOB_ROOT -ErrorAction SilentlyContinue
        $replacementEnvName = 'SYNAPSE_CALYX_GPU_REPLACEMENT_RESERVATION_ID'
        $previousReplacementId = Get-Item "Env:$replacementEnvName" -ErrorAction SilentlyContinue
        try {
            $env:SYNAPSE_SHELL_JOB_ROOT = $candidateShellJobRoot
            if ([string]::IsNullOrWhiteSpace($ReplacementReservationId)) {
                Remove-Item "Env:$replacementEnvName" -ErrorAction SilentlyContinue
            } else {
                Set-Item "Env:$replacementEnvName" -Value $ReplacementReservationId
            }
            $candidateArgs = @('--mode','http','--bind',$candidateBind,'--db',$candidateDb,'--profile-dir',$ProfilesDir,'--calyx-vault-dir',$candidateCalyxVault,'--log-level','info')
            if (-not [string]::IsNullOrWhiteSpace($CalyxConfigPath)) {
                $candidateArgs += @('--calyx-config', $CalyxConfigPath)
            }
            if ($EnableAudio) {
                $candidateArgs += '--enable-audio'
            }
            $allowedPermissionsArgument = Normalize-SynapseAllowedPermissionsArgument -Value $AllowedPermissions
            if (-not [string]::IsNullOrWhiteSpace($allowedPermissionsArgument)) {
                $candidateArgs += @('--allowed-permissions', $allowedPermissionsArgument)
            }
            $candidate = Start-Process `
                -FilePath $CandidateExePath `
                -ArgumentList $candidateArgs `
                -WindowStyle Hidden `
                -RedirectStandardOutput $candidateStdout `
                -RedirectStandardError $candidateStderr `
                -PassThru
        } finally {
            if ($previousShellJobRoot) {
                $env:SYNAPSE_SHELL_JOB_ROOT = $previousShellJobRoot.Value
            } else {
                Remove-Item Env:SYNAPSE_SHELL_JOB_ROOT -ErrorAction SilentlyContinue
            }
            if ($previousReplacementId) {
                Set-Item "Env:$replacementEnvName" -Value $previousReplacementId.Value
            } else {
                Remove-Item "Env:$replacementEnvName" -ErrorAction SilentlyContinue
            }
        }
        $candidateWatchdog = New-SynapseDaemonStartupWatchdog `
            -Phase 'candidate' `
            -LogDir $candidateRoot `
            -Bind $candidateBind `
            -DbPath $candidateDb `
            -VaultPath $candidateCalyxVault `
            -BaseTimeoutSeconds 25 `
            -MaxSeconds 180 `
            -StallSeconds 45 `
            -SampleIntervalSeconds 5
        $candidateGateVerdict = 'continue'
        Info ("SYNAPSE_STARTUP_WATCHDOG_ARMED phase=candidate base_timeout_s={0} absolute_cap_s={1} stall_window_s={2} sample_interval_s={3} vault={4} remediation=candidate validation keeps waiting only while the exact isolated process proves CPU, I/O, or vault progress and always retains a hard ceiling" -f `
            $candidateWatchdog.BaseTimeoutSeconds,
            $candidateWatchdog.MaxSeconds,
            $candidateWatchdog.StallSeconds,
            $candidateWatchdog.SampleIntervalSeconds,
            $candidateWatchdog.VaultPath)
        while ($true) {
            Start-Sleep -Milliseconds 500
            $read = Read-SynapseHealthForRestartGuard -Bind $candidateBind -Token $tokenRead.Token -TimeoutSec 2
            if ($read.Ok) {
                $health = $read.Health
                break
            }
            $lastHealthError = $read.Error
            $candidateTick = Update-SynapseDaemonStartupWatchdog -Watchdog $candidateWatchdog
            if (-not $candidateTick.Continue) {
                $candidateGateVerdict = $candidateTick.Verdict
                break
            }
        }

        if ($null -eq $health) {
            $alive = [bool](Get-Process -Id $candidate.Id -ErrorAction SilentlyContinue)
            $exit = Get-SynapseCandidateExitReadback -Process $candidate
            $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $candidateBind)
            $candidatePhysicalState = Format-SynapseDaemonStartupPhysicalState -DbPath $candidateDb -CalyxVaultPath $candidateCalyxVault
            $stdoutHash = if (Test-Path -LiteralPath $candidateStdout -PathType Leaf) { Get-SynapseFileSha256 -Path $candidateStdout } else { '<missing>' }
            $stderrHash = if (Test-Path -LiteralPath $candidateStderr -PathType Leaf) { Get-SynapseFileSha256 -Path $candidateStderr } else { '<missing>' }
            Die ("SYNAPSE_CANDIDATE_HEALTH_FAILED exe={0} sha256={1} pid={2} alive={3} exit_code_signed={4} exit_code_hex={5} bind={6} listeners={7} gate_verdict={8} last_error={9} evidence_root={10} stdout={11} stdout_sha256={12} stderr={13} stderr_sha256={14}`nstartup_watchdog:`n{15}`nphysical_storage_state:`n{16}`nremediation={17} Old live daemon was not touched. Inspect retained candidate-diagnostic.json, stdout, stderr, and isolated lifecycle ledgers." -f `
                $CandidateExePath,
                $candidateHash,
                $candidate.Id,
                $alive,
                $(if ($null -eq $exit.ExitCodeSigned) { '<running-or-unavailable>' } else { $exit.ExitCodeSigned }),
                $(if ($null -eq $exit.ExitCodeHex) { '<running-or-unavailable>' } else { $exit.ExitCodeHex }),
                $candidateBind,
                (Format-SynapseTcpBindListenerSnapshot -Snapshot $listeners),
                $candidateGateVerdict,
                $lastHealthError,
                $candidateRoot,
                $candidateStdout,
                $stdoutHash,
                $candidateStderr,
                $stderrHash,
                (Format-SynapseDaemonStartupWatchdogState -Watchdog $candidateWatchdog),
                $candidatePhysicalState,
                (Get-SynapseDaemonStartupWatchdogRemediation -Verdict $candidateGateVerdict -Phase 'candidate'))
        }

        $healthPid = [int]$health.pid
        if ($healthPid -ne [int]$candidate.Id) {
            Die "SYNAPSE_CANDIDATE_PID_MISMATCH expected_pid=$($candidate.Id) health_pid=$healthPid bind=$candidateBind remediation=health came from an unexpected process; refusing handoff"
        }
        $initialHealthEvidence = Write-SynapseCandidateJsonEvidence `
            -CandidateRoot $candidateRoot `
            -LeafName 'candidate-health-before.json' `
            -Value $health
        Info "Candidate initial health evidence written path=$($initialHealthEvidence.Path) sha256=$($initialHealthEvidence.Sha256) length=$($initialHealthEvidence.Length) ok=$($health.ok)"

        # #2196: global health deliberately treats a known dirty build as
        # degraded rather than operationally dead, but a production installer
        # has a stronger contract: it must never replace the live binary with
        # bytes that do not attest to one exact clean checkout. Keep that policy
        # here at the deployment boundary instead of weakening health's useful
        # diagnostic distinction.
        $buildProvenance = $health.subsystems.build_provenance
        $buildProvenanceAcceptable = (
            $null -ne $buildProvenance -and
            [string]$buildProvenance.status -eq 'ok' -and
            [string]$buildProvenance.build_tree_state -eq 'clean' -and
            $buildProvenance.build_matches_checkout -eq $true -and
            [int64]$buildProvenance.build_changed_input_count -eq 0 -and
            [int64]$buildProvenance.build_changed_input_omitted -eq 0)
        if (-not $buildProvenanceAcceptable) {
            $provenanceReadback = if ($null -eq $buildProvenance) { '<missing>' } else { $buildProvenance | ConvertTo-Json -Depth 12 -Compress }
            Die "SYNAPSE_CANDIDATE_BUILD_PROVENANCE_UNACCEPTABLE exe=$CandidateExePath sha256=$candidateHash pid=$healthPid bind=$candidateBind health_evidence=$($initialHealthEvidence.Path) health_evidence_sha256=$($initialHealthEvidence.Sha256) build_provenance=$provenanceReadback remediation=build from one exact clean Git checkout, commit it locally before release validation, and rerun setup; the old installed executable and live daemon have not been touched"
        }
        if ($EnableAudio) {
            $audioHealth = $health.subsystems.audio
            if ($null -eq $audioHealth -or [string]$audioHealth.status -eq 'disabled' -or $audioHealth.stt_model_available -ne $true) {
                $audioReadback = if ($null -eq $audioHealth) { '<missing>' } else { $audioHealth | ConvertTo-Json -Depth 8 -Compress }
                Die "SYNAPSE_CANDIDATE_AUDIO_CONTRACT_FAILED pid=$healthPid bind=$candidateBind audio=$audioReadback remediation=verify --enable-audio reached the candidate, READ_AUDIO is granted, and the pinned Whisper/ORT Extensions artifacts are packaged before replacing the live daemon"
            }
        }

        # The isolated vault is intentionally new, so its active panel has no
        # search generation yet. Do not reinterpret that real error as success.
        # Bootstrap it through the same public, permission-gated, audited route
        # an operator uses, then independently read health and the manifest from
        # disk. Any other error is left untouched and refused below.
        if ($health.ok -ne $true) {
            $initialFailures = @(Format-SynapseCandidateHealthFailures -Health $health)
            $searchHealth = $health.subsystems.calyx_search_generation
            $searchPanelVersion = try { [int64]$searchHealth.calyx_search_generation_panel_version } catch { 0 }
            $isExactEmptySearchState = (
                $initialFailures.Count -eq 1 -and
                [string]$initialFailures[0].name -eq 'calyx_search_generation' -and
                [string]$searchHealth.status -eq 'error' -and
                [string]$searchHealth.calyx_search_generation_state -eq 'absent' -and
                $searchHealth.calyx_search_generation_manifest_present -eq $false -and
                $searchPanelVersion -gt 0 -and
                [string]$health.subsystems.calyx_vault.status -eq 'ok' -and
                [string]$health.subsystems.storage.status -eq 'ok')
            if ($isExactEmptySearchState) {
                $searchArgs = [ordered]@{
                    operation = 'search_rebuild'
                    search_rebuild = [ordered]@{
                        expected_panel_version = $searchPanelVersion
                    }
                }
                Info "Candidate isolated vault has no active-panel search generation; bootstrapping through public storage facade panel_version=$searchPanelVersion initial_health_sha256=$($initialHealthEvidence.Sha256)"
                $searchBootstrap = Invoke-SynapseSetupMcpTool `
                    -Bind $candidateBind `
                    -Token $tokenRead.Token `
                    -Name 'storage' `
                    -Arguments $searchArgs `
                    -Profile 'break_glass' `
                    -ProfileReason 'synapse-setup candidate preflight must build and physically verify the isolated active-panel search generation before handoff' `
                    -AcquireForegroundLease `
                    -TimeoutSec 120
                $searchBootstrapEvidence = Write-SynapseCandidateJsonEvidence `
                    -CandidateRoot $candidateRoot `
                    -LeafName 'candidate-search-bootstrap.json' `
                    -Value $searchBootstrap.Json
                $searchReadback = $searchBootstrap.Json.search_rebuild
                $manifestPath = [string]$searchReadback.manifest_path
                $manifestExpectedSha256 = [string]$searchReadback.manifest_sha256
                if ($null -eq $searchReadback -or [int64]$searchReadback.panel_version -ne $searchPanelVersion -or [string]::IsNullOrWhiteSpace($manifestPath) -or [string]::IsNullOrWhiteSpace($manifestExpectedSha256) -or -not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
                    $bootstrapReadback = if ($null -eq $searchReadback) { '<missing>' } else { $searchReadback | ConvertTo-Json -Depth 16 -Compress }
                    Die "SYNAPSE_CANDIDATE_SEARCH_BOOTSTRAP_READBACK_INVALID pid=$healthPid bind=$candidateBind panel_version=$searchPanelVersion evidence=$($searchBootstrapEvidence.Path) evidence_sha256=$($searchBootstrapEvidence.Sha256) readback=$bootstrapReadback remediation=the real storage search_rebuild call did not publish and report the exact active-panel manifest; inspect retained candidate evidence before retrying"
                }
                $manifestActualSha256 = Get-SynapseFileSha256 -Path $manifestPath
                if ($manifestActualSha256 -ine $manifestExpectedSha256) {
                    Die "SYNAPSE_CANDIDATE_SEARCH_BOOTSTRAP_MANIFEST_MISMATCH pid=$healthPid bind=$candidateBind panel_version=$searchPanelVersion manifest=$manifestPath expected_sha256=$manifestExpectedSha256 actual_sha256=$manifestActualSha256 evidence=$($searchBootstrapEvidence.Path) remediation=the independently read physical manifest bytes do not match the storage facade's committed readback; inspect the isolated vault and candidate logs"
                }
                $postBootstrapHealthRead = Read-SynapseHealthForRestartGuard -Bind $candidateBind -Token $tokenRead.Token -TimeoutSec 30
                if (-not $postBootstrapHealthRead.Ok) {
                    Die "SYNAPSE_CANDIDATE_POST_BOOTSTRAP_HEALTH_UNREADABLE pid=$healthPid bind=$candidateBind panel_version=$searchPanelVersion manifest=$manifestPath manifest_sha256=$manifestActualSha256 error=$($postBootstrapHealthRead.Error) remediation=the search manifest exists on disk but authenticated health cannot independently report candidate state; inspect retained candidate evidence"
                }
                $health = $postBootstrapHealthRead.Health
                if ([int]$health.pid -ne $healthPid) {
                    Die "SYNAPSE_CANDIDATE_POST_BOOTSTRAP_PID_MISMATCH expected_pid=$healthPid health_pid=$($health.pid) bind=$candidateBind remediation=post-bootstrap health came from an unexpected process; refusing handoff"
                }
                $postBootstrapHealthEvidence = Write-SynapseCandidateJsonEvidence `
                    -CandidateRoot $candidateRoot `
                    -LeafName 'candidate-health-after-bootstrap.json' `
                    -Value $health
                Info "Candidate isolated search generation physically verified panel_version=$searchPanelVersion manifest=$manifestPath manifest_sha256=$manifestActualSha256 health_evidence=$($postBootstrapHealthEvidence.Path) health_evidence_sha256=$($postBootstrapHealthEvidence.Sha256) health_ok=$($health.ok)"
            }
        }
        if ($health.ok -ne $true) {
            $failures = @(Format-SynapseCandidateHealthFailures -Health $health)
            $failuresJson = if ($failures.Count -eq 0) { '[]' } else { $failures | ConvertTo-Json -Depth 12 -Compress }
            $subsystemStatuses = @()
            if ($health.subsystems) {
                foreach ($prop in @($health.subsystems.PSObject.Properties | Sort-Object Name)) {
                    $subsystemStatuses += ("{0}={1}" -f $prop.Name, ([string]$prop.Value.status))
                }
            }
            $statusText = if ($subsystemStatuses.Count -gt 0) { $subsystemStatuses -join ',' } else { '<none>' }
            Die "SYNAPSE_CANDIDATE_HEALTH_UNHEALTHY exe=$CandidateExePath sha256=$candidateHash pid=$healthPid bind=$candidateBind failures=$failuresJson subsystem_statuses=$statusText health_evidence=$($initialHealthEvidence.Path) health_evidence_sha256=$($initialHealthEvidence.Sha256) evidence_root=$candidateRoot stdout=$candidateStdout stderr=$candidateStderr remediation=repair every subsystem reporting error and rerun setup; setup refuses to replace the old installed executable or live daemon while candidate semantic health is false"
        }
        $surface = Read-SynapseDaemonToolSurface -Bind $candidateBind -Token $tokenRead.Token -Health $health
        if ($surface.tool_count -lt 1) {
            Die "SYNAPSE_CANDIDATE_TOOL_SURFACE_EMPTY pid=$healthPid bind=$candidateBind remediation=tools/list returned no tools; refusing handoff"
        }
        Info "Candidate daemon health preflight passed pid=$healthPid bind=$candidateBind tool_count=$($surface.tool_count) tool_surface_sha256=$($surface.tool_surface_sha256)"
        $candidateSucceeded = $true
        return [pscustomobject]@{
            Ok = $true
            Pid = $healthPid
            Bind = $candidateBind
            DbPath = $candidateDb
            ShellJobRoot = $candidateShellJobRoot
            ExePath = $CandidateExePath
            Sha256 = $candidateHash
            ToolCount = $surface.tool_count
            ToolSurfaceSha256 = $surface.tool_surface_sha256
            ToolNames = $surface.tool_names
            ToolSchemas = $surface.tool_schemas
            ToolSurface = $surface
            tool_count = $surface.tool_count
            tool_surface_sha256 = $surface.tool_surface_sha256
            tool_names = $surface.tool_names
            tool_schemas = $surface.tool_schemas
            daemon_pid = $healthPid
        }
    } catch {
        $candidateFailureMessage = $_.Exception.Message
        throw
    } finally {
        if ($candidate -and (Get-Process -Id $candidate.Id -ErrorAction SilentlyContinue)) {
            Stop-SynapseExactCandidateProcess -ProcessId ([int]$candidate.Id) -Bind $candidateBind -Token $tokenRead.Token -Reason 'candidate_health'
        } elseif ($candidateBind) {
            Wait-SynapseBindReleased -Reason 'candidate_health' -Bind $candidateBind -TimeoutSeconds 5
        }
        if ((Test-Path -LiteralPath $candidateRoot) -and -not $candidateSucceeded) {
            try {
                $diagnostic = Write-SynapseCandidateFailureEvidence `
                    -CandidateRoot $candidateRoot `
                    -Process $candidate `
                    -FailureMessage $(if ([string]::IsNullOrWhiteSpace($candidateFailureMessage)) { 'candidate validation failed without a captured exception message' } else { $candidateFailureMessage }) `
                    -Bind $candidateBind `
                    -ExecutablePath $CandidateExePath `
                    -ExecutableSha256 $candidateHash
                Info "Candidate failure evidence retained path=$($diagnostic.Path) sha256=$($diagnostic.Sha256) evidence_count=$($diagnostic.EvidenceCount) exit_code_signed=$($diagnostic.Exit.ExitCodeSigned) exit_code_hex=$($diagnostic.Exit.ExitCodeHex)"
                Remove-SynapseExpiredCandidateFailureEvidence -Root (Join-Path $LogDir 'setup-candidates') -Keep 5
            } catch {
                throw "SYNAPSE_CANDIDATE_EVIDENCE_WRITE_FAILED path=$candidateRoot error=$($_.Exception.Message) original_failure=[$candidateFailureMessage] remediation=preserve the exact candidate directory and repair log-directory permissions before rerunning setup"
            }
        } elseif (Test-Path -LiteralPath $candidateRoot) {
            [void](Remove-SynapseCandidateArtifact `
                -Path $candidateRoot `
                -ExpectedRoot (Join-Path $LogDir 'setup-candidates') `
                -CandidateProcessId ([int]$candidate.Id) `
                -Bind $candidateBind `
                -Reason 'validated_candidate_success')
        }
    }
}

function Write-SynapseCodexToolSurfaceSnapshot {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)]$Surface
    )

    try {
        $dir = Split-Path -Parent $Path
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $json = $Surface | ConvertTo-Json -Depth 20
        $encoding = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllText($Path, $json, $encoding)
    } catch {
        Die "SYNAPSE_CODEX_TOOL_SURFACE_SNAPSHOT_WRITE_FAILED path=$Path error=$($_.Exception.Message) remediation=repair permissions on the Synapse appdata directory before starting Codex"
    }
    Info "Codex tool-surface snapshot written path=$Path daemon_pid=$($Surface.daemon_pid) tool_count=$($Surface.tool_count) tool_surface_sha256=$($Surface.tool_surface_sha256)"
}

function Read-SynapseCodexToolSurfaceSnapshotOrNull {
    param([AllowNull()][string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path)) {
        return $null
    }
    try {
        return Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
    } catch {
        return [pscustomobject]@{
            unreadable = $true
            path = $Path
            error = $_.Exception.Message
        }
    }
}

function New-SynapseToolRecordMap {
    param([AllowNull()]$Surface)

    $map = @{}
    if ($null -eq $Surface -or $Surface.unreadable) {
        return $map
    }
    foreach ($record in @($Surface.tool_schemas)) {
        $name = [string]$record.name
        if (-not [string]::IsNullOrWhiteSpace($name)) {
            $map[$name] = $record
        }
    }
    return $map
}

function Get-SynapseNullableSchemaHash {
    param([AllowNull()]$Schema)

    if ($null -eq $Schema) {
        return '<null>'
    }
    return Get-SynapseSha256Hex -Text (Get-SynapseCanonicalJson -Value $Schema)
}

function Get-SynapseStoredHashOrEmpty {
    param(
        [AllowNull()]$Record,
        [Parameter(Mandatory=$true)][string]$Name
    )

    if ($null -eq $Record) {
        return ''
    }
    if ($Record -is [System.Collections.IDictionary] -and $Record.Contains($Name)) {
        if ($null -eq $Record[$Name]) {
            return ''
        }
        return [string]$Record[$Name]
    }
    $property = $Record.PSObject.Properties[$Name]
    if (-not $property -or $null -eq $property.Value) {
        return ''
    }
    return [string]$property.Value
}

function Get-SynapseRecordSchemaHash {
    param(
        [AllowNull()]$Record,
        [Parameter(Mandatory=$true)][string]$StoredHashName,
        [Parameter(Mandatory=$true)][string]$SchemaPropertyName
    )

    $stored = Get-SynapseStoredHashOrEmpty -Record $Record -Name $StoredHashName
    if (-not [string]::IsNullOrWhiteSpace($stored)) {
        return $stored
    }
    if ($null -eq $Record) {
        return '<null>'
    }
    if ($Record -is [System.Collections.IDictionary] -and $Record.Contains($SchemaPropertyName)) {
        return Get-SynapseNullableSchemaHash -Schema $Record[$SchemaPropertyName]
    }
    $property = $Record.PSObject.Properties[$SchemaPropertyName]
    $schema = if ($property) { $property.Value } else { $null }
    return Get-SynapseNullableSchemaHash -Schema $schema
}

function Format-SynapseLimitedList {
    param(
        [AllowNull()]$Items,
        [int]$Limit = 20
    )

    $values = @($Items | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) } | Sort-Object -Unique)
    if ($values.Count -eq 0) {
        return 'none'
    }
    $shown = @($values | Select-Object -First $Limit)
    $suffix = if ($values.Count -gt $Limit) { ",+$($values.Count - $Limit)_more" } else { '' }
    return (($shown -join ',') + $suffix)
}

function Get-SynapseToolSurfaceDiff {
    param(
        [AllowNull()]$StartSurface,
        [Parameter(Mandatory=$true)]$CurrentSurface
    )

    if ($null -eq $StartSurface) {
        return [pscustomobject]([ordered]@{
            Summary = 'start_snapshot=missing added=unknown removed=unknown callable_schema_changed=unknown'
            SchemaDetail = 'missing'
            HasRestartRequired = $true
            HasNameDelta = $true
            HasCallableSchemaChange = $true
        })
    }
    if ($StartSurface.unreadable) {
        return [pscustomobject]([ordered]@{
            Summary = "start_snapshot=unreadable error=$($StartSurface.error) added=unknown removed=unknown callable_schema_changed=unknown"
            SchemaDetail = 'unreadable'
            HasRestartRequired = $true
            HasNameDelta = $true
            HasCallableSchemaChange = $true
        })
    }

    $startNames = @($StartSurface.tool_names | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object -Unique)
    $currentNames = @($CurrentSurface.tool_names | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object -Unique)
    $added = @($currentNames | Where-Object { $startNames -notcontains $_ })
    $removed = @($startNames | Where-Object { $currentNames -notcontains $_ })

    $startMap = New-SynapseToolRecordMap -Surface $StartSurface
    $currentMap = New-SynapseToolRecordMap -Surface $CurrentSurface
    $inputChanged = @()
    $outputChanged = @()
    $descriptionChanged = @()
    $storedHashChanged = @()
    $storedSchemaHashOnlyChanged = @()
    if ($startMap.Count -gt 0 -and $currentMap.Count -gt 0) {
        foreach ($name in $currentNames) {
            if ($startMap.ContainsKey($name) -and $currentMap.ContainsKey($name)) {
                $startRecord = $startMap[$name]
                $currentRecord = $currentMap[$name]
                $startInputHash = Get-SynapseRecordSchemaHash -Record $startRecord -StoredHashName 'input_schema_sha256' -SchemaPropertyName 'input_schema'
                $currentInputHash = Get-SynapseRecordSchemaHash -Record $currentRecord -StoredHashName 'input_schema_sha256' -SchemaPropertyName 'input_schema'
                $startOutputHash = Get-SynapseRecordSchemaHash -Record $startRecord -StoredHashName 'output_schema_sha256' -SchemaPropertyName 'output_schema'
                $currentOutputHash = Get-SynapseRecordSchemaHash -Record $currentRecord -StoredHashName 'output_schema_sha256' -SchemaPropertyName 'output_schema'
                if ($startInputHash -ne $currentInputHash) {
                    $inputChanged += $name
                }
                if ($startOutputHash -ne $currentOutputHash) {
                    $outputChanged += $name
                }
                if ([string]$startRecord.description -ne [string]$currentRecord.description) {
                    $descriptionChanged += $name
                }
                $storedInputChanged = (Get-SynapseStoredHashOrEmpty -Record $startRecord -Name 'input_schema_sha256') -ne (Get-SynapseStoredHashOrEmpty -Record $currentRecord -Name 'input_schema_sha256')
                $storedOutputChanged = (Get-SynapseStoredHashOrEmpty -Record $startRecord -Name 'output_schema_sha256') -ne (Get-SynapseStoredHashOrEmpty -Record $currentRecord -Name 'output_schema_sha256')
                $storedToolChanged = (Get-SynapseStoredHashOrEmpty -Record $startRecord -Name 'tool_sha256') -ne (Get-SynapseStoredHashOrEmpty -Record $currentRecord -Name 'tool_sha256')
                if ($storedToolChanged) {
                    $storedHashChanged += $name
                }
                if (($storedInputChanged -or $storedOutputChanged) -and $startInputHash -eq $currentInputHash -and $startOutputHash -eq $currentOutputHash) {
                    $storedSchemaHashOnlyChanged += $name
                }
            }
        }
    }
    $schemaDetail = if ($startMap.Count -gt 0 -and $currentMap.Count -gt 0) { 'present' } else { 'missing' }
    $callableSchemaChanged = @($inputChanged + $outputChanged | Sort-Object -Unique)
    $hasNameDelta = ($added.Count -gt 0 -or $removed.Count -gt 0)
    $hasCallableSchemaChange = ($schemaDetail -ne 'present' -or $callableSchemaChanged.Count -gt 0)
    $summary = ("start_snapshot_schema_detail={0} added={1} removed={2} callable_schema_changed={3} input_schema_changed={4} output_schema_changed={5} description_changed={6} stored_tool_hash_changed={7} stored_schema_hash_only_changed={8}" -f `
        $schemaDetail,
        (Format-SynapseLimitedList -Items $added),
        (Format-SynapseLimitedList -Items $removed),
        (Format-SynapseLimitedList -Items $callableSchemaChanged),
        (Format-SynapseLimitedList -Items $inputChanged),
        (Format-SynapseLimitedList -Items $outputChanged),
        (Format-SynapseLimitedList -Items $descriptionChanged),
        (Format-SynapseLimitedList -Items $storedHashChanged),
        (Format-SynapseLimitedList -Items $storedSchemaHashOnlyChanged))
    return [pscustomobject]([ordered]@{
        Summary = $summary
        SchemaDetail = $schemaDetail
        Added = $added
        Removed = $removed
        InputSchemaChanged = $inputChanged
        OutputSchemaChanged = $outputChanged
        CallableSchemaChanged = $callableSchemaChanged
        DescriptionChanged = $descriptionChanged
        StoredToolHashChanged = $storedHashChanged
        StoredSchemaHashOnlyChanged = $storedSchemaHashOnlyChanged
        HasDescriptionChange = ($descriptionChanged.Count -gt 0)
        HasNameDelta = $hasNameDelta
        HasCallableSchemaChange = $hasCallableSchemaChange
        HasRestartRequired = ($hasNameDelta -or $hasCallableSchemaChange -or $descriptionChanged.Count -gt 0)
    })
}

function Get-SynapseToolSurfaceDiffSummary {
    param(
        [AllowNull()]$StartSurface,
        [Parameter(Mandatory=$true)]$CurrentSurface
    )

    $diff = Get-SynapseToolSurfaceDiff -StartSurface $StartSurface -CurrentSurface $CurrentSurface
    return $diff.Summary
}

function Write-SynapseUtf8NoBomFile {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$Text
    )

    $dir = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($Path, $Text, $encoding)
}

function Write-SynapseChromeBridgeCheckpoint {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][System.Collections.IDictionary]$Checkpoint
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw 'SYNAPSE_SETUP_BRIDGE_CHECKPOINT_PATH_EMPTY remediation=provide an absolute checkpoint path under the configured host local appdata directory'
    }
    $resolvedPath = [System.IO.Path]::GetFullPath($Path)
    $directory = Split-Path -Parent $resolvedPath
    if ([string]::IsNullOrWhiteSpace($directory)) {
        throw "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_DIRECTORY_EMPTY path=$resolvedPath remediation=provide a checkpoint path with a parent directory"
    }
    New-Item -ItemType Directory -Force -Path $directory | Out-Null

    $now = [DateTime]::UtcNow.ToString('o')
    $Checkpoint['schema'] = 'synapse_setup_bridge_pending/v3'
    $Checkpoint['phase'] = 'chrome_bridge_activation'
    if (-not $Checkpoint.Contains('checkpoint_generation_id') -or [string]::IsNullOrWhiteSpace([string]$Checkpoint['checkpoint_generation_id'])) {
        throw "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_GENERATION_MISSING path=$resolvedPath state=$($Checkpoint['state']) remediation=a bridge checkpoint must retain the exact setup generation that created the pending transaction"
    }
    if (-not $Checkpoint.Contains('created_at_utc') -or [string]::IsNullOrWhiteSpace([string]$Checkpoint['created_at_utc'])) {
        $Checkpoint['created_at_utc'] = $now
    }
    $Checkpoint['updated_at_utc'] = $now
    $Checkpoint['checkpoint_path'] = $resolvedPath

    $temporaryPath = "$resolvedPath.tmp.$PID.$([guid]::NewGuid().ToString('N'))"
    $replacementBackupPath = "$resolvedPath.replace-backup.$PID.$([guid]::NewGuid().ToString('N'))"
    $replacementCompleted = $false
    try {
        Write-SynapseUtf8NoBomFile `
            -Path $temporaryPath `
            -Text (($Checkpoint | ConvertTo-Json -Depth 40) + "`n")
        if (Test-Path -LiteralPath $resolvedPath -PathType Leaf) {
            [System.IO.File]::Replace($temporaryPath, $resolvedPath, $replacementBackupPath, $true)
            $replacementCompleted = $true
        } else {
            [System.IO.File]::Move($temporaryPath, $resolvedPath)
            $replacementCompleted = $true
        }
    } catch {
        throw "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_ATOMIC_WRITE_FAILED path=$resolvedPath temp=$temporaryPath recovery_backup=$replacementBackupPath replacement_completed=$replacementCompleted error=$($_.Exception.Message) remediation=repair checkpoint directory permissions and free space; preserve any named recovery backup for inspection because a resumable phase is never reported without an atomic durable write"
    } finally {
        if (Test-Path -LiteralPath $temporaryPath -PathType Leaf) {
            Remove-Item -LiteralPath $temporaryPath -Force -ErrorAction SilentlyContinue
        }
    }
    if ($replacementCompleted -and (Test-Path -LiteralPath $replacementBackupPath -PathType Leaf)) {
        try {
            Remove-Item -LiteralPath $replacementBackupPath -Force -ErrorAction Stop
        } catch {
            throw "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_BACKUP_CLEANUP_FAILED path=$resolvedPath recovery_backup=$replacementBackupPath error=$($_.Exception.Message) remediation=the new checkpoint is committed, but its exact replace backup could not be removed; repair permissions and remove only the named backup"
        }
        if (Test-Path -LiteralPath $replacementBackupPath) {
            throw "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_BACKUP_STILL_PRESENT path=$resolvedPath recovery_backup=$replacementBackupPath remediation=the new checkpoint is committed, but the exact replace backup still exists; repair storage state before accepting resume"
        }
    }

    try {
        $bytes = [System.IO.File]::ReadAllBytes($resolvedPath)
        $readback = [System.Text.Encoding]::UTF8.GetString($bytes) | ConvertFrom-Json
    } catch {
        throw "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_READBACK_FAILED path=$resolvedPath error=$($_.Exception.Message) remediation=inspect the checkpoint storage device; the atomically written JSON could not be read back"
    }
    if ([string]$readback.schema -ne 'synapse_setup_bridge_pending/v3' -or
        [string]$readback.phase -ne 'chrome_bridge_activation' -or
        [string]$readback.state -ne [string]$Checkpoint['state'] -or
        [string]$readback.checkpoint_generation_id -ne [string]$Checkpoint['checkpoint_generation_id']) {
        throw "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_READBACK_MISMATCH path=$resolvedPath expected_schema=synapse_setup_bridge_pending/v3 actual_schema=$($readback.schema) expected_phase=chrome_bridge_activation actual_phase=$($readback.phase) expected_state=$($Checkpoint['state']) actual_state=$($readback.state) expected_generation=$($Checkpoint['checkpoint_generation_id']) actual_generation=$($readback.checkpoint_generation_id) remediation=repair the checkpoint storage path before retrying"
    }
    return [pscustomobject]([ordered]@{
        Path = $resolvedPath
        Sha256 = Get-SynapseFileSha256 -Path $resolvedPath
        LenBytes = $bytes.Length
        State = [string]$readback.state
    })
}

function Read-SynapseChromeBridgeCheckpoint {
    param([Parameter(Mandatory=$true)][string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        Die 'SYNAPSE_SETUP_BRIDGE_CHECKPOINT_PATH_EMPTY remediation=-ResumeChromeBridgePending requires the durable checkpoint path'
    }
    $resolvedPath = [System.IO.Path]::GetFullPath($Path)
    if (-not (Test-Path -LiteralPath $resolvedPath -PathType Leaf)) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_MISSING path=$resolvedPath remediation=resume only a physically present bridge_pending checkpoint; run ordinary setup repair when no phase is pending"
    }
    try {
        $bytes = [System.IO.File]::ReadAllBytes($resolvedPath)
    } catch {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_READ_FAILED path=$resolvedPath error=$($_.Exception.Message) remediation=repair checkpoint file permissions before retrying resume"
    }
    if ($bytes.Length -eq 0) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_EMPTY path=$resolvedPath remediation=inspect the interrupted checkpoint write; resume refuses to infer state from an empty file"
    }
    try {
        $checkpoint = [System.Text.Encoding]::UTF8.GetString($bytes) | ConvertFrom-Json
    } catch {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_JSON_INVALID path=$resolvedPath error=$($_.Exception.Message) remediation=inspect the checkpoint bytes; resume refuses to infer state from malformed JSON"
    }

    if ([string]$checkpoint.schema -ne 'synapse_setup_bridge_pending/v3') {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_SCHEMA_INVALID path=$resolvedPath expected=synapse_setup_bridge_pending/v3 actual=$($checkpoint.schema) remediation=v2 and unknown checkpoints have no trustworthy deployment-generation binding; preserve them and run a new full setup"
    }
    if ([string]$checkpoint.phase -ne 'chrome_bridge_activation') {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_PHASE_INVALID path=$resolvedPath expected=chrome_bridge_activation actual=$($checkpoint.phase) remediation=resume refuses to infer or replay an unknown phase"
    }
    if ([string]$checkpoint.state -ne 'pending') {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_NOT_PENDING path=$resolvedPath actual=$($checkpoint.state) remediation=completed and superseded records are terminal evidence, not resumable work; run ordinary full setup if repair is still required"
    }
    foreach ($required in @(
        'checkpoint_generation_id',
        'daemon_handoff',
        'daemon_pid',
        'bind',
        'db_path',
        'installed_binary_path',
        'installed_binary_sha256',
        'daemon_process_executable_path',
        'daemon_process_command_line',
        'daemon_process_creation_date',
        'daemon_run_current_path',
        'daemon_run_current_sha256',
        'token_path',
        'token_sha256',
        'setup_script_path',
        'setup_script_sha256',
        'chrome_bridge_installer_path',
        'chrome_bridge_installer_sha256',
        'chrome_native_host_exe_path',
        'task_name',
        'task_definition_sha256',
        'task_action_execute',
        'task_action_arguments',
        'maintenance_lock_path'
    )) {
        $property = $checkpoint.PSObject.Properties[$required]
        if ($null -eq $property -or [string]::IsNullOrWhiteSpace([string]$property.Value)) {
            Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_FIELD_MISSING path=$resolvedPath field=$required remediation=resume requires a complete identity-bound checkpoint and never guesses missing state"
        }
    }
    if ([string]$checkpoint.daemon_handoff -ne 'committed') {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_HANDOFF_UNCOMMITTED path=$resolvedPath actual=$($checkpoint.daemon_handoff) remediation=only a physically committed daemon handoff can resume at Chrome activation"
    }
    $checkpoint | Add-Member -NotePropertyName checkpoint_file_path -NotePropertyValue $resolvedPath -Force
    $checkpoint | Add-Member -NotePropertyName checkpoint_file_sha256 -NotePropertyValue (Get-SynapseFileSha256 -Path $resolvedPath) -Force
    $checkpoint | Add-Member -NotePropertyName checkpoint_file_len_bytes -NotePropertyValue $bytes.Length -Force
    return $checkpoint
}

function Get-SynapseScheduledTaskIdentity {
    param([Parameter(Mandatory=$true)][string]$Name)

    $task = Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue
    if ($null -eq $task) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TASK_MISSING task=$Name remediation=restore the exact scheduled task that owns the committed daemon before resuming Chrome activation"
    }
    try {
        $xml = Export-ScheduledTask -TaskName $Name -ErrorAction Stop
    } catch {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TASK_EXPORT_FAILED task=$Name error=$($_.Exception.Message) remediation=repair Task Scheduler access before resuming Chrome activation"
    }
    $actions = @($task.Actions)
    if ($actions.Count -ne 1) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TASK_ACTION_COUNT_INVALID task=$Name expected=1 actual=$($actions.Count) remediation=repair the Synapse daemon task definition before resuming Chrome activation"
    }
    return [pscustomobject]([ordered]@{
        Name = $Name
        State = [string]$task.State
        DefinitionSha256 = Get-SynapseSha256Hex -Text ([string]$xml)
        ActionExecute = [string]$actions[0].Execute
        ActionArguments = [string]$actions[0].Arguments
        ActionWorkingDirectory = [string]$actions[0].WorkingDirectory
    })
}

function Get-SynapseDaemonProcessIdentity {
    param([Parameter(Mandatory=$true)][int]$ProcessId)

    $process = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction SilentlyContinue
    if ($null -eq $process) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_DAEMON_PROCESS_MISSING pid=$ProcessId remediation=the checkpointed daemon is no longer live; perform a new full setup repair instead of resuming stale Chrome activation"
    }
    if (-not (Test-SynapseMcpExecutableLeafName -Name ([string]$process.Name))) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_DAEMON_PROCESS_NAME_INVALID pid=$ProcessId name=$($process.Name) remediation=the checkpoint PID was reused by a non-Synapse process; perform a new full setup repair"
    }
    return [pscustomobject]([ordered]@{
        Pid = [int]$process.ProcessId
        ParentPid = [int]$process.ParentProcessId
        CreationDate = [string]$process.CreationDate
        Name = [string]$process.Name
        ExecutablePath = [string]$process.ExecutablePath
        CommandLine = [string]$process.CommandLine
    })
}

function Assert-SynapseChromeBridgeCheckpointFileIdentity {
    param(
        [Parameter(Mandatory=$true)][string]$Kind,
        [Parameter(Mandatory=$true)][string]$ExpectedPath,
        [Parameter(Mandatory=$true)][string]$ExpectedSha256
    )

    $resolvedExpectedPath = [System.IO.Path]::GetFullPath($ExpectedPath)
    if (-not (Test-Path -LiteralPath $resolvedExpectedPath -PathType Leaf)) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_${Kind}_MISSING path=$resolvedExpectedPath remediation=restore the exact checkpointed file or perform a new full setup repair"
    }
    $actualSha256 = Get-SynapseFileSha256 -Path $resolvedExpectedPath
    if ($actualSha256 -ine $ExpectedSha256) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_${Kind}_DRIFT path=$resolvedExpectedPath expected_sha256=$ExpectedSha256 actual_sha256=$actualSha256 remediation=the checkpointed phase belongs to different bytes; perform a new full setup repair"
    }
    return [pscustomobject]([ordered]@{
        Path = $resolvedExpectedPath
        Sha256 = $actualSha256
    })
}

function Invoke-SynapseChromeBridgePendingResume {
    param([Parameter(Mandatory=$true)][string]$CheckpointPath)

    if ($Remove -or $Purge -or $ForceRestart -or $SkipBuild -or $SkipClientWiring -or
        $ManualInstallHealthRollbackProbe -or $script:SynapsePostExitStartOnly) {
        Die "SYNAPSE_SETUP_BRIDGE_RESUME_MODE_CONFLICT remove=$Remove purge=$Purge force_restart=$ForceRestart skip_build=$SkipBuild skip_client_wiring=$SkipClientWiring rollback_probe=$ManualInstallHealthRollbackProbe post_exit=$script:SynapsePostExitStartOnly remediation=-ResumeChromeBridgePending is a complete phase-specific mode; remove all full-setup/remove/rollback switches"
    }

    Step 'Resuming checkpointed Chrome bridge activation'
    $checkpoint = Read-SynapseChromeBridgeCheckpoint -Path $CheckpointPath
    $script:SynapseChromeBridgePendingPath = [string]$checkpoint.checkpoint_file_path

    $checkpointLockPath = [System.IO.Path]::GetFullPath([string]$checkpoint.maintenance_lock_path)
    $actualLockPath = [System.IO.Path]::GetFullPath($MaintenanceLockPath)
    if ($checkpointLockPath -ine $actualLockPath) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_MAINTENANCE_LOCK_DRIFT expected=$checkpointLockPath actual=$actualLockPath remediation=resume must reacquire the exact maintenance lock recorded by the committed setup"
    }

    $scriptIdentity = Assert-SynapseChromeBridgeCheckpointFileIdentity `
        -Kind 'SETUP_SCRIPT' `
        -ExpectedPath ([string]$checkpoint.setup_script_path) `
        -ExpectedSha256 ([string]$checkpoint.setup_script_sha256)
    $runningScriptPath = [System.IO.Path]::GetFullPath($PSCommandPath)
    if ($runningScriptPath -ine $scriptIdentity.Path) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_SETUP_SCRIPT_PATH_DRIFT expected=$($scriptIdentity.Path) actual=$runningScriptPath remediation=invoke the exact setup script that wrote the checkpoint"
    }
    $installerIdentity = Assert-SynapseChromeBridgeCheckpointFileIdentity `
        -Kind 'CHROME_INSTALLER' `
        -ExpectedPath ([string]$checkpoint.chrome_bridge_installer_path) `
        -ExpectedSha256 ([string]$checkpoint.chrome_bridge_installer_sha256)
    $binaryIdentity = Assert-SynapseChromeBridgeCheckpointFileIdentity `
        -Kind 'INSTALLED_BINARY' `
        -ExpectedPath ([string]$checkpoint.installed_binary_path) `
        -ExpectedSha256 ([string]$checkpoint.installed_binary_sha256)
    $tokenIdentity = Assert-SynapseChromeBridgeCheckpointFileIdentity `
        -Kind 'TOKEN' `
        -ExpectedPath ([string]$checkpoint.token_path) `
        -ExpectedSha256 ([string]$checkpoint.token_sha256)
    $daemonRunIdentity = Assert-SynapseChromeBridgeCheckpointFileIdentity `
        -Kind 'DAEMON_RUN_LEDGER' `
        -ExpectedPath ([string]$checkpoint.daemon_run_current_path) `
        -ExpectedSha256 ([string]$checkpoint.daemon_run_current_sha256)

    $Bind = [string]$checkpoint.bind
    $DbPath = [string]$checkpoint.db_path
    $ExePath = $binaryIdentity.Path
    $TokenPath = $tokenIdentity.Path
    $TaskName = [string]$checkpoint.task_name
    $ChromeNativeHostExePath = [string]$checkpoint.chrome_native_host_exe_path

    $token = (Get-Content -Raw -LiteralPath $TokenPath).Trim()
    if ([string]::IsNullOrWhiteSpace($token)) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TOKEN_EMPTY path=$TokenPath remediation=restore the exact non-empty checkpointed bearer token or perform a new full setup repair"
    }
    $healthRead = Read-SynapseHealthForRestartGuard -Bind $Bind -Token $token -TimeoutSec 30
    if (-not $healthRead.Ok) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_HEALTH_UNREACHABLE bind=$Bind error=$($healthRead.Error) remediation=restore the exact checkpointed daemon before resuming Chrome activation"
    }
    $health = $healthRead.Health
    $expectedPid = [int]$checkpoint.daemon_pid
    $actualPid = [int]$health.pid
    if ($actualPid -ne $expectedPid) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_DAEMON_PID_DRIFT expected=$expectedPid actual=$actualPid bind=$Bind remediation=the checkpointed daemon identity is stale; perform a new full setup repair"
    }
    $daemonProcess = Get-SynapseDaemonProcessIdentity -ProcessId $actualPid
    if ([System.IO.Path]::GetFullPath($daemonProcess.ExecutablePath) -ine [System.IO.Path]::GetFullPath([string]$checkpoint.daemon_process_executable_path)) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_DAEMON_PATH_DRIFT pid=$actualPid expected=$($checkpoint.daemon_process_executable_path) actual=$($daemonProcess.ExecutablePath) remediation=the checkpointed daemon identity is stale; perform a new full setup repair"
    }
    if ($daemonProcess.CommandLine -cne [string]$checkpoint.daemon_process_command_line) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_DAEMON_ARGUMENT_DRIFT pid=$actualPid expected=[$($checkpoint.daemon_process_command_line)] actual=[$($daemonProcess.CommandLine)] remediation=the live daemon arguments changed after the checkpoint; perform a new full setup repair"
    }
    if ($daemonProcess.CreationDate -cne [string]$checkpoint.daemon_process_creation_date) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_DAEMON_CREATION_DRIFT pid=$actualPid expected=$($checkpoint.daemon_process_creation_date) actual=$($daemonProcess.CreationDate) remediation=the checkpoint PID belongs to a different process generation; perform a new full setup repair"
    }

    $taskIdentity = Get-SynapseScheduledTaskIdentity -Name $TaskName
    if ($taskIdentity.State -ne 'Running') {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TASK_NOT_RUNNING task=$TaskName actual_state=$($taskIdentity.State) remediation=restore the scheduled task instance that owns the checkpointed daemon before resuming Chrome activation"
    }
    if ($taskIdentity.DefinitionSha256 -ine [string]$checkpoint.task_definition_sha256 -or
        $taskIdentity.ActionExecute -cne [string]$checkpoint.task_action_execute -or
        $taskIdentity.ActionArguments -cne [string]$checkpoint.task_action_arguments) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TASK_DRIFT task=$TaskName expected_definition_sha256=$($checkpoint.task_definition_sha256) actual_definition_sha256=$($taskIdentity.DefinitionSha256) expected_execute=[$($checkpoint.task_action_execute)] actual_execute=[$($taskIdentity.ActionExecute)] expected_arguments=[$($checkpoint.task_action_arguments)] actual_arguments=[$($taskIdentity.ActionArguments)] remediation=the task definition changed after the checkpoint; perform a new full setup repair"
    }

    $health = Assert-SynapseChromeBridgeLiveAfterSetup `
        -Bind $Bind `
        -Token $token `
        -Health $health `
        -ChromeBridgeInstallerPath $installerIdentity.Path `
        -ChromeNativeHostExePath $ChromeNativeHostExePath
    $toolSurface = Read-SynapseDaemonToolSurface -Bind $Bind -Token $token -Health $health
    $bridge = $health.subsystems.chrome_bridge
    $completion = [ordered]@{
        completed_at_utc = [DateTime]::UtcNow.ToString('o')
        daemon_pid = $actualPid
        bind = $Bind
        installed_binary_path = $binaryIdentity.Path
        installed_binary_sha256 = $binaryIdentity.Sha256
        daemon_process_command_line = $daemonProcess.CommandLine
        daemon_run_current_path = $daemonRunIdentity.Path
        daemon_run_current_sha256 = $daemonRunIdentity.Sha256
        token_sha256 = $tokenIdentity.Sha256
        setup_script_sha256 = $scriptIdentity.Sha256
        chrome_bridge_installer_sha256 = $installerIdentity.Sha256
        task_definition_sha256 = $taskIdentity.DefinitionSha256
        chrome_bridge_status = [string]$bridge.status
        chrome_bridge_detail = [string]$bridge.detail
        tool_count = $toolSurface.tool_count
        tool_surface_sha256 = $toolSurface.tool_surface_sha256
    }
    $checkpointMap = [ordered]@{}
    foreach ($property in $checkpoint.PSObject.Properties) {
        if ($property.Name -notin @('checkpoint_file_path','checkpoint_file_sha256','checkpoint_file_len_bytes')) {
            $checkpointMap[$property.Name] = $property.Value
        }
    }
    $checkpointMap['state'] = 'completed'
    $checkpointMap['resume_attempt_count'] = ([int]$checkpoint.resume_attempt_count) + 1
    $checkpointMap['completion'] = $completion
    $checkpointFile = Write-SynapseChromeBridgeCheckpoint -Path $checkpoint.checkpoint_file_path -Checkpoint $checkpointMap
    $completion['checkpoint_path'] = $checkpointFile.Path
    $completion['checkpoint_sha256'] = $checkpointFile.Sha256
    $completion['checkpoint_len_bytes'] = $checkpointFile.LenBytes
    Write-SynapseSetupRepairManifestState `
        -State 'completed' `
        -Message 'checkpointed Chrome bridge activation completed without replaying daemon installation or task registration' `
        -ExitCode 0 `
        -Readback $completion
    Info "SYNAPSE_SETUP_BRIDGE_RESUME_COMPLETED checkpoint=$($checkpointFile.Path) checkpoint_sha256=$($checkpointFile.Sha256) daemon_pid=$actualPid tool_count=$($toolSurface.tool_count) tool_surface_sha256=$($toolSurface.tool_surface_sha256)"
}

function Complete-SynapseObsoleteChromeBridgeCheckpoint {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][int]$DaemonPid,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$InstalledBinaryPath,
        [Parameter(Mandatory=$true)][string]$InstalledBinarySha256,
        [Parameter(Mandatory=$true)]$ToolSurface,
        [Parameter(Mandatory=$true)]$ChromeBridge
    )

    $resolvedPath = [System.IO.Path]::GetFullPath($Path)
    if (-not (Test-Path -LiteralPath $resolvedPath -PathType Leaf)) {
        Info "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_ABSENT path=$resolvedPath terminalizing_generation=$($script:SynapseSetupInvocationId)"
        return [ordered]@{ state = 'absent'; path = $resolvedPath; terminalizing_generation_id = $script:SynapseSetupInvocationId }
    }
    try {
        $priorBytes = [System.IO.File]::ReadAllBytes($resolvedPath)
        $prior = [System.Text.Encoding]::UTF8.GetString($priorBytes) | ConvertFrom-Json
    } catch {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TERMINALIZE_READ_FAILED path=$resolvedPath error=$($_.Exception.Message) remediation=the current setup succeeded but refuses to conceal malformed historical transaction state; inspect and repair the named checkpoint"
    }
    $priorSha256 = Get-SynapseFileSha256 -Path $resolvedPath
    $priorSchema = [string]$prior.schema
    $priorState = [string]$prior.state
    if ($priorSchema -notin @('synapse_setup_bridge_pending/v2','synapse_setup_bridge_pending/v3')) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TERMINALIZE_SCHEMA_UNKNOWN path=$resolvedPath schema=$priorSchema sha256=$priorSha256 remediation=setup refuses to reinterpret or delete an unknown transaction schema"
    }
    if ($priorState -in @('completed','superseded')) {
        Info "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_ALREADY_TERMINAL path=$resolvedPath schema=$priorSchema state=$priorState sha256=$priorSha256"
        return [ordered]@{ state = $priorState; path = $resolvedPath; sha256 = $priorSha256; schema = $priorSchema }
    }
    if ($priorState -ne 'pending') {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TERMINALIZE_STATE_UNKNOWN path=$resolvedPath schema=$priorSchema state=$priorState sha256=$priorSha256 remediation=setup refuses to reinterpret or delete an unknown transaction state"
    }

    $checkpointMap = [ordered]@{}
    foreach ($property in $prior.PSObject.Properties) {
        $checkpointMap[$property.Name] = $property.Value
    }
    if ($priorSchema -eq 'synapse_setup_bridge_pending/v2' -or [string]::IsNullOrWhiteSpace([string]$checkpointMap['checkpoint_generation_id'])) {
        $checkpointMap['checkpoint_generation_id'] = "legacy-unbound-$($priorSha256.ToLowerInvariant())"
    }
    $daemonProcess = Get-SynapseDaemonProcessIdentity -ProcessId $DaemonPid
    $daemonRunPath = Join-Path $DbPath 'daemon-run-current.json'
    if (-not (Test-Path -LiteralPath $daemonRunPath -PathType Leaf)) {
        Die "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_TERMINALIZE_DAEMON_RUN_MISSING path=$daemonRunPath daemon_pid=$DaemonPid remediation=the newer deployment generation is not physically proven; repair its lifecycle ledger before terminalizing older work"
    }
    $checkpointMap['state'] = 'superseded'
    $checkpointMap['terminal_generation_id'] = $script:SynapseSetupInvocationId
    $checkpointMap['terminalized_at_utc'] = [DateTime]::UtcNow.ToString('o')
    $checkpointMap['supersession'] = [ordered]@{
        reason = 'newer_full_setup_completed_and_physically_verified'
        prior_schema = $priorSchema
        prior_state = $priorState
        prior_checkpoint_sha256 = $priorSha256
        daemon_pid = $DaemonPid
        daemon_process_creation_date = $daemonProcess.CreationDate
        daemon_process_executable_path = $daemonProcess.ExecutablePath
        daemon_process_command_line = $daemonProcess.CommandLine
        bind = $Bind
        daemon_run_current_path = $daemonRunPath
        daemon_run_current_sha256 = (Get-SynapseFileSha256 -Path $daemonRunPath)
        installed_binary_path = $InstalledBinaryPath
        installed_binary_sha256 = $InstalledBinarySha256
        tool_count = $ToolSurface.tool_count
        tool_surface_sha256 = $ToolSurface.tool_surface_sha256
        chrome_bridge_status = [string]$ChromeBridge.status
        chrome_bridge_detail = [string]$ChromeBridge.detail
    }
    $terminal = Write-SynapseChromeBridgeCheckpoint -Path $resolvedPath -Checkpoint $checkpointMap
    Info "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_SUPERSEDED path=$($terminal.Path) sha256=$($terminal.Sha256) prior_sha256=$priorSha256 prior_schema=$priorSchema checkpoint_generation=$($checkpointMap['checkpoint_generation_id']) terminal_generation=$($script:SynapseSetupInvocationId) daemon_pid=$DaemonPid"
    return [ordered]@{
        state = 'superseded'
        path = $terminal.Path
        sha256 = $terminal.Sha256
        len_bytes = $terminal.LenBytes
        prior_sha256 = $priorSha256
        checkpoint_generation_id = [string]$checkpointMap['checkpoint_generation_id']
        terminal_generation_id = $script:SynapseSetupInvocationId
    }
}

function Get-SynapseHandoffGitReadback {
    param([AllowNull()][string]$SourceDir)

    if ([string]::IsNullOrWhiteSpace($SourceDir) -or -not (Test-Path -LiteralPath $SourceDir)) {
        return [ordered]@{
            available = $false
            reason = 'source_dir_missing_or_not_supplied'
            source_dir = $SourceDir
        }
    }
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        return [ordered]@{
            available = $false
            reason = 'git_not_found'
            source_dir = $SourceDir
        }
    }

    try {
        $status = @(& git -C $SourceDir status --short --branch 2>&1)
        $head = @(& git -C $SourceDir rev-parse HEAD 2>&1)
        $origin = @(& git -C $SourceDir rev-parse origin/main 2>&1)
        $branch = @(& git -C $SourceDir branch --show-current 2>&1)
        return [ordered]@{
            available = $true
            source_dir = $SourceDir
            branch = (($branch | Select-Object -First 1) -join '').Trim()
            head = (($head | Select-Object -First 1) -join '').Trim()
            origin_main = (($origin | Select-Object -First 1) -join '').Trim()
            status_short_branch = @($status)
        }
    } catch {
        return [ordered]@{
            available = $false
            reason = 'git_readback_failed'
            source_dir = $SourceDir
            error = $_.Exception.Message
        }
    }
}

function Get-SynapseRecoveryNotesPath {
    param([AllowNull()][string]$SourceDir)

    $candidates = @()
    if (-not [string]::IsNullOrWhiteSpace($SourceDir)) {
        $candidates += (Join-Path $SourceDir 'STATE\RECOVERY_NOTES.md')
    }
    $repoRootFromScript = Split-Path -Parent $PSScriptRoot
    if (-not [string]::IsNullOrWhiteSpace($repoRootFromScript)) {
        $candidates += (Join-Path $repoRootFromScript 'STATE\RECOVERY_NOTES.md')
    }

    foreach ($candidate in @($candidates | Select-Object -Unique)) {
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }
    $first = @($candidates | Select-Object -First 1)
    if ($first.Count -gt 0) {
        return [string]$first[0]
    }
    return $null
}

function Get-SynapseNormalizedIssueRef {
    param([AllowNull()][string]$Issue)

    if ([string]::IsNullOrWhiteSpace($Issue)) {
        return $null
    }

    $trimmed = $Issue.Trim()
    if ($trimmed -match '^#?(?<number>[0-9]+)$') {
        return "#$($Matches['number'])"
    }
    if ($trimmed -match '^https://github\.com/ChrisRoyse/Synapse/issues/(?<number>[0-9]+)(?:[/?#].*)?$') {
        return "#$($Matches['number'])"
    }

    Die "SYNAPSE_ACTIVE_ISSUE_INVALID value=$trimmed remediation=pass an issue number like 1441, #1441, or https://github.com/ChrisRoyse/Synapse/issues/1441"
}

function Get-SynapseIssueNumberFromRef {
    param([AllowNull()][string]$IssueRef)

    if ([string]::IsNullOrWhiteSpace($IssueRef)) {
        return $null
    }
    if ($IssueRef -match '^#(?<number>[0-9]+)$') {
        return $Matches['number']
    }
    return $null
}

function ConvertTo-SynapseHandoffDiffObject {
    param([AllowNull()]$Diff)

    if ($null -eq $Diff) {
        return [ordered]@{
            available = $false
            reason = 'diff_not_supplied'
        }
    }

    return [ordered]@{
        available = $true
        summary = [string]$Diff.Summary
        schema_detail = [string]$Diff.SchemaDetail
        has_restart_required = [bool]$Diff.HasRestartRequired
        has_name_delta = [bool]$Diff.HasNameDelta
        has_callable_schema_change = [bool]$Diff.HasCallableSchemaChange
        has_description_change = [bool]$Diff.HasDescriptionChange
        added = @($Diff.Added | ForEach-Object { [string]$_ })
        removed = @($Diff.Removed | ForEach-Object { [string]$_ })
        input_schema_changed = @($Diff.InputSchemaChanged | ForEach-Object { [string]$_ })
        output_schema_changed = @($Diff.OutputSchemaChanged | ForEach-Object { [string]$_ })
        callable_schema_changed = @($Diff.CallableSchemaChanged | ForEach-Object { [string]$_ })
        description_changed = @($Diff.DescriptionChanged | ForEach-Object { [string]$_ })
        stored_tool_hash_changed = @($Diff.StoredToolHashChanged | ForEach-Object { [string]$_ })
        stored_schema_hash_only_changed = @($Diff.StoredSchemaHashOnlyChanged | ForEach-Object { [string]$_ })
    }
}

function ConvertTo-SynapseTcpClientEvidenceObject {
    param([object[]]$TcpClients)

    return @($TcpClients | ForEach-Object {
        [ordered]@{
            state = [string]$_.State
            local_address = [string]$_.LocalAddress
            local_port = [int]$_.LocalPort
            remote_address = [string]$_.RemoteAddress
            remote_port = [int]$_.RemotePort
            owning_process = [int]$_.OwningProcess
            owner_name = [string]$_.OwnerName
            owner_command_line = [string]$_.OwnerCommandLine
            peer_owning_process = [int]$_.PeerOwningProcess
            peer_owner_exists = [bool]$_.PeerOwnerExists
            peer_owner_name = [string]$_.PeerOwnerName
            peer_owner_command_line = [string]$_.PeerOwnerCommandLine
            has_live_peer = [bool]$_.HasLivePeer
        }
    })
}

function ConvertTo-SynapseListenerEvidenceObject {
    param([object[]]$Listeners)

    return @($Listeners | ForEach-Object {
        [ordered]@{
            state = [string]$_.State
            local_address = [string]$_.LocalAddress
            local_port = [int]$_.LocalPort
            owning_process = [int]$_.OwningProcess
            owner_exists = [bool]$_.OwnerExists
            owner_name = [string]$_.OwnerName
            owner_command_line = [string]$_.OwnerCommandLine
            creation_time = [string]$_.CreationTime
        }
    })
}

function Get-SynapseCodexPeerRows {
    param([object[]]$TcpClients)

    return @($TcpClients | Where-Object {
        if (-not $_.HasLivePeer -or [int]$_.PeerOwningProcess -le 0) {
            $false
        } else {
            $peer = Get-CimInstance Win32_Process -Filter "ProcessId=$([int]$_.PeerOwningProcess)" -ErrorAction SilentlyContinue
            Test-SynapseCodexProcess -Process $peer
        }
    })
}

function Write-SynapseCodexSocketRestartHandoff {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][object[]]$CodexTcpClients,
        [Parameter(Mandatory=$true)][object[]]$TcpClients,
        [Parameter(Mandatory=$true)][object[]]$StaleListeners,
        [Parameter(Mandatory=$true)][AllowNull()][string]$BindProbeError,
        [AllowNull()][string]$SourceDir,
        [AllowNull()][string]$TokenPath,
        [AllowNull()][string]$ActiveIssue
    )

    $root = Join-Path $env:LOCALAPPDATA 'synapse\codex-restart-handoffs'
    $stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
    $firstCodexPid = @($CodexTcpClients | ForEach-Object { [int]$_.PeerOwningProcess } | Sort-Object -Unique | Select-Object -First 1)
    $codexPidForName = if ($firstCodexPid.Count -gt 0) { [int]$firstCodexPid[0] } else { 0 }
    $baseName = "codex-socket-handoff-$codexPidForName-$stamp"
    $jsonPath = Join-Path $root "$baseName.json"
    $mdPath = Join-Path $root "$baseName.md"
    $recoveryNotesPath = Get-SynapseRecoveryNotesPath -SourceDir $SourceDir
    $activeIssueRef = Get-SynapseNormalizedIssueRef -Issue $ActiveIssue
    $activeIssueNumber = Get-SynapseIssueNumberFromRef -IssueRef $activeIssueRef
    $activeIssueRead = if ([string]::IsNullOrWhiteSpace($activeIssueNumber)) {
        $null
    } else {
        "gh issue view $activeIssueNumber --repo ChrisRoyse/Synapse --comments"
    }
    $codexPeers = @($CodexTcpClients | ForEach-Object {
        $peerPid = [int]$_.PeerOwningProcess
        $peer = Get-CimInstance Win32_Process -Filter "ProcessId=$peerPid" -ErrorAction SilentlyContinue
        [ordered]@{
            pid = $peerPid
            name = if ($peer) { [string]$peer.Name } else { [string]$_.PeerOwnerName }
            command_line = if ($peer) { [string]$peer.CommandLine } else { [string]$_.PeerOwnerCommandLine }
            tcp_local = "$($_.LocalAddress):$($_.LocalPort)"
            tcp_remote = "$($_.RemoteAddress):$($_.RemotePort)"
        }
    })
    $codexPeerPids = @($codexPeers | ForEach-Object { [int]$_.pid } | Sort-Object -Unique)
    $postRestartRequiredReads = @(
        'C:\Users\hotra\Downloads\AICodingAgentSuperPrompt.md',
        'C:\code\Synapse\docs\compressionprompt.md',
        'C:\code\Synapse\AGENTS.md',
        $recoveryNotesPath
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    $githubReads = @(
        'gh issue view 351 --repo ChrisRoyse/Synapse --comments',
        $activeIssueRead,
        'gh issue view 1405 --repo ChrisRoyse/Synapse --comments',
        'gh issue list --repo ChrisRoyse/Synapse --state open --limit 100'
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    $restartCommandHint = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) {
        "Close the exact Codex peer process(es) named in this handoff by ending their owning Codex session(s), start a new Codex session through the patched launcher, verify those stale peer PID(s) are gone, then resume from GitHub issue state."
    } else {
        "Close the exact Codex peer process(es) named in this handoff by ending their owning Codex session(s), start a new Codex session through the patched launcher, verify those stale peer PID(s) are gone, then resume $activeIssueRef."
    }

    $record = [ordered]@{
        schema_version = 1
        artifact_kind = 'synapse_codex_socket_restart_handoff'
        created_at_utc = [DateTime]::UtcNow.ToString('o')
        reason_code = 'SYNAPSE_CODEX_CURRENT_PROCESS_SOCKET_STALE'
        reason = $Reason
        phase = 'dead_owner_bind_drain'
        required_restart = $true
        no_in_process_socket_release = $true
        explanation = 'The old synapse-mcp daemon exited, but Windows still reports dead-owner listener/socket rows because a live Codex MCP client peer is attached to the stopped daemon socket. Setup must not kill Codex, terminal, IDE, or WSL processes globally; restart the exact Codex session named here so Windows releases the stale socket, then rerun setup repair.'
        bind = $Bind
        bind_probe = [ordered]@{
            ok = $false
            error = $BindProbeError
        }
        codex_peer_pids = $codexPeerPids
        codex_peers = $codexPeers
        stale_listeners = ConvertTo-SynapseListenerEvidenceObject -Listeners $StaleListeners
        tcp_clients = ConvertTo-SynapseTcpClientEvidenceObject -TcpClients $TcpClients
        active_issue = [ordered]@{
            issue_ref = $activeIssueRef
            issue_number = $activeIssueNumber
            source = 'ActiveIssue parameter or SYNAPSE_ACTIVE_ISSUE environment variable'
            status = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown' } else { 'provided' }
        }
        github_reads = $githubReads
        post_restart_required_reads = $postRestartRequiredReads
        post_restart_verification = @(
            'Run git status --short --branch and confirm the working tree matches this handoff/recovery note.',
            "Read the OS process table and confirm stale Codex peer PID(s) $($codexPeerPids -join ',') no longer exist.",
            'Read Get-NetTCPConnection for 127.0.0.1:7700 and confirm no rows point at the stopped daemon PID from this handoff.',
            'Run deferred Synapse tool discovery, then call real mcp__synapse.health and setup status from the fresh Codex session.',
            'Rerun setup.repair through the real MCP setup facade; direct HTTP/stdio helper calls are diagnostics only.'
        )
        restart_command_hint = $restartCommandHint
        repo_readback = Get-SynapseHandoffGitReadback -SourceDir $SourceDir
        token_path = $TokenPath
        recovery_notes_path = $recoveryNotesPath
    }

    try {
        New-Item -ItemType Directory -Force -Path $root | Out-Null
        Write-SynapseUtf8NoBomFile -Path $jsonPath -Text (($record | ConvertTo-Json -Depth 40) + "`n")
        $md = @(
            '# Synapse Codex Socket Restart Handoff',
            '',
            "- Reason: SYNAPSE_CODEX_CURRENT_PROCESS_SOCKET_STALE ($Reason)",
            '- Phase: dead_owner_bind_drain',
            "- Created UTC: $($record.created_at_utc)",
            "- Bind: $Bind",
            "- Codex peer PID(s): $($codexPeerPids -join ',')",
            "- Active issue: $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown; recover from caller/session context or open issue queue' } else { $activeIssueRef })",
            '',
            '## Required Restart',
            'The running Codex peer still has a TCP connection to the stopped daemon. Setup cannot safely kill Codex or terminal/IDE/WSL hosts. End the exact Codex session owning the listed PID(s), start a new Codex session through the patched launcher, and prove those PID(s) disappeared before retrying setup repair.',
            '',
            '## Socket Evidence',
            '```text',
            "stale_listeners:",
            (Format-SynapseTcpBindListenerSnapshot -Snapshot $StaleListeners),
            '',
            "tcp_clients:",
            (Format-SynapseTcpClientSnapshot -Snapshot $TcpClients),
            '',
            "bind_probe_error=$BindProbeError",
            '```',
            '',
            '## Read After Restart'
        )
        foreach ($item in $postRestartRequiredReads) {
            $md += "- $item"
        }
        $md += @(
            '',
            '## GitHub Reads'
        )
        foreach ($item in $record.github_reads) {
            $md += "- $item"
        }
        $md += @(
            '',
            '## Verification'
        )
        foreach ($item in $record.post_restart_verification) {
            $md += "- $item"
        }
        $md += @(
            '',
            "JSON artifact: $jsonPath",
            ''
        )
        Write-SynapseUtf8NoBomFile -Path $mdPath -Text (($md -join "`n") + "`n")

        if (-not [string]::IsNullOrWhiteSpace($recoveryNotesPath)) {
            $notes = @(
                '# Synapse Recovery Notes',
                '',
                '## Latest Codex Socket Restart Handoff',
                '',
                "- Reason: SYNAPSE_CODEX_CURRENT_PROCESS_SOCKET_STALE ($Reason)",
                '- Phase: dead_owner_bind_drain',
                "- Created UTC: $($record.created_at_utc)",
                "- JSON: $jsonPath",
                "- Markdown: $mdPath",
                "- Stale Codex peer PID(s): $($codexPeerPids -join ',')",
                "- Daemon bind: $Bind",
                "- Active issue: $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown; recover from caller/session context or open issue queue' } else { $activeIssueRef })",
                '',
                "After restart, re-read AGENTS.md, #351, #1405, $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'the active issue from the caller/session context' } else { $activeIssueRef }), git status, and this file before resuming. Prove the stale Codex peer PID(s) are gone and rerun real mcp__synapse setup repair; direct helper calls are diagnostics only.",
                ''
            )
            Write-SynapseUtf8NoBomFile -Path $recoveryNotesPath -Text (($notes -join "`n") + "`n")
        }
    } catch {
        Die "SYNAPSE_CODEX_SOCKET_RESTART_HANDOFF_WRITE_FAILED reason=$Reason bind=$Bind path=$jsonPath error=$($_.Exception.Message) remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-restart-handoffs and the repo STATE directory, then rerun setup"
    }

    Info "SYNAPSE_CODEX_CURRENT_PROCESS_SOCKET_STALE handoff_written reason=$Reason json=$jsonPath markdown=$mdPath recovery_notes=$recoveryNotesPath codex_peer_pids=$($codexPeerPids -join ',')"
    return [pscustomobject]@{
        JsonPath = $jsonPath
        MarkdownPath = $mdPath
        RecoveryNotesPath = $recoveryNotesPath
        CodexPeerPids = $codexPeerPids
    }
}

function Start-SynapsePostExitSetupContinuation {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$SourceDir,
        [Parameter(Mandatory=$true)][string]$ExePath,
        [Parameter(Mandatory=$true)][string]$ChromeNativeHostExePath,
        [Parameter(Mandatory=$true)][AllowEmptyString()][string]$CargoTarget,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [Parameter(Mandatory=$true)][string]$CodexToolSurfaceSnapshotPath,
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$MaintenanceLockPath,
        [AllowNull()][string]$CalyxConfigPath,
        [string]$ActiveIssue,
        [object]$DeadOwnerDetail
    )

    $root = Join-Path $env:LOCALAPPDATA 'synapse\setup-continuations'
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssfffZ')
    $runId = "post-exit-$PID-$stamp"
    $runDir = Join-Path $root $runId
    New-Item -ItemType Directory -Force -Path $runDir | Out-Null
    $manifestPath = Join-Path $runDir 'continuation.json'
    $stdoutPath = Join-Path $runDir 'stdout.log'
    $stderrPath = Join-Path $runDir 'stderr.log'
    $wrapperPath = Join-Path $runDir 'launch-continuation.ps1'
    $launcherPath = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $launcherPath)) {
        Die "SYNAPSE_POST_EXIT_CONTINUATION_POWERSHELL_MISSING path=$launcherPath remediation=repair Windows PowerShell before retrying setup"
    }

    $args = @(
        '-NoProfile',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        $PSCommandPath,
        '-SourceDir',
        $SourceDir,
        '-Bind',
        $Bind,
        '-ExePath',
        $ExePath,
        '-ChromeNativeHostExePath',
        $ChromeNativeHostExePath,
        '-DbPath',
        $DbPath,
        '-ProfilesDir',
        $ProfilesDir,
        '-LogDir',
        $LogDir,
        '-TokenPath',
        $TokenPath,
        '-CodexToolSurfaceSnapshotPath',
        $CodexToolSurfaceSnapshotPath,
        '-TaskName',
        $TaskName,
        '-MaintenanceLockPath',
        $MaintenanceLockPath,
        '-BuildTimeoutMinutes',
        ([string]$BuildTimeoutMinutes),
        '-PostExitParentPid',
        ([string]$PID),
        '-PostExitContinuationReason',
        'dead_owner_bind_after_install',
        '-PostExitManifestPath',
        $manifestPath,
        '-ForceRestart',
        '-SkipBuild'
    )
    if (-not [string]::IsNullOrWhiteSpace($ActiveIssue)) {
        $args += @('-ActiveIssue', $ActiveIssue)
    }
    if (-not [string]::IsNullOrWhiteSpace($CalyxConfigPath)) {
        $args += @('-CalyxConfigPath', $CalyxConfigPath)
    }
    # Relay the resolved build tree only when one was actually resolved. The
    # continuation always runs -SkipBuild, so this is provenance for the child's
    # readbacks, not a build instruction; relaying an empty value would bind a
    # blank -CargoTarget and relaying a resolved alternate must carry its
    # authorization with it (#1857).
    if (-not [string]::IsNullOrWhiteSpace($CargoTarget)) {
        $args += @('-CargoTarget', $CargoTarget)
        if ($AllowAlternateBuildTarget) {
            $args += '-AllowAlternateBuildTarget'
        }
    }
    if ($SkipClientWiring) {
        $args += '-SkipClientWiring'
    }
    $continuationTaskName = "SynapsePostExitSetup-$runId"
    $taskArgument = "-NoProfile -ExecutionPolicy Bypass -File $(Quote-WindowsCommandArgument -Value $wrapperPath)"
    $argLiteralLines = @($args | ForEach-Object { "    $(Quote-PowerShellSingleQuotedString -Value $_)" })
    $wrapperLines = @(
        '$ErrorActionPreference = ''Stop''',
        "`$taskName = $(Quote-PowerShellSingleQuotedString -Value $continuationTaskName)",
        "`$launcherPath = $(Quote-PowerShellSingleQuotedString -Value $launcherPath)",
        "`$sourceDir = $(Quote-PowerShellSingleQuotedString -Value $SourceDir)",
        "`$stdoutPath = $(Quote-PowerShellSingleQuotedString -Value $stdoutPath)",
        "`$stderrPath = $(Quote-PowerShellSingleQuotedString -Value $stderrPath)",
        '$argList = @('
    )
    $wrapperLines += $argLiteralLines
    $wrapperLines += @(
        ')',
        '$exitCode = 1',
        'try {',
        '    Set-Location -LiteralPath $sourceDir',
        '    & $launcherPath @argList 1> $stdoutPath 2> $stderrPath 3>&1 4>&1 5>&1 6>&1',
        '    if ($null -ne $global:LASTEXITCODE) {',
        '        $exitCode = [int]$global:LASTEXITCODE',
        '    } else {',
        '        $exitCode = 0',
        '    }',
        '} catch {',
        '    try { ($_ | Out-String) | Add-Content -LiteralPath $stderrPath -Encoding UTF8 } catch {}',
        '    $exitCode = 1',
        '} finally {',
        '    try { Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue } catch {}',
        '}',
        'exit $exitCode'
    )
    Write-SynapseUtf8NoBomFile -Path $wrapperPath -Text (($wrapperLines -join "`n") + "`n")

    $manifest = [ordered]@{
        schema = 'synapse_setup_post_exit_continuation/v1'
        state = 'launching'
        run_id = $runId
        reason = $Reason
        bind = $Bind
        parent_pid = $PID
        launch_mode = 'scheduled_task'
        task_name = $continuationTaskName
        task_argument = $taskArgument
        launcher_path = $launcherPath
        wrapper_path = $wrapperPath
        setup_script_path = $PSCommandPath
        source_dir = $SourceDir
        stdout_log = $stdoutPath
        stderr_log = $stderrPath
        command_args = $args
        active_issue = $ActiveIssue
        dead_owner_detail = $DeadOwnerDetail
        created_at_utc = (Get-Date).ToUniversalTime().ToString('o')
        remediation = 'continuation waits for parent setup process exit, reacquires setup maintenance lock, then reruns setup with -SkipBuild against the installed verified daemon bytes'
    }
    Write-SynapseUtf8NoBomFile -Path $manifestPath -Text (($manifest | ConvertTo-Json -Depth 24) + "`n")

    try {
        if (Get-ScheduledTask -TaskName $continuationTaskName -ErrorAction SilentlyContinue) {
            Unregister-ScheduledTask -TaskName $continuationTaskName -Confirm:$false -ErrorAction SilentlyContinue
        }
        $action = New-ScheduledTaskAction -Execute $launcherPath -Argument $taskArgument -WorkingDirectory $SourceDir
        $trigger = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(5)
        $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Hours 2)
        $settings.Hidden = $true
        Register-ScheduledTask -TaskName $continuationTaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description "Synapse post-exit setup continuation $runId" | Out-Null
        Start-ScheduledTask -TaskName $continuationTaskName
        Start-Sleep -Seconds 1
        $child = Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe'" -ErrorAction SilentlyContinue |
            Where-Object { $_.CommandLine -and ($_.CommandLine -like "*$wrapperPath*" -or $_.CommandLine -like "*$runId*") } |
            Sort-Object CreationDate -Descending |
            Select-Object -First 1
    } catch {
        Die "SYNAPSE_POST_EXIT_CONTINUATION_START_FAILED run_id=$runId manifest=$manifestPath error=$($_.Exception.Message) remediation=repair process creation permissions and rerun setup; no daemon was started while the bind probe failed"
    }

    $manifest.state = 'started'
    $manifest.child_pid = if ($child) { [int]$child.ProcessId } else { 0 }
    $manifest.task_state = try { (Get-ScheduledTask -TaskName $continuationTaskName -ErrorAction Stop).State.ToString() } catch { "unknown:$($_.Exception.Message)" }
    $manifest.started_at_utc = (Get-Date).ToUniversalTime().ToString('o')
    Write-SynapseUtf8NoBomFile -Path $manifestPath -Text (($manifest | ConvertTo-Json -Depth 24) + "`n")

    Info "SYNAPSE_POST_EXIT_CONTINUATION_STARTED run_id=$runId launch_mode=scheduled_task task_name=$continuationTaskName parent_pid=$PID child_pid=$($manifest.child_pid) manifest=$manifestPath wrapper=$wrapperPath stdout=$stdoutPath stderr=$stderrPath"
    return [pscustomobject]@{
        RunId = $runId
        RunDir = $runDir
        ManifestPath = $manifestPath
        StdoutPath = $stdoutPath
        StderrPath = $stderrPath
        ChildPid = $manifest.child_pid
        LaunchMode = 'scheduled_task'
        TaskName = $continuationTaskName
        WrapperPath = $wrapperPath
    }
}

function Write-SynapseCodexRestartHandoff {
    param(
        [Parameter(Mandatory=$true)][string]$Phase,
        [Parameter(Mandatory=$true)][string]$Reason,
        [AllowNull()]$CodexAncestor,
        [AllowNull()]$Surface,
        [AllowNull()]$Diff,
        [AllowNull()][string]$ProcessHashAtStart,
        [AllowNull()][string]$ProcessSnapshotAtStart,
        [AllowNull()][string]$CurrentSnapshotPath,
        [AllowNull()][string]$SourceDir,
        [AllowNull()][string]$Bind,
        [AllowNull()][string]$TokenPath,
        [AllowNull()][string]$ActiveIssue
    )

    $root = Join-Path $env:LOCALAPPDATA 'synapse\codex-restart-handoffs'
    $stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
    $codexPid = if ($CodexAncestor) { [int]$CodexAncestor.ProcessId } else { 0 }
    $baseName = "codex-restart-handoff-$codexPid-$stamp"
    $jsonPath = Join-Path $root "$baseName.json"
    $mdPath = Join-Path $root "$baseName.md"
    $recoveryNotesPath = Get-SynapseRecoveryNotesPath -SourceDir $SourceDir
    $startSnapshotStatus = if ([string]::IsNullOrWhiteSpace($ProcessSnapshotAtStart)) {
        'missing_env'
    } elseif (Test-Path -LiteralPath $ProcessSnapshotAtStart) {
        'readable'
    } else {
        'missing_file'
    }

    $daemon = [ordered]@{
        bind = $Bind
        pid = if ($Surface -and $Surface.PSObject.Properties['daemon_pid']) { $Surface.daemon_pid } else { $null }
        pid_role = if ($Phase -eq 'pre_handoff_candidate') { 'preflight_candidate' } else { 'installed_configured_daemon' }
        pid_authoritative_for_configured_bind = ($Phase -ne 'pre_handoff_candidate')
        pid_expectation = if ($Phase -eq 'pre_handoff_candidate') {
            'This PID belongs to the isolated candidate daemon used before live handoff and is expected to be stopped before the configured daemon is installed.'
        } else {
            'This PID is the installed daemon observed after live handoff and should own the configured bind unless a later setup run superseded it.'
        }
        tool_count = if ($Surface -and $Surface.PSObject.Properties['tool_count']) { $Surface.tool_count } else { $null }
        tool_surface_sha256 = if ($Surface -and $Surface.PSObject.Properties['tool_surface_sha256']) { [string]$Surface.tool_surface_sha256 } else { $null }
        snapshot_path = $CurrentSnapshotPath
    }
    $codexProcess = if ($CodexAncestor) {
        [ordered]@{
            pid = [int]$CodexAncestor.ProcessId
            name = [string]$CodexAncestor.Name
            command_line = [string]$CodexAncestor.CommandLine
        }
    } else {
        [ordered]@{
            pid = $null
            name = $null
            command_line = $null
        }
    }
    $diffObject = ConvertTo-SynapseHandoffDiffObject -Diff $Diff
    $gitReadback = Get-SynapseHandoffGitReadback -SourceDir $SourceDir
    $activeIssueRef = Get-SynapseNormalizedIssueRef -Issue $ActiveIssue
    $activeIssueNumber = Get-SynapseIssueNumberFromRef -IssueRef $activeIssueRef
    $activeIssueRead = if ([string]::IsNullOrWhiteSpace($activeIssueNumber)) {
        $null
    } else {
        "gh issue view $activeIssueNumber --repo ChrisRoyse/Synapse --comments"
    }
    $resumeInstruction = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) {
        'Resume the active GitHub issue from the caller/session context; if unknown, read the open issue queue and choose the issue that produced this setup handoff. Perform manual real-MCP FSV; do not use direct helper calls as acceptance.'
    } else {
        "Resume $activeIssueRef and perform manual real-MCP FSV; do not use direct helper calls as acceptance."
    }
    $restartCommandHint = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) {
        "Close this Codex session completely, start a new Codex session through the patched Codex launcher, verify the active codex.exe PID is not $codexPid, then resume the active issue from the caller/session context."
    } else {
        "Close this Codex session completely, start a new Codex session through the patched Codex launcher, verify the active codex.exe PID is not $codexPid, then resume $activeIssueRef."
    }
    $postRestartRequiredReads = @(
        'C:\Users\hotra\Downloads\AICodingAgentSuperPrompt.md',
        'C:\code\Synapse\docs\compressionprompt.md',
        'C:\code\Synapse\AGENTS.md',
        $recoveryNotesPath
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    $githubReads = @(
        'gh issue view 351 --repo ChrisRoyse/Synapse --comments',
        $activeIssueRead,
        'gh issue list --repo ChrisRoyse/Synapse --state open --limit 100'
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }

    $record = [ordered]@{
        schema_version = 2
        artifact_kind = 'synapse_codex_restart_handoff'
        created_at_utc = [DateTime]::UtcNow.ToString('o')
        reason_code = 'SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE'
        reason = $Reason
        phase = $Phase
        required_restart = $true
        no_in_process_hot_refresh = $true
        explanation = 'The already-running Codex process has process-local MCP callable metadata that does not match the current daemon tools/list surface. Restart through the patched Codex launcher is the same-agent recovery boundary.'
        codex_process = $codexProcess
        daemon = $daemon
        current_process_start_surface = [ordered]@{
            env_hash_present = (-not [string]::IsNullOrWhiteSpace($ProcessHashAtStart))
            env_hash = $ProcessHashAtStart
            env_snapshot_path = $ProcessSnapshotAtStart
            snapshot_status = $startSnapshotStatus
        }
        diff = $diffObject
        post_restart_required_reads = $postRestartRequiredReads
        active_issue = [ordered]@{
            issue_ref = $activeIssueRef
            issue_number = $activeIssueNumber
            source = 'ActiveIssue parameter or SYNAPSE_ACTIVE_ISSUE environment variable'
            status = if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown' } else { 'provided' }
        }
        stale_schema_context_issue = [ordered]@{
            issue_ref = '#1398'
            role = 'background context for the stale-schema bug class; not the resume target'
        }
        github_reads = $githubReads
        post_restart_verification = @(
            'Run git status --short --branch and confirm the working tree matches the handoff/recovery notes.',
            "Read the active Codex process parent chain and confirm the active codex.exe PID is not stale PID $codexPid from this handoff.",
            'Run deferred tool discovery for Synapse first, then call real mcp__synapse.health and verify daemon pid/tool_surface_sha256 matches or intentionally supersedes this handoff.',
            'If Synapse tool discovery, approval, or metadata is still stale, rerun scripts\synapse-setup.ps1 and keep the issue open.',
            $resumeInstruction
        )
        restart_command_hint = $restartCommandHint
        repo_readback = $gitReadback
        token_path = $TokenPath
        recovery_notes_path = $recoveryNotesPath
    }

    try {
        New-Item -ItemType Directory -Force -Path $root | Out-Null
        Write-SynapseUtf8NoBomFile -Path $jsonPath -Text (($record | ConvertTo-Json -Depth 40) + "`n")
        $md = @(
            '# Synapse Codex Restart Handoff',
            '',
            "- Reason: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE ($Reason)",
            "- Phase: $Phase",
            "- Created UTC: $($record.created_at_utc)",
            "- Codex PID: $codexPid",
            "- Daemon: pid=$($daemon.pid) bind=$($daemon.bind) tool_count=$($daemon.tool_count) tool_surface_sha256=$($daemon.tool_surface_sha256)",
            "- Active issue: $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown; recover from caller/session context or open issue queue' } else { $activeIssueRef })",
            "- Stale-schema context issue: #1398 (background only; not the resume target)",
            "- Current process start snapshot: status=$startSnapshotStatus hash=$ProcessHashAtStart path=$ProcessSnapshotAtStart",
            "- Current daemon snapshot: $CurrentSnapshotPath",
            "- Diff: $($diffObject.summary)",
            '',
            '## Required Restart',
            "The running Codex process cannot hot-add changed MCP tools or mutate cached tool schemas. Close stale Codex PID $codexPid completely, restart Codex through the patched launcher, and prove the active codex.exe PID changed before continuing. Typing continue into the same PID is not a restart.",
            '',
            '## Read After Restart'
        )
        foreach ($item in $postRestartRequiredReads) {
            $md += "- $item"
        }
        $md += @(
            '',
            '## GitHub Reads'
        )
        foreach ($item in $record.github_reads) {
            $md += "- $item"
        }
        $md += @(
            '',
            '## Verification'
        )
        foreach ($item in $record.post_restart_verification) {
            $md += "- $item"
        }
        $md += @(
            '',
            "JSON artifact: $jsonPath",
            ''
        )
        Write-SynapseUtf8NoBomFile -Path $mdPath -Text (($md -join "`n") + "`n")

        if (-not [string]::IsNullOrWhiteSpace($recoveryNotesPath)) {
            $notes = @(
                '# Synapse Recovery Notes',
                '',
                '## Latest Codex Restart Handoff',
                '',
                "- Reason: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE ($Reason)",
                "- Phase: $Phase",
                "- Created UTC: $($record.created_at_utc)",
                "- JSON: $jsonPath",
                "- Markdown: $mdPath",
                "- Stale Codex PID: $codexPid",
                "- Daemon bind: $Bind",
                "- Daemon tool surface: $($daemon.tool_surface_sha256)",
                "- Active issue: $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'unknown; recover from caller/session context or open issue queue' } else { $activeIssueRef })",
                "- Stale-schema context issue: #1398 (background only; not the resume target)",
                '',
                "After restart, re-read AGENTS.md, #351, $(if ([string]::IsNullOrWhiteSpace($activeIssueRef)) { 'the active issue from the caller/session context' } else { $activeIssueRef }), git status, and this file before resuming. #1398 is stale-schema background context only. Run deferred Synapse tool discovery before calling real mcp__synapse tools for FSV; direct helper calls are diagnostics only.",
                ''
            )
            Write-SynapseUtf8NoBomFile -Path $recoveryNotesPath -Text (($notes -join "`n") + "`n")
        }
    } catch {
        Die "SYNAPSE_CODEX_RESTART_HANDOFF_WRITE_FAILED phase=$Phase reason=$Reason path=$jsonPath error=$($_.Exception.Message) remediation=repair permissions on %LOCALAPPDATA%\synapse\codex-restart-handoffs and the repo STATE directory, then rerun setup"
    }

    Info "SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE handoff_written phase=$Phase reason=$Reason json=$jsonPath markdown=$mdPath recovery_notes=$recoveryNotesPath"
    return [pscustomobject]@{
        JsonPath = $jsonPath
        MarkdownPath = $mdPath
        RecoveryNotesPath = $recoveryNotesPath
    }
}

function Assert-CodexCandidateHandoffPreservesCurrentProcess {
    param(
        [AllowNull()]$CodexAncestor,
        [Parameter(Mandatory=$true)]$CandidateSurface,
        [AllowNull()][string]$ProcessHashAtStart,
        [AllowNull()][string]$ProcessSnapshotAtStart,
        [AllowNull()][string]$SourceDir,
        [AllowNull()][string]$Bind,
        [AllowNull()][string]$TokenPath,
        [AllowNull()][string]$ActiveIssue
    )

    if ($null -eq $CodexAncestor) {
        return
    }

    $candidateHash = [string]$CandidateSurface.tool_surface_sha256
    if ([string]::IsNullOrWhiteSpace($candidateHash)) {
        Die "SYNAPSE_CODEX_CANDIDATE_TOOL_SURFACE_HASH_MISSING codex_pid=$($CodexAncestor.ProcessId) remediation=candidate tools/list preflight did not produce a usable tool_surface_sha256; refusing to touch the live daemon"
    }

    if ($ProcessHashAtStart -eq $candidateHash) {
        Info "Codex current-process tool surface will survive candidate handoff codex_pid=$($CodexAncestor.ProcessId) tool_surface_sha256=$candidateHash tool_count=$($CandidateSurface.tool_count)"
        return
    }

    $startSurface = Read-SynapseCodexToolSurfaceSnapshotOrNull -Path $ProcessSnapshotAtStart
    $diff = Get-SynapseToolSurfaceDiff -StartSurface $startSurface -CurrentSurface $CandidateSurface
    $diffSummary = $diff.Summary

    if ([string]::IsNullOrWhiteSpace($ProcessHashAtStart)) {
        $handoff = Write-SynapseCodexRestartHandoff `
            -Phase 'pre_handoff_candidate' `
            -Reason 'start_snapshot_missing_before_candidate_handoff' `
            -CodexAncestor $CodexAncestor `
            -Surface $CandidateSurface `
            -Diff $diff `
            -ProcessHashAtStart $ProcessHashAtStart `
            -ProcessSnapshotAtStart $ProcessSnapshotAtStart `
            -CurrentSnapshotPath $null `
            -SourceDir $SourceDir `
            -Bind $Bind `
            -TokenPath $TokenPath `
            -ActiveIssue $ActiveIssue
        Info ("WARN: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE_PRE_HANDOFF codex_pid={0} tool_surface_at_process_start=missing candidate_tool_surface_sha256={1} candidate_tool_count={2} candidate_pid={3} start_snapshot={4} handoff={5} {6} remediation=setup will continue only to install the verified daemon; final setup must fail closed if this Codex process remains stale." -f `
            $CodexAncestor.ProcessId,
            $candidateHash,
            $CandidateSurface.tool_count,
            $CandidateSurface.daemon_pid,
            $ProcessSnapshotAtStart,
            $handoff.JsonPath,
            $diffSummary)
        return
    }

    if (-not $diff.HasRestartRequired) {
        Info ("Codex current-process tool surface hash will change after candidate handoff but callable schema is unchanged; continuing codex_pid={0} start_tool_surface_sha256={1} candidate_tool_surface_sha256={2} candidate_tool_count={3} candidate_pid={4} start_snapshot={5} {6}" -f `
            $CodexAncestor.ProcessId,
            $ProcessHashAtStart,
            $candidateHash,
            $CandidateSurface.tool_count,
            $CandidateSurface.daemon_pid,
            $ProcessSnapshotAtStart,
            $diffSummary)
        return
    }

    $handoff = Write-SynapseCodexRestartHandoff `
        -Phase 'pre_handoff_candidate' `
        -Reason 'start_snapshot_hash_mismatch_before_candidate_handoff' `
        -CodexAncestor $CodexAncestor `
        -Surface $CandidateSurface `
        -Diff $diff `
        -ProcessHashAtStart $ProcessHashAtStart `
        -ProcessSnapshotAtStart $ProcessSnapshotAtStart `
        -CurrentSnapshotPath $null `
        -SourceDir $SourceDir `
        -Bind $Bind `
        -TokenPath $TokenPath `
        -ActiveIssue $ActiveIssue
    Info ("WARN: SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE_PRE_HANDOFF codex_pid={0} start_tool_surface_sha256={1} candidate_tool_surface_sha256={2} candidate_tool_count={3} candidate_pid={4} start_snapshot={5} handoff={6} {7} remediation=setup will continue only to install the verified daemon; final setup must fail closed if this Codex process remains stale." -f `
        $CodexAncestor.ProcessId,
        $ProcessHashAtStart,
        $candidateHash,
        $CandidateSurface.tool_count,
        $CandidateSurface.daemon_pid,
        $ProcessSnapshotAtStart,
        $handoff.JsonPath,
        $diffSummary)
    return
}

function Assert-CodexCurrentProcessToolSurfaceFresh {
    param(
        [AllowNull()]$CodexAncestor,
        [Parameter(Mandatory=$true)]$CurrentSurface,
        [AllowNull()][string]$ProcessHashAtStart,
        [AllowNull()][string]$ProcessSnapshotAtStart,
        [Parameter(Mandatory=$true)][string]$SnapshotPath,
        [AllowNull()][string]$SourceDir,
        [AllowNull()][string]$Bind,
        [AllowNull()][string]$TokenPath,
        [AllowNull()][string]$ActiveIssue,
        [switch]$NonFatal
    )

    if ($null -eq $CodexAncestor) {
        return
    }

    $currentHash = [string]$CurrentSurface.tool_surface_sha256
    $startSurface = Read-SynapseCodexToolSurfaceSnapshotOrNull -Path $ProcessSnapshotAtStart
    $diff = Get-SynapseToolSurfaceDiff -StartSurface $startSurface -CurrentSurface $CurrentSurface
    $diffSummary = $diff.Summary
    if ([string]::IsNullOrWhiteSpace($ProcessHashAtStart)) {
        $handoff = Write-SynapseCodexRestartHandoff `
            -Phase 'post_handoff_current_daemon' `
            -Reason 'start_snapshot_missing_after_daemon_handoff' `
            -CodexAncestor $CodexAncestor `
            -Surface $CurrentSurface `
            -Diff $diff `
            -ProcessHashAtStart $ProcessHashAtStart `
            -ProcessSnapshotAtStart $ProcessSnapshotAtStart `
            -CurrentSnapshotPath $SnapshotPath `
            -SourceDir $SourceDir `
            -Bind $Bind `
            -TokenPath $TokenPath `
            -ActiveIssue $ActiveIssue
        $message = ("SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE codex_pid={0} tool_surface_at_process_start=missing current_tool_surface_sha256={1} tool_count={2} daemon_pid={3} snapshot={4} start_snapshot={5} handoff={6} {7} remediation=restart Codex through the patched launcher, read the handoff plus STATE\\RECOVERY_NOTES.md, then resume the active issue named in the handoff and verify real mcp__synapse metadata." -f `
            $CodexAncestor.ProcessId,
            $currentHash,
            $CurrentSurface.tool_count,
            $CurrentSurface.daemon_pid,
            $SnapshotPath,
            $ProcessSnapshotAtStart,
            $handoff.JsonPath,
            $diffSummary)
        if ($NonFatal) {
            Info "WARN: $message"
            return
        }
        Die $message
    }

    if ($ProcessHashAtStart -ne $currentHash) {
        if (-not $diff.HasRestartRequired) {
            Info ("Codex current-process tool surface hash changed but callable schema is unchanged; continuing without restart handoff codex_pid={0} start_tool_surface_sha256={1} current_tool_surface_sha256={2} tool_count={3} daemon_pid={4} snapshot={5} start_snapshot={6} {7}" -f `
                $CodexAncestor.ProcessId,
                $ProcessHashAtStart,
                $currentHash,
                $CurrentSurface.tool_count,
                $CurrentSurface.daemon_pid,
                $SnapshotPath,
                $ProcessSnapshotAtStart,
                $diffSummary)
            return
        }
        $handoff = Write-SynapseCodexRestartHandoff `
            -Phase 'post_handoff_current_daemon' `
            -Reason 'start_snapshot_hash_mismatch_after_daemon_handoff' `
            -CodexAncestor $CodexAncestor `
            -Surface $CurrentSurface `
            -Diff $diff `
            -ProcessHashAtStart $ProcessHashAtStart `
            -ProcessSnapshotAtStart $ProcessSnapshotAtStart `
            -CurrentSnapshotPath $SnapshotPath `
            -SourceDir $SourceDir `
            -Bind $Bind `
            -TokenPath $TokenPath `
            -ActiveIssue $ActiveIssue
        $message = ("SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE codex_pid={0} tool_surface_at_process_start=mismatch start_tool_surface_sha256={1} current_tool_surface_sha256={2} tool_count={3} daemon_pid={4} snapshot={5} start_snapshot={6} handoff={7} {8} remediation=restart Codex through the patched launcher, read the handoff plus STATE\\RECOVERY_NOTES.md, then resume the active issue named in the handoff and verify real mcp__synapse metadata." -f `
            $CodexAncestor.ProcessId,
            $ProcessHashAtStart,
            $currentHash,
            $CurrentSurface.tool_count,
            $CurrentSurface.daemon_pid,
            $SnapshotPath,
            $ProcessSnapshotAtStart,
            $handoff.JsonPath,
            $diffSummary)
        if ($NonFatal) {
            Info "WARN: $message"
            return
        }
        Die $message
    }

    Info "Codex current-process tool surface matches daemon snapshot codex_pid=$($CodexAncestor.ProcessId) tool_surface_sha256=$currentHash tool_count=$($CurrentSurface.tool_count)"
}

function Request-SynapseGracefulShutdown {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][int[]]$ExpectedPids,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    try {
        $response = Invoke-RestMethod `
            -Method Post `
            -Uri "http://$Bind/shutdown" `
            -Headers @{ Authorization = "Bearer $Token" } `
            -UserAgent "synapse-setup/$Reason" `
            -TimeoutSec 4
    } catch {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_GRACEFUL_SHUTDOWN_REQUEST_FAILED'; Response = $null; Error = $_.Exception.Message }
    }

    $responsePid = 0
    try {
        $responsePid = [int]$response.pid
    } catch {
        return [pscustomobject]@{ Ok = $false; Code = 'SYNAPSE_GRACEFUL_SHUTDOWN_PID_UNREADABLE'; Response = $response; Error = $_.Exception.Message }
    }

    if ($ExpectedPids -notcontains $responsePid) {
        return [pscustomobject]@{
            Ok = $false
            Code = 'SYNAPSE_GRACEFUL_SHUTDOWN_PID_MISMATCH'
            Response = $response
            Error = "response_pid=$responsePid expected_pids=$($ExpectedPids -join ',')"
        }
    }

    if ($response.ok -ne $true -or "$($response.shutdown)" -ne 'requested') {
        return [pscustomobject]@{
            Ok = $false
            Code = 'SYNAPSE_GRACEFUL_SHUTDOWN_RESPONSE_INVALID'
            Response = $response
            Error = "response=$($response | ConvertTo-Json -Compress -Depth 6)"
        }
    }

    [pscustomobject]@{ Ok = $true; Code = 'OK'; Response = $response; Error = $null }
}

function Read-SynapseHttpErrorResponseBody {
    param([Parameter(Mandatory=$true)]$ErrorRecord)

    $errorDetailsBody = [string]$ErrorRecord.ErrorDetails.Message
    if (-not [string]::IsNullOrWhiteSpace($errorDetailsBody)) {
        return $errorDetailsBody
    }

    $response = $ErrorRecord.Exception.Response
    if ($null -eq $response) {
        return $null
    }
    if ($null -ne $response.Content) {
        try {
            return $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
        } catch {
            return "SYNAPSE_HTTP_ERROR_BODY_READ_FAILED source=http_response_content error=$($_.Exception.Message)"
        }
    }
    try {
        $stream = $response.GetResponseStream()
        if ($null -eq $stream) {
            return $null
        }
        $reader = New-Object System.IO.StreamReader($stream)
        try {
            return $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
    } catch {
        return "SYNAPSE_HTTP_ERROR_BODY_READ_FAILED source=response_stream error=$($_.Exception.Message)"
    }
}

function Read-SynapseHttpErrorStatus {
    param([Parameter(Mandatory=$true)]$ErrorRecord)

    $response = $ErrorRecord.Exception.Response
    if ($null -eq $response) {
        return $null
    }
    try {
        return [int]$response.StatusCode
    } catch {
        return $null
    }
}

function Request-SynapseChromeBridgeMaintenancePause {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][string]$Reason,
        [int]$PauseMs = $SynapseChromeBridgeMaintenancePauseMs,
        [int]$ResumeProbeAfterMs = $SynapseChromeBridgeMaintenanceResumeProbeAfterMs
    )

    $health = $null
    $healthAttempts = @()
    $healthTimeouts = @(4, 8, 12)
    for ($healthAttempt = 1; $healthAttempt -le $healthTimeouts.Count; $healthAttempt++) {
        $timeoutSec = [int]$healthTimeouts[$healthAttempt - 1]
        try {
            $health = Invoke-RestMethod `
                -Method Get `
                -Uri "http://$Bind/health" `
                -Headers @{ Authorization = "Bearer $Token" } `
                -UserAgent "synapse-setup/$Reason" `
                -TimeoutSec $timeoutSec
            $healthAttempts += [pscustomobject]@{
                attempt = $healthAttempt
                ok = $true
                timeout_sec = $timeoutSec
                error = $null
            }
            if ($healthAttempt -gt 1) {
                Info ("SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_HEALTH_RETRY_OK reason={0} bind={1} attempt={2} timeout_sec={3}" -f `
                    $Reason,
                    $Bind,
                    $healthAttempt,
                    $timeoutSec)
            }
            break
        } catch {
            $healthAttempts += [pscustomobject]@{
                attempt = $healthAttempt
                ok = $false
                timeout_sec = $timeoutSec
                error = $_.Exception.Message
            }
            if ($healthAttempt -lt $healthTimeouts.Count) {
                Start-Sleep -Seconds 1
            }
        }
    }

    if ($null -ne $health) {
        $chromeBridge = $health.subsystems.chrome_bridge
        if ($null -eq $chromeBridge) {
            return [pscustomobject]@{
                Ok = $false
                Skipped = $false
                Code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_HEALTH_MISSING'
                Response = [pscustomobject]@{
                    health = $health
                    health_attempts = $healthAttempts
                }
                Error = 'health.subsystems.chrome_bridge missing'
                Detail = $null
            }
        }

        $status = "$($chromeBridge.status)"
        $detail = "$($chromeBridge.detail)"
        if ($detail -match 'no_active_chrome_bridge_host') {
            return [pscustomobject]@{
                Ok = $true
                Skipped = $true
                Code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_SKIPPED_NO_ACTIVE_HOST'
                Response = [pscustomobject]@{
                    health = $health
                    health_attempts = $healthAttempts
                }
                Error = $null
                Detail = $detail
            }
        }
        if ([string]::IsNullOrWhiteSpace($status)) {
            return [pscustomobject]@{
                Ok = $false
                Skipped = $false
                Code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_STATUS_UNREADABLE'
                Response = [pscustomobject]@{
                    health = $health
                    health_attempts = $healthAttempts
                }
                Error = 'health.subsystems.chrome_bridge.status missing'
                Detail = $detail
            }
        }
        if ($Reason -eq 'install_health_failed_rollback' -and
            $script:SynapseManualInstallHealthRollbackProbe -and
            $script:SynapseManualInstallHealthRollbackPauseMode -eq 'force_unacknowledged') {
            return [pscustomobject]@{
                Ok = $false
                Skipped = $false
                Code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_MANUAL_UNACKNOWLEDGED_PROBE'
                Response = [pscustomobject]@{
                    manual_probe = 'install_health_failed_rollback'
                    pause_mode = $script:SynapseManualInstallHealthRollbackPauseMode
                    health = $health
                    health_attempts = $healthAttempts
                    bridge_status = $status
                    bridge_detail = $detail
                }
                Error = 'manual rollback probe forced the maintenance pause to remain unacknowledged after /health readback'
                Detail = $detail
                Attempts = @()
            }
        }
    } else {
        $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        $tcpClients = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
        $liveListeners = @($listeners | Where-Object { $_.OwnerExists })
        $liveTcpClients = @($tcpClients | Where-Object { $_.HasLivePeer -and [int]$_.PeerOwningProcess -gt 0 })
        if ($liveListeners.Count -eq 0 -and $liveTcpClients.Count -eq 0) {
            $detail = "health_preflight_unreadable attempts=$($healthAttempts.Count); socket_sot=reachable_daemon_endpoint_absent live_listener_count=0 live_tcp_clients=0"
            Info ("SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_SKIPPED_NO_REACHABLE_DAEMON_ENDPOINT reason={0} bind={1} attempts={2} last_error={3}`nlisteners:`n{4}`ntcp_clients:`n{5}`nremediation=/health is unreadable and the independent process/socket Source of Truth proves there is no live HTTP endpoint or live bridge/client peer to acknowledge. Setup will continue only through exact verified synapse-mcp.exe PID stop and a separate bind-release readback." -f `
                $Reason,
                $Bind,
                $healthAttempts.Count,
                $healthAttempts[-1].error,
                (Format-SynapseTcpBindListenerSnapshot -Snapshot $listeners),
                (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients))
            return [pscustomobject]@{
                Ok = $true
                Skipped = $true
                Code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_SKIPPED_NO_REACHABLE_DAEMON_ENDPOINT'
                Response = [pscustomobject]@{
                    health = $null
                    health_attempts = $healthAttempts
                    listeners = $listeners
                    tcp_clients = $tcpClients
                }
                Error = $null
                Detail = $detail
                Attempts = @()
            }
        }

        $detail = "health_preflight_unreadable attempts=$($healthAttempts.Count); live_listener_count=$($liveListeners.Count); live_tcp_clients=$($liveTcpClients.Count); proceeding_to_maintenance_pause_post because the POST endpoint is the authoritative bridge pause acknowledgement gate when a live daemon endpoint or live client peer exists"
        Info ("SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_HEALTH_PREFLIGHT_UNREADABLE_PROCEEDING reason={0} bind={1} attempts={2} last_error={3} live_listener_count={4} live_tcp_clients={5}`nlisteners:`n{6}`ntcp_clients:`n{7}" -f `
            $Reason,
            $Bind,
            $healthAttempts.Count,
            $healthAttempts[-1].error,
            $liveListeners.Count,
            $liveTcpClients.Count,
            (Format-SynapseTcpBindListenerSnapshot -Snapshot $listeners),
            (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients))
        if ($Reason -eq 'install_health_failed_rollback' -and
            $script:SynapseManualInstallHealthRollbackProbe -and
            $script:SynapseManualInstallHealthRollbackPauseMode -eq 'force_unacknowledged') {
            return [pscustomobject]@{
                Ok = $false
                Skipped = $false
                Code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_MANUAL_UNACKNOWLEDGED_PROBE'
                Response = [pscustomobject]@{
                    manual_probe = 'install_health_failed_rollback'
                    pause_mode = $script:SynapseManualInstallHealthRollbackPauseMode
                    health = $null
                    health_attempts = $healthAttempts
                    bridge_status = '<unreadable>'
                    bridge_detail = $detail
                }
                Error = 'manual rollback probe forced the maintenance pause to remain unacknowledged after /health readback failed'
                Detail = $detail
                Attempts = @()
            }
        }
    }

    $body = [ordered]@{
        pause_ms = $PauseMs
        resume_probe_after_ms = $ResumeProbeAfterMs
        reason = $Reason
    } | ConvertTo-Json -Compress -Depth 4

    $attempts = @()
    $maxAttempts = 5
    for ($attempt = 1; $attempt -le $maxAttempts; $attempt++) {
        try {
            $response = Invoke-RestMethod `
                -Method Post `
                -Uri "http://$Bind/chrome-debugger/native/maintenance-pause" `
                -Headers @{ Authorization = "Bearer $Token" } `
                -ContentType 'application/json' `
                -UserAgent "synapse-setup/$Reason" `
                -Body $body `
                -TimeoutSec 8
        } catch {
            $responseBody = Read-SynapseHttpErrorResponseBody -ErrorRecord $_
            $statusCode = Read-SynapseHttpErrorStatus -ErrorRecord $_
            $responseDetail = $responseBody
            $parsedResponseBody = $null
            if (-not [string]::IsNullOrWhiteSpace($responseBody)) {
                try {
                    $parsedResponseBody = $responseBody | ConvertFrom-Json -ErrorAction Stop
                    if ($null -ne $parsedResponseBody.detail) {
                        $responseDetail = "$($parsedResponseBody.detail)"
                    }
                } catch {
                    $parsedResponseBody = $null
                }
            }
            if ($statusCode -eq 503 -and $responseDetail -match 'no_active_chrome_bridge_host') {
                return [pscustomobject]@{
                    Ok = $true
                    Skipped = $true
                    Code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_SKIPPED_NO_ACTIVE_HOST'
                    Response = [pscustomobject]@{
                        health_attempts = $healthAttempts
                        maintenance_pause_attempt = $attempt
                        post_response = $parsedResponseBody
                        post_response_body = $responseBody
                    }
                    Error = $null
                    Detail = $responseDetail
                    Attempts = $attempts
                }
            }
            $attempts += [pscustomobject]@{
                attempt = $attempt
                code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_REQUEST_FAILED'
                ok = $false
                status = $statusCode
                error = $_.Exception.Message
                response = $responseBody
            }
            if ($attempt -lt $maxAttempts) {
                Start-Sleep -Seconds 1
                continue
            }
            return [pscustomobject]@{
                Ok = $false
                Skipped = $false
                Code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_REQUEST_FAILED'
                Response = $attempts
                Error = "attempts=$($attempts.Count) last_status=$statusCode message=$($_.Exception.Message)"
                Detail = $detail
                Attempts = $attempts
            }
        }

        $pause = $response.pause
        $websocketClose = $null
        if ($null -ne $pause) {
            $websocketClose = $pause.websocket_close
        }
        $activeSocketWasOpen = $false
        if ($null -ne $websocketClose -and $null -ne $websocketClose.ready_state_before) {
            $readyStateBefore = [int]$websocketClose.ready_state_before
            $activeSocketWasOpen = ($readyStateBefore -eq 0 -or $readyStateBefore -eq 1)
        }
        $websocketCloseFailed = $false
        if ($null -eq $websocketClose) {
            $websocketCloseFailed = $true
        } elseif ($null -ne $websocketClose.close_error) {
            $websocketCloseFailed = $true
        } elseif ($activeSocketWasOpen -and $websocketClose.close_requested -ne $true) {
            $websocketCloseFailed = $true
        }

        $attemptReadback = [pscustomobject]@{
            attempt = $attempt
            code = 'OK'
            ok = ($response.ok -eq $true)
            status = 200
            pause_ms = if ($null -eq $pause) { $null } else { $pause.pause_ms }
            resume_probe_after_ms = if ($null -eq $pause) { $null } else { $pause.resume_probe_after_ms }
            resume_probe_after_unix_ms = if ($null -eq $pause) { $null } else { $pause.resume_probe_after_unix_ms }
            paused_daemon_pid = if ($null -eq $pause) { $null } else { $pause.paused_daemon_pid }
            paused_daemon_instance_id = if ($null -eq $pause) { $null } else { $pause.paused_daemon_instance_id }
            reconnect_suppressed = if ($null -eq $pause) { $null } else { $pause.reconnect_suppressed }
            persisted = if ($null -eq $pause) { $null } else { $pause.persisted }
            websocket_close = $websocketClose
        }
        $attempts += $attemptReadback

        $resumeProbeReadbackOk = (
            $null -eq $pause.resume_probe_after_ms -or
            [int64]$pause.resume_probe_after_ms -eq [int64]$ResumeProbeAfterMs
        )
        if ($response.ok -eq $true -and $null -ne $pause -and $pause.reconnect_suppressed -eq $true -and $pause.persisted -eq $true -and $resumeProbeReadbackOk -and -not $websocketCloseFailed) {
            return [pscustomobject]@{
                Ok = $true
                Skipped = $false
                Code = 'OK'
                Response = $response
                Error = $null
                Detail = $detail
                Attempts = $attempts
            }
        }

        $attemptReadback.code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_RESPONSE_INVALID'
        if ($attempt -lt $maxAttempts) {
            Start-Sleep -Seconds 1
        }
    }

    [pscustomobject]@{
        Ok = $false
        Skipped = $false
        Code = 'SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_RESPONSE_INVALID'
        Response = $attempts
        Error = "attempts=$($attempts.Count) last_response=$($attempts[-1] | ConvertTo-Json -Compress -Depth 8)"
        Detail = $detail
        Attempts = $attempts
    }
}

function Enter-SynapseChromeBridgeMaintenancePause {
    param(
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$Token,
        [Parameter(Mandatory=$true)][string]$Reason,
        [switch]$AllowUnacknowledgedChromeBridgePauseForRollback
    )

    if ($script:SynapseChromeBridgeMaintenancePausePrepared) {
        if ($script:SynapseChromeBridgeMaintenancePausePreparedBind -cne $Bind -or
            $script:SynapseChromeBridgeMaintenancePausePreparedReason -cne $Reason) {
            Die ("SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_PREPARED_SCOPE_MISMATCH prepared_bind={0} requested_bind={1} prepared_reason={2} requested_reason={3} remediation=a maintenance pause acknowledgement is scoped to one daemon handoff; start a fresh setup run instead of reusing it for another bind or reason" -f `
                $script:SynapseChromeBridgeMaintenancePausePreparedBind,
                $Bind,
                $script:SynapseChromeBridgeMaintenancePausePreparedReason,
                $Reason)
        }
        if ($null -ne $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs) {
            Assert-SynapseChromeBridgeMaintenancePauseBudget -Reason $Reason -Bind $Bind -Phase 'prepared_pause_reuse'
        }
        Info ("FORCE_RESTART: SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_PREPARED_REUSED reason={0} bind={1} pause_until_unix_ms={2} resume_probe_after_unix_ms={3}" -f `
            $Reason,
            $Bind,
            ($(if ($null -eq $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs) { '<skipped-no-active-host>' } else { $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs })),
            ($(if ($null -eq $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs) { '<none>' } else { $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs })))
        return $script:SynapseChromeBridgeMaintenancePausePreparedResult
    }

    $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs = $null
    $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs = $null
    $pause = Request-SynapseChromeBridgeMaintenancePause `
        -Bind $Bind `
        -Token $Token `
        -Reason $Reason `
        -PauseMs $SynapseChromeBridgeMaintenancePauseMs `
        -ResumeProbeAfterMs $SynapseChromeBridgeMaintenanceResumeProbeAfterMs
    if (-not $pause.Ok) {
        if ($AllowUnacknowledgedChromeBridgePauseForRollback -and $Reason -eq 'install_health_failed_rollback') {
            Info ("FORCE_RESTART: SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_UNACKNOWLEDGED_ROLLBACK_CONTINUING reason={0} bind={1} code={2} error={3} detail={4} response={5} remediation=install health already failed after replacing the daemon binary, so rollback must restore the previous verified daemon instead of stranding the failed candidate; setup will continue only through authenticated shutdown or exact verified synapse-mcp.exe PID stop, then separately verify the restored daemon and Chrome bridge Source of Truth." -f `
                $Reason,
                $Bind,
                $pause.Code,
                $pause.Error,
                $pause.Detail,
                ($(if ($null -eq $pause.Response) { '<none>' } else { $pause.Response | ConvertTo-Json -Compress -Depth 8 })))
            return $pause
        }
        Die ("{0} reason={1} bind={2} error={3} detail={4} response={5} remediation=forced daemon maintenance requires the already-open Chrome bridge to acknowledge a bounded reconnect pause before shutdown when an active bridge host exists. Inspect the exact extension error code/detail in this response and daemon CHROME_DEBUGGER_RESPONSE_ACCEPTED log; setup keeps restart authority intact until this prepare gate succeeds." -f `
            $pause.Code,
            $Reason,
            $Bind,
            $pause.Error,
            $pause.Detail,
            ($(if ($null -eq $pause.Response) { '<none>' } else { $pause.Response | ConvertTo-Json -Compress -Depth 8 })))
    }

    if ($pause.Skipped) {
        Info ("FORCE_RESTART: {0} reason={1} bind={2} detail={3}" -f `
            $pause.Code,
            $Reason,
            $Bind,
            $pause.Detail)
    } else {
        $pauseUntil = $pause.Response.pause.pause_until_unix_ms
        $resumeProbeAfter = $pause.Response.pause.resume_probe_after_unix_ms
        try {
            $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs = [int64]$pauseUntil
            $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs = [int64]$resumeProbeAfter
        } catch {
            Die ("SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_ACK_TIME_INVALID reason={0} bind={1} pause_until_unix_ms={2} resume_probe_after_unix_ms={3} error={4} remediation=the bridge acknowledgement did not contain exact integer lease timestamps; restart authority remains intact and setup refuses the handoff" -f `
                $Reason,
                $Bind,
                $pauseUntil,
                $resumeProbeAfter,
                $_.Exception.Message)
        }
        if ($script:SynapseChromeBridgeMaintenancePauseUntilUnixMs -le 0 -or
            $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs -le 0) {
            Die ("SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_ACK_TIME_INVALID reason={0} bind={1} pause_until_unix_ms={2} resume_probe_after_unix_ms={3} remediation=the bridge acknowledgement returned non-positive lease timestamps; restart authority remains intact and setup refuses the handoff" -f `
                $Reason,
                $Bind,
                $script:SynapseChromeBridgeMaintenancePauseUntilUnixMs,
                $script:SynapseChromeBridgeMaintenanceResumeProbeAfterUnixMs)
        }
        Assert-SynapseChromeBridgeMaintenancePauseBudget -Reason $Reason -Bind $Bind -Phase 'pause_prepare'
        Info ("FORCE_RESTART: SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_ACK reason={0} bind={1} pause={2}" -f `
            $Reason,
            $Bind,
            ($pause.Response.pause | ConvertTo-Json -Compress -Depth 8))
        $webSocketClose = $pause.Response.pause.websocket_close
        if ($webSocketClose -and $webSocketClose.had_socket -eq $true -and $webSocketClose.close_requested -eq $true) {
            Info ("FORCE_RESTART: SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_SOCKET_DRAIN_WAIT reason={0} bind={1} wait_ms={2} had_socket={3} close_requested={4} close_deferred={5} remediation=the bridge intentionally sends the pause response before closing its active WebSocket, so setup waits for the bounded response-drain window before daemon shutdown." -f `
                $Reason,
                $Bind,
                $SynapseChromeBridgeMaintenanceCloseDrainMs,
                $webSocketClose.had_socket,
                $webSocketClose.close_requested,
                ($(if ($null -eq $webSocketClose.close_deferred) { '<missing>' } else { $webSocketClose.close_deferred })))
            Start-Sleep -Milliseconds $SynapseChromeBridgeMaintenanceCloseDrainMs
        }
    }

    $script:SynapseChromeBridgeMaintenancePausePrepared = $true
    $script:SynapseChromeBridgeMaintenancePausePreparedBind = $Bind
    $script:SynapseChromeBridgeMaintenancePausePreparedReason = $Reason
    $script:SynapseChromeBridgeMaintenancePausePreparedResult = $pause
    return $pause
}

function Get-SynapseActiveSessionCount {
    param([Parameter(Mandatory=$true)]$Health)

    $value = $Health.subsystems.http.active_sessions
    if ($null -eq $value) {
        return $null
    }

    try {
        return [int]$value
    } catch {
        return $null
    }
}

function Assert-SynapseRestartAllowed {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [ValidateRange(1, 300)][int]$HealthTimeoutSec = 120,
        [switch]$ForceRestart,
        [switch]$AllowActiveClientDrain
    )

    $allProcesses = @(Get-SynapseMcpProcessSnapshot)
    $processes = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $allProcesses -Bind $Bind -DbPath $DbPath)
    $ignoredProcesses = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $allProcesses -Bind $Bind -DbPath $DbPath -Invert)
    if ($ignoredProcesses.Count -gt 0) {
        Info ("Synapse restart guard reason={0} ignored_non_target_process_count={1}`nignored:`n{2}" -f `
            $Reason,
            $ignoredProcesses.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $ignoredProcesses))
    }
    if ($processes.Count -eq 0) {
        Info "Synapse restart guard reason=$Reason target_process_count=0 ignored_non_target_process_count=$($ignoredProcesses.Count) verdict=clear"
        return
    }

    $null = Get-SynapseBindEndpoint -Bind $Bind
    $nonHttpProcesses = @($processes | Where-Object { $_.CommandLine -notmatch '(?i)--mode\s+http' })
    $tokenRead = Read-SynapseSetupTokenForRestartGuard -TokenPath $TokenPath
    if (-not $tokenRead.Ok) {
        $message = "$($tokenRead.Code) reason=$Reason process_count=$($processes.Count) $($tokenRead.Detail) remediation=do not restart blindly while the daemon may have clients; repair token state or rerun with -ForceRestart after coordinating a maintenance window"
        if ($ForceRestart) {
            Info "FORCE_RESTART: $message"
        } else {
            Die $message
        }
    }

    $activeSessions = $null
    $healthRead = $null
    if ($tokenRead.Ok) {
        $healthRead = Read-SynapseHealthForRestartGuard -Bind $Bind -Token $tokenRead.Token -TimeoutSec $HealthTimeoutSec
        if (-not $healthRead.Ok) {
            $message = "SYNAPSE_RESTART_GUARD_HEALTH_UNREADABLE reason=$Reason bind=$Bind timeout_s=$($healthRead.TimeoutSec) error=$($healthRead.Error) remediation=do not restart blindly; repair the daemon/token or rerun with -ForceRestart after coordinating a maintenance window"
            if ($ForceRestart) {
                Info "FORCE_RESTART: $message"
            } else {
                Die $message
            }
        }
    }

    if ($tokenRead.Ok -and $healthRead.Ok) {
        $activeSessions = Get-SynapseActiveSessionCount -Health $healthRead.Health
        if ($null -eq $activeSessions) {
            $message = "SYNAPSE_RESTART_GUARD_ACTIVE_SESSIONS_UNREADABLE reason=$Reason bind=$Bind remediation=health did not expose subsystems.http.active_sessions; do not restart blindly"
            if ($ForceRestart) {
                Info "FORCE_RESTART: $message"
            } else {
                Die $message
            }
        }
    }

    $tcpConnections = @(Get-SynapseTcpClientSnapshot -Bind $Bind)
    $setupLineagePids = @((Get-ProcessLineage -StartPid $PID) | ForEach-Object { [int]$_.ProcessId })
    $selfProbeTcpClients = @($tcpConnections | Where-Object {
        $_.HasLivePeer -and $setupLineagePids -contains [int]$_.PeerOwningProcess
    })
    $tcpClients = @($tcpConnections | Where-Object {
        $_.HasLivePeer -and $setupLineagePids -notcontains [int]$_.PeerOwningProcess
    })
    $staleTcpConnections = @($tcpConnections | Where-Object { -not $_.HasLivePeer })
    $blockers = @()
    $clientDrainBlockers = @()
    if ($nonHttpProcesses.Count -gt 0) { $blockers += "non_http_synapse_processes=$($nonHttpProcesses.Count)" }
    if ($tcpClients.Count -gt 0) {
        $blockers += "live_tcp_clients=$($tcpClients.Count)"
        $clientDrainBlockers += "live_tcp_clients=$($tcpClients.Count)"
    }
    if ($null -ne $activeSessions -and $activeSessions -gt 0 -and $tcpClients.Count -gt 0) {
        $blockers += "active_sessions=$activeSessions"
        $clientDrainBlockers += "active_sessions=$activeSessions"
    }
    if ($null -ne $activeSessions -and $activeSessions -gt 0 -and $tcpClients.Count -eq 0) {
        Info "Synapse restart guard reason=$Reason idle_session_map_entries=$activeSessions live_tcp_clients=0 verdict=not_blocking_idle_sessions"
    }
    if ($staleTcpConnections.Count -gt 0) {
        Info ("Synapse restart guard reason={0} stale_tcp_connections={1}`nstale_tcp:`n{2}" -f `
            $Reason,
            $staleTcpConnections.Count,
            (Format-SynapseTcpClientSnapshot -Snapshot $staleTcpConnections))
    }
    if ($selfProbeTcpClients.Count -gt 0) {
        Info ("Synapse restart guard reason={0} self_probe_tcp_connections={1}`nself_probe_tcp:`n{2}" -f `
            $Reason,
            $selfProbeTcpClients.Count,
            (Format-SynapseTcpClientSnapshot -Snapshot $selfProbeTcpClients))
    }

    if ($blockers.Count -gt 0) {
        $message = ("SYNAPSE_ACTIVE_CLIENTS_PRESENT reason={0} blockers={1} process_count={2} active_sessions={3} live_tcp_clients={4} idle_session_map_entries={5} stale_tcp_connections={6}`nprocesses:`n{7}`ntcp_clients:`n{8}`nstale_tcp:`n{9}`nremediation=wait for MCP clients to disconnect, close only the exact owner-known helper process listed here, or rerun with -ForceRestart only after coordinating a maintenance window. Do not close terminal windows or broad shell processes." -f `
            $Reason,
            ($blockers -join ','),
            $processes.Count,
            ($(if ($null -eq $activeSessions) { 'unknown' } else { $activeSessions })),
            $tcpClients.Count,
            ($(if ($null -eq $activeSessions) { 'unknown' } else { $activeSessions })),
            $staleTcpConnections.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $processes),
            (Format-SynapseTcpClientSnapshot -Snapshot $tcpClients),
            (Format-SynapseTcpClientSnapshot -Snapshot $staleTcpConnections))
        if ($nonHttpProcesses.Count -gt 0 -and -not $ForceRestart) {
            Die $message
        } elseif ($AllowActiveClientDrain -and $clientDrainBlockers.Count -gt 0 -and $nonHttpProcesses.Count -eq 0) {
            Info ("Synapse restart guard reason={0} verdict=drain_permitted blockers={1} active_sessions={2} live_tcp_clients={3} stale_tcp_connections={4} process_count={5} drain=authenticated_http_shutdown" -f `
                $Reason,
                ($clientDrainBlockers -join ','),
                ($(if ($null -eq $activeSessions) { 'unknown' } else { $activeSessions })),
                $tcpClients.Count,
                $staleTcpConnections.Count,
                $processes.Count)
        } elseif ($ForceRestart) {
            Info "FORCE_RESTART: $message"
        } else {
            Die $message
        }
    } else {
        Info "Synapse restart guard reason=$Reason verdict=clear health_timeout_s=$($healthRead.TimeoutSec) active_sessions=$activeSessions live_tcp_clients=0 stale_tcp_connections=$($staleTcpConnections.Count) process_count=$($processes.Count)"
    }
}

function Assert-SynapseProcessStopTarget {
    param(
        [Parameter(Mandatory=$true)]$SnapshotProcess,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath
    )

    $pidValue = [int]$SnapshotProcess.ProcessId
    $current = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
    if (-not $current) {
        Info "Synapse process stop target already exited pid=$pidValue"
        return $null
    }

    $protectedNames = @(
        'cmd.exe',
        'powershell.exe',
        'pwsh.exe',
        'WindowsTerminal.exe',
        'OpenConsole.exe',
        'conhost.exe',
        'wsl.exe',
        'wslhost.exe',
        'Code.exe'
    )
    if ($protectedNames -contains $current.Name) {
        Die ("SYNAPSE_PROTECTED_PROCESS_STOP_REFUSED pid={0} name={1} command_line={2} remediation=terminal/IDE/WSL host processes are operator and agent workspaces; never close them from setup, tests, or FSV. Stop only exact owner-known helper PIDs." -f `
            $pidValue,
            $current.Name,
            $current.CommandLine)
    }

    $exeLeaf = if ($current.ExecutablePath) { Split-Path -Leaf $current.ExecutablePath } else { '' }
    if (-not (Test-SynapseMcpExecutableLeafName -Name $current.Name) -and -not (Test-SynapseMcpExecutableLeafName -Name $exeLeaf)) {
        Die ("SYNAPSE_PROCESS_STOP_TARGET_MISMATCH pid={0} expected=synapse-mcp.exe_or_content_addressed_name actual_name={1} actual_path={2} command_line={3} remediation=PID was reused or snapshot was not a Synapse process; refusing exact-PID stop" -f `
            $pidValue,
            $current.Name,
            $current.ExecutablePath,
            $current.CommandLine)
    }
    if ($current.CommandLine -notmatch '(?i)synapse-mcp(\.exe)?') {
        Die ("SYNAPSE_PROCESS_STOP_TARGET_UNVERIFIED pid={0} name={1} command_line={2} remediation=command line does not prove a Synapse MCP target; refusing exact-PID stop" -f `
            $pidValue,
            $current.Name,
            $current.CommandLine)
    }

    $targetMatch = Get-SynapseMcpDeployTargetMatch -Process $current -Bind $Bind -DbPath $DbPath
    if (-not $targetMatch.IsMatch) {
        Die ("SYNAPSE_PROCESS_STOP_TARGET_SCOPE_MISMATCH pid={0} bind={1} db={2} actual_bind_arg={3} actual_db_arg={4} command_line={5} remediation=setup only stops synapse-mcp.exe processes whose command line targets the deployed --bind or --db; refusing collateral stop" -f `
            $pidValue,
            $Bind,
            $targetMatch.ExpectedDb,
            $targetMatch.BindArg,
            $targetMatch.DbArg,
            $current.CommandLine)
    }
    $rules = $targetMatch.Rules -join ','
    $current | Add-Member -NotePropertyName DeployTargetMatched -NotePropertyValue $true -Force
    $current | Add-Member -NotePropertyName DeployTargetRules -NotePropertyValue $rules -Force
    $current | Add-Member -NotePropertyName DeployTargetBindArg -NotePropertyValue $targetMatch.BindArg -Force
    $current | Add-Member -NotePropertyName DeployTargetDbArg -NotePropertyValue $targetMatch.DbArg -Force
    Info "Synapse process stop target verified pid=$pidValue match_rules=$rules path=$($current.ExecutablePath) cmd=$($current.CommandLine)"

    return $current
}

# --- #2131: one shutdown verdict law, shared with the daemon -----------------
#
# The defect this replaces, confirmed on production 2026-08-08: a graceful
# `-Stop` recorded its phase-one exit-intent marker, spent ~82 s in the
# post-flush teardown, and was killed by the daemon's own 90 s HTTP watchdog.
# The watchdog writes an exit record on its way out -- that is its job -- so
# `ended_at_unix_ms` was present, and every classifier in this script asked only
# that one question. The deploy drain printed
# `ended_reason=http_shutdown_watchdog_expired clean_shutdown=True
# expected_next_boot_previous_shutdown=clean`, i.e. it read the cause, printed
# the cause, and then ignored the cause when deciding the verdict.
#
# `Invoke-SynapseDaemonStart` DIES on
# `SYNAPSE_DAEMON_START_PREVIOUS_SHUTDOWN_MISMATCH` when its expectation and the
# daemon's boot verdict disagree, so this is not merely a cosmetic string: the
# two laws must be the same law. This function is the PowerShell half of
# `daemon_lifecycle::classify_previous_shutdown`, and the two arms of the cause
# taxonomy below mirror `GRACEFUL_EXIT_CAUSES` / `classify_exit_cause` exactly.
# Changing one without the other breaks `-Start`.
$script:SynapseGracefulExitCauses = @(
    'graceful',
    'os_console_close',
    'os_window_close',
    'os_logoff',
    'os_shutdown',
    'os_session_end'
)
$script:SynapseKnownForcedExitCauses = @(
    'http_shutdown_watchdog_expired',
    'http_shutdown_watchdog_spawn_failed',
    'panic',
    'top_level_error',
    'stdio_storage_or_calyx_open_or_maintenance_start_failed'
)

function Get-SynapseDaemonExitCauseClass {
    param([AllowNull()][string]$Cause)

    if ([string]::IsNullOrWhiteSpace($Cause)) { return 'none' }
    $trimmed = $Cause.Trim()
    if ($script:SynapseGracefulExitCauses -contains $trimmed) { return 'graceful' }
    if (($script:SynapseKnownForcedExitCauses -contains $trimmed) -or
        $trimmed.StartsWith('startup_') -or
        $trimmed.EndsWith('_vault_not_closed')) { return 'forced' }
    return 'unrecognized'
}

# Returns the verdict the NEXT daemon boot will report for this run record, plus
# the evidence behind it. `Readable=$false` means the record could not be read at
# all, which is 'unknown' -- distinct from 'dirty', which is a claim about the
# daemon rather than about this script's ability to read a file.
function Get-SynapseDaemonPreviousShutdownVerdict {
    param(
        [AllowNull()]$Record,
        [bool]$Readable = $true
    )

    if (-not $Readable -or $null -eq $Record) {
        return [pscustomobject]@{
            Verdict       = 'unknown'
            Clean         = $false
            EndedAtUnixMs = $null
            EndedReason   = '<unreadable>'
            CauseClass    = 'none'
            EndingPhase   = '<unreadable>'
            MarkerPresent = $false
            Contradictions = @()
            Detail        = 'basis=the daemon lifecycle run record could not be read'
        }
    }

    $endedAt = $Record.ended_at_unix_ms
    $endedReasonRaw = [string]$Record.ended_reason
    $endedReason = if ([string]::IsNullOrWhiteSpace($endedReasonRaw)) { $null } else { $endedReasonRaw.Trim() }
    $endingAt = $Record.ending_at_unix_ms
    $endingPhase = if ($null -eq $Record.ending_phase) { 'none' } else { [string]$Record.ending_phase }
    $endingReason = if ($null -eq $Record.ending_reason) { 'none' } else { [string]$Record.ending_reason }
    $markerPresent = ($null -ne $endingAt)
    $causeClass = Get-SynapseDaemonExitCauseClass -Cause $endedReason

    $contradictions = @()
    if ($null -ne $endedAt -and $null -eq $endedReason) {
        $contradictions += "ended_at_unix_ms=$endedAt was finalized with no ended_reason naming the cause"
    }
    if ($null -eq $endedAt -and $null -ne $endedReason) {
        $contradictions += "ended_reason=$endedReason was recorded with no ended_at_unix_ms finalizing it"
    }
    if ($causeClass -eq 'unrecognized') {
        $contradictions += "ended_reason=$endedReason is not a terminal cause this script can classify"
    }
    if ($null -ne $endedAt -and $null -ne $endingAt -and ([int64]$endedAt -lt [int64]$endingAt)) {
        $contradictions += "ended_at_unix_ms=$endedAt precedes the ending_at_unix_ms=$endingAt it supersedes"
    }

    $finalizedGraceful = ($null -ne $endedAt -and $causeClass -eq 'graceful')
    $verdict = if ($finalizedGraceful -and $contradictions.Count -eq 0) {
        'clean'
    } elseif ($markerPresent) {
        'interrupted_graceful'
    } else {
        'dirty'
    }

    $basis = switch ($verdict) {
        'clean' { 'the exit record was finalized and its cause names a completed drain' }
        'interrupted_graceful' {
            if ($causeClass -eq 'none') { 'a close was commanded and no exit record finalized it' }
            elseif ($causeClass -eq 'graceful') { 'a close was commanded and its finalization is contradicted by its own fields' }
            else { 'a close was commanded and then ended by a forced/abnormal cause before it finished' }
        }
        default {
            if ($causeClass -eq 'none') { 'no close was ever commanded and no exit record finalized the run' }
            else { 'no close was ever commanded and the run ended by a forced/abnormal cause' }
        }
    }
    $contradictionText = if ($contradictions.Count -eq 0) { 'none' } else { ($contradictions -join ' | ') }

    [pscustomobject]@{
        Verdict        = $verdict
        Clean          = ($verdict -eq 'clean')
        EndedAtUnixMs  = $endedAt
        EndedReason    = $(if ($null -eq $endedReason) { 'none' } else { $endedReason })
        CauseClass     = $causeClass
        EndingPhase    = $endingPhase
        MarkerPresent  = $markerPresent
        Contradictions = $contradictions
        Detail         = ("basis={0}; ended_at_unix_ms={1} ended_reason={2} ended_cause_class={3} ending_marker={4} ending_reason={5} ending_phase={6} contradictions={7}" -f `
            $basis,
            $(if ($null -eq $endedAt) { 'none' } else { $endedAt }),
            $(if ($null -eq $endedReason) { 'none' } else { $endedReason }),
            $causeClass,
            $(if ($markerPresent) { 'present' } else { 'absent' }),
            $endingReason,
            $endingPhase,
            $contradictionText)
    }
}

# --- #2100: the drain's exit-wait is sized by STALL, not by wall clock -------
#
# The failure this replaces, from the first production run of the #2092 unified
# drain: the daemon accepted the shutdown at 00:16:23.87Z, flushed durably at
# 00:16:29.22Z, and then went completely silent inside its vault close. The
# drain waited its flat 90 s budget, escalated to an identity-verified kill at
# 00:17:56.64Z -- mid-close, after the flush, before the graceful exit record --
# and the next boot read `previous_shutdown=dirty`.
#
# A flat budget cannot tell a close that is working from one that is hung, so it
# has to guess, and any constant it guesses is wrong in one direction: too short
# kills healthy closes (what happened), too long stalls every deploy behind a
# genuinely wedged daemon. The daemon's own log is the discriminator that was
# always available and never read -- its last-write timestamp advances exactly
# while the close is making progress.
#
# Two liveness witnesses, both physical and both already on disk:
#
#   1. the daemon stderr log's (path, length, LastWriteTimeUtc) -- the close now
#      emits `SYNAPSE_CALYX_VAULT_CLOSE_PHASE` per phase for precisely this
#      reason, so a working close always advances it;
#   2. `daemon-run-current.json`'s phase-one `ending_*` marker -- written BEFORE
#      the close begins, with an `ending_phase` the close refreshes.
#
# While either advances, the deadline extends. When neither has advanced for
# `StallSeconds` the wait escalates, which is the honest signal: the daemon is
# not merely slow, it has stopped doing anything. An absolute ceiling still
# bounds the whole wait so a daemon that logs forever cannot block a deploy.
function Get-SynapseDaemonLivenessSample {
    param(
        [AllowNull()][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$DbPath
    )

    $logPath = '<none>'
    $logLength = -1
    $logWriteTicks = 0
    if (-not [string]::IsNullOrWhiteSpace($LogDir) -and (Test-Path -LiteralPath $LogDir -PathType Container)) {
        $newest = Get-ChildItem -LiteralPath $LogDir -Filter 'daemon-stderr-gen*.log' -File -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTimeUtc -Descending |
            Select-Object -First 1
        if ($null -ne $newest) {
            $logPath = $newest.FullName
            $logLength = [int64]$newest.Length
            $logWriteTicks = [int64]$newest.LastWriteTimeUtc.Ticks
        }
    }

    $endingPhase = '<none>'
    $endingAt = 0
    $endedAt = 0
    $runCurrentPath = Join-Path $DbPath 'daemon-run-current.json'
    if (Test-Path -LiteralPath $runCurrentPath -PathType Leaf) {
        try {
            $run = Get-Content -Raw -LiteralPath $runCurrentPath -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
            if ($null -ne $run.ending_phase) { $endingPhase = [string]$run.ending_phase }
            if ($null -ne $run.ending_at_unix_ms) { $endingAt = [int64]$run.ending_at_unix_ms }
            if ($null -ne $run.ended_at_unix_ms) { $endedAt = [int64]$run.ended_at_unix_ms }
        } catch {
            # A torn read of a file being atomically replaced is expected and is
            # not evidence of anything; the next poll resolves it.
            $endingPhase = '<unreadable>'
        }
    }

    [pscustomobject]@{
        LogPath        = $logPath
        LogLength      = $logLength
        LogWriteTicks  = $logWriteTicks
        EndingPhase    = $endingPhase
        EndingAtUnixMs = $endingAt
        EndedAtUnixMs  = $endedAt
        RunCurrentPath = $runCurrentPath
        # One comparable value: any change in any witness is progress.
        Fingerprint    = "$logPath|$logLength|$logWriteTicks|$endingPhase|$endingAt|$endedAt"
    }
}

function Stop-SynapseMcpProcesses {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [switch]$ForceRestart,
        [switch]$AllowUnacknowledgedChromeBridgePauseForRollback,
        # Install-handoff escalation (#1800): when graceful shutdown times out,
        # proceed to the identity-verified exact-PID force stop instead of dying.
        # By that point the daemon has already closed its listener and committed
        # to exit (retained-owner fail-closed state); aborting the deploy only
        # leaves a zombie window until the daemon's own 90s shutdown watchdog
        # forces the same termination without installing the new binary.
        [switch]$EscalateAfterGracefulTimeout,
        [int]$TimeoutSeconds = 15,
        # #2100: where the daemon's own liveness evidence lives. Optional so the
        # `-Remove` path and any caller without a log directory keeps the old
        # flat-budget behaviour explicitly rather than by accident -- when this is
        # absent the run-record marker is still read, and when neither witness is
        # available the wait degrades to the wall-clock budget and SAYS SO.
        [AllowNull()][string]$LogDir,
        # Longest the daemon may produce no evidence of progress at all before
        # the exit-wait escalates. This, not $TimeoutSeconds, is the number that
        # decides a kill on a healthy-but-slow close.
        [int]$StallSeconds = 25,
        # Absolute backstop on the extended wait, so a daemon that keeps writing
        # forever still cannot hold a deploy open indefinitely.
        [int]$MaxWaitSeconds = 600
    )

    $allBefore = @(Get-SynapseMcpProcessSnapshot)
    $before = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $allBefore -Bind $Bind -DbPath $DbPath)
    $ignoredBefore = @(Select-SynapseMcpDeployTargetProcesses -Snapshot $allBefore -Bind $Bind -DbPath $DbPath -Invert)
    Info "Synapse process stop requested reason=$Reason before_all_count=$($allBefore.Count) target_count=$($before.Count) ignored_non_target_count=$($ignoredBefore.Count) bind=$Bind db=$DbPath"
    Info ("Synapse process stop target candidates:`n{0}" -f (Format-SynapseMcpProcessSnapshot -Snapshot $before))
    if ($ignoredBefore.Count -gt 0) {
        Info ("Synapse process stop ignored non-target processes:`n{0}" -f (Format-SynapseMcpProcessSnapshot -Snapshot $ignoredBefore))
    }
    if ($before.Count -eq 0) {
        Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds $TimeoutSeconds -ForceRestart:$ForceRestart
        return
    }

    foreach ($proc in $before) {
        $null = Assert-SynapseProcessStopTarget -SnapshotProcess $proc -Bind $Bind -DbPath $DbPath
    }

    $httpProcesses = @($before | Where-Object { $_.CommandLine -match '(?i)--mode\s+http' })
    $nonHttpProcesses = @($before | Where-Object { $_.CommandLine -notmatch '(?i)--mode\s+http' })
    if ($nonHttpProcesses.Count -gt 0 -and -not $ForceRestart) {
        Die ("SYNAPSE_GRACEFUL_SHUTDOWN_NON_HTTP_PROCESS reason={0} count={1}`nprocesses:`n{2}`nremediation=setup will not force-stop stdio/bridge/non-http Synapse processes without -ForceRestart; run synapse-mcp --mode doctor to inspect ownership, or coordinate a maintenance window before forcing exact verified PIDs. Do not close terminal windows." -f `
            $Reason,
            $nonHttpProcesses.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $nonHttpProcesses))
    }

    if ($httpProcesses.Count -gt 0) {
        if ($ForceRestart) {
            $liveTcpClients = @(Get-SynapseTcpClientSnapshot -Bind $Bind | Where-Object { $_.HasLivePeer })
            if ($liveTcpClients.Count -gt 0) {
                $targetPids = (($httpProcesses | ForEach-Object { [int]$_.ProcessId }) -join ',')
                Info ("FORCE_RESTART: SYNAPSE_FORCE_RESTART_LIVE_CLIENTS_GRACEFUL_FIRST reason={0} bind={1} target_pids={2} live_tcp_client_count={3}`ntcp_clients:`n{4}`nremediation=-ForceRestart is explicit maintenance. Setup still asks the HTTP daemon to shut down first so it can close accepted sockets cleanly; if Windows keeps dead-owner rows because client peers remain connected, setup closes only exact known non-terminal peers and then requires a normal bind probe before installing or starting anything." -f `
                    $Reason,
                    $Bind,
                    $targetPids,
                    $liveTcpClients.Count,
                    (Format-SynapseTcpClientSnapshot -Snapshot $liveTcpClients))
            }
        }

        $tokenRead = Read-SynapseSetupTokenForRestartGuard -TokenPath $TokenPath
        if (-not $tokenRead.Ok) {
            $message = ("{0} reason={1} process_count={2} {3} remediation=graceful shutdown requires the daemon bearer token; repair token state before setup, or use -ForceRestart only after manually verifying no held inputs and no live clients." -f `
                $tokenRead.Code,
                $Reason,
                $httpProcesses.Count,
                $tokenRead.Detail)
            if ($ForceRestart) {
                Info "FORCE_RESTART: $message"
            } else {
                Die $message
            }
        } else {
            $expectedPids = @($httpProcesses | ForEach-Object { [int]$_.ProcessId })
            if ($ForceRestart) {
                $null = Enter-SynapseChromeBridgeMaintenancePause `
                    -Bind $Bind `
                    -Token $tokenRead.Token `
                    -Reason $Reason `
                    -AllowUnacknowledgedChromeBridgePauseForRollback:$AllowUnacknowledgedChromeBridgePauseForRollback
            }
            $shutdown = Request-SynapseGracefulShutdown -Bind $Bind -Token $tokenRead.Token -ExpectedPids $expectedPids -Reason $Reason
            if (-not $shutdown.Ok) {
                $message = ("{0} reason={1} bind={2} error={3} response={4} remediation=the running daemon did not accept authenticated graceful shutdown; inspect daemon logs and token/bind state. Use -ForceRestart only for a coordinated legacy-runtime transition after manual input-state readback." -f `
                    $shutdown.Code,
                    $Reason,
                    $Bind,
                    $shutdown.Error,
                    ($(if ($null -eq $shutdown.Response) { '<none>' } else { $shutdown.Response | ConvertTo-Json -Compress -Depth 6 })))
                if ($ForceRestart) {
                    Info "FORCE_RESTART: $message"
                } else {
                    Die $message
                }
            } else {
                Info ("Synapse graceful shutdown requested reason={0} pid={1} active_sessions_before_shutdown={2}" -f `
                    $Reason,
                    $shutdown.Response.pid,
                    $shutdown.Response.active_sessions_before_shutdown)
            }
        }

        # --- #2100: stall-based exit wait -------------------------------------
        $waitStarted = Get-Date
        $hardDeadline = $waitStarted.AddSeconds([Math]::Max($TimeoutSeconds, $MaxWaitSeconds))
        $liveness = Get-SynapseDaemonLivenessSample -LogDir $LogDir -DbPath $DbPath
        $livenessAvailable = ($liveness.LogWriteTicks -gt 0) -or ($liveness.EndingAtUnixMs -gt 0)
        $lastProgressAt = $waitStarted
        $lastFingerprint = $liveness.Fingerprint
        $progressObservations = 0
        $extendedSeconds = 0
        $lastProgressLogAt = $waitStarted
        $exitWaitVerdict = 'pending'
        Info ("SYNAPSE_GRACEFUL_SHUTDOWN_EXIT_WAIT_START reason={0} base_timeout_s={1} stall_s={2} max_wait_s={3} liveness_available={4} log_path={5} ending_phase={6} remediation=the exit-wait escalates when the daemon stops producing evidence of progress, not when a constant expires (#2100)." -f `
            $Reason,
            $TimeoutSeconds,
            $StallSeconds,
            $MaxWaitSeconds,
            $livenessAvailable,
            $liveness.LogPath,
            $liveness.EndingPhase)

        do {
            Start-Sleep -Milliseconds 250
            $remainingHttpPids = @($httpProcesses | Where-Object {
                $pidValue = [int]$_.ProcessId
                $current = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
                $exeLeaf = if ($current -and $current.ExecutablePath) { Split-Path -Leaf $current.ExecutablePath } else { '' }
                $null -ne $current -and ((Test-SynapseMcpExecutableLeafName -Name $current.Name) -or (Test-SynapseMcpExecutableLeafName -Name $exeLeaf))
            })
            if ($remainingHttpPids.Count -eq 0) {
                $exitWaitVerdict = 'exited'
                $waitedMs = [int64]((Get-Date) - $waitStarted).TotalMilliseconds
                Info ("Synapse graceful shutdown verified reason={0} http_process_count=0 waited_ms={1} extended_s={2} progress_observations={3}" -f `
                    $Reason, $waitedMs, $extendedSeconds, $progressObservations)
                Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds $TimeoutSeconds -ForceRestart:$ForceRestart
                break
            }

            $now = Get-Date
            $liveness = Get-SynapseDaemonLivenessSample -LogDir $LogDir -DbPath $DbPath
            if ($liveness.Fingerprint -ne $lastFingerprint) {
                $lastFingerprint = $liveness.Fingerprint
                $lastProgressAt = $now
                $progressObservations += 1
            }
            $stalledSeconds = [int]($now - $lastProgressAt).TotalSeconds
            $elapsedSeconds = [int]($now - $waitStarted).TotalSeconds

            # The base budget is spent. From here the wait continues ONLY while
            # the daemon keeps proving it is doing something.
            if ($elapsedSeconds -ge $TimeoutSeconds) {
                if (-not $livenessAvailable) {
                    $exitWaitVerdict = 'no_liveness_witness'
                    break
                }
                if ($stalledSeconds -ge $StallSeconds) {
                    $exitWaitVerdict = 'stalled'
                    break
                }
                if ($now -ge $hardDeadline) {
                    $exitWaitVerdict = 'ceiling'
                    break
                }
                $extendedSeconds = $elapsedSeconds - $TimeoutSeconds
                if (($now - $lastProgressLogAt).TotalSeconds -ge 5) {
                    $lastProgressLogAt = $now
                    Info ("SYNAPSE_GRACEFUL_SHUTDOWN_EXIT_WAIT_EXTENDED reason={0} elapsed_s={1} extended_s={2} stalled_s={3} stall_budget_s={4} ceiling_s={5} progress_observations={6} ending_phase={7} log_path={8} log_bytes={9}" -f `
                        $Reason,
                        $elapsedSeconds,
                        $extendedSeconds,
                        $stalledSeconds,
                        $StallSeconds,
                        $MaxWaitSeconds,
                        $progressObservations,
                        $liveness.EndingPhase,
                        $liveness.LogPath,
                        $liveness.LogLength)
                }
            }
        } while ($true)

        $remainingHttpPids = @($httpProcesses | Where-Object {
            $pidValue = [int]$_.ProcessId
            $current = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
            $exeLeaf = if ($current -and $current.ExecutablePath) { Split-Path -Leaf $current.ExecutablePath } else { '' }
            $null -ne $current -and ((Test-SynapseMcpExecutableLeafName -Name $current.Name) -or (Test-SynapseMcpExecutableLeafName -Name $exeLeaf))
        })
        if ($remainingHttpPids.Count -gt 0) {
            $waitedMs = [int64]((Get-Date) - $waitStarted).TotalMilliseconds
            $liveness = Get-SynapseDaemonLivenessSample -LogDir $LogDir -DbPath $DbPath
            # The verdict names WHY the wait ended, which is the whole point: a
            # kill after `stalled` is a kill of a daemon that had stopped, and a
            # kill after `no_liveness_witness` says the drain could not tell and
            # fell back to the wall clock. #2100's escalation reported neither.
            $message = ("SYNAPSE_GRACEFUL_SHUTDOWN_TIMEOUT reason={0} verdict={1} base_timeout_s={2} waited_ms={3} stall_budget_s={4} ceiling_s={5} progress_observations={6} liveness_available={7} log_path={8} log_bytes={9} ending_phase={10} ending_at_unix_ms={11} remaining_count={12}`nremaining:`n{13}" -f `
                $Reason,
                $exitWaitVerdict,
                $TimeoutSeconds,
                $waitedMs,
                $StallSeconds,
                $MaxWaitSeconds,
                $progressObservations,
                $livenessAvailable,
                $liveness.LogPath,
                $liveness.LogLength,
                $liveness.EndingPhase,
                $liveness.EndingAtUnixMs,
                $remainingHttpPids.Count,
                (Format-SynapseMcpProcessSnapshot -Snapshot $remainingHttpPids))
            if ($ForceRestart) {
                Info "FORCE_RESTART: $message"
            } elseif ($EscalateAfterGracefulTimeout) {
                Info "GRACEFUL_TIMEOUT_ESCALATION: $message"
                Info "GRACEFUL_TIMEOUT_ESCALATION: escalating to identity-verified exact-PID force stop reason=$Reason (the daemon has closed its listener and retained-owner evidence is preserved in the daemon log)"
            } else {
                Die $message
            }
        }
    }

    $remaining = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
    if ($remaining.Count -eq 0) {
        Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds $TimeoutSeconds -ForceRestart:$ForceRestart
        $ignoredAfter = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath -Invert)
        Info "Synapse process stop verified reason=$Reason target_after_count=0 ignored_non_target_after_count=$($ignoredAfter.Count)"
        return
    }

    if (-not $ForceRestart -and -not $EscalateAfterGracefulTimeout) {
        Die ("SYNAPSE_PROCESS_STOP_INCOMPLETE reason={0} remaining_count={1} remaining=`n{2}" -f `
            $Reason,
            $remaining.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $remaining))
    }

    Info ("FORCE_RESTART: exact-PID stop for remaining verified Synapse processes reason={0} remaining_count={1}`nremaining:`n{2}" -f `
        $Reason,
        $remaining.Count,
        (Format-SynapseMcpProcessSnapshot -Snapshot $remaining))
    foreach ($proc in $remaining) {
        $verified = Assert-SynapseProcessStopTarget -SnapshotProcess $proc -Bind $Bind -DbPath $DbPath
        if (-not $verified) { continue }
        $pidValue = [int]$verified.ProcessId
        # Capture the exact verified daemon identity BEFORE the stop attempt so a
        # readback can distinguish a natural exit / terminating-image ghost / PID
        # reuse from a still-live verified daemon.  CreationDate (the OS process
        # start time) is the canonical PID-reuse discriminator: a reused PID is
        # always created strictly later than the process we verified.
        $verifiedCreationDate = $verified.CreationDate
        $verifiedExecutablePath = [string]$verified.ExecutablePath
        $verifiedExecutablePathNorm = Normalize-SynapseSetupPathForCompare -Path $verifiedExecutablePath
        $verifiedCreationText = if ($verifiedCreationDate) { $verifiedCreationDate.ToString('o') } else { '<unknown>' }
        try {
            Stop-Process -Id $pidValue -Force -ErrorAction Stop
            Info "Synapse process exact-PID force stop issued pid=$pidValue reason=$Reason match_rules=$($verified.DeployTargetRules) path=$($verified.ExecutablePath) cmd=$($verified.CommandLine)"
        } catch {
            # The verified process can exit between the CIM ownership snapshot and
            # Stop-Process (e.g. Stop-Process returns "Cannot find a process with
            # the process identifier N").  Resolve that race by re-reading the exact
            # PID identity: accept success ONLY when the exact verified identity is
            # confirmed absent (PID fully gone, or the PID is now held by a
            # different process = reuse, which proves the original exited).  A live
            # or terminating-image process that still matches the verified identity
            # remains a hard stop failure and a reused live PID is never stopped
            # from stale data.
            $stopError = ($_.Exception.Message -replace '\s+', ' ').Trim()
            $afterStopError = Get-Process -Id $pidValue -ErrorAction SilentlyContinue
            # Bounded reread window (race resolution, not a stop retry): give a
            # terminating image time to clear from Win32_Process before deciding.
            $cimDeadline = (Get-Date).AddSeconds(2)
            do {
                $afterStopErrorCim = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
                if (-not $afterStopErrorCim) { break }
                Start-Sleep -Milliseconds 100
            } while ((Get-Date) -lt $cimDeadline)
            $getProcessPresence = if ($afterStopError) { 'present' } else { 'absent' }

            if (-not $afterStopErrorCim) {
                if (-not $afterStopError) {
                    # Exact verified PID is fully gone from both process tables.
                    Info ("Synapse process exact-PID stop already satisfied pid={0} reason={1} stop_error={2} readback=Get-Process:absent,Win32_Process:absent verified_creation={3} identity=verified-target-gone" -f `
                        $pidValue, $Reason, $stopError, $verifiedCreationText)
                    continue
                }
                # Get-Process still lists the PID but Win32_Process has no row: the
                # identity cannot be re-proven, so fail closed rather than assume exit.
                Die ("SYNAPSE_PROCESS_STOP_FAILED pid={0} reason={1} error={2} readback=Get-Process:present,Win32_Process:absent verified_creation={3} remediation=the exact verified PID is still listed by Get-Process while Win32_Process reports no row; setup cannot re-prove the verified identity is gone, refuses to assume exit, and only stops verified synapse-mcp.exe PIDs; inspect process ownership and retry after the daemon exits" -f `
                    $pidValue, $Reason, $stopError, $verifiedCreationText)
            }

            # A Win32_Process row exists for the PID.  Compare its identity to the
            # exact daemon we verified before the stop attempt.
            $observedCreationDate = $afterStopErrorCim.CreationDate
            $observedCreationText = if ($observedCreationDate) { $observedCreationDate.ToString('o') } else { '<unknown>' }
            $observedExecutablePath = [string]$afterStopErrorCim.ExecutablePath
            $observedExecutablePathNorm = Normalize-SynapseSetupPathForCompare -Path $observedExecutablePath
            $identityMatches = ($null -ne $verifiedCreationDate) -and ($null -ne $observedCreationDate) -and `
                ($observedCreationDate -eq $verifiedCreationDate) -and `
                ($observedExecutablePathNorm -ieq $verifiedExecutablePathNorm)

            if (-not $identityMatches) {
                # PID reuse: the PID now (or still) belongs to a different process
                # image / start time, which proves the exact verified daemon exited.
                # The reused PID may be a live unrelated process and must NEVER be
                # stopped from stale data, nor counted as our daemon still running.
                Info ("Synapse process exact-PID stop already satisfied pid={0} reason={1} stop_error={2} readback=Get-Process:{3},Win32_Process:present identity=pid-reused verified_creation={4} observed_creation={5} verified_path={6} observed_path={7} observed_name={8} => exact verified daemon absent; leaving reused PID untouched" -f `
                    $pidValue, $Reason, $stopError, $getProcessPresence, $verifiedCreationText, $observedCreationText, $verifiedExecutablePath, $observedExecutablePath, $afterStopErrorCim.Name)
                continue
            }

            # The Win32_Process row matches the exact verified identity.
            if (-not $afterStopError) {
                Die ("SYNAPSE_PROCESS_TERMINATING_IMAGE_STILL_MAPPED pid={0} reason={1} path={2} command={3} stop_error={4} readback=Get-Process:absent,Win32_Process:present identity=verified-target-terminating verified_creation={5} observed_creation={6} remediation=the exact verified daemon is absent from Get-Process but remains in Win32_Process/tasklist with a matching start time and image path and may keep its executable image mapped; inspect the named PID and image lock, do not report it as stopped, and use a content-addressed executable handoff or restart Windows only in a coordinated maintenance window" -f `
                    $pidValue,
                    $Reason,
                    $observedExecutablePath,
                    $afterStopErrorCim.CommandLine,
                    $stopError,
                    $verifiedCreationText,
                    $observedCreationText)
            }
            Die ("SYNAPSE_PROCESS_STOP_FAILED pid={0} reason={1} error={2} readback=Get-Process:present,Win32_Process:present identity=verified-target-live verified_creation={3} observed_creation={4} path={5} remediation=the exact verified daemon is still live with a matching start time and image path; setup only stops verified synapse-mcp.exe PIDs; inspect process ownership and retry after the daemon exits" -f `
                $pidValue,
                $Reason,
                $stopError,
                $verifiedCreationText,
                $observedCreationText,
                $observedExecutablePath)
        }
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        Start-Sleep -Milliseconds 250
        $after = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
        if ($after.Count -eq 0) {
            Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds $TimeoutSeconds -ForceRestart:$ForceRestart
            $ignoredAfter = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath -Invert)
            Info "Synapse process stop verified reason=$Reason target_after_count=0 ignored_non_target_after_count=$($ignoredAfter.Count)"
            return
        }
    } while ((Get-Date) -lt $deadline)

    $remaining = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
    Die ("SYNAPSE_PROCESS_STOP_FAILED reason={0} timeout_s={1} remaining_count={2} remaining=`n{3}" -f `
        $Reason, $TimeoutSeconds, $remaining.Count, (Format-SynapseMcpProcessSnapshot -Snapshot $remaining))
}

function Get-SynapseDaemonSupervisorProcessSnapshot {
    param([Parameter(Mandatory=$true)][string]$SupervisorPath)

    $resolvedSupervisorPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($SupervisorPath)
    @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object {
            $name = [string]$_.Name
            $commandLine = [string]$_.CommandLine
            ($name -ieq 'powershell.exe' -or $name -ieq 'pwsh.exe') -and
                ($commandLine.IndexOf('-File', [System.StringComparison]::OrdinalIgnoreCase) -ge 0) -and
                ($commandLine.IndexOf($resolvedSupervisorPath, [System.StringComparison]::OrdinalIgnoreCase) -ge 0)
        } |
        Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,CommandLine)
}

function Format-SynapseDaemonSupervisorProcessSnapshot {
    param([AllowNull()][object[]]$Snapshot)

    if (-not $Snapshot -or $Snapshot.Count -eq 0) {
        return '<none>'
    }
    return (($Snapshot | ForEach-Object {
        "pid=$($_.ProcessId) ppid=$($_.ParentProcessId) name=$($_.Name) path=$($_.ExecutablePath) cmd=$($_.CommandLine)"
    }) -join "`n")
}

# #2092: when a drain times out waiting for the supervisor to park, "it did not
# park" is not a diagnosis. The supervisor's own last ledger line says whether it
# never saw the stop-request, saw it and threw, or is sitting in a restart
# backoff -- three different remediations. Read-only and never fatal: a missing
# or unreadable ledger is reported as such, because this runs on a path that is
# already failing.
function Get-SynapseDaemonSupervisorLastEventText {
    param([Parameter(Mandatory=$true)][string]$LogDir)

    $eventsPath = Join-Path $LogDir 'daemon-supervisor-events.jsonl'
    if (-not (Test-Path -LiteralPath $eventsPath -PathType Leaf)) {
        return "<missing:$eventsPath>"
    }
    try {
        $last = Get-Content -LiteralPath $eventsPath -Tail 1 -ErrorAction Stop
        if ([string]::IsNullOrWhiteSpace($last)) { return "<empty:$eventsPath>" }
        return ($last -replace '\s+', ' ').Trim()
    } catch {
        return "<unreadable:$eventsPath error=$($_.Exception.Message)>"
    }
}

function Stop-SynapseDaemonSupervisorProcessesForInstallHandoff {
    param(
        [Parameter(Mandatory=$true)][string]$SupervisorPath,
        [int]$TimeoutSeconds = 20
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $before = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $SupervisorPath)
    if ($before.Count -eq 0) {
        Info "Synapse daemon supervisor stop not needed before binary handoff: supervisor_path=$SupervisorPath"
        return
    }

    Info ("Synapse daemon supervisor exact-path stop requested before binary handoff count={0}`nsupervisors:`n{1}" -f `
        $before.Count,
        (Format-SynapseDaemonSupervisorProcessSnapshot -Snapshot $before))

    foreach ($proc in $before) {
        $pidValue = [int]$proc.ProcessId
        if ($pidValue -eq $PID) {
            Die "SYNAPSE_SUPERVISOR_STOP_REFUSED_SELF pid=$pidValue supervisor_path=$SupervisorPath remediation=setup refused to stop its own process while preparing daemon handoff"
        }
        $current = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
        if (-not $current) {
            continue
        }
        $currentCommand = [string]$current.CommandLine
        if ($currentCommand.IndexOf('-File', [System.StringComparison]::OrdinalIgnoreCase) -lt 0 -or
            $currentCommand.IndexOf($SupervisorPath, [System.StringComparison]::OrdinalIgnoreCase) -lt 0) {
            Die ("SYNAPSE_SUPERVISOR_STOP_TARGET_MISMATCH pid={0} supervisor_path={1} actual_name={2} actual_path={3} actual_command={4} remediation=PID was reused or is not the setup-owned hidden Synapse daemon supervisor; refusing protected-shell stop" -f `
                $pidValue,
                $SupervisorPath,
                $current.Name,
                $current.ExecutablePath,
                $current.CommandLine)
        }
        try {
            Stop-Process -Id $pidValue -Force -ErrorAction Stop
            Info "Synapse daemon supervisor exact-PID stop issued pid=$pidValue supervisor_path=$SupervisorPath"
        } catch {
            Die "SYNAPSE_SUPERVISOR_STOP_FAILED pid=$pidValue supervisor_path=$SupervisorPath error=$($_.Exception.Message) remediation=setup only stops the exact hidden daemon supervisor launched from synapse-daemon-supervisor.ps1; inspect the task/process SoT before retrying"
        }
    }

    do {
        Start-Sleep -Milliseconds 250
        $after = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $SupervisorPath)
        if ($after.Count -eq 0) {
            Info "Synapse daemon supervisor stop verified before binary handoff supervisor_path=$SupervisorPath"
            return
        }
    } while ((Get-Date) -lt $deadline)

    $remaining = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $SupervisorPath)
    Die ("SYNAPSE_SUPERVISOR_STOP_TIMEOUT supervisor_path={0} timeout_s={1} remaining_count={2}`nremaining:`n{3}`nremediation=the scheduled-task hidden supervisor is still running and can relaunch the installed daemon during binary replacement; inspect Task Scheduler and process ownership before retrying" -f `
        $SupervisorPath,
        $TimeoutSeconds,
        $remaining.Count,
        (Format-SynapseDaemonSupervisorProcessSnapshot -Snapshot $remaining))
}

function Assert-SynapseDaemonTaskRestartAuthorityIdentity {
    param(
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$SupervisorPath,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $task) {
        Info "Synapse daemon scheduled task identity preflight: task=$TaskName reason=$Reason task_present=false"
        return $null
    }

    # The launcher directory follows the supervisor. #1862 moved both out of the
    # log directory, so a machine installed before that change still has a task
    # registered against the LEGACY log-dir launcher. That legacy layout is a
    # recognized setup-owned identity during upgrade -- refusing it would make
    # the fix un-installable on exactly the machines that need it. It is accepted
    # for ownership purposes only; section 7 then re-registers the task at the
    # new location and deletes the legacy copies.
    $launcherDir = Split-Path -Parent $SupervisorPath
    $expectedExecutablePath = Join-Path $env:SystemRoot 'System32\wscript.exe'
    $candidateLauncherDirs = @($launcherDir)
    $legacyLauncherDir = $LogDir
    if (-not [string]::IsNullOrWhiteSpace($legacyLauncherDir) -and
        ([System.IO.Path]::GetFullPath($legacyLauncherDir).TrimEnd('\') -ine [System.IO.Path]::GetFullPath($launcherDir).TrimEnd('\'))) {
        $candidateLauncherDirs += $legacyLauncherDir
    }

    $actions = @($task.Actions)
    if ($actions.Count -ne 1) {
        Die "SYNAPSE_TASK_HANDOFF_IDENTITY_MISMATCH task=$TaskName reason=$Reason expected_action_count=1 actual_action_count=$($actions.Count) remediation=the named task is not the single setup-owned Synapse daemon launcher; inspect the Task Scheduler action SoT and remove only the verified owner"
    }

    $action = $actions[0]
    $actualExecutablePath = try {
        [System.IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables([string]$action.Execute))
    } catch {
        [string]$action.Execute
    }
    $actualWorkingDirectory = try {
        [System.IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables([string]$action.WorkingDirectory)).TrimEnd('\')
    } catch {
        [string]$action.WorkingDirectory
    }

    $matchedLauncherDir = $null
    foreach ($candidateDir in $candidateLauncherDirs) {
        $candidateLauncher = Join-Path $candidateDir 'synapse-daemon-launch-hidden.vbs'
        $candidateArguments = '//B //Nologo "{0}"' -f $candidateLauncher
        $candidateWorkingDirectory = [System.IO.Path]::GetFullPath($candidateDir).TrimEnd('\')
        if ($actualExecutablePath -ieq [System.IO.Path]::GetFullPath($expectedExecutablePath) -and
            ([string]$action.Arguments -ceq $candidateArguments) -and
            $actualWorkingDirectory -ieq $candidateWorkingDirectory) {
            $matchedLauncherDir = $candidateWorkingDirectory
            break
        }
    }

    if (-not $matchedLauncherDir) {
        $expectedRendering = (@($candidateLauncherDirs | ForEach-Object { '//B //Nologo "{0}"' -f (Join-Path $_ 'synapse-daemon-launch-hidden.vbs') }) -join ' | ')
        Die ("SYNAPSE_TASK_HANDOFF_IDENTITY_MISMATCH task={0} reason={1} expected_execute={2} actual_execute={3} accepted_arguments={4} actual_arguments={5} accepted_working_directories={6} actual_working_directory={7} remediation=the named task action is not the exact setup-owned Synapse hidden launcher (current or pre-#1862 legacy layout); inspect Task Scheduler and refuse to stop or unregister an unverified task" -f `
            $TaskName,
            $Reason,
            $expectedExecutablePath,
            $action.Execute,
            $expectedRendering,
            $action.Arguments,
            (@($candidateLauncherDirs) -join ' | '),
            $action.WorkingDirectory)
    }

    $isLegacyLayout = ($matchedLauncherDir -ine [System.IO.Path]::GetFullPath($launcherDir).TrimEnd('\'))
    if ($isLegacyLayout) {
        Warn "SYNAPSE_TASK_HANDOFF_LEGACY_LAUNCHER_LAYOUT task=$TaskName reason=$Reason matched_launcher_dir=$matchedLauncherDir current_launcher_dir=$([System.IO.Path]::GetFullPath($launcherDir).TrimEnd('\')) effect=ownership accepted for this upgrade; the task will be re-registered against the runtime bin directory so log cleanup can no longer break autostart (#1862)"
    }
    Info "Synapse daemon scheduled task identity preflight verified: task=$TaskName state=$($task.State) reason=$Reason matched_launcher_dir=$matchedLauncherDir legacy_layout=$isLegacyLayout"
    return $task
}

function Assert-SynapseLiveDaemonAdoptionIdentity {
    param(
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$HiddenLauncherPath,
        [Parameter(Mandatory=$true)][string]$SupervisorPath,
        [Parameter(Mandatory=$true)][string]$ExePath,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$ProfilesDir,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [Parameter(Mandatory=$true)][string]$MaintenanceLockPath,
        [bool]$EnableAudio,
        [AllowNull()][string]$AllowedPermissions,
        [AllowNull()][string]$CalyxConfigPath
    )

    $task = Assert-SynapseDaemonTaskRestartAuthorityIdentity `
        -TaskName $TaskName `
        -SupervisorPath $SupervisorPath `
        -Reason 'live_adoption'
    if ($null -eq $task) {
        Die "SYNAPSE_LIVE_ADOPTION_TASK_MISSING task=$TaskName remediation=the unchanged live daemon has no verified Task Scheduler restart authority; use an explicit handoff instead of silently adopting incomplete ownership"
    }
    if ([string]$task.State -ne 'Running') {
        Die "SYNAPSE_LIVE_ADOPTION_TASK_STATE_INVALID task=$TaskName expected=Running actual=$($task.State) remediation=the unchanged daemon is not owned by a running setup task; use an explicit handoff to restore one authoritative supervisor"
    }
    foreach ($requiredFile in @($HiddenLauncherPath, $SupervisorPath)) {
        if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
            Die "SYNAPSE_LIVE_ADOPTION_LAUNCHER_FILE_MISSING task=$TaskName path=$requiredFile remediation=the running task launcher chain is incomplete; use an explicit handoff to regenerate it safely"
        }
    }

    $powerShellExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $launcherLog = Join-Path $LogDir 'daemon-launcher.log'
    $supervisorCommand = @(
        (Quote-WindowsCommandArgument $powerShellExe),
        '-NoProfile',
        '-ExecutionPolicy', 'Bypass',
        '-File', (Quote-WindowsCommandArgument $SupervisorPath)
    ) -join ' '
    $expectedWrapperAssignments = @(
        "launcherLog = $(Vbs-Literal $launcherLog)",
        "supervisorCommand = $(Vbs-Literal $supervisorCommand)"
    )
    $wrapperLines = @(Get-Content -LiteralPath $HiddenLauncherPath -ErrorAction Stop)
    foreach ($expectedLine in $expectedWrapperAssignments) {
        $assignmentName = ($expectedLine -split '\s*=\s*', 2)[0]
        $actualLines = @($wrapperLines | Where-Object { $_ -match "^$([regex]::Escape($assignmentName))\s*=" })
        if ($actualLines.Count -ne 1 -or [string]$actualLines[0] -cne $expectedLine) {
            Die "SYNAPSE_LIVE_ADOPTION_WRAPPER_DRIFT task=$TaskName path=$HiddenLauncherPath assignment=$assignmentName expected=[$expectedLine] actual=[$($actualLines -join ' || ')] remediation=the running task wrapper does not name the exact expected supervisor/log; use an explicit handoff instead of overwriting a live launcher"
        }
    }

    $requiredAssignments = @(
        'ExePath',
        'Bind',
        'DbPath',
        'ProfilesDir',
        'DaemonLogDir',
        'TokenPath',
        'LauncherLog',
        'SupervisorState',
        'SupervisorEvents',
        'MaintenanceLockPath',
        'ExpectedCalyxConfigPath',
        'DaemonArgumentText',
        'ExpectedAllowedPermissions',
        'ExpectedEnableAudio'
    )
    $actualAssignments = @{}
    $assignmentPattern = '^\$(?<name>[A-Za-z][A-Za-z0-9]*)\s*=\s*''(?<value>(?:''''|[^''])*)''\s*$'
    foreach ($line in @(Get-Content -LiteralPath $SupervisorPath -ErrorAction Stop)) {
        if ($line -match $assignmentPattern -and
            $requiredAssignments -contains $Matches.name) {
            if ($actualAssignments.ContainsKey($Matches.name)) {
                Die "SYNAPSE_LIVE_ADOPTION_SUPERVISOR_ASSIGNMENT_DUPLICATE task=$TaskName path=$SupervisorPath assignment=$($Matches.name) remediation=the persisted supervisor is structurally ambiguous; use an explicit handoff to regenerate it"
            }
            $actualAssignments[$Matches.name] = $Matches.value.Replace("''", "'")
        }
    }
    $supervisorStatePath = Join-Path $LogDir 'daemon-supervisor-current.json'
    $supervisorEventsPath = Join-Path $LogDir 'daemon-supervisor-events.jsonl'
    $expectedAllowed = Normalize-SynapseAllowedPermissionsArgument -Value $AllowedPermissions
    $expectedCalyxConfig = if ([string]::IsNullOrWhiteSpace($CalyxConfigPath)) { '' } else { $CalyxConfigPath }
    $expectedAssignments = [ordered]@{
        ExePath = $ExePath
        Bind = $Bind
        DbPath = $DbPath
        ProfilesDir = $ProfilesDir
        DaemonLogDir = $LogDir
        TokenPath = $TokenPath
        LauncherLog = $launcherLog
        SupervisorState = $supervisorStatePath
        SupervisorEvents = $supervisorEventsPath
        MaintenanceLockPath = $MaintenanceLockPath
        ExpectedCalyxConfigPath = $expectedCalyxConfig
        DaemonArgumentText = Get-SynapseDaemonArgumentText -Bind $Bind -DbPath $DbPath -ProfilesDir $ProfilesDir -EnableAudio $EnableAudio -AllowedPermissions $AllowedPermissions -CalyxConfigPath $CalyxConfigPath
        ExpectedAllowedPermissions = $expectedAllowed
        ExpectedEnableAudio = [string]$EnableAudio
    }
    $assignmentDrift = [System.Collections.Generic.List[string]]::new()
    foreach ($name in $requiredAssignments) {
        $actual = if ($actualAssignments.ContainsKey($name)) { [string]$actualAssignments[$name] } else { '<missing>' }
        $expected = [string]$expectedAssignments[$name]
        if ($actual -cne $expected) {
            $assignmentDrift.Add("$name expected=[$expected] actual=[$actual]")
        }
    }
    if ($assignmentDrift.Count -gt 0) {
        Die "SYNAPSE_LIVE_ADOPTION_SUPERVISOR_CONFIG_DRIFT task=$TaskName path=$SupervisorPath drift=$($assignmentDrift -join '; ') remediation=the persisted supervisor would launch different state on restart; use an explicit handoff instead of adopting it"
    }

    try {
        $supervisorState = Get-Content -LiteralPath $supervisorStatePath -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
    } catch {
        Die "SYNAPSE_LIVE_ADOPTION_SUPERVISOR_STATE_UNREADABLE task=$TaskName path=$supervisorStatePath error=$($_.Exception.Message) remediation=repair the supervisor state Source of Truth before live adoption"
    }
    $listeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
    if ($listeners.Count -ne 1) {
        Die "SYNAPSE_LIVE_ADOPTION_LISTENER_AMBIGUOUS task=$TaskName bind=$Bind listeners=$(Format-SynapseTcpBindListenerSnapshot -Snapshot $listeners) remediation=live adoption requires exactly one listener owned by the persisted supervisor child"
    }
    $listenerPid = [int]$listeners[0].OwningProcess
    if ([string]$supervisorState.state -notin @('running', 'adopted_existing') -or
        [int]$supervisorState.child_pid -ne $listenerPid) {
        Die "SYNAPSE_LIVE_ADOPTION_SUPERVISOR_STATE_MISMATCH task=$TaskName state=$($supervisorState.state) supervisor_child_pid=$($supervisorState.child_pid) listener_pid=$listenerPid remediation=repair supervisor/daemon ownership before live adoption"
    }
    $supervisors = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $SupervisorPath)
    if ($supervisors.Count -ne 1 -or [int]$supervisors[0].ProcessId -ne [int]$supervisorState.supervisor_pid) {
        Die "SYNAPSE_LIVE_ADOPTION_SUPERVISOR_PROCESS_MISMATCH task=$TaskName expected_pid=$($supervisorState.supervisor_pid) actual_count=$($supervisors.Count) actual=$(Format-SynapseDaemonSupervisorProcessSnapshot -Snapshot $supervisors) remediation=live adoption requires one exact supervisor process matching persisted state"
    }
    try {
        $taskXml = Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    } catch {
        Die "SYNAPSE_LIVE_ADOPTION_TASK_EXPORT_FAILED task=$TaskName error=$($_.Exception.Message) remediation=repair Task Scheduler read access before live adoption"
    }

    return [pscustomobject]@{
        TaskState = [string]$task.State
        TaskDefinitionSha256 = Get-SynapseSha256Hex -Text ([string]$taskXml)
        HiddenLauncherSha256 = Get-SynapseFileSha256 -Path $HiddenLauncherPath
        SupervisorSha256 = Get-SynapseFileSha256 -Path $SupervisorPath
        SupervisorPid = [int]$supervisorState.supervisor_pid
        DaemonPid = $listenerPid
        SupervisorState = [string]$supervisorState.state
        DaemonArgumentText = [string]$expectedAssignments.DaemonArgumentText
    }
}

# Task Scheduler's default Priority is 7, which Windows maps to
# BELOW_NORMAL_PRIORITY_CLASS + THREAD_PRIORITY_BELOW_NORMAL, and priority class
# is inherited all the way down the launch chain (task -> wscript wrapper ->
# supervisor -> synapse-mcp.exe). The daemon serves interactive MCP tool calls,
# so it belongs in the band Microsoft designates for interactive tasks:
#
#   4, 5, 6 -> NORMAL_PRIORITY_CLASS      "used for interactive tasks"
#   7, 8    -> BELOW_NORMAL_PRIORITY_CLASS "used for background tasks"
#   https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings-priority
#
# 5 (the middle of the NORMAL band), not 1/2/3: AboveNormal or High would let the
# daemon contend with the user's own foreground application, which is the
# opposite failure. Normal makes it an equal, not a winner.
#
# Measured on this host (i7-1355U, 6 competing Normal-priority CPU burners on 12
# logical CPUs, identical query and corpus, A/B/A on the same process):
#   BelowNormal  p95 = 965 / 319 / 463 / 428 ms
#   Normal       p95 = 227 / 255 ms
# The median moves little; the TAIL is where a deprioritized daemon is felt,
# which is the signature of priority inversion rather than of slow work (#1910).
$script:SynapseDaemonTaskPriority = 5

function Assert-SynapseDaemonTaskPriority {
    <#
    .SYNOPSIS
    Converges the daemon task's Priority onto the interactive band, on the
    adoption path as well as the registration path.

    .DESCRIPTION
    Setting the value only at Register-ScheduledTask is not enough: setup adopts
    an already-running task whenever one is healthy, and never re-registers it.
    A host installed before this invariant existed would therefore keep
    BELOW_NORMAL forever across every redeploy -- which is exactly how #1910 was
    found. This reads the live value, repairs it when wrong, and re-reads to
    prove the repair landed rather than trusting Set-ScheduledTask's return.
    #>
    param(
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$Phase
    )

    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $task) {
        Die "SYNAPSE_DAEMON_TASK_PRIORITY_TASK_MISSING task=$TaskName phase=$Phase remediation=register the daemon task before asserting its priority"
    }

    $observed = [int]$task.Settings.Priority
    if ($observed -eq $script:SynapseDaemonTaskPriority) {
        Info "SYNAPSE_DAEMON_TASK_PRIORITY_OK task=$TaskName phase=$Phase priority=$observed priority_class=NORMAL remediation=none"
        return
    }

    Info "SYNAPSE_DAEMON_TASK_PRIORITY_REPAIRING task=$TaskName phase=$Phase observed=$observed expected=$($script:SynapseDaemonTaskPriority) reason=the daemon serves interactive MCP calls and must not run in the background priority band"
    try {
        $task.Settings.Priority = $script:SynapseDaemonTaskPriority
        Set-ScheduledTask -TaskName $TaskName -Settings $task.Settings -ErrorAction Stop | Out-Null
    } catch {
        Die "SYNAPSE_DAEMON_TASK_PRIORITY_REPAIR_FAILED task=$TaskName phase=$Phase observed=$observed expected=$($script:SynapseDaemonTaskPriority) error=$($_.Exception.Message) remediation=grant Task Scheduler write access for this task and rerun setup"
    }

    # Read back from Task Scheduler, not from the object we just mutated.
    $after = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $readback = if ($after) { [int]$after.Settings.Priority } else { -1 }
    if ($readback -ne $script:SynapseDaemonTaskPriority) {
        Die "SYNAPSE_DAEMON_TASK_PRIORITY_REPAIR_NOT_DURABLE task=$TaskName phase=$Phase expected=$($script:SynapseDaemonTaskPriority) readback=$readback remediation=Task Scheduler accepted the write but did not persist it; inspect the task definition and rerun setup"
    }
    Info "SYNAPSE_DAEMON_TASK_PRIORITY_REPAIRED task=$TaskName phase=$Phase priority=$readback priority_class=NORMAL remediation=none; the repair takes effect on the next daemon start"
}

function Remove-SynapseDaemonTaskRestartAuthority {
    param(
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$SupervisorPath,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    $task = Assert-SynapseDaemonTaskRestartAuthorityIdentity `
        -TaskName $TaskName `
        -SupervisorPath $SupervisorPath `
        -Reason $Reason
    if (-not $task) {
        Info "Synapse daemon scheduled task already absent before restart-authority removal: task=$TaskName reason=$Reason"
        Stop-SynapseDaemonSupervisorProcessesForInstallHandoff -SupervisorPath $SupervisorPath
        return
    }

    Info "Removing Synapse daemon Task Scheduler restart authority before process drain: task=$TaskName state=$($task.State) reason=$Reason"
    try {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction Stop
    } catch {
        Die "SYNAPSE_TASK_HANDOFF_UNREGISTER_FAILED task=$TaskName reason=$Reason error=$($_.Exception.Message) remediation=setup must revoke Task Scheduler restart-on-failure authority before stopping the hidden supervisor or replacing the daemon binary"
    }

    $readback = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($readback) {
        Die "SYNAPSE_TASK_HANDOFF_UNREGISTER_READBACK_FAILED task=$TaskName reason=$Reason state=$($readback.State) remediation=Task Scheduler still exposes restart authority after unregister; do not stop the supervisor or replace the daemon binary"
    }
    Info "Synapse daemon Task Scheduler restart authority removal verified: task=$TaskName reason=$Reason task_present=false"
    Stop-SynapseDaemonSupervisorProcessesForInstallHandoff -SupervisorPath $SupervisorPath
}

# ---------------------------------------------------------------------------
# #2083 -- reversible Task Scheduler restart-authority revocation.
#
# Remove-SynapseDaemonTaskRestartAuthority above UNREGISTERS the task, which is
# right for the deploy path (section 7 re-registers it from the freshly written
# launcher) and for -Remove (uninstall). It is wrong for an operator stop: a
# stop must be undoable by -Start alone, without rebuilding the task from a
# source checkout. Disable+Stop is the reversible form of the same revocation,
# and it is verified by the same identity assertion.
#
# Why the disable is required at all, and why `Stop-ScheduledTask` on its own is
# not: Task Scheduler stops the task *instance* it launched (wscript.exe); it
# does not walk the descendant tree, so the wscript -> powershell supervisor ->
# synapse-mcp.exe chain is only partially torn down and the supervisor is
# orphaned with its restart loop intact. This is documented Windows behaviour,
# not a Synapse quirk -- see the Stop-ScheduledTask reference ("Stops all
# running instances of a task") and Microsoft's own Q&A answer that Task
# Scheduler "stops the task container, but it does not forcibly terminate child
# processes". The only in-box mechanism that would give tree semantics is a Job
# Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, and Synapse cannot use it here
# because Task Scheduler owns the task's job, not setup.
# ---------------------------------------------------------------------------
function Suspend-SynapseDaemonTaskRestartAuthority {
    param(
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$SupervisorPath,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    $task = Assert-SynapseDaemonTaskRestartAuthorityIdentity `
        -TaskName $TaskName `
        -SupervisorPath $SupervisorPath `
        -Reason $Reason
    if (-not $task) {
        Info "Synapse daemon scheduled task already absent before restart-authority suspend: task=$TaskName reason=$Reason task_present=false"
        return $null
    }

    Info "Suspending Synapse daemon Task Scheduler restart authority: task=$TaskName state_before=$($task.State) reason=$Reason"
    if ([string]$task.State -eq 'Running') {
        try {
            Stop-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
        } catch {
            Die "SYNAPSE_TASK_STOP_FAILED task=$TaskName reason=$Reason error=$($_.Exception.Message) remediation=the running task instance must be stopped before the daemon is drained; inspect Task Scheduler state"
        }
        Info "Synapse daemon scheduled task instance stop issued: task=$TaskName reason=$Reason note=this stops the task container only; the hidden supervisor is parked separately because Task Scheduler does not terminate descendants"
    } else {
        Info "Synapse daemon scheduled task instance stop not needed: task=$TaskName reason=$Reason state=$($task.State)"
    }
    try {
        Disable-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
    } catch {
        Die "SYNAPSE_TASK_DISABLE_FAILED task=$TaskName reason=$Reason error=$($_.Exception.Message) remediation=setup refuses to drain the daemon while its logon trigger can relaunch a new supervisor; inspect Task Scheduler permissions"
    }

    $readback = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $readback) {
        Die "SYNAPSE_TASK_SUSPEND_READBACK_MISSING task=$TaskName reason=$Reason remediation=the scheduled task disappeared during a reversible suspend; rerun full setup to restore autostart"
    }
    if ([string]$readback.State -ne 'Disabled') {
        Die "SYNAPSE_TASK_SUSPEND_READBACK_FAILED task=$TaskName reason=$Reason expected_state=Disabled actual_state=$($readback.State) remediation=Task Scheduler still holds restart authority; do not drain the daemon"
    }
    $taskInfo = Get-ScheduledTaskInfo -TaskName $TaskName -ErrorAction SilentlyContinue
    Info ("Synapse daemon Task Scheduler restart authority suspend verified: task={0} reason={1} state=Disabled last_run_time={2} last_task_result={3}" -f `
        $TaskName,
        $Reason,
        ($(if ($taskInfo) { $taskInfo.LastRunTime } else { '<unknown>' })),
        ($(if ($taskInfo) { $taskInfo.LastTaskResult } else { '<unknown>' })))
    return $readback
}

function Resume-SynapseDaemonTaskRestartAuthority {
    param(
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$SupervisorPath,
        [Parameter(Mandatory=$true)][string]$Reason
    )

    $task = Assert-SynapseDaemonTaskRestartAuthorityIdentity `
        -TaskName $TaskName `
        -SupervisorPath $SupervisorPath `
        -Reason $Reason
    if (-not $task) {
        Die "SYNAPSE_TASK_RESUME_TASK_MISSING task=$TaskName reason=$Reason remediation=there is no setup-owned Synapse daemon task to resume; run full setup (scripts/synapse-setup.ps1 -SourceDir <checkout>) to register autostart"
    }

    Info "Restoring Synapse daemon Task Scheduler restart authority: task=$TaskName state_before=$($task.State) reason=$Reason"
    try {
        Enable-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
    } catch {
        Die "SYNAPSE_TASK_ENABLE_FAILED task=$TaskName reason=$Reason error=$($_.Exception.Message) remediation=autostart cannot be restored while the task is disabled; inspect Task Scheduler permissions"
    }
    $readback = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $readback) {
        Die "SYNAPSE_TASK_RESUME_READBACK_MISSING task=$TaskName reason=$Reason remediation=the scheduled task disappeared during resume; run full setup to restore autostart"
    }
    if ([string]$readback.State -eq 'Disabled') {
        Die "SYNAPSE_TASK_RESUME_READBACK_FAILED task=$TaskName reason=$Reason actual_state=Disabled remediation=Enable-ScheduledTask reported success but the task is still disabled"
    }
    Info "Synapse daemon Task Scheduler restart authority restore verified: task=$TaskName reason=$Reason state=$($readback.State)"
    return $readback
}

function Wait-SynapseDaemonSupervisorParked {
    param(
        [Parameter(Mandatory=$true)][string]$SupervisorPath,
        [Parameter(Mandatory=$true)][string]$Reason,
        [int]$TimeoutSeconds = 30
    )

    $started = Get-Date
    $deadline = $started.AddSeconds($TimeoutSeconds)
    do {
        $remaining = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $SupervisorPath)
        if ($remaining.Count -eq 0) {
            $elapsedMs = [int64]((Get-Date) - $started).TotalMilliseconds
            Info "Synapse daemon supervisor parked itself reason=$Reason supervisor_path=$SupervisorPath elapsed_ms=$elapsedMs supervisor_count=0 forced=false"
            return [pscustomobject]@{ Parked = $true; Forced = $false; ElapsedMs = $elapsedMs; Remaining = @() }
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)

    $remaining = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $SupervisorPath)
    $elapsedMs = [int64]((Get-Date) - $started).TotalMilliseconds
    return [pscustomobject]@{ Parked = $false; Forced = $false; ElapsedMs = $elapsedMs; Remaining = $remaining }
}

function New-SynapseDaemonStopPhase {
    param(
        [Parameter(Mandatory=$true)][string]$Name,
        [Parameter(Mandatory=$true)][datetime]$Started,
        [Parameter(Mandatory=$true)][string]$Detail
    )
    $elapsedMs = [int64]((Get-Date) - $Started).TotalMilliseconds
    Info "Synapse daemon stop phase=$Name ok=true elapsed_ms=$elapsedMs $Detail"
    return [ordered]@{ name = $Name; ok = $true; elapsed_ms = $elapsedMs; detail = $Detail }
}

function Read-SynapseDaemonLifecycleRunRecord {
    param([Parameter(Mandatory=$true)][string]$DbPath)

    $path = Join-Path $DbPath 'daemon-run-current.json'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        return [pscustomobject]@{ Ok = $false; Path = $path; Record = $null; Error = 'missing' }
    }
    try {
        $text = (Get-Content -Raw -LiteralPath $path).Trim()
        $record = $text | ConvertFrom-Json
    } catch {
        return [pscustomobject]@{ Ok = $false; Path = $path; Record = $null; Error = $_.Exception.Message }
    }
    return [pscustomobject]@{ Ok = $true; Path = $path; Record = $record; Error = $null }
}

function Write-SynapseDaemonLifecycleModeRecord {
    param(
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][hashtable]$Record
    )

    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
    $currentPath = Join-Path $LogDir 'daemon-operator-lifecycle-current.json'
    $eventsPath = Join-Path $LogDir 'daemon-operator-lifecycle-events.jsonl'
    $ordered = [ordered]@{ schema = 'synapse_daemon_operator_lifecycle/v1' }
    foreach ($key in $Record.Keys) { $ordered[$key] = $Record[$key] }
    $json = ($ordered | ConvertTo-Json -Depth 12)
    try {
        Set-Content -LiteralPath $currentPath -Value $json -Encoding ascii
        ($ordered | ConvertTo-Json -Compress -Depth 12) | Add-Content -LiteralPath $eventsPath -Encoding ascii
    } catch {
        Die "SYNAPSE_DAEMON_OPERATOR_LIFECYCLE_RECORD_WRITE_FAILED current_path=$currentPath events_path=$eventsPath error=$($_.Exception.Message) remediation=the operator stop/start transaction must leave a durable record; repair the log directory and rerun"
    }
    Info "Synapse daemon operator lifecycle record written current=$currentPath events=$eventsPath"
    return $currentPath
}

# ---------------------------------------------------------------------------
# #2092 -- THE drain. One mechanism, two reasons.
#
# Before this there were two divergent drains. `-Stop` (#2083) revoked restart
# authority durably and cooperatively; the deploy path (#2051) unregistered the
# task and force-killed the supervisor unconditionally, then ran a 300 s retry
# loop whose entire reason for existing was that a surviving supervisor might
# relaunch the daemon mid-drain. The observable cost of the second one was that
# the first boot after EVERY deploy reported `previous_shutdown=dirty`: the
# supervisor was killed, so the daemon it had already been asked to drain was
# racing a force stop rather than finishing one.
#
# The two are now the same function, parameterised by $Reason:
#
#   1. durable stop-request      -- revokes the supervisor's authority in a way
#                                   that outlives THIS process. The setup
#                                   maintenance lock cannot do this: it is only
#                                   honoured while its owner PID is alive.
#   2. Stop + Disable the task   -- revokes Task Scheduler's authority,
#                                   REVERSIBLY. This replaces the deploy path's
#                                   Unregister-ScheduledTask, whose failure mode
#                                   was that a setup that died between section 5
#                                   and section 7 left the host with no autostart
#                                   at all until the next successful full run.
#   3. authenticated /shutdown   -- the daemon's own graceful drain: refuse new
#                                   /mcp work, close sessions, release input
#                                   leases, flush + close the Calyx vault,
#                                   release lifetime locks, write the graceful
#                                   lifecycle exit record, then exit.
#   4. wait for the supervisor   -- it parks on the stop-request. Note the
#                                   daemon exits 1 on /shutdown by design (that
#                                   exit code means "restart me" to the
#                                   supervisor), so step 1 is what makes step 3
#                                   a stop rather than a restart.
#
# Why the durable revocation beats the unregister for the INSTALL HANDOFF race
# specifically (#2051's original problem): unregistering removes the trigger but
# does nothing about a supervisor that is already alive, mid-backoff, or started
# by a logon in the window before the unregister lands -- which is why #2051 also
# needed the unconditional force-kill AND the relaunch retry loop. The
# stop-request is consulted by the supervisor at all four of its launch points,
# so a supervisor that starts DURING the install handoff parks at
# `supervisor_start` before it ever touches the bind. Task re-registration in
# section 7 is therefore safe while the revocation is still in force, and the
# retry loop degrades from mechanism to fallback.
#
# Force-kill is not removed; it is demoted to an explicit, loudly logged
# escalation (`SYNAPSE_DAEMON_SUPERVISOR_STOP_FORCED`) that only runs when the
# cooperative park window expires.
# ---------------------------------------------------------------------------
function Invoke-SynapseDaemonRevokedDrain {
    param(
        [Parameter(Mandatory=$true)][ValidateSet('operator_stop','deploy')][string]$Reason,
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$RuntimeBinDir,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [switch]$ForceRestart,
        # Deploy only: the install cannot proceed while the old binary is mapped,
        # so a wedged daemon escalates to the identity-verified exact-PID stop
        # rather than aborting a half-staged deploy. -Stop never sets this.
        [switch]$EscalateAfterGracefulTimeout,
        # Deploy only: a supervisor that will not park cannot be allowed to
        # relaunch onto bytes that are about to be replaced. -Stop escalates only
        # with -ForceStop, and dies otherwise.
        [switch]$AllowParkEscalation,
        [ValidateRange(5, 600)][int]$TimeoutSeconds = 120,
        [ValidateRange(5, 300)][int]$ParkTimeoutSeconds = 60
    )

    $supervisorPath = Join-Path $RuntimeBinDir 'synapse-daemon-supervisor.ps1'
    $stopRequestPath = Get-SynapseDaemonSupervisorStopRequestPath -RuntimeBinDir $RuntimeBinDir
    $forced = $false
    $phases = @()

    # --- before state -----------------------------------------------------
    $phaseStarted = Get-Date
    $daemonsBefore = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
    $supervisorsBefore = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $supervisorPath)
    $taskBefore = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    Info ("Synapse daemon drain before-state reason={0} task_state={1} daemon_count={2} supervisor_count={3}`ndaemons:`n{4}`nsupervisors:`n{5}" -f `
        $Reason,
        ($(if ($taskBefore) { $taskBefore.State } else { '<absent>' })),
        $daemonsBefore.Count,
        $supervisorsBefore.Count,
        (Format-SynapseMcpProcessSnapshot -Snapshot $daemonsBefore),
        (Format-SynapseDaemonSupervisorProcessSnapshot -Snapshot $supervisorsBefore))
    $phases += New-SynapseDaemonStopPhase -Name 'before_state' -Started $phaseStarted -Detail ("reason={0} task_state={1} daemon_count={2} supervisor_count={3}" -f `
        $Reason, ($(if ($taskBefore) { $taskBefore.State } else { '<absent>' })), $daemonsBefore.Count, $supervisorsBefore.Count)

    # --- 1. durable supervisor restart-authority revocation ---------------
    $phaseStarted = Get-Date
    $stopRequest = Write-SynapseDaemonSupervisorStopRequest `
        -Path $stopRequestPath `
        -Bind $Bind `
        -DbPath $DbPath `
        -Reason $Reason `
        -SupervisorPath $supervisorPath
    if ($Reason -eq 'deploy') {
        # Record the revocation for the trap / post-exit-continuation restore
        # BEFORE the task is disabled, so a failure inside the task revocation
        # itself is still cleaned up.
        Set-SynapseDeployRestartAuthorityRevocation -StopRequestPath $stopRequestPath -TaskName $TaskName
    }
    $phases += New-SynapseDaemonStopPhase -Name 'supervisor_revocation' -Started $phaseStarted -Detail "path=$stopRequestPath requested_by_pid=$($stopRequest.requested_by_pid) requested_at_utc=$($stopRequest.requested_at_utc)"

    # --- 2. reversible Task Scheduler revocation --------------------------
    $phaseStarted = Get-Date
    $taskSuspended = Suspend-SynapseDaemonTaskRestartAuthority `
        -TaskName $TaskName `
        -SupervisorPath $supervisorPath `
        -Reason $Reason
    $phases += New-SynapseDaemonStopPhase -Name 'task_revocation' -Started $phaseStarted -Detail ("task_present={0} state_after={1} revocation=stop+disable (reversible; not unregister)" -f `
        ($null -ne $taskSuspended), ($(if ($taskSuspended) { $taskSuspended.State } else { '<absent>' })))

    # --- 3. authenticated graceful daemon drain ---------------------------
    $phaseStarted = Get-Date
    if ($daemonsBefore.Count -eq 0) {
        Info "Synapse daemon drain: no target daemon process was running before the drain reason=$Reason bind=$Bind db=$DbPath"
    }
    Stop-SynapseMcpProcesses `
        -Reason $Reason `
        -Bind $Bind `
        -DbPath $DbPath `
        -TokenPath $TokenPath `
        -LogDir $LogDir `
        -ForceRestart:$ForceRestart `
        -EscalateAfterGracefulTimeout:$EscalateAfterGracefulTimeout `
        -TimeoutSeconds ([Math]::Min(90, $TimeoutSeconds))
    if ($ForceRestart) {
        $forced = $true
        Warn "SYNAPSE_DAEMON_STOP_FORCED reason=$Reason force_restart=true effect=the drain was permitted to escalate past active clients and past graceful-shutdown failures to an identity-verified exact-PID stop. This is NOT the default drain path."
    }
    $phases += New-SynapseDaemonStopPhase -Name 'daemon_drain' -Started $phaseStarted -Detail "forced=$forced drain=authenticated_http_shutdown escalate_after_graceful_timeout=$([bool]$EscalateAfterGracefulTimeout)"

    # --- 4. supervisor park -----------------------------------------------
    $phaseStarted = Get-Date
    $park = Wait-SynapseDaemonSupervisorParked -SupervisorPath $supervisorPath -Reason $Reason -TimeoutSeconds ([Math]::Min($ParkTimeoutSeconds, $TimeoutSeconds))
    if (-not $park.Parked) {
        $parkFailure = ("SYNAPSE_DAEMON_SUPERVISOR_PARK_TIMEOUT reason={0} supervisor_path={1} elapsed_ms={2} remaining_count={3} stop_request={4} stop_request_present={5} supervisor_last_event={6}`nremaining:`n{7}" -f `
            $Reason,
            $supervisorPath,
            $park.ElapsedMs,
            $park.Remaining.Count,
            $stopRequestPath,
            (Test-Path -LiteralPath $stopRequestPath -PathType Leaf),
            (Get-SynapseDaemonSupervisorLastEventText -LogDir $LogDir),
            (Format-SynapseDaemonSupervisorProcessSnapshot -Snapshot $park.Remaining))
        if (-not ($ForceRestart -or $AllowParkEscalation)) {
            Die ("{0}`nremediation=the hidden supervisor did not honour the durable stop-request. A supervisor generated before #2083 does not read that file; redeploy setup once so the supervisor is regenerated, or rerun this stop with -ForceRestart to escalate to an explicit identity-verified exact-PID supervisor stop. Do not close terminal/IDE/WSL processes." -f $parkFailure)
        }
        Warn "FORCED: $parkFailure"
        Warn "SYNAPSE_DAEMON_SUPERVISOR_STOP_FORCED reason=$Reason supervisor_path=$supervisorPath effect=escalating to an identity-verified exact-PID supervisor stop because the cooperative park window expired. This is the escalation, not the default."
        Stop-SynapseDaemonSupervisorProcessesForInstallHandoff -SupervisorPath $supervisorPath
        $forced = $true
        $park = [pscustomobject]@{ Parked = $true; Forced = $true; ElapsedMs = [int64]((Get-Date) - $phaseStarted).TotalMilliseconds; Remaining = @() }
    } else {
        Info "SYNAPSE_DAEMON_SUPERVISOR_PARKED_COOPERATIVELY reason=$Reason elapsed_ms=$($park.ElapsedMs) forced=false effect=the supervisor stopped itself on the durable stop-request; no supervisor process was killed"
    }
    $phases += New-SynapseDaemonStopPhase -Name 'supervisor_park' -Started $phaseStarted -Detail "parked=true forced=$($park.Forced) elapsed_ms=$($park.ElapsedMs)"

    return [pscustomobject]@{
        Reason            = $Reason
        Phases            = $phases
        Forced            = $forced
        ParkForced        = [bool]$park.Forced
        SupervisorPath    = $supervisorPath
        StopRequestPath   = $stopRequestPath
        StopRequest       = $stopRequest
        TaskSuspended     = $taskSuspended
        TaskBefore        = $taskBefore
        DaemonsBefore     = $daemonsBefore
        SupervisorsBefore = $supervisorsBefore
    }
}

# ---------------------------------------------------------------------------
# #2083 -- the operator stop path, now a thin wrapper around the shared #2092
# drain plus the verification/settle/readback tail that only a -Stop needs.
#
# Ordering is load-bearing and is the standard cooperative-shutdown ladder for
# a supervised Windows process tree: revoke restart authority, ask the process
# to drain, wait a bounded window, and only then escalate to an explicit,
# logged forced kill.
#
#   1. durable stop-request      -- revokes the supervisor's authority in a way
#                                   that outlives THIS process. The setup
#                                   maintenance lock cannot do this: it is only
#                                   honoured while its owner PID is alive.
#   2. Stop + Disable the task   -- revokes Task Scheduler's authority,
#                                   reversibly (-Start re-enables).
#   3. authenticated /shutdown   -- the daemon's own graceful drain: refuse new
#                                   /mcp work, close sessions, release input
#                                   leases, flush + close the Calyx vault,
#                                   release lifetime locks, write the graceful
#                                   lifecycle exit record, then exit.
#   4. wait for the supervisor   -- it parks on the stop-request. Note the
#                                   daemon exits 1 on /shutdown by design (that
#                                   exit code means "restart me" to the
#                                   supervisor), so step 1 is what makes step 3
#                                   a stop rather than a restart.
#   5. verify + settle           -- zero daemons, zero supervisors, bind free,
#                                   re-verified after a settle window so a
#                                   backoff relaunch cannot hide inside it.
# ---------------------------------------------------------------------------
function Invoke-SynapseDaemonStop {
    param(
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$RuntimeBinDir,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [switch]$ForceRestart,
        [ValidateRange(5, 600)][int]$TimeoutSeconds = 120,
        [ValidateRange(1, 120)][int]$SettleSeconds = 10
    )

    $stopStarted = Get-Date
    $reason = 'operator_stop'
    $supervisorPath = Join-Path $RuntimeBinDir 'synapse-daemon-supervisor.ps1'
    $stopRequestPath = Get-SynapseDaemonSupervisorStopRequestPath -RuntimeBinDir $RuntimeBinDir
    $phases = @()

    Step "Stopping the Synapse daemon (graceful) task=$TaskName bind=$Bind"

    $runRecordBefore = Read-SynapseDaemonLifecycleRunRecord -DbPath $DbPath
    Info "Synapse daemon stop pre-drain lifecycle readback path=$($runRecordBefore.Path) readable=$($runRecordBefore.Ok)"

    # --- guard ------------------------------------------------------------
    $phaseStarted = Get-Date
    Assert-SynapseRestartAllowed `
        -Reason $reason `
        -Bind $Bind `
        -DbPath $DbPath `
        -TokenPath $TokenPath `
        -HealthTimeoutSec 30 `
        -ForceRestart:$ForceRestart `
        -AllowActiveClientDrain
    $phases += New-SynapseDaemonStopPhase -Name 'restart_guard' -Started $phaseStarted -Detail "force_restart=$([bool]$ForceRestart)"

    # --- the shared #2092 drain (before_state, revocations, drain, park) ---
    # -Stop deliberately passes neither -EscalateAfterGracefulTimeout nor
    # -AllowParkEscalation: an operator stop that cannot be done gracefully must
    # fail loudly with the exact remaining PIDs, not quietly kill things.
    $drain = Invoke-SynapseDaemonRevokedDrain `
        -Reason $reason `
        -TaskName $TaskName `
        -RuntimeBinDir $RuntimeBinDir `
        -Bind $Bind `
        -DbPath $DbPath `
        -TokenPath $TokenPath `
        -LogDir $LogDir `
        -ForceRestart:$ForceRestart `
        -TimeoutSeconds $TimeoutSeconds
    $phases += $drain.Phases
    $forced = $drain.Forced
    $taskSuspended = $drain.TaskSuspended
    $daemonsBefore = $drain.DaemonsBefore
    $supervisorsBefore = $drain.SupervisorsBefore
    $taskBefore = $drain.TaskBefore

    # --- 5. verify + settle ----------------------------------------------
    $phaseStarted = Get-Date
    Wait-SynapseBindReleased -Reason $reason -Bind $Bind -TimeoutSeconds ([Math]::Min(90, $TimeoutSeconds)) -ForceRestart:$ForceRestart
    $phases += New-SynapseDaemonStopPhase -Name 'bind_release' -Started $phaseStarted -Detail "bind=$Bind"

    $phaseStarted = Get-Date
    $settleDeadline = (Get-Date).AddSeconds($SettleSeconds)
    do {
        Start-Sleep -Milliseconds 500
        $daemonsNow = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
        $supervisorsNow = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $supervisorPath)
        if ($daemonsNow.Count -ne 0 -or $supervisorsNow.Count -ne 0) {
            Die ("SYNAPSE_DAEMON_STOP_RELAUNCH_OBSERVED reason={0} settle_seconds={1} daemon_count={2} supervisor_count={3} stop_request={4}`ndaemons:`n{5}`nsupervisors:`n{6}`nremediation=something relaunched the daemon after restart authority was revoked. Inspect logs\daemon-supervisor-events.jsonl and Task Scheduler for a second registration; do not start another daemon until exactly zero remain." -f `
                $reason,
                $SettleSeconds,
                $daemonsNow.Count,
                $supervisorsNow.Count,
                $stopRequestPath,
                (Format-SynapseMcpProcessSnapshot -Snapshot $daemonsNow),
                (Format-SynapseDaemonSupervisorProcessSnapshot -Snapshot $supervisorsNow))
        }
    } while ((Get-Date) -lt $settleDeadline)
    $phases += New-SynapseDaemonStopPhase -Name 'settle_verify' -Started $phaseStarted -Detail "settle_seconds=$SettleSeconds daemon_count=0 supervisor_count=0"

    # --- 6. storage / lifecycle readback ----------------------------------
    $phaseStarted = Get-Date
    $runRecordAfter = Read-SynapseDaemonLifecycleRunRecord -DbPath $DbPath
    if (-not $runRecordAfter.Ok) {
        Die "SYNAPSE_DAEMON_STOP_LIFECYCLE_RECORD_UNREADABLE path=$($runRecordAfter.Path) error=$($runRecordAfter.Error) remediation=the daemon lifecycle run record must be readable after a graceful stop; inspect the vault directory"
    }
    # #2131: the verdict is derived from the whole record, not from
    # `ended_at_unix_ms` alone, and it is the SAME derivation the daemon will run
    # at its next boot.
    $stopVerdict = Get-SynapseDaemonPreviousShutdownVerdict -Record $runRecordAfter.Record -Readable $true
    $endedAt = $stopVerdict.EndedAtUnixMs
    $endedReason = $stopVerdict.EndedReason
    $cleanShutdown = $stopVerdict.Clean
    if ($daemonsBefore.Count -gt 0 -and $stopVerdict.Verdict -eq 'dirty') {
        # Unchanged fatal case: nothing on disk proves a shutdown was ever even
        # commanded, so this stop is indistinguishable from a crash.
        $message = ("SYNAPSE_DAEMON_STOP_DIRTY_LIFECYCLE_RECORD path={0} run_id={1} pid={2} ended_at_unix_ms={3} ended_reason={4} verdict_detail={5} remediation=the daemon exited without proving a commanded graceful close, so storage flush/close and input-lease release cannot be proven. Inspect the daemon log for MCP_HTTP_SHUTDOWN_* and SYNAPSE_CALYX_VAULT_CLOSED, and the exit ledger daemon-exit.jsonl." -f `
            $runRecordAfter.Path,
            [string]$runRecordAfter.Record.run_id,
            [string]$runRecordAfter.Record.pid,
            ($(if ($null -eq $endedAt) { '<null>' } else { $endedAt })),
            $endedReason,
            $stopVerdict.Detail)
        if ($forced) {
            Warn "FORCED: $message"
        } else {
            Die $message
        }
    } elseif ($daemonsBefore.Count -gt 0 -and $stopVerdict.Verdict -eq 'interrupted_graceful') {
        # New in #2131, and deliberately NOT fatal: the daemon IS stopped, which
        # is what -Stop promised. What it did not do is finish its close, and
        # before this the operator was told the opposite.
        Warn ("SYNAPSE_DAEMON_STOP_INTERRUPTED_GRACEFUL_LIFECYCLE_RECORD path={0} run_id={1} pid={2} ended_reason={3} ending_phase={4} verdict_detail={5} effect=the next boot will report previous_shutdown=interrupted_graceful remediation=the close was commanded and did not finish; inspect the daemon log for SYNAPSE_CALYX_VAULT_CLOSE_PHASE (the last phase recorded is the one that did not complete) and MCP_HTTP_SHUTDOWN_WATCHDOG_EXPIRED." -f `
            $runRecordAfter.Path,
            [string]$runRecordAfter.Record.run_id,
            [string]$runRecordAfter.Record.pid,
            $endedReason,
            $stopVerdict.EndingPhase,
            $stopVerdict.Detail)
    }
    $vaultLockPath = Join-Path $DbPath 'daemon.lock'
    $vaultPidPath = Join-Path $DbPath 'daemon.pid'
    $vaultPidPresent = Test-Path -LiteralPath $vaultPidPath -PathType Leaf
    if ($vaultPidPresent) {
        $message = ("SYNAPSE_DAEMON_STOP_LIFETIME_PID_SIDECAR_PRESENT path={0} remediation=the daemon lifetime lock PID sidecar survives only when the daemon retained its locks through process teardown; inspect the daemon log for MCP_DAEMON_LIFETIME_LOCKS_CLOSE_FAILED before starting another daemon." -f $vaultPidPath)
        if ($forced) {
            Warn "FORCED: $message"
        } else {
            Die $message
        }
    }
    Info ("Synapse daemon stop storage readback lifecycle_record={0} run_id={1} ended_at_unix_ms={2} ended_reason={3} clean_shutdown={4} expected_next_boot_previous_shutdown={5} verdict_detail={6} vault_lock_path={7} vault_pid_sidecar_present={8}" -f `
        $runRecordAfter.Path,
        [string]$runRecordAfter.Record.run_id,
        ($(if ($null -eq $endedAt) { '<null>' } else { $endedAt })),
        $endedReason,
        $cleanShutdown,
        $stopVerdict.Verdict,
        $stopVerdict.Detail,
        $vaultLockPath,
        $vaultPidPresent)
    $phases += New-SynapseDaemonStopPhase -Name 'storage_readback' -Started $phaseStarted -Detail "clean_shutdown=$cleanShutdown expected_next_boot_previous_shutdown=$($stopVerdict.Verdict) ended_reason=$endedReason vault_pid_sidecar_present=$vaultPidPresent"

    $totalMs = [int64]((Get-Date) - $stopStarted).TotalMilliseconds
    $recordPath = Write-SynapseDaemonLifecycleModeRecord -LogDir $LogDir -Record @{
        mode = 'stop'
        reason = $reason
        outcome = 'stopped'
        forced = $forced
        task_name = $TaskName
        bind = $Bind
        db_path = $DbPath
        supervisor_path = $supervisorPath
        stop_request_path = $stopRequestPath
        started_utc = $stopStarted.ToUniversalTime().ToString('o')
        ended_utc = (Get-Date).ToUniversalTime().ToString('o')
        total_ms = $totalMs
        settle_seconds = $SettleSeconds
        daemon_count_before = $daemonsBefore.Count
        supervisor_count_before = $supervisorsBefore.Count
        daemon_count_after = 0
        supervisor_count_after = 0
        task_state_before = ($(if ($taskBefore) { [string]$taskBefore.State } else { 'absent' }))
        task_state_after = ($(if ($taskSuspended) { [string]$taskSuspended.State } else { 'absent' }))
        lifecycle_run_current_path = $runRecordAfter.Path
        lifecycle_run_id = [string]$runRecordAfter.Record.run_id
        lifecycle_ended_at_unix_ms = $endedAt
        lifecycle_ended_reason = $endedReason
        lifecycle_clean_shutdown = $cleanShutdown
        lifecycle_expected_next_boot_previous_shutdown = $stopVerdict.Verdict
        lifecycle_verdict_detail = $stopVerdict.Detail
        vault_pid_sidecar_present = $vaultPidPresent
        setup_pid = $PID
        phases = $phases
    }

    Info ("Synapse daemon stop verified reason={0} forced={1} daemon_count=0 supervisor_count=0 task_state={2} clean_shutdown={3} expected_next_boot_previous_shutdown={4} total_ms={5} record={6}" -f `
        $reason,
        $forced,
        ($(if ($taskSuspended) { $taskSuspended.State } else { 'absent' })),
        $cleanShutdown,
        $stopVerdict.Verdict,
        $totalMs,
        $recordPath)
    Info "Synapse daemon is stopped. Restart it with: pwsh -NoProfile -File scripts\synapse-setup.ps1 -Start"
}

# ---------------------------------------------------------------------------
# #2083 -- the matching start path. Idempotence and reversibility are the whole
# point: -Stop must be undoable without a source checkout or a rebuild.
# ---------------------------------------------------------------------------
function Invoke-SynapseDaemonStart {
    param(
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$RuntimeBinDir,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [ValidateRange(10, 3600)][int]$TimeoutSeconds = 300
    )

    $startStarted = Get-Date
    $reason = 'operator_start'
    $supervisorPath = Join-Path $RuntimeBinDir 'synapse-daemon-supervisor.ps1'
    $stopRequestPath = Get-SynapseDaemonSupervisorStopRequestPath -RuntimeBinDir $RuntimeBinDir

    Step "Starting the Synapse daemon task=$TaskName bind=$Bind"

    foreach ($requiredFile in @($supervisorPath, (Join-Path $RuntimeBinDir 'synapse-daemon-launch-hidden.vbs'))) {
        if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
            Die "SYNAPSE_DAEMON_START_LAUNCHER_MISSING path=$requiredFile reason=$reason remediation=autostart artifacts are missing; run full setup (scripts/synapse-setup.ps1 -SourceDir <checkout>) to regenerate the hidden launcher and supervisor"
        }
    }

    $daemonsBefore = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
    $supervisorsBefore = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $supervisorPath)
    if ($daemonsBefore.Count -gt 0 -or $supervisorsBefore.Count -gt 0) {
        Die ("SYNAPSE_DAEMON_START_ALREADY_RUNNING reason={0} daemon_count={1} supervisor_count={2}`ndaemons:`n{3}`nsupervisors:`n{4}`nremediation=start refuses to create a second supervisor generation. Stop first with scripts/synapse-setup.ps1 -Stop, then start." -f `
            $reason,
            $daemonsBefore.Count,
            $supervisorsBefore.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $daemonsBefore),
            (Format-SynapseDaemonSupervisorProcessSnapshot -Snapshot $supervisorsBefore))
    }

    $previousRunRecord = Read-SynapseDaemonLifecycleRunRecord -DbPath $DbPath
    # #2131: this expectation is load-bearing -- the mismatch check below DIES on
    # disagreement -- so it must be the daemon's own law, not a second one that
    # happens to agree on the easy cases. Before this, a watchdog-killed close
    # expected `clean`, the daemon reported `clean`, and both were wrong
    # together; the two-way agreement was the thing hiding the bug.
    $previousVerdict = Get-SynapseDaemonPreviousShutdownVerdict -Record $previousRunRecord.Record -Readable $previousRunRecord.Ok
    $previousEndedReason = $previousVerdict.EndedReason
    $previousEndedAt = $previousVerdict.EndedAtUnixMs
    $previousShutdownExpected = $previousVerdict.Verdict
    Info "Synapse daemon start pre-boot lifecycle readback path=$($previousRunRecord.Path) readable=$($previousRunRecord.Ok) previous_ended_reason=$previousEndedReason expected_previous_shutdown=$previousShutdownExpected verdict_detail=$($previousVerdict.Detail)"

    $clearedStopRequest = Clear-SynapseDaemonSupervisorStopRequest -Path $stopRequestPath -Reason $reason
    $taskResumed = Resume-SynapseDaemonTaskRestartAuthority -TaskName $TaskName -SupervisorPath $supervisorPath -Reason $reason

    try {
        Start-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    } catch {
        Die "SYNAPSE_DAEMON_START_TASK_START_FAILED task=$TaskName reason=$reason error=$($_.Exception.Message) remediation=Task Scheduler refused to start the enabled Synapse daemon task; inspect Task Scheduler history"
    }
    Info "Synapse daemon scheduled task start issued task=$TaskName reason=$reason"

    $tokenRead = Read-SynapseSetupTokenForRestartGuard -TokenPath $TokenPath
    if (-not $tokenRead.Ok) {
        Die "$($tokenRead.Code) reason=$reason $($tokenRead.Detail) remediation=the bearer token is required to verify the started daemon over authenticated /health; repair token state and rerun -Start"
    }

    $deadline = $startStarted.AddSeconds($TimeoutSeconds)
    $health = $null
    $lastError = '<none>'
    do {
        Start-Sleep -Seconds 2
        $healthRead = Read-SynapseHealthForRestartGuard -Bind $Bind -Token $tokenRead.Token -TimeoutSec 10
        if ($healthRead.Ok) {
            $health = $healthRead.Health
            break
        }
        $lastError = $healthRead.Error
    } while ((Get-Date) -lt $deadline)

    if ($null -eq $health) {
        $supervisorStatePath = Join-Path $LogDir 'daemon-supervisor-current.json'
        $supervisorState = if (Test-Path -LiteralPath $supervisorStatePath -PathType Leaf) {
            try { (Get-Content -Raw -LiteralPath $supervisorStatePath).Trim() } catch { "read_failed:$($_.Exception.Message)" }
        } else { '<missing>' }
        Die "SYNAPSE_DAEMON_START_HEALTH_TIMEOUT task=$TaskName reason=$reason bind=$Bind timeout_s=$TimeoutSeconds last_error=$lastError supervisor_state=$supervisorState remediation=inspect logs\daemon-stderr-gen{N}-*.log for the real startup error; the scheduled task LastTaskResult does not report child failures"
    }

    $daemonsAfter = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
    $supervisorsAfter = @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $supervisorPath)
    if ($daemonsAfter.Count -ne 1 -or $supervisorsAfter.Count -ne 1) {
        Die ("SYNAPSE_DAEMON_START_TOPOLOGY_INVALID reason={0} expected_daemon_count=1 actual_daemon_count={1} expected_supervisor_count=1 actual_supervisor_count={2}`ndaemons:`n{3}`nsupervisors:`n{4}`nremediation=a healthy start is exactly one supervisor and exactly one daemon; inspect logs\daemon-supervisor-events.jsonl for a duplicate generation" -f `
            $reason,
            $daemonsAfter.Count,
            $supervisorsAfter.Count,
            (Format-SynapseMcpProcessSnapshot -Snapshot $daemonsAfter),
            (Format-SynapseDaemonSupervisorProcessSnapshot -Snapshot $supervisorsAfter))
    }

    $lifecycleDetail = [string]$health.subsystems.daemon_lifecycle.detail
    $previousShutdownObserved = '<unreported>'
    $match = [regex]::Match($lifecycleDetail, 'previous_shutdown=(?<value>\S+)')
    if ($match.Success) { $previousShutdownObserved = $match.Groups['value'].Value }
    Info "Synapse daemon start boot readback pid=$($health.pid) previous_shutdown=$previousShutdownObserved expected=$previousShutdownExpected daemon_lifecycle_detail=$lifecycleDetail"
    if ($previousShutdownExpected -ne 'unknown' -and $previousShutdownObserved -ne '<unreported>' -and $previousShutdownObserved -ne $previousShutdownExpected) {
        Die "SYNAPSE_DAEMON_START_PREVIOUS_SHUTDOWN_MISMATCH expected=$previousShutdownExpected observed=$previousShutdownObserved lifecycle_record=$($previousRunRecord.Path) remediation=the boot verdict disagrees with the on-disk lifecycle record read before the start; inspect daemon-exit.jsonl"
    }

    $totalMs = [int64]((Get-Date) - $startStarted).TotalMilliseconds
    $recordPath = Write-SynapseDaemonLifecycleModeRecord -LogDir $LogDir -Record @{
        mode = 'start'
        reason = $reason
        outcome = 'started'
        forced = $false
        task_name = $TaskName
        bind = $Bind
        db_path = $DbPath
        supervisor_path = $supervisorPath
        stop_request_path = $stopRequestPath
        cleared_stop_request = ($null -ne $clearedStopRequest)
        started_utc = $startStarted.ToUniversalTime().ToString('o')
        ended_utc = (Get-Date).ToUniversalTime().ToString('o')
        total_ms = $totalMs
        task_state_after = [string]$taskResumed.State
        daemon_pid = [int]$health.pid
        daemon_count_after = $daemonsAfter.Count
        supervisor_count_after = $supervisorsAfter.Count
        supervisor_pid = [int]$supervisorsAfter[0].ProcessId
        previous_shutdown_expected = $previousShutdownExpected
        previous_shutdown_observed = $previousShutdownObserved
        previous_shutdown_verdict_detail = $previousVerdict.Detail
        previous_ended_reason = $previousEndedReason
        setup_pid = $PID
    }

    Info ("Synapse daemon start verified reason={0} task_state={1} daemon_pid={2} supervisor_pid={3} previous_shutdown={4} total_ms={5} record={6}" -f `
        $reason,
        $taskResumed.State,
        $health.pid,
        $supervisorsAfter[0].ProcessId,
        $previousShutdownObserved,
        $totalMs,
        $recordPath)
    Info "Synapse daemon is live on http://$Bind (MCP: http://$Bind/mcp)."
}

# ---------------------------------------------------------------------------
# #2092 -- demoted from mechanism to fallback.
#
# This retry loop exists because, under #2051, a surviving supervisor could
# relaunch the daemon in the middle of the install drain. That is no longer the
# mechanism that stops relaunches: Invoke-SynapseDaemonRevokedDrain writes the
# durable stop-request and parks the supervisor first, so on a #2083-or-later
# supervisor this loop normally verifies zero targets on attempt 1 and returns.
#
# It is kept because it is the only thing that catches a relaunch from a
# supervisor that predates the stop-request (a host whose supervisor has not been
# regenerated yet), and because a deploy must never proceed to binary replacement
# with a live daemon mapping the old image. Its timeout diagnostics now name the
# stop-request state and the supervisor's last ledger event, which is what
# distinguishes "old supervisor, ignores the record" from "new supervisor, threw
# on the record" from "something else re-registered the task".
# ---------------------------------------------------------------------------
function Stop-SynapseMcpProcessesForInstallHandoff {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [Parameter(Mandatory=$true)][string]$TokenPath,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [AllowNull()][string]$StopRequestPath,
        [AllowNull()][string]$SupervisorPath,
        [switch]$ForceRestart,
        [int]$TimeoutSeconds = 300
    )

    $stopRequestPresent = if ([string]::IsNullOrWhiteSpace($StopRequestPath)) {
        '<not_tracked>'
    } else {
        [string](Test-Path -LiteralPath $StopRequestPath -PathType Leaf)
    }
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $attempt = 0
    do {
        $attempt += 1
        $remainingBudget = [Math]::Max(5, [int](($deadline - (Get-Date)).TotalSeconds))
        Info "Synapse install handoff drain attempt=$attempt reason=$Reason remaining_budget_s=$remainingBudget stop_request=$StopRequestPath stop_request_present=$stopRequestPresent role=fallback_verification"
        Stop-SynapseMcpProcesses `
            -Reason $Reason `
            -Bind $Bind `
            -DbPath $DbPath `
            -TokenPath $TokenPath `
            -LogDir $LogDir `
            -ForceRestart:$ForceRestart `
            -EscalateAfterGracefulTimeout `
            -TimeoutSeconds ([Math]::Min(60, $remainingBudget))

        Start-Sleep -Milliseconds 500
        $targets = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
        if ($targets.Count -eq 0) {
            Wait-SynapseBindReleased -Reason $Reason -Bind $Bind -TimeoutSeconds ([Math]::Min(60, $remainingBudget)) -ForceRestart:$ForceRestart
            Info "Synapse install handoff drain verified reason=$Reason attempts=$attempt target_after_count=0"
            return
        }

        $stopRequestPresent = if ([string]::IsNullOrWhiteSpace($StopRequestPath)) {
            '<not_tracked>'
        } else {
            [string](Test-Path -LiteralPath $StopRequestPath -PathType Leaf)
        }
        Info ("Synapse install handoff observed daemon relaunch after drain attempt={0} remaining_count={1} stop_request_present={2} supervisor_last_event={3}`nremaining:`n{4}" -f `
            $attempt,
            $targets.Count,
            $stopRequestPresent,
            (Get-SynapseDaemonSupervisorLastEventText -LogDir $LogDir),
            (Format-SynapseMcpProcessSnapshot -Snapshot $targets))
        Start-Sleep -Seconds 1
    } while ((Get-Date) -lt $deadline)

    $remaining = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
    $supervisorStatePath = Join-Path $LogDir 'daemon-supervisor-current.json'
    $supervisorState = if (Test-Path -LiteralPath $supervisorStatePath) {
        try { (Get-Content -Raw -LiteralPath $supervisorStatePath).Trim() } catch { "read_failed:$($_.Exception.Message)" }
    } else {
        '<missing>'
    }
    $stopRequestBody = if ([string]::IsNullOrWhiteSpace($StopRequestPath) -or -not (Test-Path -LiteralPath $StopRequestPath -PathType Leaf)) {
        '<absent>'
    } else {
        try { ((Get-Content -Raw -LiteralPath $StopRequestPath).Trim() -replace '\s+', ' ') } catch { "read_failed:$($_.Exception.Message)" }
    }
    $survivingSupervisors = if ([string]::IsNullOrWhiteSpace($SupervisorPath)) {
        '<not_tracked>'
    } else {
        Format-SynapseDaemonSupervisorProcessSnapshot -Snapshot @(Get-SynapseDaemonSupervisorProcessSnapshot -SupervisorPath $SupervisorPath)
    }
    Die ("SYNAPSE_INSTALL_HANDOFF_RELAUNCH_DRAIN_TIMEOUT reason={0} timeout_s={1} remaining_count={2}`nremaining:`n{3}`nstop_request_path={4}`nstop_request={5}`nsupervisor_last_event={6}`nsupervisor_state={7}`nsurviving_supervisors:`n{8}`nremediation=setup revoked restart authority (durable stop-request + Disable-ScheduledTask) and repeatedly drained exact verified synapse-mcp.exe targets, but something kept relaunching the installed daemon. If stop_request is <absent> the revocation was lost -- inspect the runtime bin directory. If it is present and a supervisor still relaunched, that supervisor predates #2083 and does not read the record: stop only the verified Synapse supervisor path, then rerun so the deploy regenerates it. Do not close terminal/IDE/WSL host processes." -f `
        $Reason,
        $TimeoutSeconds,
        $remaining.Count,
        (Format-SynapseMcpProcessSnapshot -Snapshot $remaining),
        ($(if ([string]::IsNullOrWhiteSpace($StopRequestPath)) { '<not_tracked>' } else { $StopRequestPath })),
        $stopRequestBody,
        (Get-SynapseDaemonSupervisorLastEventText -LogDir $LogDir),
        $supervisorState,
        $survivingSupervisors)
}

function Assert-SynapseInstallPathUnlocked {
    param(
        [Parameter(Mandatory=$true)][string]$Path,
        [Parameter(Mandatory=$true)][string]$Bind,
        [Parameter(Mandatory=$true)][string]$DbPath,
        [int]$TimeoutSeconds = 30
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Info "Installed daemon path does not exist yet; exclusive lock check skipped path=$Path"
        return
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $lastError = $null
    do {
        try {
            $stream = [System.IO.File]::Open(
                $Path,
                [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::ReadWrite,
                [System.IO.FileShare]::None)
            $stream.Dispose()
            Info "Installed daemon path exclusive-open verified path=$Path"
            return
        } catch {
            $lastError = $_.Exception.Message
            Start-Sleep -Milliseconds 250
        }
    } while ((Get-Date) -lt $deadline)

    $holders = @(Select-SynapseMcpDeployTargetProcesses -Snapshot @(Get-SynapseMcpProcessSnapshot) -Bind $Bind -DbPath $DbPath)
    Die ("SYNAPSE_INSTALL_BINARY_LOCKED path={0} timeout_s={1} error={2}`nverified_synapse_targets:`n{3}`nremediation=installed synapse-mcp.exe is still open after daemon/task drain; inspect the listed exact targets and supervisor logs before retrying binary replacement" -f `
        $Path,
        $TimeoutSeconds,
        $lastError,
        (Format-SynapseMcpProcessSnapshot -Snapshot $holders))
}

function Get-SynapseChromeNativeHostProcessSnapshot {
    param([Parameter(Mandatory=$true)][string]$NativeHostExePath)

    $expectedPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($NativeHostExePath)
    @(Get-CimInstance Win32_Process -Filter "Name='synapse-chrome-native-host.exe'" -ErrorAction SilentlyContinue |
        Where-Object {
            $_.ExecutablePath -and
            ($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($_.ExecutablePath) -ieq $expectedPath)
        } |
        Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,CommandLine)
}

function Format-SynapseChromeNativeHostProcessSnapshot {
    param($Snapshot)
    $rows = @($Snapshot | ForEach-Object {
        "pid=$($_.ProcessId) ppid=$($_.ParentProcessId) path=$($_.ExecutablePath) cmd=$($_.CommandLine)"
    })
    if ($rows.Count -eq 0) { return '<none>' }
    return ($rows -join "`n")
}

function Assert-SynapseChromeNativeHostStopTarget {
    param(
        [Parameter(Mandatory=$true)]$SnapshotProcess,
        [Parameter(Mandatory=$true)][string]$NativeHostExePath
    )

    $pidValue = [int]$SnapshotProcess.ProcessId
    $current = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue" -ErrorAction SilentlyContinue
    if (-not $current) {
        Info "Chrome native host stop target already exited pid=$pidValue"
        return $null
    }

    $expectedPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($NativeHostExePath)
    $actualPath = if ($current.ExecutablePath) { $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($current.ExecutablePath) } else { '' }
    if ($current.Name -ine 'synapse-chrome-native-host.exe' -or $actualPath -ine $expectedPath) {
        Die ("SYNAPSE_CHROME_NATIVE_HOST_STOP_TARGET_MISMATCH pid={0} expected_path={1} actual_name={2} actual_path={3} command_line={4} remediation=PID was reused or snapshot was not the Synapse Chrome native host; refusing exact-PID stop" -f `
            $pidValue,
            $expectedPath,
            $current.Name,
            $current.ExecutablePath,
            $current.CommandLine)
    }
    if ($current.CommandLine -notmatch 'chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/') {
        Die ("SYNAPSE_CHROME_NATIVE_HOST_STOP_UNVERIFIED pid={0} name={1} command_line={2} remediation=command line does not prove the Synapse Chrome extension bridge target; refusing exact-PID stop" -f `
            $pidValue,
            $current.Name,
            $current.CommandLine)
    }

    return $current
}

function Stop-SynapseChromeNativeHostProcesses {
    param(
        [Parameter(Mandatory=$true)][string]$Reason,
        [Parameter(Mandatory=$true)][string]$NativeHostExePath,
        [int]$TimeoutSeconds = 10
    )

    $before = @(Get-SynapseChromeNativeHostProcessSnapshot -NativeHostExePath $NativeHostExePath)
    Info "Chrome native host process stop requested reason=$Reason before_count=$($before.Count)"
    Info ("Chrome native host process stop before:`n{0}" -f (Format-SynapseChromeNativeHostProcessSnapshot -Snapshot $before))
    foreach ($proc in $before) {
        $verified = Assert-SynapseChromeNativeHostStopTarget -SnapshotProcess $proc -NativeHostExePath $NativeHostExePath
        if (-not $verified) { continue }
        $pidValue = [int]$verified.ProcessId
        try {
            Stop-Process -Id $pidValue -Force -ErrorAction Stop
            Info "Chrome native host exact-PID stop issued pid=$pidValue reason=$Reason"
        } catch {
            Die ("SYNAPSE_CHROME_NATIVE_HOST_STOP_FAILED pid={0} reason={1} error={2} remediation=setup only stops verified synapse-chrome-native-host.exe PIDs; it never stops cmd.exe/native-messaging wrapper or terminal processes" -f `
                $pidValue,
                $Reason,
                $_.Exception.Message)
        }
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        Start-Sleep -Milliseconds 250
        $after = @(Get-SynapseChromeNativeHostProcessSnapshot -NativeHostExePath $NativeHostExePath)
        if ($after.Count -eq 0) {
            Info "Chrome native host process stop verified reason=$Reason after_count=0"
            return
        }
    } while ((Get-Date) -lt $deadline)

    $remaining = @(Get-SynapseChromeNativeHostProcessSnapshot -NativeHostExePath $NativeHostExePath)
    Die ("SYNAPSE_CHROME_NATIVE_HOST_STOP_FAILED reason={0} timeout_s={1} remaining_count={2} remaining=`n{3}" -f `
        $Reason, $TimeoutSeconds, $remaining.Count, (Format-SynapseChromeNativeHostProcessSnapshot -Snapshot $remaining))
}

# ---------------------------------------------------------------------------
# Uninstall path
# ---------------------------------------------------------------------------
$maintenanceReason = if ($Remove) { 'remove' } elseif ($Stop) { 'stop' } elseif ($Start) { 'start' } elseif ($ResumeChromeBridgePending) { 'resume_chrome_bridge' } else { 'setup' }

# #2083: the operator lifecycle modes are exclusive of every other mode and of
# each other. They share the maintenance lock with setup/remove so two agents
# cannot drive daemon lifecycle concurrently.
$synapseExclusiveModes = @()
if ($Stop) { $synapseExclusiveModes += '-Stop' }
if ($Start) { $synapseExclusiveModes += '-Start' }
if ($Remove) { $synapseExclusiveModes += '-Remove' }
if ($ResumeChromeBridgePending) { $synapseExclusiveModes += '-ResumeChromeBridgePending' }
if ($synapseExclusiveModes.Count -gt 1) {
    Die "SYNAPSE_SETUP_MODE_CONFLICT modes=$($synapseExclusiveModes -join ',') remediation=pass exactly one of -Stop, -Start, -Remove or -ResumeChromeBridgePending"
}
if (($Stop -or $Start) -and $SkipBuild) {
    Die "SYNAPSE_SETUP_MODE_CONFLICT modes=$($synapseExclusiveModes -join ','),-SkipBuild remediation=-Stop/-Start never build or install anything, so -SkipBuild is meaningless with them; drop it"
}
if ($Start -and $ForceRestart) {
    Die "SYNAPSE_SETUP_MODE_CONFLICT modes=-Start,-ForceRestart remediation=-Start refuses to start on top of a live daemon rather than forcing one; run -Stop -ForceStop first if a forced takedown is genuinely intended"
}

# Validate the parameter combination before taking the maintenance lock or doing
# any preflight work (#1873). -SourceDir is consumed by candidate validation and
# model-pin verification, not only by the build, so -SkipBuild does not make it
# optional. Discovering that inside a Mandatory parameter binding stranded the
# operator with a raw PowerShell exception after several minutes of preflight,
# and without naming the parameter, the reason, or the value to supply.
if (-not $Remove -and -not $Stop -and -not $Start -and [string]::IsNullOrWhiteSpace($SourceDir)) {
    $sourceDirMessage = @(
        ("SYNAPSE_SETUP_SOURCE_DIR_REQUIRED skip_build={0} remove={1} stop={2} start={3}" -f [bool]$SkipBuild, [bool]$Remove, [bool]$Stop, [bool]$Start),
        'source_of_truth=-SourceDir parameter',
        ('remediation=pass -SourceDir <path to the synapse source checkout containing Cargo.toml>. ' +
         'It is required whether or not -SkipBuild is set, because candidate-daemon validation and ' +
         'embedded model-pin verification both read the checkout. With -SkipBuild the already-installed ' +
         'binary is what gets validated and deployed; omit -SkipBuild to build and deploy that checkout.')
    ) -join ' '
    Die $sourceDirMessage
}

# Pure-input fence (#2211). Everything in this block depends only on bound
# parameters, environment-backed parameter defaults, and read-only prerequisite
# inspection. It MUST stay before parent waiting, maintenance-lock acquisition,
# artifact cleanup, build output creation, or candidate launch. In particular,
# parameter validation attributes do not validate default values, so ActiveIssue
# must be normalized explicitly here to cover SYNAPSE_ACTIVE_ISSUE as well as an
# explicitly supplied argument.
if ($maintenanceReason -eq 'setup') {
    $ActiveIssue = Get-SynapseNormalizedIssueRef -Issue $ActiveIssue

    if ($ManualInstallHealthRollbackPauseMode -ne 'normal' -and -not $ManualInstallHealthRollbackProbe) {
        Die "SYNAPSE_MANUAL_INSTALL_HEALTH_ROLLBACK_PAUSE_MODE_WITHOUT_PROBE mode=$ManualInstallHealthRollbackPauseMode remediation=-ManualInstallHealthRollbackPauseMode is only valid with -ManualInstallHealthRollbackProbe because it intentionally changes rollback maintenance-pause behavior"
    }
    if ($ManualInstallHealthRollbackProbe) {
        if (-not $ForceRestart) {
            Die "SYNAPSE_MANUAL_INSTALL_HEALTH_ROLLBACK_PROBE_REQUIRES_FORCE_RESTART remediation=the rollback drill intentionally drains and restarts the live daemon, so rerun with -ForceRestart during a maintenance window"
        }
        if ($SkipBuild) {
            Die "SYNAPSE_MANUAL_INSTALL_HEALTH_ROLLBACK_PROBE_REQUIRES_BUILD remediation=the rollback drill needs a real candidate that differs from the installed daemon; -SkipBuild cannot produce a backup/candidate handoff"
        }
        if ($script:SynapsePostExitStartOnly) {
            Die "SYNAPSE_MANUAL_INSTALL_HEALTH_ROLLBACK_PROBE_POST_EXIT_UNSUPPORTED remediation=post-exit continuation only starts the already-installed daemon; run the rollback drill from the primary setup process"
        }
    }

    if ([string]::IsNullOrWhiteSpace($CalyxConfigPath)) {
        $CalyxConfigPath = $null
    } else {
        try {
            $CalyxConfigPath = [System.IO.Path]::GetFullPath($CalyxConfigPath)
        } catch {
            Die "SYNAPSE_CALYX_CONFIG_PATH_INVALID path=$CalyxConfigPath error=$($_.Exception.Message) remediation=pass an absolute or resolvable local TOML file path to -CalyxConfigPath"
        }
        if (-not (Test-Path -LiteralPath $CalyxConfigPath -PathType Leaf)) {
            Die "SYNAPSE_CALYX_CONFIG_FILE_MISSING path=$CalyxConfigPath remediation=create the exact [calyx] TOML file or omit -CalyxConfigPath to use validated defaults"
        }
        try {
            $configStream = [System.IO.File]::Open($CalyxConfigPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
            $configStream.Dispose()
        } catch {
            Die "SYNAPSE_CALYX_CONFIG_FILE_UNREADABLE path=$CalyxConfigPath error=$($_.Exception.Message) remediation=repair the exact file permissions before setup builds or touches the live daemon"
        }
        Info "Explicit Calyx tuning source verified path=$CalyxConfigPath; candidate and installed daemon will both receive --calyx-config."
    }

    if (-not (Test-Path -LiteralPath (Join-Path $SourceDir 'Cargo.toml') -PathType Leaf)) {
        Die "-SourceDir '$SourceDir' has no Cargo.toml. Point it at a synapse source checkout on a LOCAL drive."
    }
    if ($SourceDir -match '^\\\\' -or $SourceDir -match '^[Zz]:\\home\\') {
        Die "-SourceDir '$SourceDir' looks like a UNC / WSL-mapped path. Build from a real local copy: building over \\wsl.localhost bakes transient drive paths into the binary."
    }

    # Source profiles are part of the candidate/install contract, not optional
    # state that may be borrowed from a prior deployment (#2212). Validate once
    # and carry this exact directory through candidate preflight and deployment.
    $srcProfiles = Join-Path $SourceDir 'crates\synapse-profiles\profiles'
    if (-not (Test-Path -LiteralPath $srcProfiles -PathType Container)) {
        Die "SYNAPSE_SOURCE_PROFILES_MISSING source=$srcProfiles source_dir=$SourceDir remediation=restore the tracked crates\synapse-profiles\profiles directory in the named checkout; setup refuses to validate a new daemon against stale deployed profiles"
    }
    $candidateProfilesDir = $srcProfiles
    $candidateProfileCount = @(Get-ChildItem -LiteralPath $candidateProfilesDir -Filter '*.toml' -File).Count
    if ($candidateProfileCount -lt 1) {
        Die "SYNAPSE_SOURCE_PROFILES_EMPTY source=$candidateProfilesDir source_dir=$SourceDir remediation=restore at least one tracked bundled .toml profile; setup refuses to validate a candidate against an empty or previously deployed profile set"
    }

    # An existing token is an input prerequisite. A missing token is created in
    # the later token phase, but malformed existing bytes are knowable now and
    # must not be discovered after a release build.
    if (Test-Path -LiteralPath $TokenPath -PathType Leaf) {
        try {
            $preflightToken = (Get-Content -LiteralPath $TokenPath -Raw -ErrorAction Stop).Trim()
        } catch {
            Die "SYNAPSE_TOKEN_PREFLIGHT_READ_FAILED path=$TokenPath error=$($_.Exception.Message) remediation=repair the token file permissions or remove the unreadable file so setup can create a new token before rerunning"
        }
        if ($preflightToken.Length -lt 16) {
            Die "SYNAPSE_TOKEN_PREFLIGHT_TOO_SHORT path=$TokenPath chars=$($preflightToken.Length) remediation=remove the malformed token and rerun setup so a cryptographically random replacement can be created before candidate launch"
        }
        Remove-Variable -Name preflightToken -ErrorAction SilentlyContinue
    }

    $cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
    if (-not $SkipBuild) {
        if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) {
            Die "cargo not found at $cargo. Install the Rust toolchain (https://rustup.rs) on Windows, then re-run. Synapse builds with the current stable toolchain."
        }

        # #1819: Cargo resolves these path dependencies while parsing manifests,
        # before a build script can emit a useful diagnostic.
        $vendoredCalyxCrates = Join-Path $SourceDir 'calyx\crates'
        $requiredCalyxCrates = @(
            'calyx-assay', 'calyx-aster', 'calyx-core', 'calyx-forge', 'calyx-ledger',
            'calyx-lodestar', 'calyx-loom', 'calyx-paths', 'calyx-registry',
            'calyx-search', 'calyx-sextant'
        )
        $missingCalyxCrates = @(
            $requiredCalyxCrates | Where-Object {
                -not (Test-Path -LiteralPath (Join-Path $vendoredCalyxCrates (Join-Path $_ 'Cargo.toml')) -PathType Leaf)
            }
        )
        if ($missingCalyxCrates.Count -gt 0) {
            Die ("SYNAPSE_VENDORED_CALYX_TREE_MISSING source_dir=$SourceDir " +
                 "vendored_path=$vendoredCalyxCrates missing_crate_count=$($missingCalyxCrates.Count) " +
                 "missing_crates=$($missingCalyxCrates -join ',') " +
                 "detail=crates\synapse-calyx depends on these through cargo path dependencies, which are " +
                 "resolved before any build script runs, so the build cannot report this itself. " +
                 "The tree is tracked in git and is NOT gitignored (.gitignore excludes only /calyx/target/). " +
                 "remediation=run 'git checkout -- calyx' in $SourceDir to restore the vendored Calyx " +
                 "workspace, confirm 'git status' reports no deletions under calyx/, then re-run setup")
        }

        # Resolve/authorize the output tree and select the physical math build
        # capability before maintenance state or acquisition artifacts change.
        $cargoTargetResolution = Resolve-SynapseCargoTargetDirectory `
            -SourceDir $SourceDir `
            -Requested $CargoTarget `
            -AllowAlternate ([bool]$AllowAlternateBuildTarget)
        $CargoTarget = $cargoTargetResolution.path
        $cudaBuildCapability = Get-SynapseCudaBuildCapability
    }
}

Wait-SynapsePostExitParent -ParentPid $PostExitParentPid -Reason $PostExitContinuationReason
Acquire-SynapseSetupMaintenanceLock -Path $MaintenanceLockPath -Reason $maintenanceReason
Remove-SynapseStaleDaemonStagingArtifacts -LogDir $LogDir
Resume-SynapseCandidateCleanupIntents -Root (Join-Path $LogDir 'setup-candidates')
Remove-SynapseStaleSuccessfulCandidateArtifacts -Root (Join-Path $LogDir 'setup-candidates')

if ($ResumeChromeBridgePending) {
    Invoke-SynapseChromeBridgePendingResume -CheckpointPath $ChromeBridgePendingPath
    Release-SynapseSetupMaintenanceLock -State released
    return
}

if ($Stop) {
    Invoke-SynapseDaemonStop `
        -TaskName $TaskName `
        -RuntimeBinDir $RuntimeBinDir `
        -Bind $Bind `
        -DbPath $DbPath `
        -TokenPath $TokenPath `
        -LogDir $LogDir `
        -ForceRestart:$ForceRestart
    Release-SynapseSetupMaintenanceLock -State released
    return
}

if ($Start) {
    Invoke-SynapseDaemonStart `
        -TaskName $TaskName `
        -RuntimeBinDir $RuntimeBinDir `
        -Bind $Bind `
        -DbPath $DbPath `
        -TokenPath $TokenPath `
        -LogDir $LogDir
    Release-SynapseSetupMaintenanceLock -State released
    return
}

if ($Remove) {
    Step "Removing scheduled task '$TaskName'"
    Assert-SynapseRestartAllowed -Reason 'remove' -Bind $Bind -DbPath $DbPath -TokenPath $TokenPath -HealthTimeoutSec ([Math]::Min(300, [Math]::Max(120, $InstallHealthTimeoutSeconds))) -ForceRestart:$ForceRestart
    $daemonSupervisorPath = Join-Path $RuntimeBinDir 'synapse-daemon-supervisor.ps1'
    Remove-SynapseDaemonTaskRestartAuthority -TaskName $TaskName -SupervisorPath $daemonSupervisorPath -Reason 'remove'
    Stop-SynapseMcpProcesses -Reason 'remove' -Bind $Bind -DbPath $DbPath -TokenPath $TokenPath -ForceRestart:$ForceRestart
    if ($Purge) {
        Write-SynapseVaultDeletionRecord -DbPath $DbPath -Reason 'setup-remove-purge' -Confirmed:$ConfirmVaultDestruction
        foreach ($p in @($DbPath, $ProfilesDir, (Split-Path -Parent $TokenPath))) {
            if (Test-Path $p) { Remove-Item -Recurse -Force $p; Info "Deleted $p" }
        }
    }
    Info "Done (remove)."
    Release-SynapseSetupMaintenanceLock -State released
    return
}

# ---------------------------------------------------------------------------
# 1. Preflight
# ---------------------------------------------------------------------------
Step "Preflight"
if ($ManualInstallHealthRollbackProbe) {
    Info "Manual install-health rollback probe armed pause_mode=$ManualInstallHealthRollbackPauseMode; setup will reject the first installed daemon health readback and exit fail-loud after rollback readback"
}
if (-not $SkipBuild) {
    Info "Vendored Calyx workspace verified: $($requiredCalyxCrates.Count) path-dependency crates present under $vendoredCalyxCrates"
    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
    $cargoVersionLog = Join-Path $LogDir 'setup-cargo-version.log'
    $cargoVersionDiagnosticsPath = Join-Path $LogDir 'setup-cargo-version-diagnostics.json'
    if (Test-Path -LiteralPath $cargoVersionDiagnosticsPath) { Remove-Item -LiteralPath $cargoVersionDiagnosticsPath -Force }
    $cargoVersionDiagnostics = $null
    $cargoVersionExit = Invoke-SynapseProcessInKillOnCloseJob `
        -FilePath $cargo `
        -ArgumentList @('--version') `
        -WorkingDirectory $SourceDir `
        -TimeoutMinutes 1 `
        -LogPath $cargoVersionLog `
        -Diagnostics ([ref]$cargoVersionDiagnostics)
    $cargoVersionLogSignal = Get-SynapseBuildLogSignal -Path $cargoVersionLog
    if ($cargoVersionExit -ne 0) {
        $failureKind = Get-SynapseCargoVersionFailureKind -Diagnostics $cargoVersionDiagnostics
        $versionFailure = [ordered]@{
            schema = 'synapse_setup_cargo_version_failure/v1'
            code = $failureKind.code
            source_dir = $SourceDir
            cargo = $cargo
            version_log = $cargoVersionLog
            preflight_timeout_minutes = 1
            version_exit = $cargoVersionExit
            remediation = $failureKind.remediation
            invocation = $cargoVersionDiagnostics
            log_signal = $cargoVersionLogSignal
        }
        $versionFailure | ConvertTo-Json -Depth 32 | Set-Content -LiteralPath $cargoVersionDiagnosticsPath -Encoding UTF8
        $job = $cargoVersionDiagnostics.process_job
        $childPid = if ($job -and $job.child_pid) { $job.child_pid } else { '<unknown>' }
        $completionKind = if ($job -and $job.completion_kind) { $job.completion_kind } else { '<unknown>' }
        $waitKind = if ($job -and $job.wait_kind) { $job.wait_kind } else { '<unknown>' }
        $terminateJobOk = if ($job) { [string]$job.terminate_job_ok } else { '<unknown>' }
        $cleanupWaitKind = if ($job -and $job.cleanup_wait_kind) { $job.cleanup_wait_kind } else { '<unknown>' }
        $childAliveAfter = if ($cargoVersionDiagnostics.cleanup_result) { [string]$cargoVersionDiagnostics.cleanup_result.child_process_alive_after } else { '<unknown>' }
        Die ("{0} exit={1} child_pid={2} child_alive_after={3} completion={4} wait={5} timeout_minutes=1 terminate_job_ok={6} cleanup_wait={7} diagnostics={8} log={9} remediation={10}`nTail:`n{11}" -f `
            $failureKind.code,
            $cargoVersionExit,
            $childPid,
            $childAliveAfter,
            $completionKind,
            $waitKind,
            $terminateJobOk,
            $cleanupWaitKind,
            $cargoVersionDiagnosticsPath,
            $cargoVersionLog,
            $failureKind.remediation,
            $cargoVersionLogSignal.tail_80)
    }
    $cargoVersionText = if (Test-Path -LiteralPath $cargoVersionLog) {
        ((Get-Content -LiteralPath $cargoVersionLog -ErrorAction SilentlyContinue) -join "`n").Trim()
    } else {
        ''
    }
    if ([string]::IsNullOrWhiteSpace($cargoVersionText)) {
        $emptyVersionFailure = [ordered]@{
            schema = 'synapse_setup_cargo_version_failure/v1'
            code = 'SYNAPSE_CARGO_VERSION_EMPTY'
            source_dir = $SourceDir
            cargo = $cargo
            version_log = $cargoVersionLog
            preflight_timeout_minutes = 1
            version_exit = $cargoVersionExit
            remediation = 'cargo --version exited 0 but produced no version text; repair Rust toolchain stdout/stderr before setup continues'
            invocation = $cargoVersionDiagnostics
            log_signal = $cargoVersionLogSignal
        }
        $emptyVersionFailure | ConvertTo-Json -Depth 32 | Set-Content -LiteralPath $cargoVersionDiagnosticsPath -Encoding UTF8
        Die "SYNAPSE_CARGO_VERSION_EMPTY log=$cargoVersionLog remediation=cargo --version exited 0 but produced no version text; repair Rust toolchain stdout/stderr before setup continues"
    }
    Info "cargo: $cargoVersionText"
}

# ---------------------------------------------------------------------------
# 2. Build (local source -> persistent target) and verify the binary
# ---------------------------------------------------------------------------
Set-SynapseCudaBuildEnvironment
$ortRuntimeRoot = Join-Path $env:LOCALAPPDATA 'synapse\runtime\onnxruntime-gpu'
$embeddedModelRoot = Join-Path $env:LOCALAPPDATA 'synapse\build-models'
$acquisitionCleanup = Remove-SynapseStaleAcquisitionArtifacts -Roots @($ortRuntimeRoot, $embeddedModelRoot)
$acquisitionCleanupPath = Join-Path $LogDir 'setup-acquisition-cleanup.json'
$acquisitionCleanup | ConvertTo-Json -Depth 16 | Set-Content -LiteralPath $acquisitionCleanupPath -Encoding UTF8
try {
    $acquisitionCleanupReadback = Get-Content -LiteralPath $acquisitionCleanupPath -Raw -Encoding UTF8 | ConvertFrom-Json
} catch {
    Die "SYNAPSE_ACQUISITION_CLEANUP_READBACK_UNREADABLE path=$acquisitionCleanupPath error=$($_.Exception.Message) remediation=repair the setup log directory; setup cannot prove which stale acquisition bytes it reclaimed"
}
if ([int64]$acquisitionCleanupReadback.reclaimed_bytes -ne [int64]$acquisitionCleanup.reclaimed_bytes -or
    [int]$acquisitionCleanupReadback.reaped_count -ne [int]$acquisitionCleanup.reaped_count) {
    Die "SYNAPSE_ACQUISITION_CLEANUP_READBACK_MISMATCH path=$acquisitionCleanupPath expected_reaped_count=$($acquisitionCleanup.reaped_count) actual_reaped_count=$($acquisitionCleanupReadback.reaped_count) expected_reclaimed_bytes=$($acquisitionCleanup.reclaimed_bytes) actual_reclaimed_bytes=$($acquisitionCleanupReadback.reclaimed_bytes) remediation=inspect the setup log filesystem; the durable cleanup ledger differs from the in-memory result"
}
$acquisitionCleanupSha = Get-SynapseFileSha256 -Path $acquisitionCleanupPath
Info "Acquisition cleanup readback -> $acquisitionCleanupPath sha256=$acquisitionCleanupSha reaped_count=$($acquisitionCleanupReadback.reaped_count) reclaimed_bytes=$($acquisitionCleanupReadback.reclaimed_bytes) retained_live_count=$($acquisitionCleanupReadback.retained_live_count)"
$ortRuntime = Install-SynapsePinnedOrtGpuRuntime -Root $ortRuntimeRoot
$env:ORT_LIB_LOCATION = $ortRuntime.NativeDir
$env:ORT_PREFER_DYNAMIC_LINK = '1'
Info "ONNX Runtime build linkage configured mode=dynamic ORT_LIB_LOCATION=$($env:ORT_LIB_LOCATION) version=$($ortRuntime.Version)"

if (-not $SkipBuild) {
    Step "Building synapse-mcp (release) from $SourceDir"
    $releaseBuildCompilerEnvironment = Set-SynapseReleaseBuildCompilerEnvironment
    Info ("Release build compiler environment: RUST_MIN_STACK={0} source={1}" -f `
        $releaseBuildCompilerEnvironment.rust_min_stack,
        $releaseBuildCompilerEnvironment.rust_min_stack_source)
    # The build target is the source checkout's own `target` tree unless the
    # operator explicitly authorized an alternate. Because that tree belongs to
    # exactly one checkout it is inherently per-checkout, so it cannot reproduce
    # the cross-checkout fingerprint poisoning that the old hashed
    # %LOCALAPPDATA% cache existed to prevent (observed live 2026-06-12: a
    # synapse-core compiled from a sibling clone shadowed new modules and broke
    # the deploy build). Cargo freshness is mtime-based against the dep-info
    # file list, and a checkout-owned tree is only ever written by that one
    # checkout's builds.
    Info ("Cargo target directory: {0} (kind={1} authorized_by={2} source_dir={3})" -f `
        $cargoTargetResolution.path, $cargoTargetResolution.kind,
        $cargoTargetResolution.authorized_by, $cargoTargetResolution.source_dir)

    $alternateBuildTargetInventory = Get-SynapseAlternateBuildTargetInventory
    if ($alternateBuildTargetInventory.exists -and @($alternateBuildTargetInventory.trees).Count -gt 0) {
        Info ("Pre-existing alternate build trees under {0}: count={1} total_gib={2}. Setup never removes these implicitly; remove one only after proving exact ownership and age." -f `
            $alternateBuildTargetInventory.root,
            @($alternateBuildTargetInventory.trees).Count,
            $alternateBuildTargetInventory.total_gib)
        foreach ($tree in $alternateBuildTargetInventory.trees) {
            Info ("  alternate_build_tree path={0} gib={1} files={2} last_write_utc={3} age_days={4}" -f `
                $tree.path, $tree.footprint.gib, $tree.footprint.file_count, $tree.last_write_utc, $tree.age_days)
        }
    }

    # Fail closed BEFORE burning a full release build: if any live process runs
    # its image out of the cargo target tree, Windows will refuse to let cargo
    # replace its own link output and the build dies at the very end (#1865).
    # The collision is knowable here, so the operator never pays ~20 minutes to
    # learn it.
    $buildOutputImageHolders = Get-SynapseBuildOutputImageHolders -TargetDir $CargoTarget
    if (-not $buildOutputImageHolders.process_table_read) {
        Die ("SYNAPSE_RELEASE_BUILD_OUTPUT_HOLDER_PREFLIGHT_UNREADABLE cargo_target_dir={0} error={1} remediation=setup cannot prove no live process runs from the build output tree; repair Win32_Process access for this session before rerunning setup" -f `
            $CargoTarget, $buildOutputImageHolders.process_table_error)
    }
    if (@($buildOutputImageHolders.holders).Count -gt 0) {
        Die ("SYNAPSE_RELEASE_BUILD_OUTPUT_DIR_IMAGE_LIVE cargo_target_dir={0} holder_count={1} holders={2} remediation=Windows locks a running executable image, so cargo cannot replace its own link output; stop each PID listed above (verify it exited) and rerun setup. Running the daemon directly out of target\release is the usual cause; deploy through setup so the live daemon runs from the installed path instead." -f `
            $buildOutputImageHolders.target_dir,
            @($buildOutputImageHolders.holders).Count,
            (Format-SynapseBuildOutputImageHolders -Readback $buildOutputImageHolders))
    }
    Info ("Build output holder preflight: cargo_target_dir={0} live_images_under_target=0 unreadable_path_processes={1}" -f `
        $buildOutputImageHolders.target_dir,
        @($buildOutputImageHolders.unreadable_path_processes).Count)

    New-Item -ItemType Directory -Force -Path $CargoTarget, $LogDir | Out-Null
    $env:CARGO_TARGET_DIR = $CargoTarget
    if (-not $env:CARGO_BUILD_JOBS) {
        # Size the build to the whole machine, RAM-guarded. CARGO_BUILD_JOBS
        # (env) outranks any `[build] jobs = N` in a user or repo config.toml,
        # so this holds regardless of local cargo configuration.
        #
        # A hard `Min(8, ...)` cap lived here from 2026-07-22 to 2026-08-06 as a
        # mitigation for the rustc STATUS_ACCESS_VIOLATION crashes on #1731. It
        # is removed because the evidence says it never mitigated anything:
        #
        #   * The crash it was added for was a rustc 1.96.1 fault. That compiler
        #     was replaced on 2026-07-26 (rust-toolchain.toml -> 1.97.1, c9a6b85a)
        #     precisely because it "repeatedly crashed".
        #   * The crash then recurred anyway AT 8 JOBS. The archived readback
        #     logs/setup-build-failures/release-build-20260804T004949861Z-pid27796
        #     records code=SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASHED,
        #     cargo_build_jobs=8, crash_tool=rustc.exe,
        #     crash_status=STATUS_ACCESS_VIOLATION.
        #   * It is not resource exhaustion: that same readback shows 46 GB
        #     physical still available and commit at 52.3%.
        #   * It then recurred AT 32 JOBS too (2026-08-06, archive
        #     release-build-20260806T174951173Z-pid46444, 72.8 GB free). Same
        #     fault at both ends of the range: job count is not a lever on it in
        #     EITHER direction, so the cap could not have been preventing it.
        #
        # An earlier version of this comment claimed "upstream places this fault
        # in the ThinLTO LLVM codegen workers (rust-lang/rust#125765, #109067,
        # #113433)". That citation was checked in #2029 and DOES NOT SUPPORT the
        # claim -- do not propagate it. #125765 is closed as not-a-bug (the
        # reporter's build was OOMing under an 8 GiB Docker limit) and is on
        # windows-gnu; #109067 is a miscompilation needing -Zdylib-lto; #113433
        # is an unmerged PR about building rustc itself with LTO. All three are
        # 2023-2024 and none is a compile-time AV under `-C lto=thin` on
        # windows-msvc. As of 2026-08-07 there is NO open upstream bug matching
        # this signature, and 1.97.1 is already the newest stable, so there is
        # no toolchain to bump to either.
        #
        # The live arm is the release profile's `lto = "thin"` -> false, taken
        # 2026-08-07 in the root Cargo.toml (#2029) -- where the evidence and
        # the falsification criterion live. It is NOT worked around here.
        #
        # So the cap cost a 4x parallelism throttle on the single most expensive
        # phase of setup (8 of 32 jobs on this host) while demonstrably not
        # preventing the crash it was named for. An explicit operator-set
        # CARGO_BUILD_JOBS is still honored untouched.
        $logicalCpus = [Environment]::ProcessorCount
        $ramGb = [math]::Floor((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
        # Heavy rustc/cl.exe jobs peak around 1.5 GB each; on a low-RAM host cap
        # the count so the build does not swap, which costs far more than
        # running fewer jobs. On a well-provisioned host this never binds.
        $memoryJobs = [math]::Max(1, [math]::Floor($ramGb / 1.5))
        $env:CARGO_BUILD_JOBS = [string][int][math]::Min($logicalCpus, $memoryJobs)
    }
    # cargo forwards this to build scripts as NUM_JOBS, but cmake-driven -sys
    # crates read CMAKE_BUILD_PARALLEL_LEVEL instead and otherwise serialize
    # their native compiles. synapse-update.ps1 has always set this; setup lost
    # it, so the same source tree built with different native parallelism
    # depending on which entry point the operator used.
    if (-not $env:CMAKE_BUILD_PARALLEL_LEVEL) {
        $env:CMAKE_BUILD_PARALLEL_LEVEL = $env:CARGO_BUILD_JOBS
    }
    Info "Build parallelism: CARGO_BUILD_JOBS=$($env:CARGO_BUILD_JOBS) CMAKE_BUILD_PARALLEL_LEVEL=$($env:CMAKE_BUILD_PARALLEL_LEVEL) (logical CPUs: $([Environment]::ProcessorCount))"
    $buildLog = Join-Path $LogDir 'setup-build.log'
    $buildDiagnosticsPath = Join-Path $LogDir 'setup-build-diagnostics.json'
    $buildInvocationPath = Join-Path $LogDir 'setup-build-invocation.json'
    # Preserve the last failure diagnostics across later successful retries;
    # each new failure also writes an immutable per-attempt archive below.
    $built = Join-Path $CargoTarget 'release\synapse-mcp.exe'
    Info "Build process tree is job-owned; log: $buildLog"
    $buildInvocationDiagnostics = $null
    $cargoBuildArgs = @('build','--release','-p','synapse-mcp')
    if (@($cudaBuildCapability.cargo_features).Count -gt 0) {
        $cargoBuildArgs += @('--features', ($cudaBuildCapability.cargo_features -join ','))
    }
    Info ("Cargo invocation: {0} {1} (CARGO_TARGET_DIR={2})" -f $cargo, ($cargoBuildArgs -join ' '), $env:CARGO_TARGET_DIR)

    # Bounded retry on a PHYSICALLY CRASHED toolchain only (#1975 ask 3). Every
    # other failure classification is still fatal on the first attempt. See
    # Get-SynapseReleaseBuildToolchainCrashRetryBudget for why this is a gate
    # rather than a fallback.
    $crashRetryBudget = Get-SynapseReleaseBuildToolchainCrashRetryBudget
    $maxBuildAttempts = 1 + $crashRetryBudget.budget
    $buildAttempt = 0
    $toolchainCrashHistory = @()
    Info ("Release build toolchain-crash retry budget: {0} extra attempt(s) (max_attempts={1} source={2}); retries fire ONLY on SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASHED" -f `
        $crashRetryBudget.budget, $maxBuildAttempts, $crashRetryBudget.source)

    while ($true) {
    $buildAttempt++
    $buildInvocationDiagnostics = $null
    $buildMemoryBefore = Get-SynapseWindowsMemoryReadback
    Info ("Release build attempt {0} of {1} (max). Memory before: read_succeeded={2} commit_charge_bytes={3} commit_limit_bytes={4} commit_percent={5} available_physical_bytes={6}" -f `
        $buildAttempt,
        $maxBuildAttempts,
        $buildMemoryBefore.read_succeeded,
        $buildMemoryBefore.commit_charge_bytes,
        $buildMemoryBefore.commit_limit_bytes,
        $buildMemoryBefore.commit_percent,
        $buildMemoryBefore.available_physical_bytes)
    $buildExit = Invoke-SynapseProcessInKillOnCloseJob `
        -FilePath $cargo `
        -ArgumentList $cargoBuildArgs `
        -WorkingDirectory $SourceDir `
        -TimeoutMinutes $BuildTimeoutMinutes `
        -LogPath $buildLog `
        -Diagnostics ([ref]$buildInvocationDiagnostics)
    $buildMemoryAfter = Get-SynapseWindowsMemoryReadback
    Info ("Release build memory after: read_succeeded={0} commit_charge_bytes={1} commit_limit_bytes={2} commit_percent={3} available_physical_bytes={4}" -f `
        $buildMemoryAfter.read_succeeded,
        $buildMemoryAfter.commit_charge_bytes,
        $buildMemoryAfter.commit_limit_bytes,
        $buildMemoryAfter.commit_percent,
        $buildMemoryAfter.available_physical_bytes)
    $buildInvocationDiagnostics |
        ConvertTo-Json -Depth 32 |
        Set-Content -LiteralPath $buildInvocationPath -Encoding UTF8
    $buildInvocationReadback = Get-Content -LiteralPath $buildInvocationPath -Raw |
        ConvertFrom-Json
    if ($buildInvocationReadback.schema -ne 'synapse_setup_process_job_invocation/v2' -or
        -not $buildInvocationReadback.process_job -or
        -not $buildInvocationReadback.process_job.child_pid) {
        Die "SYNAPSE_RELEASE_BUILD_INVOCATION_DIAGNOSTICS_INVALID path=$buildInvocationPath remediation=inspect filesystem integrity and setup process-job serialization"
    }
    Info "Build invocation diagnostics: $buildInvocationPath"
    if ($buildExit -eq 0) {
        # A build that only succeeded after a crash is NOT reported as a clean
        # build. The crash history is stated on success too, so the #1975 rate
        # keeps accruing instead of disappearing the moment a retry works.
        if (@($toolchainCrashHistory).Count -gt 0) {
            Warn ("Release build SUCCEEDED on attempt {0} of {1} after {2} toolchain crash(es): {3}. This is #1975; the crash is probabilistic on this host and each crashed attempt archived its own diagnostics." -f `
                $buildAttempt,
                $maxBuildAttempts,
                @($toolchainCrashHistory).Count,
                (($toolchainCrashHistory | ForEach-Object { "attempt $($_.attempt): $($_.tool) $($_.status) $($_.exit_code) -> $($_.diagnostics_archive)" }) -join ' | '))
        }
        break
    }

    # ---- failure path: classify, archive, then decide retry vs die ----
        $buildLogSignal = Get-SynapseBuildLogSignal -Path $buildLog
        $artifactReadback = Get-SynapseArtifactReadback -Path $built
        # Re-enumerate at failure time: a process can have started running out of
        # the target tree during the build itself, and the classifier must name
        # the exact PID/image rather than assume the preflight state still holds.
        $buildOutputImageHoldersAfter = Get-SynapseBuildOutputImageHolders -TargetDir $CargoTarget
        $failureKind = Get-SynapseReleaseBuildFailureKind `
            -Diagnostics $buildInvocationDiagnostics `
            -LogSignal $buildLogSignal `
            -ArtifactReadback $artifactReadback `
            -OutputImageHolders $buildOutputImageHoldersAfter
        $buildFailureStamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssfffZ')
        $buildFailureDir = Join-Path $LogDir 'setup-build-failures'
        New-Item -ItemType Directory -Force -Path $buildFailureDir | Out-Null
        $buildFailurePrefix = "release-build-$buildFailureStamp-pid$PID"
        $buildLogArchivePath = Join-Path $buildFailureDir "$buildFailurePrefix.build.log"
        $buildDiagnosticsArchivePath = Join-Path $buildFailureDir "$buildFailurePrefix.diagnostics.json"
        $buildLogArchiveSha256 = $null
        if (Test-Path -LiteralPath $buildLog) {
            Copy-Item -LiteralPath $buildLog -Destination $buildLogArchivePath -Force
            try {
                $buildLogArchiveSha256 = Get-SynapseFileSha256 -Path $buildLogArchivePath
            } catch {
                $buildLogArchiveSha256 = "hash_failed: $($_.Exception.Message)"
            }
        }
        $buildStartedAtUtc = $null
        if ($buildInvocationDiagnostics -and $buildInvocationDiagnostics.started_at_utc) {
            $buildStartedAtUtc = [string]$buildInvocationDiagnostics.started_at_utc
        }
        $werCrashReadback = Get-SynapseWerCrashReadback -SinceUtc $buildStartedAtUtc
        $rustToolchainReadback = Get-SynapseRustToolchainReadback -CargoPath $cargo
        $buildDiagnostics = [ordered]@{
            schema = 'synapse_setup_release_build_failure/v1'
            code = $failureKind.code
            attempt_id = $buildFailurePrefix
            observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
            source_dir = $SourceDir
            cargo = $cargo
            cargo_target_dir = $CargoTarget
            expected_artifact = $built
            build_log = $buildLog
            archived_build_log = $buildLogArchivePath
            archived_build_log_sha256 = $buildLogArchiveSha256
            build_timeout_minutes = $BuildTimeoutMinutes
            build_exit = $buildExit
            remediation = $failureKind.remediation
            diagnostics_archive = $buildDiagnosticsArchivePath
            compiler_environment = $releaseBuildCompilerEnvironment
            cargo_build_jobs = $env:CARGO_BUILD_JOBS
            memory_before = $buildMemoryBefore
            memory_after = $buildMemoryAfter
            invocation = $buildInvocationDiagnostics
            log_signal = $buildLogSignal
            artifact_readback = $artifactReadback
            build_output_image_holders_preflight = $buildOutputImageHolders
            build_output_image_holders_after = $buildOutputImageHoldersAfter
            rust_toolchain_readback = $rustToolchainReadback
            wer_crash_readback = $werCrashReadback
        }
        $buildDiagnosticsJson = $buildDiagnostics | ConvertTo-Json -Depth 32
        $buildDiagnosticsJson | Set-Content -LiteralPath $buildDiagnosticsPath -Encoding UTF8
        $buildDiagnosticsJson | Set-Content -LiteralPath $buildDiagnosticsArchivePath -Encoding UTF8
        $job = $buildInvocationDiagnostics.process_job
        $childPid = if ($job -and $job.child_pid) { $job.child_pid } else { '<unknown>' }
        $completionKind = if ($job -and $job.completion_kind) { $job.completion_kind } else { '<unknown>' }
        $waitKind = if ($job -and $job.wait_kind) { $job.wait_kind } else { '<unknown>' }
        $terminateJobOk = if ($job) { [string]$job.terminate_job_ok } else { '<unknown>' }
        $cleanupWaitKind = if ($job -and $job.cleanup_wait_kind) { $job.cleanup_wait_kind } else { '<unknown>' }
        $compilerError = if ($buildLogSignal.has_compiler_error) { 'true' } else { 'false' }
        $childAliveAfter = if ($buildInvocationDiagnostics.cleanup_result) { [string]$buildInvocationDiagnostics.cleanup_result.child_process_alive_after } else { '<unknown>' }
        $ownedBuildToolCount = @($buildInvocationDiagnostics.job_owned_build_tool_processes_after).Count
        $unrelatedBuildToolCount = @($buildInvocationDiagnostics.unrelated_build_tool_processes_after).Count
        $recentWerDumpCount = if ($werCrashReadback) { [int]$werCrashReadback.recent_dump_count } else { 0 }
        $outputLocked = if ($buildLogSignal.has_output_locked_error) { 'true' } else { 'false' }
        $outputHolderText = Format-SynapseBuildOutputImageHolders -Readback $buildOutputImageHoldersAfter
        # Printed beside compiler_error so the two can never be read as the same
        # fact again (#1975): a crashed tool sets this and clears that.
        $toolchainCrash = if ($buildLogSignal.has_toolchain_crash) {
            "true(tool={0} status={1} exit={2})" -f `
                ($(if ($buildLogSignal.toolchain_crash_tool) { $buildLogSignal.toolchain_crash_tool } else { '<unnamed>' })),
                ($(if ($buildLogSignal.toolchain_crash_status) { $buildLogSignal.toolchain_crash_status } else { '<unnamed>' })),
                ($(if ($buildLogSignal.toolchain_crash_exit_code) { $buildLogSignal.toolchain_crash_exit_code } else { '<unknown>' }))
        } else { 'false' }

        # The ONLY retryable state, and it takes TWO independent facts, not one.
        #
        # (1) the classification is TOOLCHAIN_CRASHED -- a named build tool was
        #     killed by the OS on an NTSTATUS; and
        # (2) the log carries ZERO source diagnostics.
        #
        # (2) is not redundant. TOOLCHAIN_CRASHED is also reached when cargo
        #     ITSELF dies on an access violation, and that branch fires whether
        #     or not the log already carried real `error[E....]` diagnostics --
        #     the classifier says so explicitly ("The log ALSO carries N
        #     source-diagnostic line(s)"). Retrying that would spend the whole
        #     budget re-proving a deterministic source error. The retry must mean
        #     "there is provably nothing here for a human to repair", so it is
        #     gated on the absence of diagnostics rather than on the crash alone.
        $sourceDiagnosticCount = @($buildLogSignal.compiler_error_matches).Count
        if ($failureKind.code -eq 'SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASHED' -and
            $sourceDiagnosticCount -eq 0 -and
            $buildAttempt -lt $maxBuildAttempts) {
            $toolchainCrashHistory += [pscustomobject]@{
                attempt = $buildAttempt
                observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
                tool = $(if ($buildLogSignal.toolchain_crash_tool) { $buildLogSignal.toolchain_crash_tool } else { '<unnamed>' })
                status = $(if ($buildLogSignal.toolchain_crash_status) { $buildLogSignal.toolchain_crash_status } else { '<unnamed>' })
                exit_code = $(if ($buildLogSignal.toolchain_crash_exit_code) { $buildLogSignal.toolchain_crash_exit_code } else { '<unknown>' })
                build_exit = $buildExit
                source_diagnostic_count = @($buildLogSignal.compiler_error_matches).Count
                diagnostics_archive = $buildDiagnosticsArchivePath
                log_archive = $buildLogArchivePath
                rust_min_stack = $releaseBuildCompilerEnvironment.rust_min_stack
            }
            Warn ("SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASHED_RETRYING attempt={0} of {1} tool={2} status={3} exit={4} source_diagnostics={5} rust_min_stack={6} diagnostics_archive={7} log_archive={8} reason=the toolchain crashed with zero source diagnostics, which no source change can repair; retrying the build. Every dependency is cached, so this attempt rebuilds only the final crate." -f `
                $buildAttempt,
                $maxBuildAttempts,
                $toolchainCrashHistory[-1].tool,
                $toolchainCrashHistory[-1].status,
                $toolchainCrashHistory[-1].exit_code,
                $toolchainCrashHistory[-1].source_diagnostic_count,
                $toolchainCrashHistory[-1].rust_min_stack,
                $buildDiagnosticsArchivePath,
                $buildLogArchivePath)
            continue
        }

        # Falling through with a crash classification means the budget is spent.
        # Say so explicitly and attach every attempt, so "it crashed 3 times" is
        # distinguishable from "it crashed once" without reading the log dir.
        $crashHistoryText = if (@($toolchainCrashHistory).Count -gt 0) {
            "attempts_crashed={0} history={1}" -f `
                (@($toolchainCrashHistory).Count + 1),
                (($toolchainCrashHistory | ForEach-Object { "#$($_.attempt) $($_.tool) $($_.status) $($_.exit_code) archive=$($_.diagnostics_archive)" }) -join ' | ')
        } else { 'attempts_crashed=0' }
        if ($failureKind.code -eq 'SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASHED' -and $sourceDiagnosticCount -gt 0) {
            Warn ("SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASH_NOT_RETRIED source_diagnostics={0} reason=a build tool crashed, but the log also carries real source diagnostics. Those are deterministic and a retry cannot clear them, so the crash-retry budget was deliberately NOT spent. Repair the diagnostics first; if the crash then persists on a clean tree it is the #1975 fault and will be retried." -f `
                $sourceDiagnosticCount)
        }
        elseif ($failureKind.code -eq 'SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASHED') {
            Warn ("SYNAPSE_RELEASE_BUILD_TOOLCHAIN_CRASH_RETRY_BUDGET_EXHAUSTED max_attempts={0} budget_source={1} rust_min_stack={2} {3} conclusion=the crash reproduced on every attempt, so it is NOT the probabilistic #1975 fault this budget exists for; treat it as deterministic and investigate the toolchain (release profile lto/codegen-units, rustc version) rather than rerunning." -f `
                $maxBuildAttempts,
                $crashRetryBudget.source,
                $releaseBuildCompilerEnvironment.rust_min_stack,
                $crashHistoryText)
        }
        Die ("{0} attempt={25} of {26} crash_history=[{27}] exit={1} child_pid={2} child_alive_after={3} completion={4} wait={5} timeout_minutes={6} terminate_job_ok={7} cleanup_wait={8} compiler_error={9} toolchain_crash={24} output_locked={22} output_dir_live_images={23} job_owned_build_tools_after={10} unrelated_build_tools_after={11} artifact_exists={12} artifact_sha256={13} artifact_exclusive_open={14} diagnostics={15} diagnostics_archive={16} log={17} log_archive={18} wer_recent_dumps={19} remediation={20}`nTail:`n{21}" -f `
            $failureKind.code,
            $buildExit,
            $childPid,
            $childAliveAfter,
            $completionKind,
            $waitKind,
            $BuildTimeoutMinutes,
            $terminateJobOk,
            $cleanupWaitKind,
            $compilerError,
            $ownedBuildToolCount,
            $unrelatedBuildToolCount,
            $artifactReadback.exists,
            ($(if ($artifactReadback.sha256) { $artifactReadback.sha256 } else { '<none>' })),
            $artifactReadback.exclusive_open,
            $buildDiagnosticsPath,
            $buildDiagnosticsArchivePath,
            $buildLog,
            $buildLogArchivePath,
            $recentWerDumpCount,
            $failureKind.remediation,
            $buildLogSignal.tail_80,
            $outputLocked,
            $outputHolderText,
            $toolchainCrash,
            $buildAttempt,
            $maxBuildAttempts,
            $crashHistoryText)
    }
    if (-not (Test-Path $built)) { Die "Build reported success but $built is missing." }
    Info "Built: $built ($([math]::Round((Get-Item $built).Length/1MB,1)) MB)"

    # Durable readback naming the EXACT build tree and feature set used, so an
    # operator can prove after the fact where artifacts landed (#1857) and which
    # acceleration was compiled in (#1859) without re-deriving it from logs.
    $buildTargetReadbackPath = Join-Path $LogDir 'setup-build-target.json'
    $builtArtifact = Get-Item -LiteralPath $built
    $buildTargetReadback = [ordered]@{
        schema = 'synapse_setup_build_target_readback/v1'
        observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
        source_dir = $cargoTargetResolution.source_dir
        cargo_target_dir = $cargoTargetResolution.path
        cargo_target_dir_kind = $cargoTargetResolution.kind
        cargo_target_dir_authorized_by = $cargoTargetResolution.authorized_by
        cargo_target_dir_env = $env:CARGO_TARGET_DIR
        canonical_checkout_target = (Join-Path $cargoTargetResolution.source_dir 'target')
        alternate_target_footprint = $cargoTargetResolution.alternate_footprint
        pre_existing_alternate_build_targets = $alternateBuildTargetInventory
        cargo_invocation = ($cargoBuildArgs -join ' ')
        cuda_build_capability = $cudaBuildCapability
        artifact_path = $built
        artifact_byte_len = $builtArtifact.Length
        artifact_sha256 = (Get-SynapseFileSha256 -Path $built)
        artifact_last_write_utc = $builtArtifact.LastWriteTimeUtc.ToString('o')
    }
    $buildTargetReadback | ConvertTo-Json -Depth 32 | Set-Content -LiteralPath $buildTargetReadbackPath -Encoding UTF8
    Info ("Build target readback -> {0} (cargo_target_dir={1} kind={2} cuda_kernels={3} artifact_sha256={4})" -f `
        $buildTargetReadbackPath,
        $buildTargetReadback.cargo_target_dir,
        $buildTargetReadback.cargo_target_dir_kind,
        $cudaBuildCapability.enabled,
        $buildTargetReadback.artifact_sha256)
    $embeddedModels = @(Install-SynapsePinnedDetectionModels -Root $embeddedModelRoot -SourceDir $SourceDir)
    foreach ($slotName in $script:SynapseEmbeddedModelSlotOrder) {
        if (-not @($embeddedModels | Where-Object { $_.Name -eq $slotName })[0]) {
            Die "SYNAPSE_EMBEDDED_MODEL_SELECTION_FAILED root=$embeddedModelRoot slot=$slotName remediation=the pinned model acquisition did not return an entry for every registered model slot"
        }
    }
    [void](Add-SynapseExecutableModelBundle -ExecutablePath $built -Models $embeddedModels)

    # Record the capability gaps this build ships with, next to the build
    # readback, so "why is STT unavailable" is answerable from disk without
    # re-deriving it (#1863).
    $capabilityGaps = @($embeddedModels | Where-Object { -not $_.Present } | ForEach-Object {
        [ordered]@{
            model = $_.Name
            capability = $_.Capability
            required = $_.Required
            expected_sha256 = $_.Sha256
            expected_length = $_.Length
            reason = $_.AbsenceReason
            recipe = $_.Recipe
            override_env = $_.OverrideEnv
            pin_path = $_.PinPath
        }
    })
    $capabilityGapPath = Join-Path $LogDir 'synapse-setup-capability-gaps.json'
    ([ordered]@{
        schema = 'synapse_setup_capability_gaps/v1'
        observed_at_utc = (Get-Date).ToUniversalTime().ToString('o')
        executable = $built
        executable_sha256 = (Get-SynapseFileSha256 -Path $built)
        absent_optional_models = @($capabilityGaps)
    }) | ConvertTo-Json -Depth 32 | Set-Content -LiteralPath $capabilityGapPath -Encoding UTF8
    if (@($capabilityGaps).Count -gt 0) {
        Warn "SYNAPSE_BUILD_CAPABILITY_GAPS count=$(@($capabilityGaps).Count) readback=$capabilityGapPath models=$(@($capabilityGaps | ForEach-Object { $_.model }) -join ',')"
    } else {
        Info "Build capability gaps: none; every registered model was packaged (readback=$capabilityGapPath)"
    }
}

# ---------------------------------------------------------------------------
# 3. Token, data dirs, and profile source resolution
# ---------------------------------------------------------------------------
Step "Bearer token + data dirs"
$tokDir = Split-Path -Parent $TokenPath
New-Item -ItemType Directory -Force -Path $tokDir, $DbPath, $LogDir | Out-Null
if (-not (Test-Path $TokenPath)) {
    $bytes = New-Object byte[] 32
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    ($bytes | ForEach-Object { $_.ToString('x2') }) -join '' | Set-Content -Path $TokenPath -NoNewline -Encoding ascii
    Info "Generated token -> $TokenPath"
} else { Info "Reusing token -> $TokenPath" }
$tokenRaw = Get-Content -Raw $TokenPath
$token = if ($null -eq $tokenRaw) { '' } else { $tokenRaw.Trim() }
if ($token.Length -lt 16) { Die "Token at $TokenPath is too short ($($token.Length) chars); delete it and re-run to regenerate." }
[Environment]::SetEnvironmentVariable('SYNAPSE_BEARER_TOKEN', $token, 'User')
$env:SYNAPSE_BEARER_TOKEN = $token
Info "Set Windows User SYNAPSE_BEARER_TOKEN from $TokenPath for native HTTP MCP clients that require env-based bearer auth."
try {
    $signature = '[DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Auto)] public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);'
    $type = Add-Type -MemberDefinition $signature -Name Win32SendMessageTimeout -Namespace SynapseEnv -PassThru -ErrorAction Stop
    $broadcastResult = [UIntPtr]::Zero
    $rawReturn = $type::SendMessageTimeout([IntPtr]0xffff, 0x001A, [UIntPtr]::Zero, 'Environment', 0x0002, 5000, [ref]$broadcastResult)
    if ($rawReturn -eq [IntPtr]::Zero) {
        Info "WARN: environment broadcast returned 0; future GUI clients may need restart before seeing SYNAPSE_BEARER_TOKEN."
    }
} catch {
    Info "WARN: environment broadcast failed: $($_.Exception.Message). Future GUI clients may need restart before seeing SYNAPSE_BEARER_TOKEN."
}

Info "Candidate profiles verified path=$candidateProfilesDir count=$candidateProfileCount"

# ---------------------------------------------------------------------------
# 4. Stage and health-check the replacement before touching the live daemon
# ---------------------------------------------------------------------------
Step "Validating candidate daemon before handoff"
$installSourcePath = $ExePath
$installSourceHash = $null
if ($SkipBuild) {
    if (-not (Test-Path -LiteralPath $ExePath)) {
        Die "SYNAPSE_SKIP_BUILD_BINARY_MISSING path=$ExePath remediation=-SkipBuild requires a real local synapse-mcp.exe at -ExePath before setup can touch the live daemon"
    }
    # Validating a pre-built binary rather than packaging one: the expectations
    # still come from the committed pins, and each slot's presence is read from
    # the binary itself. A required slot that is absent still fails closed.
    $skipBuildBundle = Get-SynapseExecutableModelBundle -ExecutablePath $ExePath
    if (-not $skipBuildBundle) {
        Die "SYNAPSE_EMBEDDED_MODEL_BUNDLE_MISSING path=$ExePath remediation=-SkipBuild requires a binary already packaged by this setup script; build without -SkipBuild to package one"
    }
    $skipBuildModels = @(Get-SynapseEmbeddedModelPins -SourceDir $SourceDir)
    foreach ($skipBuildModel in $skipBuildModels) {
        $slot = @($skipBuildBundle.Slots | Where-Object { $_.Name -eq $skipBuildModel.Name })[0]
        $skipBuildModel | Add-Member -NotePropertyName Present -NotePropertyValue ([bool]($slot -and $slot.Present)) -Force
        $skipBuildModel | Add-Member -NotePropertyName Path -NotePropertyValue $null -Force
    }
    [void](Assert-SynapseExecutableModelBundle -ExecutablePath $ExePath -Models $skipBuildModels)
    $installSourceHash = Get-SynapseFileSha256 -Path $ExePath
    Info "SkipBuild candidate binary path=$ExePath sha256=$installSourceHash"
} else {
    $stagedBinary = New-SynapseStagedDaemonBinary -BuiltPath $built -LogDir $LogDir -RuntimeDir $ortRuntime.NativeDir
    $installSourcePath = $stagedBinary.Path
    $installSourceHash = $stagedBinary.Sha256
}
$candidateRuntimeFiles = @(Get-SynapseOrtRuntimeCompanions -ExecutablePath $installSourcePath)
$candidateRuntimeSummary = ($candidateRuntimeFiles | ForEach-Object { "{0}:{1}" -f $_.Name, $_.Sha256 }) -join ','
Info "Candidate ONNX Runtime bundle verified files=$candidateRuntimeSummary"
$replacementReservationId = Get-SynapseCandidateReplacementReservationId -Bind $Bind -Token $token
$candidatePreflight = Test-SynapseCandidateDaemon -CandidateExePath $installSourcePath -ProfilesDir $candidateProfilesDir -TokenPath $TokenPath -LogDir $LogDir -EnableAudio $EnableAudio -AllowedPermissions $AllowedPermissions -CalyxConfigPath $CalyxConfigPath -ReplacementReservationId $replacementReservationId
if ($candidatePreflight.Sha256 -ne $installSourceHash) {
    Die "SYNAPSE_CANDIDATE_HASH_MISMATCH expected_sha256=$installSourceHash actual_sha256=$($candidatePreflight.Sha256) path=$installSourcePath remediation=candidate preflight observed different bytes; refusing handoff"
}
Info "Candidate daemon accepted for handoff sha256=$installSourceHash tool_count=$($candidatePreflight.ToolCount) tool_surface_sha256=$($candidatePreflight.ToolSurfaceSha256)"
$installedBinaryAlreadyVerified = $false
$liveDaemonArgumentDrift = $null
if ($SkipBuild) {
    $resolvedInstallSourcePath = [System.IO.Path]::GetFullPath($installSourcePath)
    $resolvedExePath = [System.IO.Path]::GetFullPath($ExePath)
    $installedBinaryAlreadyVerified = (
        $resolvedInstallSourcePath -ieq $resolvedExePath -and
        (Test-Path -LiteralPath $ExePath -PathType Leaf) -and
        ((Get-SynapseFileSha256 -Path $ExePath) -eq $installSourceHash)
    )
    if ($installedBinaryAlreadyVerified) {
        $liveDaemonArgumentDrift = Get-SynapseLiveDaemonArgumentDrift `
            -Snapshot @(Get-SynapseMcpProcessSnapshot) `
            -Bind $Bind `
            -DbPath $DbPath `
            -ExpectedExePath $ExePath `
            -ExpectedSha256 $installSourceHash `
            -EnableAudio $EnableAudio `
            -AllowedPermissions $AllowedPermissions `
            -CalyxConfigPath $CalyxConfigPath
        if ($liveDaemonArgumentDrift.HasDrift) {
            Info ("SkipBuild candidate is already installed, but live daemon launch arguments drifted; setup will perform a daemon handoff. path={0} sha256={1} desired_allowed_permissions={2} desired_calyx_config={3} drift={4}" -f `
                $ExePath,
                $installSourceHash,
                $liveDaemonArgumentDrift.DesiredAllowedPermissions,
                $liveDaemonArgumentDrift.DesiredCalyxConfigPath,
                ($liveDaemonArgumentDrift.Drifts | ConvertTo-Json -Depth 6 -Compress))
        } else {
            Info "SkipBuild candidate is already installed and live daemon launch arguments match; setup may let the generated supervisor adopt it. path=$ExePath sha256=$installSourceHash desired_allowed_permissions=$($liveDaemonArgumentDrift.DesiredAllowedPermissions) desired_calyx_config=$($liveDaemonArgumentDrift.DesiredCalyxConfigPath)"
        }
    }
}
if ($ManualInstallHealthRollbackProbe -and $installedBinaryAlreadyVerified) {
    Die "SYNAPSE_MANUAL_INSTALL_HEALTH_ROLLBACK_PROBE_CANDIDATE_ALREADY_INSTALLED path=$ExePath sha256=$installSourceHash remediation=the rollback drill requires candidate bytes that differ from the installed daemon so setup can back up and restore the previous artifact"
}

$codexAncestorBeforeHandoff = Get-SynapseCurrentCodexAncestor
if ($codexAncestorBeforeHandoff -and $processTokenAtStart -ne $token) {
    Info ("WARN: SYNAPSE_CODEX_CURRENT_PROCESS_ENV_STALE_PRE_HANDOFF_NONFATAL codex_pid={0} token_at_process_start={1} token_file={2} remediation=setup will keep the replacement handoff path available; after handoff call real mcp__synapse.health from this same Codex session before assuming reconnect failed. The patched launcher has been updated for future clients, and direct HTTP/token probes remain diagnostics only." -f `
        $codexAncestorBeforeHandoff.ProcessId,
        ($(if ([string]::IsNullOrWhiteSpace($processTokenAtStart)) { 'missing' } else { 'mismatch' })),
        $TokenPath)
}
Assert-CodexCandidateHandoffPreservesCurrentProcess `
    -CodexAncestor $codexAncestorBeforeHandoff `
    -CandidateSurface $candidatePreflight.ToolSurface `
    -ProcessHashAtStart $processToolSurfaceHashAtStart `
    -ProcessSnapshotAtStart $processToolSurfaceSnapshotAtStart `
    -SourceDir $SourceDir `
    -Bind $Bind `
    -TokenPath $TokenPath `
    -ActiveIssue $ActiveIssue

$chromeBridgeInstaller = Join-Path $PSScriptRoot 'install-synapse-chrome-debugger.ps1'
if ($script:SynapsePostExitStartOnly) {
    Info "SYNAPSE_POST_EXIT_SKIP_CHROME_BRIDGE_PREFLIGHT reason=$PostExitContinuationReason bind=$Bind remediation=post-exit continuation must not reload or reconnect the Chrome bridge before the daemon bind is reusable; daemon /health after start remains the bridge Source-of-Truth readback."
} else {
    Step "Preflighting Chrome direct localhost bridge before daemon handoff"
    $chromeBridgePreflight = Invoke-SynapseChromeBridgeVerifier `
        -InstallerPath $chromeBridgeInstaller `
        -NativeHostExePath $ChromeNativeHostExePath
    Info ("Chrome direct bridge verifier preflight completed transport={0} extension_id={1} native_host_registry_present={2} native_host_manifest_present={3} policy_cleanup={4} popup_shield={5} {6}" -f `
        $chromeBridgePreflight.daemon_bridge_transport, `
        $chromeBridgePreflight.extension_id, `
        $chromeBridgePreflight.native_host_registry_present, `
        $chromeBridgePreflight.native_host_manifest_present, `
        (($chromeBridgePreflight.chrome_policy_cleanup | ForEach-Object { "$($_.hive):$($_.reason)" }) -join ','), `
        (($chromeBridgePreflight.chrome_policy_popup_shield | ForEach-Object { "$($_.hive):$($_.reason)" }) -join ','), `
        (Format-SynapseChromeBridgeProfileInstallState -Readback $chromeBridgePreflight))
}

# ---------------------------------------------------------------------------
# 5. Drain the running daemon when binary bytes or launch arguments changed
# ---------------------------------------------------------------------------
$liveDaemonHandoffRequired = (
    (-not $installedBinaryAlreadyVerified) -or
    ($liveDaemonArgumentDrift -and $liveDaemonArgumentDrift.HasDrift) -or
    [bool]$ForceRestart
)
if ($ForceRestart -and $installedBinaryAlreadyVerified -and (-not ($liveDaemonArgumentDrift -and $liveDaemonArgumentDrift.HasDrift))) {
    Info "Explicit -ForceRestart requires a verified live daemon drain even though the installed binary and launch arguments are unchanged."
}
if (-not $liveDaemonHandoffRequired) {
    Step "Verified installed daemon binary without live drain -> $ExePath"
} else {
    Step "Draining live daemon and installing verified binary -> $ExePath"
    # One handoff gets one reason identity. The Chrome maintenance pause binds
    # its acknowledgement to this value and deliberately rejects reuse under a
    # different reason. Keep every phase on this single value so adding a new
    # drain/readback step cannot silently split one deploy into competing state
    # tokens (#2156).
    $deployDrainReason = 'deploy'
    Assert-SynapseRestartAllowed -Reason $deployDrainReason -Bind $Bind -DbPath $DbPath -TokenPath $TokenPath -HealthTimeoutSec ([Math]::Min(300, [Math]::Max(120, $InstallHealthTimeoutSeconds))) -ForceRestart:$ForceRestart -AllowActiveClientDrain
    $daemonSupervisorPath = Join-Path $RuntimeBinDir 'synapse-daemon-supervisor.ps1'
    $null = Assert-SynapseDaemonTaskRestartAuthorityIdentity -TaskName $TaskName -SupervisorPath $daemonSupervisorPath -Reason $deployDrainReason
    if ($ForceRestart) {
        $null = Enter-SynapseChromeBridgeMaintenancePause -Bind $Bind -Token $token -Reason $deployDrainReason
    }
    # #2092: ONE drain, shared with -Stop, parameterised reason=deploy.
    #
    # What changed from #2051: the supervisor is asked to park on the durable
    # stop-request instead of being force-killed, and Task Scheduler authority is
    # suspended (Stop + Disable) instead of unregistered. The unregister is not
    # gone -- section 7 still unregisters and re-registers when the launcher
    # actually changed -- it is just no longer how restart authority is revoked
    # for the drain, so a setup that dies mid-deploy leaves a disabled task that
    # the trap re-enables rather than no task at all.
    #
    # The deploy is the one caller allowed to escalate: it cannot leave a live
    # daemon mapping the binary it is about to replace. Both escalations are
    # loud (SYNAPSE_DAEMON_STOP_FORCED / SYNAPSE_DAEMON_SUPERVISOR_STOP_FORCED)
    # and both are the exception, not the path.
    $deployDrain = Invoke-SynapseDaemonRevokedDrain `
        -Reason $deployDrainReason `
        -TaskName $TaskName `
        -RuntimeBinDir $RuntimeBinDir `
        -Bind $Bind `
        -DbPath $DbPath `
        -TokenPath $TokenPath `
        -LogDir $LogDir `
        -ForceRestart:$ForceRestart `
        -EscalateAfterGracefulTimeout `
        -AllowParkEscalation `
        -TimeoutSeconds 120
    $deployDrainRunRecord = Read-SynapseDaemonLifecycleRunRecord -DbPath $DbPath
    # #2131: same law as -Stop and -Start. The production instance of the bug was
    # printed by exactly this line -- `ended_reason=http_shutdown_watchdog_expired
    # clean_shutdown=True expected_next_boot_previous_shutdown=clean` -- so the
    # printed expectation is now derived from the cause it was already printing.
    $deployDrainVerdict = Get-SynapseDaemonPreviousShutdownVerdict -Record $deployDrainRunRecord.Record -Readable $deployDrainRunRecord.Ok
    $deployDrainEndedReason = $deployDrainVerdict.EndedReason
    $deployDrainEndedAt = $deployDrainVerdict.EndedAtUnixMs
    $deployDrainClean = $deployDrainVerdict.Clean
    if ($deployDrain.DaemonsBefore.Count -gt 0 -and -not $deployDrainClean -and -not $deployDrain.Forced) {
        # Not fatal: the deploy can still install correctly. But this is the
        # exact fact #2092 exists to change, so it is never silent.
        Warn ("SYNAPSE_DEPLOY_DRAIN_UNCLEAN_LIFECYCLE_RECORD path={0} run_id={1} ended_at_unix_ms={2} ended_reason={3} ending_phase={4} verdict_detail={5} effect=the next boot will report previous_shutdown={6} remediation=inspect the daemon log for MCP_HTTP_SHUTDOWN_WATCHDOG_EXPIRED, the last SYNAPSE_CALYX_VAULT_CLOSE_PHASE (the phase after it is the one that did not complete) and SYNAPSE_CALYX_VAULT_CLOSED for the drained generation" -f `
            $deployDrainRunRecord.Path,
            [string]$deployDrainRunRecord.Record.run_id,
            ($(if ($null -eq $deployDrainEndedAt) { '<null>' } else { $deployDrainEndedAt })),
            $deployDrainEndedReason,
            $deployDrainVerdict.EndingPhase,
            $deployDrainVerdict.Detail,
            $deployDrainVerdict.Verdict)
    }
    Info ("Synapse deploy drain verified reason=deploy forced={0} park_forced={1} task_state_after={2} stop_request={3} lifecycle_run_id={4} ended_reason={5} clean_shutdown={6} expected_next_boot_previous_shutdown={7} ending_phase={8} verdict_detail={9}" -f `
        $deployDrain.Forced,
        $deployDrain.ParkForced,
        ($(if ($deployDrain.TaskSuspended) { $deployDrain.TaskSuspended.State } else { '<absent>' })),
        $deployDrain.StopRequestPath,
        ($(if ($deployDrainRunRecord.Ok) { [string]$deployDrainRunRecord.Record.run_id } else { '<unreadable>' })),
        $deployDrainEndedReason,
        $deployDrainClean,
        $deployDrainVerdict.Verdict,
        $deployDrainVerdict.EndingPhase,
        $deployDrainVerdict.Detail)
    # Fallback verification pass (see the function header): with the stop-request
    # in force this normally observes zero targets on attempt 1 and returns.
    Stop-SynapseMcpProcessesForInstallHandoff `
        -Reason $deployDrainReason `
        -Bind $Bind `
        -DbPath $DbPath `
        -TokenPath $TokenPath `
        -LogDir $LogDir `
        -StopRequestPath $deployDrain.StopRequestPath `
        -SupervisorPath $deployDrain.SupervisorPath `
        -ForceRestart:$ForceRestart `
        -TimeoutSeconds 300
    $script:SynapseChromeBridgeMaintenancePausePrepared = $false
    $script:SynapseChromeBridgeMaintenancePausePreparedBind = $null
    $script:SynapseChromeBridgeMaintenancePausePreparedReason = $null
    $script:SynapseChromeBridgeMaintenancePausePreparedResult = $null
    Assert-SynapseInstallPathUnlocked -Path $ExePath -Bind $Bind -DbPath $DbPath -TimeoutSeconds 30
}
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ExePath) | Out-Null
$backupPath = $null
$oldInstalledHash = $null
$runtimeCompanionBackups = @()
if ($installedBinaryAlreadyVerified) {
    $oldInstalledHash = $installSourceHash
    Info "Installed binary already matches verified SkipBuild candidate; no backup/copy needed. path=$ExePath sha256=$oldInstalledHash"
} elseif (Test-Path -LiteralPath $ExePath) {
    $oldInstalledHash = Get-SynapseFileSha256 -Path $ExePath
    $backupPath = "$ExePath.bak"
    Copy-Item -LiteralPath $ExePath -Destination $backupPath -Force
    $backupHash = Get-SynapseFileSha256 -Path $backupPath
    if ($backupHash -ne $oldInstalledHash) {
        Die "SYNAPSE_BINARY_BACKUP_HASH_MISMATCH installed=$ExePath backup=$backupPath installed_hash=$oldInstalledHash backup_hash=$backupHash remediation=backup bytes changed during copy; refusing to install candidate"
    }
    Info "Backed up old binary -> $backupPath sha256=$backupHash"
}
if ($ManualInstallHealthRollbackProbe -and (-not $backupPath -or -not $oldInstalledHash)) {
    Die "SYNAPSE_MANUAL_INSTALL_HEALTH_ROLLBACK_PROBE_BACKUP_MISSING installed=$ExePath remediation=the rollback drill requires an existing installed daemon binary so setup can prove backup hash, restore the prior artifact, and re-read daemon health after rollback"
}
if ($installedBinaryAlreadyVerified) {
    Info "SkipBuild candidate already resides at install path=$ExePath"
} elseif (-not $SkipBuild) {
    Copy-Item -LiteralPath $installSourcePath -Destination $ExePath -Force
} else {
    Info "SkipBuild candidate already resides at install path=$ExePath"
}
if (-not (Test-Path -LiteralPath $ExePath)) {
    Die "SYNAPSE_INSTALL_BINARY_MISSING path=$ExePath remediation=setup could not find the installed daemon binary after the copy step"
}
$installedHash = Get-SynapseFileSha256 -Path $ExePath
if ($installedHash -ne $installSourceHash) {
    Die "SYNAPSE_INSTALLED_BINARY_HASH_MISMATCH path=$ExePath expected_sha256=$installSourceHash actual_sha256=$installedHash remediation=installed daemon bytes do not match the candidate that passed health preflight"
}
$runtimeInstallDir = Split-Path -Parent $ExePath
foreach ($companion in $candidateRuntimeFiles) {
    $destination = Join-Path $runtimeInstallDir $companion.Name
    $sourceResolved = [System.IO.Path]::GetFullPath($companion.Path)
    $destinationResolved = [System.IO.Path]::GetFullPath($destination)
    if ($sourceResolved -ine $destinationResolved) {
        $backup = [pscustomobject]@{
            Path = $destination
            BackupPath = "$destination.bak"
            Existed = (Test-Path -LiteralPath $destination -PathType Leaf)
            Sha256 = $null
        }
        if ($backup.Existed) {
            $backup.Sha256 = Get-SynapseFileSha256 -Path $destination
            Copy-Item -LiteralPath $destination -Destination $backup.BackupPath -Force
            $backupReadback = Get-SynapseFileSha256 -Path $backup.BackupPath
            if ($backupReadback -ne $backup.Sha256) {
                Die "SYNAPSE_RUNTIME_COMPANION_BACKUP_HASH_MISMATCH path=$destination backup=$($backup.BackupPath) expected_sha256=$($backup.Sha256) actual_sha256=$backupReadback remediation=runtime bundle backup changed during copy; refusing handoff"
            }
        }
        $runtimeCompanionBackups += $backup
        Copy-Item -LiteralPath $companion.Path -Destination $destination -Force
    }
    $runtimeReadback = Get-SynapseFileSha256 -Path $destination
    if ($runtimeReadback -ne $companion.Sha256) {
        Die "SYNAPSE_INSTALLED_RUNTIME_COMPANION_HASH_MISMATCH path=$destination expected_sha256=$($companion.Sha256) actual_sha256=$runtimeReadback remediation=installed ONNX Runtime bundle is incoherent; daemon start is refused"
    }
    Info "Installed ONNX Runtime companion verified path=$destination sha256=$runtimeReadback"
}
$ver = (& $ExePath --version) 2>&1
Info "Installed binary reports: $ver"
Info "Installed binary verified path=$ExePath sha256=$installedHash previous_sha256=$oldInstalledHash"
Remove-SynapseCurrentDaemonStagingArtifact

$installDir = Split-Path -Parent $ExePath
$retiredSetupOwnedExecutables = @(
    'synapse-fsv-toast-history.exe'
)
foreach ($retiredExeName in $retiredSetupOwnedExecutables) {
    $retiredPath = Join-Path $installDir $retiredExeName
    if (-not (Test-Path -LiteralPath $retiredPath)) { continue }

    $resolvedRetiredPath = [System.IO.Path]::GetFullPath($retiredPath)
    $resolvedInstallDir = [System.IO.Path]::GetFullPath($installDir).TrimEnd('\')
    if ((Split-Path -Parent $resolvedRetiredPath).TrimEnd('\') -ine $resolvedInstallDir) {
        Die "SYNAPSE_RETIRED_EXECUTABLE_SCOPE_MISMATCH path=$resolvedRetiredPath install_dir=$resolvedInstallDir remediation=setup only prunes retired executables inside the installed Synapse binary directory"
    }

    $retiredOwners = @(Get-CimInstance Win32_Process -Filter "Name='$retiredExeName'" -ErrorAction SilentlyContinue |
        Where-Object {
            try {
                $candidatePath = [System.IO.Path]::GetFullPath([string]$_.ExecutablePath)
                $candidatePath -ieq $resolvedRetiredPath
            } catch {
                $false
            }
        })
    if ($retiredOwners.Count -gt 0) {
        $ownerPids = ($retiredOwners | ForEach-Object { $_.ProcessId }) -join ','
        Die "SYNAPSE_RETIRED_EXECUTABLE_STILL_RUNNING path=$resolvedRetiredPath pids=$ownerPids remediation=close the retired helper process before setup can prune its installed executable"
    }

    $retiredHash = Get-SynapseFileSha256 -Path $resolvedRetiredPath
    Remove-Item -LiteralPath $resolvedRetiredPath -Force
    if (Test-Path -LiteralPath $resolvedRetiredPath) {
        Die "SYNAPSE_RETIRED_EXECUTABLE_PRUNE_FAILED path=$resolvedRetiredPath sha256=$retiredHash remediation=setup removed the retired helper but the file still exists; inspect file permissions/locks and retry"
    }
    Info "Pruned retired setup-owned executable path=$resolvedRetiredPath sha256=$retiredHash"
}

if ($script:SynapsePostExitStartOnly) {
    Info "SYNAPSE_POST_EXIT_SKIP_CHROME_BRIDGE_VERIFY reason=$PostExitContinuationReason bind=$Bind remediation=post-exit continuation avoids creating bridge peers while the dead-owner bind is still draining; daemon /health after start verifies the active Chrome bridge."
} else {
    Step "Verifying Chrome direct localhost bridge"
    $chromeBridgeReadback = Invoke-SynapseChromeBridgeVerifier `
        -InstallerPath $chromeBridgeInstaller `
        -NativeHostExePath $ChromeNativeHostExePath
    Info ("Chrome direct bridge verifier completed transport={0} extension_id={1} native_host_registry_present={2} native_host_manifest_present={3} policy_cleanup={4} popup_shield={5} {6}" -f `
        $chromeBridgeReadback.daemon_bridge_transport, `
        $chromeBridgeReadback.extension_id, `
        $chromeBridgeReadback.native_host_registry_present, `
        $chromeBridgeReadback.native_host_manifest_present, `
        (($chromeBridgeReadback.chrome_policy_cleanup | ForEach-Object { "$($_.hive):$($_.reason)" }) -join ','), `
        (($chromeBridgeReadback.chrome_policy_popup_shield | ForEach-Object { "$($_.hive):$($_.reason)" }) -join ','), `
        (Format-SynapseChromeBridgeProfileInstallState -Readback $chromeBridgeReadback))
}

# ---------------------------------------------------------------------------
# 6. Deploy bundled profiles next to the exe (executable-relative lookup) +
#    keep an explicit --profile-dir for belt-and-suspenders.
# ---------------------------------------------------------------------------
Step "Deploying bundled profiles -> $ProfilesDir"
$profileDeploy = Install-SynapseBundledProfiles -SourceProfilesDir $srcProfiles -ProfilesDir $ProfilesDir -LogDir $LogDir
if ($profileDeploy.BundledProfileCount -lt 1) {
    Die "SYNAPSE_PROFILES_DEPLOYED_EMPTY path=$ProfilesDir source=$srcProfiles remediation=reconciled bundled profiles but found 0 top-level .toml files in the setup-owned manifest"
}
Info "Deployed $($profileDeploy.BundledProfileCount) bundled profiles from manifest $($profileDeploy.ManifestPath)."

if ($script:SynapseBindPostExitContinuationRequired) {
    # #2092: this branch hands off to a separate process and then Dies, so the
    # deploy's durable restart-authority revocation must be released here and not
    # left to the trap. The continuation reacquires everything it needs from
    # scratch (it re-runs the whole deploy with -SkipBuild), and if it never runs
    # at all, an enabled task with no stop-request still restores the daemon at
    # the next logon. Nothing can start a daemon in the gap: the task trigger is
    # AtLogOn and the supervisor is already parked.
    Restore-SynapseDeployRestartAuthorityBestEffort -Reason 'post_exit_continuation_handoff'
    $continuation = Start-SynapsePostExitSetupContinuation `
        -Reason 'install_binary' `
        -Bind $Bind `
        -SourceDir $SourceDir `
        -ExePath $ExePath `
        -ChromeNativeHostExePath $ChromeNativeHostExePath `
        -CargoTarget $CargoTarget `
        -DbPath $DbPath `
        -ProfilesDir $ProfilesDir `
        -LogDir $LogDir `
        -TokenPath $TokenPath `
        -CodexToolSurfaceSnapshotPath $CodexToolSurfaceSnapshotPath `
        -TaskName $TaskName `
        -MaintenanceLockPath $MaintenanceLockPath `
        -CalyxConfigPath $CalyxConfigPath `
        -ActiveIssue $ActiveIssue `
        -DeadOwnerDetail $script:SynapseBindPostExitContinuationDetail
    Die ("SYNAPSE_BIND_POST_EXIT_CONTINUATION_STARTED reason=install_binary bind={0} child_pid={1} manifest={2} stdout={3} stderr={4} remediation=the verified daemon bytes and profiles were installed, but Windows kept the dead-owner listener unavailable until this setup process exits. A hidden continuation has been launched and will wait for parent_pid={5}, reacquire the maintenance lock, start the daemon through the normal setup path, and write its own stdout/stderr/readbacks. Inspect the continuation manifest/logs and final process/socket SoT before accepting repair." -f `
        $Bind,
        $continuation.ChildPid,
        $continuation.ManifestPath,
        $continuation.StdoutPath,
        $continuation.StderrPath,
        $PID)
}

function Assert-SynapseAutostartLauncherIntegrity {
    <#
        Proves the registered autostart task can actually start the daemon.

        A scheduled task whose action points at a deleted file still reports
        State=Ready, so any check that only inspects task state calls a dead
        autostart healthy. That is exactly how log cleanup silently disabled
        autostart here: the launcher lived in the log directory and went out with
        the rotated logs. This asserts the registered action's target file
        physically exists, is the exact launcher setup owns, and does not live in
        a directory that log hygiene empties (#1862).
    #>
    param(
        [Parameter(Mandatory=$true)][string]$TaskName,
        [Parameter(Mandatory=$true)][string]$ExpectedLauncherPath,
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$Phase,
        # Adoption preserves a live daemon's restart authority and does not
        # re-register the task, so a machine still on the pre-#1862 layout must
        # not be blocked from installing -- the defect is reported loudly and
        # repaired by the next handoff run. On the post-register path the task
        # was just written by this script, so the layout is enforced strictly.
        [switch]$AllowLegacyLayout
    )

    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $task) {
        Die "SYNAPSE_AUTOSTART_TASK_MISSING task=$TaskName phase=$Phase remediation=the autostart task is not registered; re-run setup to register it"
    }
    $actions = @($task.Actions)
    if ($actions.Count -ne 1) {
        Die "SYNAPSE_AUTOSTART_TASK_ACTION_AMBIGUOUS task=$TaskName phase=$Phase action_count=$($actions.Count) remediation=the autostart task must have exactly one setup-owned action; inspect Task Scheduler"
    }
    $arguments = [string]$actions[0].Arguments
    $match = [regex]::Match($arguments, '"(?<path>[^"]+\.vbs)"')
    if (-not $match.Success) {
        Die "SYNAPSE_AUTOSTART_TASK_ACTION_UNPARSEABLE task=$TaskName phase=$Phase arguments=$arguments remediation=the registered action does not name a quoted .vbs launcher; re-run setup to re-register the task"
    }
    $registeredLauncher = [System.IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($match.Groups['path'].Value))
    $expected = [System.IO.Path]::GetFullPath($ExpectedLauncherPath)
    $resolvedLogDir = [System.IO.Path]::GetFullPath($LogDir).TrimEnd('\')
    $launcherDir = [System.IO.Path]::GetFullPath((Split-Path -Parent $registeredLauncher)).TrimEnd('\')
    $inLogDir = ($launcherDir -ieq $resolvedLogDir -or $launcherDir.StartsWith($resolvedLogDir + '\', [System.StringComparison]::OrdinalIgnoreCase))
    $isLegacyLayout = ($registeredLauncher -ine $expected -and $inLogDir)

    # Report the most specific defect. A registration that differs from what
    # setup owns is either the known pre-#1862 log-dir layout -- in which case
    # the useful message names that root cause -- or an unknown third-party
    # action, which is an ownership problem.
    if ($registeredLauncher -ine $expected -and -not $isLegacyLayout) {
        Die "SYNAPSE_AUTOSTART_LAUNCHER_PATH_MISMATCH task=$TaskName phase=$Phase registered=$registeredLauncher expected=$expected remediation=the registered task launches a different file than the one setup owns; re-run setup to re-register the task"
    }
    # A launcher that does not exist is the more urgent fact than where it lives:
    # autostart is dead right now, not merely fragile.
    if (-not (Test-Path -LiteralPath $registeredLauncher -PathType Leaf)) {
        Die "SYNAPSE_AUTOSTART_LAUNCHER_MISSING task=$TaskName phase=$Phase task_state=$($task.State) registered_launcher=$registeredLauncher launcher_in_log_dir=$inLogDir remediation=the autostart task is registered and reports State=$($task.State) but its launcher file does not exist, so it can never start the daemon; re-run setup to regenerate the launcher"
    }
    if ($inLogDir -and -not $AllowLegacyLayout) {
        Die "SYNAPSE_AUTOSTART_LAUNCHER_IN_LOG_DIR task=$TaskName phase=$Phase registered_launcher=$registeredLauncher expected=$expected log_dir=$resolvedLogDir remediation=the daemon launcher must not live inside the log directory, where routine log cleanup silently deletes it; re-run setup so the launcher is written to the runtime bin directory"
    }
    if ($inLogDir) {
        # Adoption path on a pre-#1862 machine: the defect is real and must be
        # visible, but blocking the install would leave it unfixable.
        Warn "SYNAPSE_AUTOSTART_LAUNCHER_IN_LOG_DIR task=$TaskName phase=$Phase registered_launcher=$registeredLauncher log_dir=$resolvedLogDir effect=autostart still lives in the log directory and remains vulnerable to log cleanup; it is repaired the next time setup performs a daemon handoff (run with -ForceRestart to repair now)"
    }
    $launcherHash = Get-SynapseFileSha256 -Path $registeredLauncher
    $launcherLength = (Get-Item -LiteralPath $registeredLauncher).Length
    Info "SYNAPSE_AUTOSTART_LAUNCHER_VERIFIED task=$TaskName phase=$Phase task_state=$($task.State) launcher=$registeredLauncher length=$launcherLength sha256=$launcherHash launcher_dir=$launcherDir log_dir=$resolvedLogDir"
    return [pscustomobject]@{
        TaskName = $TaskName
        TaskState = [string]$task.State
        LauncherPath = $registeredLauncher
        LauncherSha256 = $launcherHash
        LauncherLength = [int64]$launcherLength
    }
}

function Remove-SynapseLegacyLogDirLauncherArtifacts {
    <#
        Removes launcher/supervisor copies left behind in the log directory by
        pre-#1862 installs, once the runtime-bin copies exist. They are dead
        program artifacts in a directory reserved for logs, and leaving them
        there invites a future task registration to point back at a
        cleanup-vulnerable path.
    #>
    param(
        [Parameter(Mandatory=$true)][string]$LogDir,
        [Parameter(Mandatory=$true)][string]$RuntimeBinDir
    )

    foreach ($name in @('synapse-daemon-launch-hidden.vbs', 'synapse-daemon-supervisor.ps1')) {
        $legacyPath = Join-Path $LogDir $name
        $currentPath = Join-Path $RuntimeBinDir $name
        if (-not (Test-Path -LiteralPath $legacyPath -PathType Leaf)) { continue }
        if (-not (Test-Path -LiteralPath $currentPath -PathType Leaf)) {
            Warn "SYNAPSE_LEGACY_LAUNCHER_RETAINED legacy=$legacyPath reason=the replacement in $RuntimeBinDir does not exist yet; refusing to delete the only copy"
            continue
        }
        $legacyHash = Get-SynapseFileSha256 -Path $legacyPath
        Remove-Item -LiteralPath $legacyPath -Force
        Info "SYNAPSE_LEGACY_LAUNCHER_REMOVED path=$legacyPath sha256=$legacyHash replacement=$currentPath reason=program artifacts must not live in the log directory (#1862)"
    }
}

function Remove-SynapseLegacyEnsureDaemonSupervisor {
    param(
        [Parameter(Mandatory=$true)][string]$RuntimeBinDir,
        [Parameter(Mandatory=$true)][string]$CanonicalSupervisorPath
    )

    try {
        $resolvedRuntimeBinDir = (Resolve-Path -LiteralPath $RuntimeBinDir -ErrorAction Stop).Path
    } catch {
        Die "SYNAPSE_RUNTIME_BIN_RESOLVE_FAILED path=$RuntimeBinDir error=$($_.Exception.Message) remediation=repair the configured runtime-bin path before setup attempts legacy-artifact retirement"
    }
    $runtimeRoot = Split-Path -Parent $resolvedRuntimeBinDir
    if ([string]::IsNullOrWhiteSpace($runtimeRoot)) {
        Die "SYNAPSE_RUNTIME_ROOT_EMPTY runtime_bin=$resolvedRuntimeBinDir remediation=configure RuntimeBinDir as a concrete child directory before setup attempts legacy-artifact retirement"
    }
    $legacyPath = Join-Path $runtimeRoot 'ensure-daemon-supervisor.ps1'
    if (-not (Test-Path -LiteralPath $legacyPath)) {
        Info "SYNAPSE_LEGACY_ENSURE_SUPERVISOR_ABSENT path=$legacyPath"
        return
    }
    if (-not (Test-Path -LiteralPath $legacyPath -PathType Leaf)) {
        Die "SYNAPSE_LEGACY_ENSURE_SUPERVISOR_NOT_FILE path=$legacyPath remediation=inspect this unexpected filesystem object; setup will not remove it"
    }
    if (-not (Test-Path -LiteralPath $CanonicalSupervisorPath -PathType Leaf)) {
        Die "SYNAPSE_CANONICAL_SUPERVISOR_MISSING path=$CanonicalSupervisorPath legacy_path=$legacyPath remediation=repair the canonical runtime-bin supervisor before retiring any legacy entry point"
    }
    $legacyText = Get-Content -Raw -LiteralPath $legacyPath
    $legacyHash = (Get-FileHash -LiteralPath $legacyPath -Algorithm SHA256).Hash
    $legacyShape = $legacyText -match 'Idempotent entry point for the SynapseMcpDaemon scheduled task' -and
        $legacyText -match [regex]::Escape("synapse\logs\synapse-daemon-supervisor.ps1") -and
        $legacyText -match 'SYNAPSE_DAEMON_ENSURE_SUPERVISOR_MISSING'
    if (-not $legacyShape) {
        Die "SYNAPSE_LEGACY_ENSURE_SUPERVISOR_IDENTITY_UNKNOWN path=$legacyPath sha256=$legacyHash remediation=inspect this operator-modified or foreign script; setup will not delete bytes it cannot identify as the obsolete pre-#1862 launcher"
    }
    Remove-Item -LiteralPath $legacyPath -Force
    if (Test-Path -LiteralPath $legacyPath) {
        Die "SYNAPSE_LEGACY_ENSURE_SUPERVISOR_REMOVE_FAILED path=$legacyPath sha256=$legacyHash remediation=repair file permissions and rerun setup"
    }
    Info "SYNAPSE_LEGACY_ENSURE_SUPERVISOR_RETIRED path=$legacyPath sha256=$legacyHash canonical_supervisor=$CanonicalSupervisorPath state=absent_after_readback"
}

# ---------------------------------------------------------------------------
# 7. Verify adoption or register + start the auto-start HTTP daemon
# ---------------------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $RuntimeBinDir | Out-Null
$legacyLauncher = Join-Path $LogDir 'synapse-daemon-launch.cmd'
# Launcher and supervisor are executable program artifacts, so they live in the
# runtime bin directory. $launcherLog stays in $LogDir because it genuinely is a
# log (#1862).
$hiddenLauncher = Join-Path $RuntimeBinDir 'synapse-daemon-launch-hidden.vbs'
$launcherLog = Join-Path $LogDir 'daemon-launcher.log'
$daemonSupervisorPath = Join-Path $RuntimeBinDir 'synapse-daemon-supervisor.ps1'
$wscriptExe = Join-Path $env:SystemRoot 'System32\wscript.exe'
if (-not (Test-Path $wscriptExe)) {
    Die "SYNAPSE_HIDDEN_LAUNCHER_MISSING path=$wscriptExe remediation=repair Windows Script Host or run the daemon manually with a hidden process supervisor"
}
# #2083: a full deploy is an explicit "make the daemon live" instruction, so it
# clears any durable stop-request left by a previous -Stop. Without this an
# operator who parked the daemon and then deployed would get a supervisor that
# correctly refuses to launch and a deploy that correctly fails on health --
# which is fail-closed, but pointlessly so. The clear is idempotent and is a
# no-op on a host that was never stopped. It also re-enables the task, because
# -Stop disables it and section 7's adoption branch never re-registers.
[void](Clear-SynapseDaemonSupervisorStopRequest -Path (Get-SynapseDaemonSupervisorStopRequestPath -RuntimeBinDir $RuntimeBinDir) -Reason 'setup_deploy')
$deployTaskBeforeStart = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($deployTaskBeforeStart -and [string]$deployTaskBeforeStart.State -eq 'Disabled') {
    Info "Synapse daemon scheduled task is Disabled before deploy start (a previous -Stop parked it); re-enabling task=$TaskName"
    try {
        Enable-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
    } catch {
        Die "SYNAPSE_TASK_ENABLE_FAILED task=$TaskName reason=setup_deploy error=$($_.Exception.Message) remediation=a previous -Stop disabled the daemon task; setup cannot restore autostart while Enable-ScheduledTask fails"
    }
    $deployTaskEnabledReadback = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $deployTaskEnabledReadback -or [string]$deployTaskEnabledReadback.State -eq 'Disabled') {
        Die "SYNAPSE_TASK_ENABLE_READBACK_FAILED task=$TaskName reason=setup_deploy state=$($deployTaskEnabledReadback.State) remediation=the daemon task is still disabled after Enable-ScheduledTask"
    }
    Info "Synapse daemon scheduled task re-enabled before deploy start: task=$TaskName state=$($deployTaskEnabledReadback.State)"
}
# #2092: restart authority is now restored for real, so the trap / post-exit
# handoff must stop trying to restore it. Everything after this point either
# starts the daemon or adopts a live one; a failure from here on is not a failure
# that stranded autostart.
Clear-SynapseDeployRestartAuthorityRevocation
$deployAuthorityRestoreTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
$deployAuthorityStopRequestPresent = Test-Path -LiteralPath (Get-SynapseDaemonSupervisorStopRequestPath -RuntimeBinDir $RuntimeBinDir) -PathType Leaf
Info ("SYNAPSE_DEPLOY_RESTART_AUTHORITY_RESTORE_COMPLETE reason=setup_deploy stop_request_present={0} task={1} task_state={2}" -f `
    $deployAuthorityStopRequestPresent,
    $TaskName,
    ($(if ($deployAuthorityRestoreTask) { $deployAuthorityRestoreTask.State } else { '<absent>' })))
if (-not $liveDaemonHandoffRequired) {
    Step "Verifying live adoption of auto-start daemon task '$TaskName'"
    # Adoption preserves the running task, so its launcher must be proven intact
    # here too -- a Ready task with a deleted launcher would otherwise be adopted
    # as healthy and never start again after the next reboot (#1862).
    [void](Assert-SynapseAutostartLauncherIntegrity -TaskName $TaskName -ExpectedLauncherPath $hiddenLauncher -LogDir $LogDir -Phase 'adoption' -AllowLegacyLayout)
    $liveAdoption = Assert-SynapseLiveDaemonAdoptionIdentity `
        -TaskName $TaskName `
        -HiddenLauncherPath $hiddenLauncher `
        -SupervisorPath $daemonSupervisorPath `
        -ExePath $ExePath `
        -Bind $Bind `
        -DbPath $DbPath `
        -ProfilesDir $ProfilesDir `
        -LogDir $LogDir `
        -TokenPath $TokenPath `
        -MaintenanceLockPath $MaintenanceLockPath `
        -EnableAudio $EnableAudio `
        -AllowedPermissions $AllowedPermissions `
        -CalyxConfigPath $CalyxConfigPath
    Info ("SYNAPSE_LIVE_DAEMON_ADOPTION_VERIFIED task={0} task_state={1} task_definition_sha256={2} hidden_launcher_sha256={3} supervisor_sha256={4} supervisor_pid={5} daemon_pid={6} supervisor_state={7} daemon_arguments=[{8}] remediation=none; setup preserved the exact running task/supervisor/daemon instead of re-registering live restart authority" -f `
        $TaskName,
        $liveAdoption.TaskState,
        $liveAdoption.TaskDefinitionSha256,
        $liveAdoption.HiddenLauncherSha256,
        $liveAdoption.SupervisorSha256,
        $liveAdoption.SupervisorPid,
        $liveAdoption.DaemonPid,
        $liveAdoption.SupervisorState,
        $liveAdoption.DaemonArgumentText)
    # Adoption keeps the task exactly as it was, so a host installed before the
    # priority invariant existed would never converge without this (#1910).
    Assert-SynapseDaemonTaskPriority -TaskName $TaskName -Phase 'adoption'
} else {
    Step "Registering auto-start daemon task '$TaskName'"
    Wait-SynapseBindReleased -Reason 'pre_start' -Bind $Bind -TimeoutSeconds 300
    if (Test-Path $legacyLauncher) {
        Remove-Item -LiteralPath $legacyLauncher -Force
    }
    New-HiddenDaemonLauncher `
        -OutputPath $hiddenLauncher `
        -ExePath $ExePath `
        -Bind $Bind `
        -DbPath $DbPath `
        -ProfilesDir $ProfilesDir `
        -LogDir $LogDir `
        -TokenPath $TokenPath `
        -MaintenanceLockPath $MaintenanceLockPath `
        -EnableAudio $EnableAudio `
        -AllowedPermissions $AllowedPermissions `
        -CalyxConfigPath $CalyxConfigPath

    $action  = New-ScheduledTaskAction -Execute $wscriptExe -Argument "//B //Nologo `"$hiddenLauncher`"" -WorkingDirectory $RuntimeBinDir
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
    $princ   = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
    $set     = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
                -StartWhenAvailable -MultipleInstances IgnoreNew -RestartCount 3 `
                -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero) `
                -Priority $SynapseDaemonTaskPriority
    $set.Hidden = $true
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    }
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $princ `
        -Settings $set -Description "Synapse MCP HTTP daemon (loopback) - the single body controlling Windows + WSL programs." | Out-Null
    # Prove the registered value rather than trusting the settings object: this
    # is the field whose silent default cost the daemon its interactive priority
    # on every install to date (#1910).
    Assert-SynapseDaemonTaskPriority -TaskName $TaskName -Phase 'post_register'
    Start-ScheduledTask -TaskName $TaskName
    Info "Task registered and started."
    [void](Assert-SynapseAutostartLauncherIntegrity -TaskName $TaskName -ExpectedLauncherPath $hiddenLauncher -LogDir $LogDir -Phase 'post_register')
    Remove-SynapseLegacyEnsureDaemonSupervisor -RuntimeBinDir $RuntimeBinDir -CanonicalSupervisorPath $daemonSupervisorPath
    Remove-SynapseLegacyLogDirLauncherArtifacts -LogDir $LogDir -RuntimeBinDir $RuntimeBinDir
}

# ---------------------------------------------------------------------------
# 8. Health verify (source of truth: the live daemon)
# ---------------------------------------------------------------------------
Step "Verifying daemon health (http://$Bind/health)"
$ok = $false
$healthPid = $null
$daemonIdentityReadback = $null
$terminalDaemonIdentityFailure = $null
$lastHealthError = $null
$lastHealthSubsystemStatuses = '<none>'
$installHealthTimeoutSeconds = $InstallHealthTimeoutSeconds
if ($ManualInstallHealthRollbackProbe) {
    Info "Manual install-health rollback probe using production candidate health timeout seconds=$installHealthTimeoutSeconds"
} else {
    Info "Installed daemon health timeout seconds=$installHealthTimeoutSeconds"
}
$installHealthWatchdog = New-SynapseDaemonStartupWatchdog `
    -Phase 'install' `
    -LogDir $LogDir `
    -Bind $Bind `
    -DbPath $DbPath `
    -VaultPath $DbPath `
    -BaseTimeoutSeconds $installHealthTimeoutSeconds `
    -MaxSeconds $InstallHealthMaxSeconds `
    -StallSeconds $InstallHealthProgressStallSeconds
$installHealthStartedAt = $installHealthWatchdog.StartedAt
$installHealthStartedAtUtc = $installHealthWatchdog.SinceUtc
$installHealthGateVerdict = 'continue'
$installHealthAttempt = 0
Info ("SYNAPSE_STARTUP_WATCHDOG_ARMED phase=install base_timeout_s={0} absolute_cap_s={1} stall_window_s={2} sample_interval_s={3} vault={4} remediation=setup waits at least base_timeout_s, then keeps waiting only while the daemon proves forward progress (startup log phase, CPU time, disk I/O, or vault file mutation) and fails closed the moment progress stops" -f `
    $installHealthWatchdog.BaseTimeoutSeconds,
    $installHealthWatchdog.MaxSeconds,
    $installHealthWatchdog.StallSeconds,
    $installHealthWatchdog.SampleIntervalSeconds,
    $installHealthWatchdog.VaultPath)
while ($true) {
    $installHealthAttempt++
    Start-Sleep -Seconds 2
    $remainingSeconds = [Math]::Max(1, [int][Math]::Ceiling(($installHealthStartedAt.AddSeconds($installHealthWatchdog.MaxSeconds) - (Get-Date)).TotalSeconds))
    $healthTimeoutSec = [Math]::Min(30, [Math]::Max(5, $remainingSeconds))
    try {
        $h = Invoke-RestMethod -Uri "http://$Bind/health" -Headers @{ Authorization = "Bearer $token" } -TimeoutSec $healthTimeoutSec
        $lastHealthSubsystemStatuses = Format-SynapseHealthSubsystemStatuses -Health $h
        $criticalReady = Test-SynapseHealthCriticalSubsystemsReady -Health $h
        if ($criticalReady.Ok) {
            $daemonIdentityReadback = Get-SynapseInstalledDaemonIdentityReadback `
                -HealthPid ([int]$h.pid) `
                -Bind $Bind `
                -DbPath $DbPath `
                -ProfilesDir $ProfilesDir `
                -ExpectedExePath $ExePath `
                -ExpectedSha256 $installedHash `
                -LogDir $LogDir `
                -AllowedPermissions $AllowedPermissions `
                -CalyxConfigPath $CalyxConfigPath
            if (-not $daemonIdentityReadback.Ok) {
                $terminalDaemonIdentityFailure = $daemonIdentityReadback.Detail
                $installHealthGateVerdict = 'daemon_identity_mismatch'
                $lastHealthError = "SYNAPSE_INSTALL_DAEMON_IDENTITY_MISMATCH $terminalDaemonIdentityFailure"
                Info "ERROR: $lastHealthError"
                break
            }
            Info ("Daemon OK: pid={0} version={1} db={2} exe={3} sha256={4} supervisor_state={5} supervisor_child_pid={6} supervisor_settle_reads={7} supervisor_settle_wait_ms={8}" -f $h.pid, $h.version, $h.subsystems.storage.db_path, $daemonIdentityReadback.ExecutablePath, $daemonIdentityReadback.ExecutableSha256, $daemonIdentityReadback.SupervisorState, $daemonIdentityReadback.SupervisorChildPid, $daemonIdentityReadback.SupervisorSettleReads, $daemonIdentityReadback.SupervisorSettleWaitMs)
            if ($ManualInstallHealthRollbackProbe) {
                if ($ManualInstallHealthRollbackPauseMode -eq 'require_active_ack') {
                    $candidateChromeBridge = $h.subsystems.chrome_bridge
                    $candidateChromeBridgeStatus = "$($candidateChromeBridge.status)"
                    $candidateChromeBridgeDetail = "$($candidateChromeBridge.detail)"
                    $candidateChromeBridgeActive = (
                        $candidateChromeBridgeStatus -eq 'ok' -and
                        $candidateChromeBridgeDetail -match 'tab_control_available=true' -and
                        $candidateChromeBridgeDetail -match 'host_count=1' -and
                        $candidateChromeBridgeDetail -notmatch 'no_active_chrome_bridge_host')
                    if (-not $candidateChromeBridgeActive) {
                        $lastHealthError = "manual install-health rollback probe waiting for active Chrome bridge before ACK edge; pid=$($h.pid) chrome_bridge_status=$candidateChromeBridgeStatus"
                        Info "WARN: $lastHealthError subsystem_statuses=$lastHealthSubsystemStatuses chrome_bridge_detail=$candidateChromeBridgeDetail"
                        Start-Sleep -Seconds 2
                        if ((Get-Date) -ge $installHealthStartedAt.AddSeconds($installHealthWatchdog.MaxSeconds)) {
                            $installHealthGateVerdict = 'manual_probe_bridge_ack_absolute_cap'
                            $lastHealthError = "SYNAPSE_INSTALL_HEALTH_MANUAL_PROBE_BRIDGE_ACK_TIMEOUT the daemon is critical-subsystem ready but no active Chrome bridge host appeared within absolute_cap_s=$($installHealthWatchdog.MaxSeconds); remediation=attach a real Chrome bridge host before rerunning -ManualInstallHealthRollbackProbe with pause mode require_active_ack"
                            Info "ERROR: $lastHealthError"
                            break
                        }
                        continue
                    }
                    Info "Manual install-health rollback probe observed active Chrome bridge host before ACK edge pid=$($h.pid)"
                }
                $installHealthGateVerdict = 'manual_probe_rejected_ready_daemon'
                $lastHealthError = "manual install-health rollback probe rejected critical-ready daemon pid=$($h.pid)"
                Info "WARN: $lastHealthError subsystem_statuses=$lastHealthSubsystemStatuses"
                break
            }
            if ($h.ok -ne $true) {
                Info "WARN: daemon /health returned ok=false after install, but critical non-Chrome subsystems are ready; continuing to Chrome bridge repair/readback. subsystem_statuses=$lastHealthSubsystemStatuses"
            }
            $healthPid = [int]$h.pid
            $ok = $true; break
        } else {
            $lastHealthError = $criticalReady.Detail
            Info "WARN: daemon /health responded but critical subsystems are not ready yet attempt=$installHealthAttempt detail=$($criticalReady.Detail) subsystem_statuses=$lastHealthSubsystemStatuses"
        }
    } catch {
        $lastHealthError = $_.Exception.Message
        Info "WARN: daemon /health not ready yet attempt=$installHealthAttempt timeout_s=$healthTimeoutSec remaining_s=$remainingSeconds error=$lastHealthError"
    }
    if ($ok) { break }
    $installHealthTick = Update-SynapseDaemonStartupWatchdog -Watchdog $installHealthWatchdog
    if ($installHealthAttempt -eq 1 -or ($installHealthAttempt % 15) -eq 0) {
        $progressListeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        $progressProcesses = @(Get-SynapseMcpProcessSnapshot)
        $progressStartupLog = Get-SynapseDaemonStartupLogSignal -LogDir $LogDir -SinceUtc $installHealthStartedAtUtc
        Info ("SYNAPSE_INSTALL_HEALTH_PROGRESS attempt={0} elapsed_s={1} watchdog={2} last_health_error={3}`nlisteners:`n{4}`nprocesses:`n{5}`nstartup_log:`n{6}" -f `
            $installHealthAttempt,
            $installHealthTick.ElapsedSeconds,
            (Format-SynapseDaemonStartupWatchdogState -Watchdog $installHealthWatchdog),
            ($(if ([string]::IsNullOrWhiteSpace($lastHealthError)) { '<none>' } else { $lastHealthError })),
            (Format-SynapseTcpBindListenerSnapshot -Snapshot $progressListeners),
            (Format-SynapseMcpProcessSnapshot -Snapshot $progressProcesses),
            (Format-SynapseDaemonStartupLogSignal -Signal $progressStartupLog))
    }
    if (-not $installHealthTick.Continue) {
        $installHealthGateVerdict = $installHealthTick.Verdict
        Info ("SYNAPSE_INSTALL_HEALTH_WATCHDOG_FAILED_CLOSED verdict={0} {1} remediation={2}" -f `
            $installHealthGateVerdict,
            (Format-SynapseDaemonStartupWatchdogState -Watchdog $installHealthWatchdog),
            (Get-SynapseDaemonStartupWatchdogRemediation -Verdict $installHealthGateVerdict -Phase 'install'))
        break
    }
}
if (-not $ok) {
    $failureListeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
    $failureProcesses = @(Get-SynapseMcpProcessSnapshot)
    $failureStartupLog = Get-SynapseDaemonStartupLogSignal -LogDir $LogDir -SinceUtc $installHealthStartedAtUtc
    $failurePhysicalState = Format-SynapseDaemonStartupPhysicalState -DbPath $DbPath
    $failureDetail = ("SYNAPSE_INSTALL_HEALTH_FAILED bind={0} candidate_sha256={1} installed_sha256={2} backup={3} health_gate_verdict={4} health_gate_remediation={5} last_health_error={6} last_subsystem_statuses={7} manual_probe={8} manual_probe_pause_mode={9} terminal_identity_failure={10}`nstartup_watchdog:`n{11}`nlisteners:`n{12}`nprocesses:`n{13}`nstartup_log:`n{14}`nphysical_storage_state:`n{15}`nremediation=read health_gate_verdict first - it names WHY the gate ended (stalled / daemon_not_running / daemon_restart_loop / terminal_failure / absolute_cap / progress_unobservable) - then inspect {16} and synapse.log.* under {17} for launch / STORAGE_* / bind errors" -f `
        $Bind,
        $installSourceHash,
        $installedHash,
        ($(if ($backupPath) { $backupPath } else { '<none>' })),
        $installHealthGateVerdict,
        (Get-SynapseDaemonStartupWatchdogRemediation -Verdict $installHealthGateVerdict -Phase 'install'),
        ($(if ([string]::IsNullOrWhiteSpace($lastHealthError)) { '<none>' } else { $lastHealthError })),
        $lastHealthSubsystemStatuses,
        $ManualInstallHealthRollbackProbe,
        $ManualInstallHealthRollbackPauseMode,
        ($(if ([string]::IsNullOrWhiteSpace($terminalDaemonIdentityFailure)) { '<none>' } else { $terminalDaemonIdentityFailure })),
        (Format-SynapseDaemonStartupWatchdogState -Watchdog $installHealthWatchdog),
        (Format-SynapseTcpBindListenerSnapshot -Snapshot $failureListeners),
        (Format-SynapseMcpProcessSnapshot -Snapshot $failureProcesses),
        (Format-SynapseDaemonStartupLogSignal -Signal $failureStartupLog),
        $failurePhysicalState,
        $launcherLog,
        $LogDir)

    if ($backupPath -and (Test-Path -LiteralPath $backupPath) -and $oldInstalledHash) {
        Info "WARN: $failureDetail"
        Info "Attempting rollback to previous daemon binary backup=$backupPath sha256=$oldInstalledHash"
        if ($ManualInstallHealthRollbackProbe) {
            New-HiddenDaemonLauncher `
                -OutputPath $hiddenLauncher `
                -ExePath $ExePath `
                -Bind $Bind `
                -DbPath $DbPath `
                -ProfilesDir $ProfilesDir `
                -LogDir $LogDir `
                -TokenPath $TokenPath `
                -MaintenanceLockPath $MaintenanceLockPath `
                -EnableAudio $EnableAudio `
                -AllowedPermissions $AllowedPermissions `
                -CalyxConfigPath $CalyxConfigPath
            Info "Manual install-health rollback probe restored normal daemon launcher before rollback stop path=$hiddenLauncher"
        }
        if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
            Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        }
        Stop-SynapseMcpProcesses -Reason 'install_health_failed_rollback' -Bind $Bind -DbPath $DbPath -TokenPath $TokenPath -ForceRestart -AllowUnacknowledgedChromeBridgePauseForRollback -TimeoutSeconds 300
        Copy-Item -LiteralPath $backupPath -Destination $ExePath -Force
        $rollbackHash = Get-SynapseFileSha256 -Path $ExePath
        if ($rollbackHash -ne $oldInstalledHash) {
            Die "SYNAPSE_INSTALL_HEALTH_FAILED_ROLLBACK_HASH_MISMATCH expected_sha256=$oldInstalledHash actual_sha256=$rollbackHash backup=$backupPath install_path=$ExePath original_failure=[$failureDetail]"
        }
        foreach ($runtimeBackup in $runtimeCompanionBackups) {
            if ($runtimeBackup.Existed) {
                Copy-Item -LiteralPath $runtimeBackup.BackupPath -Destination $runtimeBackup.Path -Force
                $runtimeRollbackHash = Get-SynapseFileSha256 -Path $runtimeBackup.Path
                if ($runtimeRollbackHash -ne $runtimeBackup.Sha256) {
                    Die "SYNAPSE_INSTALL_HEALTH_FAILED_RUNTIME_ROLLBACK_HASH_MISMATCH path=$($runtimeBackup.Path) expected_sha256=$($runtimeBackup.Sha256) actual_sha256=$runtimeRollbackHash original_failure=[$failureDetail]"
                }
            } elseif (Test-Path -LiteralPath $runtimeBackup.Path -PathType Leaf) {
                Remove-Item -LiteralPath $runtimeBackup.Path -Force
                if (Test-Path -LiteralPath $runtimeBackup.Path) {
                    Die "SYNAPSE_INSTALL_HEALTH_FAILED_RUNTIME_ROLLBACK_REMOVE_FAILED path=$($runtimeBackup.Path) original_failure=[$failureDetail]"
                }
            }
        }
        if ($ManualInstallHealthRollbackProbe) {
            New-HiddenDaemonLauncher `
                -OutputPath $hiddenLauncher `
                -ExePath $ExePath `
                -Bind $Bind `
                -DbPath $DbPath `
                -ProfilesDir $ProfilesDir `
                -LogDir $LogDir `
                -TokenPath $TokenPath `
                -MaintenanceLockPath $MaintenanceLockPath `
                -EnableAudio $EnableAudio `
                -AllowedPermissions $AllowedPermissions `
                -CalyxConfigPath $CalyxConfigPath
            Info "Manual install-health rollback probe restored normal daemon launcher before rollback start path=$hiddenLauncher"
        }
        Start-ScheduledTask -TaskName $TaskName
        $rollbackOk = $false
        $rollbackHealth = $null
        $rollbackLastHealthError = $null
        $rollbackLastSubsystemStatuses = '<none>'
        $rollbackSupervisorPath = Join-Path $RuntimeBinDir 'synapse-daemon-supervisor.ps1'
        $rollbackWatchdog = New-SynapseDaemonStartupWatchdog `
            -Phase 'rollback' `
            -LogDir $LogDir `
            -Bind $Bind `
            -DbPath $DbPath `
            -VaultPath $DbPath `
            -BaseTimeoutSeconds $installHealthTimeoutSeconds `
            -MaxSeconds $InstallHealthMaxSeconds `
            -StallSeconds $InstallHealthProgressStallSeconds
        $rollbackGateVerdict = 'continue'
        $rollbackHealthAttempt = 0
        Info ("SYNAPSE_STARTUP_WATCHDOG_ARMED phase=rollback base_timeout_s={0} absolute_cap_s={1} stall_window_s={2} sample_interval_s={3} remediation=the rollback daemon opens the SAME vault the candidate could not finish opening, so it gets the same progress-aware budget; a rollback that is still making forward progress is never reported as a failed rollback" -f `
            $rollbackWatchdog.BaseTimeoutSeconds,
            $rollbackWatchdog.MaxSeconds,
            $rollbackWatchdog.StallSeconds,
            $rollbackWatchdog.SampleIntervalSeconds)
        while ($true) {
            $rollbackHealthAttempt++
            Start-Sleep -Seconds 2
            $remainingSeconds = [Math]::Max(1, [int][Math]::Ceiling(($rollbackWatchdog.StartedAt.AddSeconds($rollbackWatchdog.MaxSeconds) - (Get-Date)).TotalSeconds))
            $rollbackHealthTimeoutSec = [Math]::Min(30, [Math]::Max(5, $remainingSeconds))
            try {
                $rh = Invoke-RestMethod -Uri "http://$Bind/health" -Headers @{ Authorization = "Bearer $token" } -TimeoutSec $rollbackHealthTimeoutSec
                $rollbackLastSubsystemStatuses = Format-SynapseHealthSubsystemStatuses -Health $rh
                $rollbackCriticalReady = Test-SynapseHealthCriticalSubsystemsReady -Health $rh
                if ($rollbackCriticalReady.Ok) {
                    $rollbackHealth = $rh
                    $rollbackOk = $true
                    break
                } else {
                    $rollbackLastHealthError = $rollbackCriticalReady.Detail
                    Info "WARN: rollback daemon /health responded but critical subsystems are not ready yet attempt=$rollbackHealthAttempt detail=$($rollbackCriticalReady.Detail) subsystem_statuses=$rollbackLastSubsystemStatuses"
                }
            } catch {
                $rollbackLastHealthError = $_.Exception.Message
                Info "WARN: rollback daemon /health not ready yet attempt=$rollbackHealthAttempt timeout_s=$rollbackHealthTimeoutSec remaining_s=$remainingSeconds error=$rollbackLastHealthError"
            }
            $rollbackTick = Update-SynapseDaemonStartupWatchdog -Watchdog $rollbackWatchdog
            if (-not $rollbackTick.Continue) {
                $rollbackGateVerdict = $rollbackTick.Verdict
                Info ("SYNAPSE_ROLLBACK_HEALTH_WATCHDOG_FAILED_CLOSED verdict={0} {1} remediation={2}" -f `
                    $rollbackGateVerdict,
                    (Format-SynapseDaemonStartupWatchdogState -Watchdog $rollbackWatchdog),
                    (Get-SynapseDaemonStartupWatchdogRemediation -Verdict $rollbackGateVerdict -Phase 'rollback'))
                break
            }
        }
        if ($rollbackOk) {
            $rollbackHealth = Assert-SynapseChromeBridgeLiveAfterSetup `
                -Bind $Bind `
                -Token $token `
                -Health $rollbackHealth `
                -ChromeBridgeInstallerPath $chromeBridgeInstaller `
                -ChromeNativeHostExePath $ChromeNativeHostExePath
            $rollbackBridge = $rollbackHealth.subsystems.chrome_bridge
            $rollbackHealthyFacts = Get-SynapseRollbackPhysicalFacts `
                -ExePath $ExePath `
                -ExpectedSha256 $oldInstalledHash `
                -BackupPath $backupPath `
                -TaskName $TaskName `
                -SupervisorPath $rollbackSupervisorPath `
                -LogDir $LogDir `
                -Bind $Bind `
                -DbPath $DbPath `
                -Watchdog $rollbackWatchdog `
                -DaemonHealthy
            Die ("SYNAPSE_INSTALL_HEALTH_FAILED_ROLLED_BACK candidate_sha256={0} rollback_sha256={1} rollback_pid={2} rollback_subsystem_statuses={3} rollback_chrome_bridge_status={4} rollback_chrome_bridge_detail={5}`nrollback_facts:`n{6}`noriginal_failure=[{7}]`nremediation={8} Old daemon and Chrome bridge Source of Truth were re-read after rollback; inspect candidate startup logs before retrying." -f `
                $installSourceHash,
                $rollbackHash,
                $rollbackHealth.pid,
                $rollbackLastSubsystemStatuses,
                ($(if ($null -eq $rollbackBridge) { '<missing>' } else { $rollbackBridge.status })),
                ($(if ($null -eq $rollbackBridge) { '<missing>' } else { $rollbackBridge.detail })),
                (Format-SynapseRollbackPhysicalFacts -Facts $rollbackHealthyFacts),
                $failureDetail,
                (Get-SynapseRollbackVerdictRemediation -Facts $rollbackHealthyFacts))
        }

        $rollbackListeners = @(Get-SynapseTcpBindListenerSnapshot -Bind $Bind)
        $rollbackProcesses = @(Get-SynapseMcpProcessSnapshot)
        $rollbackStartupLog = Get-SynapseDaemonStartupLogSignal -LogDir $LogDir -SinceUtc $rollbackWatchdog.SinceUtc
        $rollbackFacts = Get-SynapseRollbackPhysicalFacts `
            -ExePath $ExePath `
            -ExpectedSha256 $oldInstalledHash `
            -BackupPath $backupPath `
            -TaskName $TaskName `
            -SupervisorPath $rollbackSupervisorPath `
            -LogDir $LogDir `
            -Bind $Bind `
            -DbPath $DbPath `
            -Watchdog $rollbackWatchdog
        $rollbackFactsText = Format-SynapseRollbackPhysicalFacts -Facts $rollbackFacts
        $rollbackRemediation = Get-SynapseRollbackVerdictRemediation -Facts $rollbackFacts
        $rollbackDieCode = $(if ($rollbackFacts.RollbackSucceeded) {
            'SYNAPSE_INSTALL_HEALTH_FAILED_ROLLED_BACK_DAEMON_RECOVERING'
        } else {
            'SYNAPSE_INSTALL_HEALTH_FAILED_ROLLBACK_FAILED'
        })
        Die ("{0} candidate_sha256={1} rollback_sha256={2} rollback_gate_verdict={3} rollback_last_health_error={4} rollback_last_subsystem_statuses={5}`nrollback_facts:`n{6}`nrollback_startup_watchdog:`n{7}`nrollback_listeners:`n{8}`nrollback_processes:`n{9}`nrollback_startup_log:`n{10}`noriginal_failure=[{11}]`nremediation={12} Inspect {13} and synapse.log.* under {14}." -f `
            $rollbackDieCode,
            $installSourceHash,
            $rollbackHash,
            $rollbackGateVerdict,
            ($(if ([string]::IsNullOrWhiteSpace($rollbackLastHealthError)) { '<none>' } else { $rollbackLastHealthError })),
            $rollbackLastSubsystemStatuses,
            $rollbackFactsText,
            (Format-SynapseDaemonStartupWatchdogState -Watchdog $rollbackWatchdog),
            (Format-SynapseTcpBindListenerSnapshot -Snapshot $rollbackListeners),
            (Format-SynapseMcpProcessSnapshot -Snapshot $rollbackProcesses),
            (Format-SynapseDaemonStartupLogSignal -Signal $rollbackStartupLog),
            $failureDetail,
            $rollbackRemediation,
            $launcherLog,
            $LogDir)
    }

    Die $failureDetail
}

# The daemon handoff and strict MCP surface are committed before Chrome bridge
# activation begins. Persist that independently verified surface now: a
# background-only Chrome activation may legitimately checkpoint as pending, and
# withholding the daemon snapshot until after that separate transaction traps
# every freshly restarted MCP client on the previous tools/list hash.
$toolSurface = Read-SynapseDaemonToolSurface -Bind $Bind -Token $token -Health $h
Write-SynapseCodexToolSurfaceSnapshot -Path $CodexToolSurfaceSnapshotPath -Surface $toolSurface

try {
    $h = Assert-SynapseChromeBridgeLiveAfterSetup `
        -Bind $Bind `
        -Token $token `
        -Health $h `
        -ChromeBridgeInstallerPath $chromeBridgeInstaller `
        -ChromeNativeHostExePath $ChromeNativeHostExePath
} catch {
    # A committed daemon handoff and a pending Chrome activation are two
    # different transactions.  In particular, a legacy worker that predates
    # reloadSelf cannot consume freshly deployed bytes without either a natural
    # Chrome lifecycle transition or foreground UI.  Rolling the healthy daemon
    # back does not repair that browser state, while trying UI violates the
    # background-only contract.  Persist the exact committed identities and
    # fail loudly so -ResumeChromeBridgePending can finish only this phase after
    # Chrome has naturally loaded the debugger-free worker.
    $bridgeActivationError = $_.Exception.Message
    $pendingDaemonPid = [int]$h.pid
    $pendingDaemonProcess = Get-SynapseDaemonProcessIdentity -ProcessId $pendingDaemonPid
    $pendingTask = Get-SynapseScheduledTaskIdentity -Name $TaskName
    $pendingDaemonRunPath = Join-Path $DbPath 'daemon-run-current.json'
    if (-not (Test-Path -LiteralPath $pendingDaemonRunPath -PathType Leaf)) {
        Die "SYNAPSE_SETUP_BRIDGE_PENDING_DAEMON_RUN_MISSING path=$pendingDaemonRunPath daemon_pid=$pendingDaemonPid bridge_error=$bridgeActivationError remediation=repair the daemon lifecycle ledger before setup can persist an identity-bound Chrome activation checkpoint"
    }
    $pendingReadback = [ordered]@{
        state = 'pending'
        checkpoint_generation_id = $script:SynapseSetupInvocationId
        resume_attempt_count = 0
        daemon_handoff = 'committed'
        daemon_pid = $pendingDaemonPid
        bind = $Bind
        db_path = $DbPath
        installed_binary_path = [System.IO.Path]::GetFullPath($ExePath)
        installed_binary_sha256 = $installedHash
        daemon_process_executable_path = $pendingDaemonProcess.ExecutablePath
        daemon_process_command_line = $pendingDaemonProcess.CommandLine
        daemon_process_creation_date = $pendingDaemonProcess.CreationDate
        daemon_run_current_path = [System.IO.Path]::GetFullPath($pendingDaemonRunPath)
        daemon_run_current_sha256 = Get-SynapseFileSha256 -Path $pendingDaemonRunPath
        token_path = [System.IO.Path]::GetFullPath($TokenPath)
        token_sha256 = Get-SynapseFileSha256 -Path $TokenPath
        setup_script_path = [System.IO.Path]::GetFullPath($PSCommandPath)
        setup_script_sha256 = Get-SynapseFileSha256 -Path $PSCommandPath
        chrome_bridge_installer_path = [System.IO.Path]::GetFullPath($chromeBridgeInstaller)
        chrome_bridge_installer_sha256 = Get-SynapseFileSha256 -Path $chromeBridgeInstaller
        chrome_native_host_exe_path = [System.IO.Path]::GetFullPath($ChromeNativeHostExePath)
        task_name = $TaskName
        task_definition_sha256 = $pendingTask.DefinitionSha256
        task_action_execute = $pendingTask.ActionExecute
        task_action_arguments = $pendingTask.ActionArguments
        maintenance_lock_path = [System.IO.Path]::GetFullPath($MaintenanceLockPath)
        chrome_bridge_activation_error = $bridgeActivationError
        chrome_bridge_health = $h.subsystems.chrome_bridge
        remediation = 'leave the healthy daemon running and normal Chrome bridge commands failed closed; after Chrome naturally loads the deployed debugger-free worker, run scripts\synapse-setup.ps1 -ResumeChromeBridgePending to verify the exact checkpointed daemon/task/binary/token/DB identities and complete only Chrome activation'
    }
    Die-SynapseChromeBridgePending `
        -Message "SYNAPSE_CHROME_BRIDGE_ACTIVATION_PENDING daemon_pid=$pendingDaemonPid bind=$Bind checkpoint=$($script:SynapseChromeBridgePendingPath) bridge_error=$bridgeActivationError remediation=do not activate, restore, navigate, click, type into, or restart a human Chrome window; wait for a natural Chrome lifecycle transition, then run -ResumeChromeBridgePending" `
        -Readback $pendingReadback
}
$healthPid = [int]$h.pid
$daemonLineage = Get-ProcessLineage -StartPid $healthPid
$cmdAncestor = $daemonLineage | Where-Object { $_.Name -ieq 'cmd.exe' } | Select-Object -First 1
if ($cmdAncestor) {
    $lineageText = ($daemonLineage | ForEach-Object { "{0}:{1}" -f $_.ProcessId, $_.Name }) -join ' <- '
    Die "SYNAPSE_DAEMON_CMD_ANCESTOR_FORBIDDEN pid=$healthPid cmd_pid=$($cmdAncestor.ProcessId) lineage=$lineageText remediation=rerun setup after removing legacy daemon launchers; daemon must not be launched through cmd.exe."
}

# ---------------------------------------------------------------------------
# 9. Wire the Windows-side MCP clients
# ---------------------------------------------------------------------------
if (-not $SkipClientWiring) {
    Step "Wiring Windows-side MCP clients"

    # Claude Code (Windows) speaks Streamable HTTP natively -> point at the daemon.
    $claude = Get-Command claude -ErrorAction SilentlyContinue
    if ($claude) {
        try {
            & $claude.Source mcp remove synapse -s user 2>$null | Out-Null
            & $claude.Source mcp add --scope user --transport http synapse "http://$Bind/mcp" --header "Authorization: Bearer $token"
            Info "Claude Code (Windows) wired via HTTP transport."
        } catch { Info "WARN: 'claude mcp add' failed: $($_.Exception.Message). Wire it manually (transport http -> http://$Bind/mcp)." }
    } else { Info "claude CLI not found on Windows PATH; skipping Claude Code wiring." }

    # Codex speaks Streamable HTTP; Claude Desktop remains stdio-only -> connect bridge.
    $bridgeArgs = @('--mode','connect','--bind',$Bind)

    $codex = Get-Command codex -ErrorAction SilentlyContinue
    $codexCfg = "$env:USERPROFILE\.codex\config.toml"
    if ($codex) {
        if (Test-CodexSynapseHttpTransportConfig -ConfigPath $codexCfg -Bind $Bind) {
            Info "Codex MCP entry already uses the required Streamable HTTP transport."
        } else {
            & $codex.Source mcp remove synapse 2>$null | Out-Null
            & $codex.Source mcp add synapse --url "http://$Bind/mcp" --bearer-token-env-var SYNAPSE_BEARER_TOKEN
            $codexAddExit = $LASTEXITCODE
            if ($codexAddExit -ne 0 -and -not (Test-CodexSynapseHttpTransportConfig -ConfigPath $codexCfg -Bind $Bind)) {
                Die "codex mcp add failed (exit $codexAddExit). Codex must be wired to HTTP, not the connect bridge."
            }
            if (-not (Test-CodexSynapseHttpTransportConfig -ConfigPath $codexCfg -Bind $Bind)) {
                Die "codex mcp add completed but Codex config is not the required HTTP transport."
            }
            if ($codexAddExit -ne 0) {
                Info "WARN: codex mcp add exited $codexAddExit but Codex config now contains the required HTTP entry; continuing."
            }
        }
        Set-CodexSynapseClientPolicy -ConfigPath $codexCfg -Bind $Bind -StartupTimeoutSec $CodexMcpStartupTimeoutSeconds
        if (-not (Test-CodexSynapseHttpConfig -ConfigPath $codexCfg -Bind $Bind -StartupTimeoutSec $CodexMcpStartupTimeoutSeconds)) {
            Die "SYNAPSE_CODEX_MCP_CONFIG_INCOMPLETE path=$codexCfg remediation=repair [mcp_servers.synapse] so it contains url=http://$Bind/mcp, bearer_token_env_var=SYNAPSE_BEARER_TOKEN, required=true, default_tools_approval_mode=approve, and exactly one startup_timeout_sec=$CodexMcpStartupTimeoutSeconds."
        }
        Install-CodexSynapseTokenLoader -CodexCommandPath $codex.Source -TokenPath $TokenPath
        Info "Codex (Windows) wired via Streamable HTTP transport with required=true, default_tools_approval_mode=approve, and startup_timeout_sec=$CodexMcpStartupTimeoutSeconds."
    } elseif (Test-Path $codexCfg) {
        $c = Get-Content -Raw $codexCfg
        if ($c -match '(?m)^\[mcp_servers\.synapse\]' -and
            -not (Test-CodexSynapseHttpConfig -ConfigPath $codexCfg -Bind $Bind -StartupTimeoutSec $CodexMcpStartupTimeoutSeconds)) {
            Die "Codex config exists at $codexCfg but codex CLI is not on PATH and the synapse entry is not the required HTTP transport/client policy. Install/repair Codex CLI, then re-run."
        }
        Info "Codex CLI not found; existing Codex config is already HTTP or has no synapse entry."
    } else { Info "codex CLI/config not found; skipping Codex wiring." }

    $desktopCfg = "$env:APPDATA\Claude\claude_desktop_config.json"
    if (Test-Path $desktopCfg) {
        try {
            $j = Get-Content -Raw $desktopCfg | ConvertFrom-Json
            if (-not $j.mcpServers) { $j | Add-Member -NotePropertyName mcpServers -NotePropertyValue (@{}) -Force }
            $desktopEntry = @{ command = $ExePath; args = $bridgeArgs; env = @{ SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY = '1' } }
            # $j.mcpServers is a hashtable when freshly created above, but a PSCustomObject when
            # parsed from an existing config. Dot-assigning a NEW property to a PSCustomObject throws
            # "The property 'synapse' cannot be found on this object" under Windows PowerShell 5.1,
            # so branch on type: index-assign dictionaries, Add-Member -Force PSCustomObjects (the
            # latter both adds-or-overwrites and works on PS 5.1 and 7+).
            if ($j.mcpServers -is [System.Collections.IDictionary]) {
                $j.mcpServers['synapse'] = $desktopEntry
            } else {
                $j.mcpServers | Add-Member -NotePropertyName synapse -NotePropertyValue $desktopEntry -Force
            }
            ($j | ConvertTo-Json -Depth 12) | Set-Content $desktopCfg -Encoding utf8
            Info "Claude Desktop wired -> connect bridge."
        } catch { Info "WARN: could not update $desktopCfg : $($_.Exception.Message)" }
    } else { Info "No Claude Desktop config at $desktopCfg; skipping." }
}

if (-not $SkipClientWiring) {
    $codexAncestor = Get-SynapseCurrentCodexAncestor
    if ($codexAncestor -and $processTokenAtStart -ne $token) {
        Info ("WARN: SYNAPSE_CODEX_CURRENT_PROCESS_ENV_STALE_NONFATAL codex_pid={0} token_at_process_start={1} token_file={2} remediation=do not assume the current Codex process is disconnected; first call real mcp__synapse.health from this same session and verify daemon PID/tool_surface readback. The patched launcher has been updated for future clients; if this already-running process has no authenticated MCP connection, use the existing live daemon and token file as diagnostics until the client can refresh its environment." -f $codexAncestor.ProcessId, ($(if ([string]::IsNullOrWhiteSpace($processTokenAtStart)) { 'missing' } else { 'mismatch' })), $TokenPath)
    }
    Assert-CodexCurrentProcessToolSurfaceFresh `
        -CodexAncestor $codexAncestor `
        -CurrentSurface $toolSurface `
        -ProcessHashAtStart $processToolSurfaceHashAtStart `
        -ProcessSnapshotAtStart $processToolSurfaceSnapshotAtStart `
        -SnapshotPath $CodexToolSurfaceSnapshotPath `
        -SourceDir $SourceDir `
        -Bind $Bind `
        -TokenPath $TokenPath `
        -ActiveIssue $ActiveIssue
} else {
    $codexAncestor = Get-SynapseCurrentCodexAncestor
    if ($codexAncestor) {
        Assert-CodexCurrentProcessToolSurfaceFresh `
            -CodexAncestor $codexAncestor `
            -CurrentSurface $toolSurface `
            -ProcessHashAtStart $processToolSurfaceHashAtStart `
            -ProcessSnapshotAtStart $processToolSurfaceSnapshotAtStart `
            -SnapshotPath $CodexToolSurfaceSnapshotPath `
            -SourceDir $SourceDir `
            -Bind $Bind `
            -TokenPath $TokenPath `
            -ActiveIssue $ActiveIssue `
            -NonFatal
        Info "Skipped client wiring because -SkipClientWiring was set; current-process freshness check still wrote any required current-daemon handoff in nonfatal mode."
    } else {
        Info "Skipped client wiring because -SkipClientWiring was set; no current Codex ancestor was found for a freshness handoff."
    }
}

$checkpointTerminalization = Complete-SynapseObsoleteChromeBridgeCheckpoint `
    -Path $script:SynapseChromeBridgePendingPath `
    -DaemonPid $healthPid `
    -Bind $Bind `
    -DbPath $DbPath `
    -InstalledBinaryPath $ExePath `
    -InstalledBinarySha256 $installedHash `
    -ToolSurface $toolSurface `
    -ChromeBridge $h.subsystems.chrome_bridge

if ($script:SynapsePostExitStartOnly) {
    $completionReadback = [ordered]@{
        daemon_pid = $healthPid
        bind = $Bind
        db_path = $DbPath
        daemon_run_current_path = (Join-Path $DbPath 'daemon-run-current.json')
        installed_binary_path = $ExePath
        installed_binary_sha256 = $installedHash
        codex_tool_surface_snapshot_path = $CodexToolSurfaceSnapshotPath
        tool_count = $toolSurface.tool_count
        tool_surface_sha256 = $toolSurface.tool_surface_sha256
        chrome_bridge_status = $h.subsystems.chrome_bridge.status
        chrome_bridge_detail = $h.subsystems.chrome_bridge.detail
        chrome_bridge_checkpoint = $checkpointTerminalization
    }
    Write-SynapsePostExitManifestState `
        -State 'completed' `
        -Message 'post-exit setup continuation completed after daemon, Chrome bridge, tool-surface, and client-config readbacks passed' `
        -ExitCode 0 `
        -Readback $completionReadback
    Info "SYNAPSE_POST_EXIT_CONTINUATION_COMPLETED manifest=$PostExitManifestPath daemon_pid=$healthPid tool_count=$($toolSurface.tool_count) tool_surface_sha256=$($toolSurface.tool_surface_sha256)"
}

$setupRepairCompletionReadback = [ordered]@{
    daemon_pid = $healthPid
    bind = $Bind
    db_path = $DbPath
    daemon_run_current_path = (Join-Path $DbPath 'daemon-run-current.json')
    installed_binary_path = $ExePath
    installed_binary_sha256 = $installedHash
    codex_tool_surface_snapshot_path = $CodexToolSurfaceSnapshotPath
    tool_count = $toolSurface.tool_count
    tool_surface_sha256 = $toolSurface.tool_surface_sha256
    chrome_bridge_status = $h.subsystems.chrome_bridge.status
    chrome_bridge_detail = $h.subsystems.chrome_bridge.detail
    chrome_bridge_checkpoint = $checkpointTerminalization
}
Write-SynapseSetupRepairManifestState `
    -State 'completed' `
    -Message 'setup repair completed after daemon, Chrome bridge, tool-surface, and client-config readbacks passed' `
    -ExitCode 0 `
    -Readback $setupRepairCompletionReadback

# The maintenance record and completion ledger are the durable success
# authorities. Publish them before any informational output: a setup child may
# legitimately outlive a bounded caller, whose stdout pipe is no longer a
# dependency the transaction can trust (#2207).
Release-SynapseSetupMaintenanceLock -State released
$phaseLedgerPath = Write-SynapseSetupPhaseLedger -Outcome 'completed' -Message 'setup completed'
Step "Done"
Info "Synapse daemon is live on http://$Bind (MCP: http://$Bind/mcp)."
Info "Token: $TokenPath   DB: $DbPath   Profiles: $ProfilesDir"
Info "WSL clients: run scripts/synapse-install.sh from WSL to wire Claude Code + Codex there."
if ($phaseLedgerPath) {
    $ledgerReadback = Get-Content -LiteralPath $phaseLedgerPath -Raw | ConvertFrom-Json
    Info ("Setup wall clock: total={0}s phases={1} cargo_build_jobs={2} ledger={3}" -f `
        $ledgerReadback.total_elapsed_seconds,
        $ledgerReadback.phase_count,
        ($(if ($ledgerReadback.cargo_build_jobs) { $ledgerReadback.cargo_build_jobs } else { '<unset:SkipBuild>' })),
        $phaseLedgerPath)
    foreach ($slow in @($ledgerReadback.slowest_phases)) {
        Info ("  slowest_phase elapsed_s={0} phase={1}" -f $slow.elapsed_seconds, $slow.phase)
    }
} else {
    if (-not [string]::IsNullOrWhiteSpace($script:SynapseSetupPhaseLedgerWriteError)) {
        Warn $script:SynapseSetupPhaseLedgerWriteError
    } else {
        Warn "SYNAPSE_SETUP_PHASE_LEDGER_UNWRITTEN log_dir=$LogDir remediation=setup completed but its phase timing ledger could not be written; inspect LogDir permissions"
    }
}
