# Registers (or removes) the SynapseMcpPreCalyx scheduled task that keeps the
# restored RocksDB daemon running (#2265).
#
# Triggers: at logon, plus every 5 minutes. The repeating trigger recovers the
# daemon when it dies while the user stays signed in (e.g. a Start-menu restart
# that closes apps and then aborts leaves no new logon). While the daemon runs,
# the launcher instance stays alive and MultipleInstances=IgnoreNew skips the
# repeat, so it only acts after the daemon has exited.
[CmdletBinding()]
param(
    [string]$TaskName = 'SynapseMcpPreCalyx',
    [string]$SourceDir = (Split-Path -Parent $PSScriptRoot),
    [switch]$Remove
)
$ErrorActionPreference = 'Stop'

if ($Remove) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    return
}

$launcher = Join-Path $SourceDir 'scripts\start-pre-calyx-daemon.ps1'
if (-not (Test-Path -LiteralPath $launcher -PathType Leaf)) {
    throw "Launcher is missing: $launcher"
}
$user = "$env:USERDOMAIN\$env:USERNAME"
$action = New-ScheduledTaskAction `
    -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" `
    -Argument "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$launcher`"" `
    -WorkingDirectory $SourceDir
$logon = New-ScheduledTaskTrigger -AtLogOn -User $user
$watchdog = New-ScheduledTaskTrigger -Once -At (Get-Date) `
    -RepetitionInterval (New-TimeSpan -Minutes 5)
$settings = New-ScheduledTaskSettingsSet `
    -MultipleInstances IgnoreNew -StartWhenAvailable `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited

Register-ScheduledTask -TaskName $TaskName -Action $action `
    -Trigger @($logon, $watchdog) -Settings $settings -Principal $principal -Force |
    Out-Null
Get-ScheduledTask -TaskName $TaskName | Select-Object TaskName, State,
    @{ n = 'Triggers'; e = { ($_.Triggers | ForEach-Object { $_.CimClass.CimClassName }) -join ', ' } }
