# Registers (or removes) the SynapseMcpPreCalyx scheduled task that keeps the
# restored RocksDB daemon running (#2265).
#
# Triggers: at logon, plus every minute. The repeating trigger recovers the
# daemon when it dies while the user stays signed in (e.g. a Start-menu restart
# that closes apps and then aborts leaves no new logon). While the daemon runs,
# the launcher instance stays alive and MultipleInstances=IgnoreNew skips the
# repeat, so it only acts after the daemon has exited.
[CmdletBinding()]
param(
    [string]$TaskName = 'SynapseMcpPreCalyx',
    [string]$SourceDir,
    [switch]$Remove
)
$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 leaves $PSScriptRoot empty inside param() defaults; resolve it here.
if (-not $SourceDir) { $SourceDir = Split-Path -Parent $PSScriptRoot }

if ($Remove) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    return
}

$launcher = Join-Path $SourceDir 'scripts\start-pre-calyx-daemon.ps1'
if (-not (Test-Path -LiteralPath $launcher -PathType Leaf)) {
    throw "Launcher is missing: $launcher"
}
# The task never starts a console program directly: powershell.exe launched by Task Scheduler
# gets a visible console (an OpenConsole window under Windows Terminal) before
# -WindowStyle Hidden applies, so the 1-minute watchdog flashed a terminal every minute.
# The windowless wscript.exe host starts the launcher hidden and returns its exit code.
$hiddenLauncher = Join-Path $SourceDir 'scripts\start-pre-calyx-daemon-hidden.vbs'
if (-not (Test-Path -LiteralPath $hiddenLauncher -PathType Leaf)) {
    throw "Hidden launcher is missing: $hiddenLauncher"
}
$user = "$env:USERDOMAIN\$env:USERNAME"
$action = New-ScheduledTaskAction `
    -Execute "$env:SystemRoot\System32\wscript.exe" `
    -Argument "//B //Nologo `"$hiddenLauncher`"" `
    -WorkingDirectory $SourceDir
$logon = New-ScheduledTaskTrigger -AtLogOn -User $user
$watchdog = New-ScheduledTaskTrigger -Once -At (Get-Date) `
    -RepetitionInterval (New-TimeSpan -Minutes 1)
$settings = New-ScheduledTaskSettingsSet `
    -MultipleInstances IgnoreNew -StartWhenAvailable `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited

Register-ScheduledTask -TaskName $TaskName -Action $action `
    -Trigger @($logon, $watchdog) -Settings $settings -Principal $principal -Force |
    Out-Null
# Two invariants, checked against what Task Scheduler actually stored:
# 1. No console window: the action is the windowless wscript.exe host, never a console
#    program (powershell/pwsh/cmd/conhost/wt), which flashes a terminal on every run.
# 2. The task result is the launcher's own exit code. The .vbs returns Run()'s exit code
#    (measured 2026-11-11: exit 7 -> 7, throw -> 1, exit 0 -> 0). conhost.exe --headless
#    stays rejected: it recorded 0 even when the launcher threw.
$registered = @((Get-ScheduledTask -TaskName $TaskName).Actions)
$consoleHosts = 'powershell.exe', 'pwsh.exe', 'cmd.exe', 'conhost.exe', 'wt.exe', 'OpenConsole.exe'
if ($registered.Count -ne 1 -or
    $registered[0].Execute -ne $action.Execute -or
    $registered[0].Arguments -ne $action.Arguments -or
    $consoleHosts -contains (Split-Path -Leaf $registered[0].Execute)) {
    throw "Registered action is not the windowless launcher: $(($registered | ForEach-Object { "$($_.Execute) $($_.Arguments)" }) -join ', ')"
}
Get-ScheduledTask -TaskName $TaskName | Select-Object TaskName, State,
    @{ n = 'Triggers'; e = { ($_.Triggers | ForEach-Object { $_.CimClass.CimClassName }) -join ', ' } }
