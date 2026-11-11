# Dedicated configured-host startup for the restored RocksDB daemon (#2265).
# This is a runtime launcher, not a verification or test driver.
[CmdletBinding()]
param(
    [string]$ExePath = "$env:USERPROFILE\.cargo\bin\synapse-mcp.exe",
    [string]$DbPath = "$env:LOCALAPPDATA\synapse\db-daemon",
    [string]$ProfilesDir = "$env:USERPROFILE\.cargo\bin\profiles-pre-calyx",
    [switch]$EnableAgentSpawn
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
# Do not launch a duplicate or touch whichever program currently owns the port. A keeper that
# fires while this daemon is already serving is an idempotent no-op, not a failure: it is only a
# failure when the holder is NOT this daemon, which is decided by the owning process's image path,
# never by "something is listening".
$existingListeners = @(Get-NetTCPConnection -LocalPort 7700 -State Listen -ErrorAction SilentlyContinue)
if ($existingListeners.Count -gt 0) {
    $expectedImage = (Resolve-Path -LiteralPath $ExePath).ProviderPath
    $owners = foreach ($listener in $existingListeners) {
        $owner = Get-Process -Id $listener.OwningProcess -ErrorAction SilentlyContinue
        # An unreadable owner is a foreign holder until proven otherwise; never assume it is ours.
        [pscustomobject]@{
            ProcessId = $listener.OwningProcess
            Path      = if ($owner -and $owner.Path) { $owner.Path } else { $null }
        }
    }
    $foreign = @($owners | Where-Object { $_.Path -ne $expectedImage })
    if ($foreign.Count -gt 0) {
        # Windows PowerShell 5.1 runs this task: no null-coalescing operator here.
        $detail = ($foreign | ForEach-Object {
            $image = if ($_.Path) { $_.Path } else { 'unreadable image' }
            '{0} (pid {1})' -f $image, $_.ProcessId }) -join ', '
        throw "Port 7700 is held by a process that is not $expectedImage : $detail. Refusing duplicate startup."
    }
    Write-Output ('Pre-calyx daemon already serving 7700 as {0} (pid {1}); nothing to start.' -f
        $expectedImage, (($owners | ForEach-Object { $_.ProcessId }) -join ','))
    exit 0
}
$runtimeLogDir = Join-Path $env:LOCALAPPDATA 'synapse\logs'
New-Item -ItemType Directory -Path $runtimeLogDir -Force | Out-Null
$runtimeStamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
# Explicit operator opt-in only; inherited environment cannot silently enable it.
$env:SYNAPSE_AGENT_SPAWN_ENABLED = if ($EnableAgentSpawn) { '1' } else { '0' }
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
