<#
.SYNOPSIS
  Start an issue-diagnostic Synapse daemon from a copied dev binary.

.DESCRIPTION
  Windows locks a running executable. Running
  target\debug\synapse-mcp.exe as a long-lived daemon blocks Cargo from
  relinking that same output on the next edit/build loop.

  This helper keeps D5 fast loops unblocked:
    1. Builds the dev synapse-mcp binary unless -SkipBuild is set.
    2. Resolves the repo Cargo target directory.
    3. Copies the built exe to %LOCALAPPDATA%\synapse\codex-run.
    4. Starts only the copied exe.
    5. Verifies the copied process owns the requested listener.

  It does not stop or replace an existing daemon. If the requested bind is in
  use, it fails closed and prints the owning process rows.

.PARAMETER SourceDir
  Local Synapse checkout. Defaults to this script's repository root.

.PARAMETER Bind
  Loopback bind for the issue daemon. Defaults to the shared daemon bind.

.PARAMETER DbPath
  RocksDB path for the issue daemon.

.PARAMETER ProfileDir
  Profile directory passed to synapse-mcp.

.PARAMETER RunRoot
  Directory where copied issue-daemon binaries, logs, and readback JSON are
  stored.

.PARAMETER RunLabel
  Label included in the copied binary name and readback directory.

.PARAMETER SkipBuild
  Do not run cargo build; require the resolved source exe to already exist.

.PARAMETER ExePath
  Optional source exe to copy. If omitted, the script uses Cargo metadata and
  target\debug\synapse-mcp.exe from SourceDir.

.EXAMPLE
  pwsh -File .\scripts\run-issue-daemon.ps1 -Bind 127.0.0.1:7799 -RunLabel issue1413
#>
[CmdletBinding()]
param(
    [string]$SourceDir,
    [string]$Bind = '127.0.0.1:7700',
    [string]$DbPath = "$env:LOCALAPPDATA\synapse\db-issue-daemon",
    [string]$ProfileDir = "$env:USERPROFILE\.cargo\bin\profiles",
    [string]$RunRoot = "$env:LOCALAPPDATA\synapse\codex-run",
    [string]$RunLabel = 'issue-diagnostic',
    [string]$LogLevel = 'info',
    [ValidateRange(1, 120)][int]$StartupTimeoutSeconds = 15,
    [switch]$SkipBuild,
    [string]$ExePath
)

$ErrorActionPreference = 'Stop'

function Info($m) { Write-Host "[issue-daemon] $m" }
function Die($m) { throw "[issue-daemon] FATAL: $m" }

function Get-SynapseFileSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Die "SYNAPSE_ISSUE_DAEMON_FILE_HASH_MISSING path=$Path remediation=build or copy the daemon binary before hashing it"
    }
    try {
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try {
            $share = [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
            $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, $share)
            try {
                $hash = $sha.ComputeHash($stream)
            } finally {
                $stream.Dispose()
            }
        } finally {
            $sha.Dispose()
        }
        return (($hash | ForEach-Object { $_.ToString('X2') }) -join '').ToLowerInvariant()
    } catch {
        Die "SYNAPSE_ISSUE_DAEMON_FILE_HASH_FAILED path=$Path error=$($_.Exception.Message) remediation=verify the binary is readable and retry issue-daemon launch"
    }
}

function Resolve-ExistingPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    return [System.IO.Path]::GetFullPath((Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path)
}

function Resolve-NewPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    return [System.IO.Path]::GetFullPath($Path)
}

function Assert-LocalPath {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Path
    )
    if ($Path.StartsWith('\\')) {
        Die "$Name path must be on a local drive, not UNC: $Path"
    }
}

function Get-BindParts {
    param([Parameter(Mandatory = $true)][string]$BindValue)
    $idx = $BindValue.LastIndexOf(':')
    if ($idx -lt 1 -or $idx -eq ($BindValue.Length - 1)) {
        Die "SYNAPSE_ISSUE_DAEMON_BIND_INVALID bind=$BindValue remediation=use host:port, for example 127.0.0.1:7799"
    }
    $bindHost = $BindValue.Substring(0, $idx)
    $portText = $BindValue.Substring($idx + 1)
    $port = 0
    if (-not [int]::TryParse($portText, [ref]$port) -or $port -lt 1 -or $port -gt 65535) {
        Die "SYNAPSE_ISSUE_DAEMON_BIND_PORT_INVALID bind=$BindValue remediation=use a TCP port from 1 to 65535"
    }
    [pscustomobject]@{ BindHost = $bindHost; Port = $port }
}

function Get-ListenerRows {
    param(
        [Parameter(Mandatory = $true)][string]$BindHost,
        [Parameter(Mandatory = $true)][int]$Port
    )
    @(Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue |
        Where-Object {
            $_.LocalAddress -eq $BindHost -or
            $_.LocalAddress -eq '0.0.0.0' -or
            $_.LocalAddress -eq '::' -or
            ($BindHost -eq '127.0.0.1' -and $_.LocalAddress -eq '::1')
        })
}

function Format-ProcessRows {
    param([object[]]$Rows)
    $details = @()
    foreach ($row in $Rows) {
        $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$($row.OwningProcess)" -ErrorAction SilentlyContinue
        $details += [pscustomobject]@{
            pid = $row.OwningProcess
            local_address = $row.LocalAddress
            local_port = $row.LocalPort
            executable = $proc.ExecutablePath
            command_line = $proc.CommandLine
        }
    }
    return ($details | ConvertTo-Json -Depth 4 -Compress)
}

function Stop-OwnedProcess {
    param([Parameter(Mandatory = $true)][int]$Pid)
    $current = Get-CimInstance Win32_Process -Filter "ProcessId=$Pid" -ErrorAction SilentlyContinue
    if (-not $current) { return }
    $leaf = Split-Path -Leaf $current.ExecutablePath
    if ($leaf -ine 'synapse-mcp.exe' -and $leaf -notmatch '^synapse-mcp-.+\.exe$') {
        Die "SYNAPSE_ISSUE_DAEMON_STOP_REFUSED pid=$Pid executable=$($current.ExecutablePath) remediation=PID no longer belongs to the issue daemon copy"
    }
    Stop-Process -Id $Pid -Force -ErrorAction Stop
}

if ([string]::IsNullOrWhiteSpace($SourceDir)) {
    $SourceDir = Split-Path -Parent $PSScriptRoot
}

$sourceRoot = Resolve-ExistingPath $SourceDir
Assert-LocalPath -Name 'SourceDir' -Path $sourceRoot
if (-not (Test-Path -LiteralPath (Join-Path $sourceRoot 'Cargo.toml') -PathType Leaf)) {
    Die "SYNAPSE_ISSUE_DAEMON_SOURCE_INVALID path=$sourceRoot remediation=SourceDir must be the Synapse repo root with Cargo.toml"
}

$bindParts = Get-BindParts -BindValue $Bind
$existingListeners = Get-ListenerRows -BindHost $bindParts.BindHost -Port $bindParts.Port
if ($existingListeners.Count -gt 0) {
    Die "SYNAPSE_ISSUE_DAEMON_BIND_IN_USE bind=$Bind owners=$(Format-ProcessRows $existingListeners) remediation=choose an unused bind or stop only the exact daemon process you own"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Die "SYNAPSE_ISSUE_DAEMON_CARGO_MISSING remediation=install Rust/Cargo before launching a repo-built issue daemon"
}

if (-not $SkipBuild) {
    Info "Building dev synapse-mcp binary with cargo build -p synapse-mcp --bin synapse-mcp"
    Push-Location $sourceRoot
    try {
        & cargo build -p synapse-mcp --bin synapse-mcp
        if ($LASTEXITCODE -ne 0) {
            Die "SYNAPSE_ISSUE_DAEMON_BUILD_FAILED exit_code=$LASTEXITCODE remediation=fix the compile error before launching the issue daemon"
        }
    } finally {
        Pop-Location
    }
}

Push-Location $sourceRoot
try {
    $metadataJson = & cargo metadata --format-version 1 --no-deps
    if ($LASTEXITCODE -ne 0) {
        Die "SYNAPSE_ISSUE_DAEMON_METADATA_FAILED exit_code=$LASTEXITCODE remediation=fix cargo metadata before launching the issue daemon"
    }
} finally {
    Pop-Location
}
$metadata = $metadataJson | ConvertFrom-Json
$targetDir = Resolve-NewPath $metadata.target_directory
$debugDir = Resolve-NewPath (Join-Path $targetDir 'debug')

if ($ExePath) {
    $sourceExe = Resolve-ExistingPath $ExePath
} else {
    $sourceExe = Resolve-ExistingPath (Join-Path $debugDir 'synapse-mcp.exe')
}
if ((Split-Path -Leaf $sourceExe) -ine 'synapse-mcp.exe') {
    Die "SYNAPSE_ISSUE_DAEMON_EXE_INVALID path=$sourceExe remediation=source exe must be synapse-mcp.exe"
}
Assert-LocalPath -Name 'SourceExe' -Path $sourceExe

$runRootFull = Resolve-NewPath $RunRoot
Assert-LocalPath -Name 'RunRoot' -Path $runRootFull
$safeLabel = ($RunLabel -replace '[^A-Za-z0-9_.-]', '_').Trim('._-')
if ([string]::IsNullOrWhiteSpace($safeLabel)) { $safeLabel = 'issue-diagnostic' }
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$runDir = Join-Path $runRootFull $safeLabel
$runExe = Join-Path $runDir "synapse-mcp-$safeLabel-$stamp.exe"

$debugPrefix = $debugDir.TrimEnd('\') + '\'
$runExeFull = Resolve-NewPath $runExe
if ($runExeFull.StartsWith($debugPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    Die "SYNAPSE_ISSUE_DAEMON_TARGET_DEBUG_DESTINATION path=$runExeFull remediation=issue daemon copies must live outside Cargo target directories"
}
if ($runExeFull -ieq $sourceExe) {
    Die "SYNAPSE_ISSUE_DAEMON_IN_PLACE_REFUSED path=$sourceExe remediation=never launch target\\debug\\synapse-mcp.exe as a long-lived daemon; launch the copied issue binary"
}

New-Item -ItemType Directory -Path $runDir -Force | Out-Null
Copy-Item -LiteralPath $sourceExe -Destination $runExeFull -Force
$sourceHash = Get-SynapseFileSha256 -Path $sourceExe
$runHash = Get-SynapseFileSha256 -Path $runExeFull
if ($sourceHash -ne $runHash) {
    Die "SYNAPSE_ISSUE_DAEMON_COPY_HASH_MISMATCH source=$sourceHash copy=$runHash remediation=delete the copied exe and retry"
}

$dbFull = Resolve-NewPath $DbPath
$profileFull = Resolve-ExistingPath $ProfileDir
New-Item -ItemType Directory -Path $dbFull -Force | Out-Null

$daemonArgs = @('--mode', 'http', '--bind', $Bind, '--db', $dbFull, '--profile-dir', $profileFull, '--log-level', $LogLevel)
Info "Starting copied daemon: $runExeFull $($daemonArgs -join ' ')"
$process = Start-Process -FilePath $runExeFull -ArgumentList $daemonArgs -PassThru -WindowStyle Hidden

$listener = $null
$deadline = (Get-Date).AddSeconds($StartupTimeoutSeconds)
while ((Get-Date) -lt $deadline) {
    $current = Get-Process -Id $process.Id -ErrorAction SilentlyContinue
    if (-not $current) {
        Die "SYNAPSE_ISSUE_DAEMON_EXITED_EARLY pid=$($process.Id) remediation=inspect the daemon db/event logs and fix daemon startup"
    }
    $rows = @(Get-ListenerRows -BindHost $bindParts.BindHost -Port $bindParts.Port | Where-Object { $_.OwningProcess -eq $process.Id })
    if ($rows.Count -gt 0) {
        $listener = $rows[0]
        break
    }
    Start-Sleep -Milliseconds 250
}

if (-not $listener) {
    Stop-OwnedProcess -Pid $process.Id
    Die "SYNAPSE_ISSUE_DAEMON_LISTENER_TIMEOUT pid=$($process.Id) bind=$Bind remediation=daemon did not bind before timeout and the owned copy was stopped; inspect the daemon db/event logs"
}

$procRow = Get-CimInstance Win32_Process -Filter "ProcessId=$($process.Id)" -ErrorAction Stop
if ($procRow.ExecutablePath -ine $runExeFull) {
    Stop-OwnedProcess -Pid $process.Id
    Die "SYNAPSE_ISSUE_DAEMON_PROCESS_PATH_MISMATCH pid=$($process.Id) expected=$runExeFull actual=$($procRow.ExecutablePath) remediation=PID reuse or launch mismatch; owned copy was stopped"
}

$readback = [ordered]@{
    ok = $true
    source_of_truth = 'Win32_Process.ExecutablePath + Get-NetTCPConnection listener + copied exe SHA-256'
    pid = $process.Id
    bind = $Bind
    listener_local_address = $listener.LocalAddress
    listener_local_port = $listener.LocalPort
    source_exe = $sourceExe
    source_sha256 = $sourceHash
    run_exe = $runExeFull
    run_sha256 = $runHash
    run_exe_outside_target_debug = (-not $runExeFull.StartsWith($debugPrefix, [System.StringComparison]::OrdinalIgnoreCase))
    db_path = $dbFull
    profile_dir = $profileFull
    log_note = 'daemon stdout/stderr are not redirected by this helper so caller stream captures do not hang on long-lived child handles; use db/event logs for runtime diagnostics'
    readback_path = (Join-Path $runDir "synapse-mcp-$safeLabel-$stamp.readback.json")
    command_line = $procRow.CommandLine
    remediation = 'Stop this exact PID when the issue-diagnostic daemon is no longer needed; do not kill terminal or IDE processes.'
}

$readbackJson = $readback | ConvertTo-Json -Depth 8
$readbackJson | Set-Content -LiteralPath $readback.readback_path -Encoding UTF8
[Console]::Out.WriteLine($readbackJson)
[Console]::Out.Flush()
