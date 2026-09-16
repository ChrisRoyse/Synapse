<#
.SYNOPSIS
  Register (or remove) a Windows Scheduled Task that runs repo-maintenance.ps1
  automatically, so git-worktree + Cargo target/ disk buildup can never silently
  return. This is the "set it and forget it" half of the buildup prevention.

.DESCRIPTION
  Creates a scheduled task named "SynapseRepoMaintenance" that runs:

      pwsh -NoProfile -File <repo>\scripts\repo-maintenance.ps1 -Apply

  on a weekly cadence (default: Sunday 03:00) plus once shortly after each logon
  (a one-off catch-up if the machine was off at the scheduled time). The task
  runs as the current user (no admin needed — all it does is remove merged/stale
  worktrees and sweep stale build artifacts, both in user-owned directories).

  Re-running this installer is idempotent: it replaces any existing task.

  No paid services, no cloud, no GitHub Actions — this is a purely local OS
  scheduler entry, consistent with this project's local-only operating model.

.PARAMETER Remove
  Unregister the scheduled task instead of installing it.

.PARAMETER At
  Time of day (HH:mm) for the weekly run. Default 03:00.

.PARAMETER DayOfWeek
  Day for the weekly run. Default Sunday.

.PARAMETER SweepDays
  Forwarded to repo-maintenance.ps1 (-SweepDays). Default 10.

.EXAMPLE
  pwsh -File .\scripts\install-maintenance-task.ps1
.EXAMPLE
  pwsh -File .\scripts\install-maintenance-task.ps1 -Remove
#>
[CmdletBinding()]
param(
    [switch]$Remove,
    [string]$At = '03:00',
    [ValidateSet('Sunday','Monday','Tuesday','Wednesday','Thursday','Friday','Saturday')]
    [string]$DayOfWeek = 'Sunday',
    [int]$SweepDays = 10
)

$ErrorActionPreference = 'Stop'
$TaskName = 'SynapseRepoMaintenance'
function Info($m) { Write-Host "[maint-task] $m" }
function Die($m)  { throw "[maint-task] FATAL: $m" }

if ($Remove) {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Info "Removed scheduled task '$TaskName'."
    } else { Info "No task '$TaskName' to remove." }
    return
}

# Resolve pwsh 7 (the script uses PS7-only syntax; Windows PowerShell 5.1 cannot run it).
$pwshCmd = Get-Command pwsh -ErrorAction SilentlyContinue
if (-not $pwshCmd) { Die "pwsh (PowerShell 7+) not found on PATH. Install it: winget install Microsoft.PowerShell" }
$pwsh = $pwshCmd.Source

$repoRoot   = Split-Path -Parent $PSScriptRoot
$maintScript = Join-Path $PSScriptRoot 'repo-maintenance.ps1'
if (-not (Test-Path $maintScript)) { Die "Cannot find $maintScript" }

# The task cleans ALL repos under the parent of this repo (e.g. C:\code), so the
# whole dev tree stays flat — not just Synapse.
$scanRoot = Split-Path -Parent $repoRoot

$argline = "-NoProfile -ExecutionPolicy Bypass -File `"$maintScript`" -Apply -Root `"$scanRoot`" -SweepDays $SweepDays"
$action  = New-ScheduledTaskAction -Execute $pwsh -Argument $argline

# Weekly trigger only. A logon trigger would require elevation to register
# (logon triggers are machine-scoped), so instead we rely on -StartWhenAvailable
# below: if the PC is off at the scheduled time, the task runs at the next
# opportunity. This keeps the whole installer non-elevated.
$tWeekly = New-ScheduledTaskTrigger -Weekly -DaysOfWeek $DayOfWeek -At $At

$settings  = New-ScheduledTaskSettingsSet -StartWhenAvailable -DontStopOnIdleEnd `
                -ExecutionTimeLimit (New-TimeSpan -Hours 2) -MultipleInstances IgnoreNew

if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Info "Replaced existing task."
}
# No -Principal => registers for the current interactive user; runs while logged
# on. Needs no elevation; deleting worktrees and sweeping target/ are all
# user-owned file operations.
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $tWeekly `
    -Settings $settings `
    -Description "Synapse: prune merged/stale git worktrees and sweep stale Cargo target/ artifacts across $scanRoot. Prevents disk buildup. Source: scripts/repo-maintenance.ps1" | Out-Null

$t = Get-ScheduledTask -TaskName $TaskName
Info "Installed '$TaskName': runs weekly on $DayOfWeek at $At (catches up via -StartWhenAvailable if the PC was off)."
Info "  exec: $pwsh $argline"
Info "Verify any time with:  Get-ScheduledTask -TaskName $TaskName ;  run now with:  Start-ScheduledTask -TaskName $TaskName"
