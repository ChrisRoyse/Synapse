# Dedicated configured-host startup for the restored RocksDB daemon (#2265).
# This is a runtime launcher, not a verification or test driver.
[CmdletBinding()]
param(
    [string]$ExePath = "$env:USERPROFILE\.cargo\bin\synapse-mcp.exe",
    [string]$DbPath = "$env:LOCALAPPDATA\synapse\db-daemon",
    [string]$ProfilesDir = "$env:USERPROFILE\.cargo\bin\profiles-pre-calyx"
)
$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Restored Synapse executable is missing: $ExePath"
}
if (Test-Path -LiteralPath (Join-Path $DbPath '.anneal')) {
    throw "Refusing to open a Calyx database with RocksDB: $DbPath"
}
if (-not (Test-Path -LiteralPath $ProfilesDir -PathType Container)) {
    throw "Restored Synapse profiles are missing: $ProfilesDir"
}
# Do not launch a duplicate or touch whichever program currently owns the port.
if (Get-NetTCPConnection -LocalPort 7700 -State Listen -ErrorAction SilentlyContinue) {
    throw 'Port 7700 already has a listener; refusing duplicate startup.'
}
$runtimeLogDir = Join-Path $env:LOCALAPPDATA 'synapse\logs'
New-Item -ItemType Directory -Path $runtimeLogDir -Force | Out-Null
$runtimeStamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$daemonArguments = @(
    '--mode', 'http', '--bind', '127.0.0.1:7700',
    '--db', ('"{0}"' -f $DbPath),
    '--profile-dir', ('"{0}"' -f $ProfilesDir),
    '--log-level', 'info',
    '--allowed-permissions', 'READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE'
)
$runtimeProcess = Start-Process -FilePath $ExePath -ArgumentList $daemonArguments `
    -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput (Join-Path $runtimeLogDir "pre-calyx-$runtimeStamp.stdout.log") `
    -RedirectStandardError (Join-Path $runtimeLogDir "pre-calyx-$runtimeStamp.stderr.log")
$runtimeProcess.WaitForExit()
exit $runtimeProcess.ExitCode
