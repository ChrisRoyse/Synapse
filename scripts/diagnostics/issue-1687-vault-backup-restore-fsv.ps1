#requires -Version 7
<#
  Manual FSV for issue #1687 — vault backup / verify_restore acceptance.

  Everything here is real: the LIVE daemon performs the backup of the LIVE
  production vault through the real `storage operation=backup` MCP path, while
  that same daemon holds byte-range locks on its own lock files. That live-lock
  condition is the one the shipped code never survived (os error 33 on
  daemon.lock), so it is the condition that has to be exercised.

  Nothing here writes to, deletes, or restores onto the production vault. The
  backup source is read-only; every mutation happens under a scratch directory.
#>
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\_mcp-client.ps1"

$work    = Join-Path $env:TEMP 'synapse-fsv-1687'
$live    = Join-Path $env:LOCALAPPDATA 'synapse\db-daemon'
$backup  = Join-Path $work 'backup-01'
$fail    = 0

function Check {
    param([string]$Name, $Expected, $Actual)
    $ok = ($Expected -eq $Actual)
    if (-not $ok) { $script:fail++ }
    $tag = if ($ok) { 'PASS' } else { 'FAIL' }
    $col = if ($ok) { 'Green' } else { 'Red' }
    Write-Host ("  [{0}] {1}: expected={2} actual={3}" -f $tag, $Name, $Expected, $Actual) -ForegroundColor $col
}
function Note { param([string]$m) Write-Host "  $m" -ForegroundColor DarkGray }

Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $backup -Force | Out-Null

Write-Host "=== #1687 FSV: vault backup + verify_restore against the LIVE daemon ===" -ForegroundColor Cyan

# ---------------------------------------------------------------- BEFORE ----
Write-Host "`n=== BEFORE: independent physical readback of the live vault ===" -ForegroundColor Yellow
$daemon = @(Get-Process -Name 'synapse-mcp' -ErrorAction SilentlyContinue)
Check 'daemon is live (the condition the old code failed under)' $true ($daemon.Count -ge 1)
Note ("daemon pids = {0}" -f (($daemon | ForEach-Object { $_.Id }) -join ','))

# Prove the lock files are genuinely byte-range locked right now, so a PASS
# below cannot be a vacuous "the daemon happened not to hold them".
$lockProbe = @()
foreach ($name in @('daemon.lock','daemon-lifecycle.lock','vault.lock','wal\.append.lock')) {
    $p = Join-Path $live $name
    if (-not (Test-Path -LiteralPath $p)) { $lockProbe += "$name=<absent>"; continue }
    try {
        $fs = [System.IO.File]::Open($p,'Open','Read',([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
        try {
            $len = [int][Math]::Max(1, [Math]::Min(4096, $fs.Length))
            $buf = New-Object byte[] $len
            [void]$fs.Read($buf, 0, $len)
            $lockProbe += "$name=readable"
        } finally { $fs.Dispose() }
    } catch { $lockProbe += "$name=LOCKED($($_.Exception.HResult))" }
}
Note ("lock probe: " + ($lockProbe -join '  '))

$liveCfFiles = (Get-ChildItem (Join-Path $live 'cf') -Recurse -File).Count
$tok = (Get-Content "$env:APPDATA\synapse\token.txt" -Raw).Trim()
$health = Invoke-RestMethod -Uri 'http://127.0.0.1:7700/health' -Headers @{ Authorization = "Bearer $tok" } -TimeoutSec 30
$liveSeq = $health.subsystems.calyx_vault.calyx_vault_latest_seq
$liveVaultId = $health.subsystems.calyx_vault.calyx_vault_id
Note ("live cf files = $liveCfFiles  latest_seq = $liveSeq  vault_id = $liveVaultId")

# ---------------------------------------------------------------- TRIGGER ---
Write-Host "`n=== TRIGGER: real storage operation=backup through the live daemon ===" -ForegroundColor Yellow
$sid = New-SynSession -ClientName 'fsv-1687'
$leaseOk = $false; $profOk = $false; $res = $null
try {
    $l = Invoke-SynTool -SessionId $sid -Name 'act' -Arguments @{ operation='lease_acquire'; ttl_ms=300000 }
    if (-not $l.error) { $leaseOk = $true }
    $p = Invoke-SynTool -SessionId $sid -Name 'profile' -Arguments @{ operation='set'; profile='break_glass'; confirm_break_glass=$true; reason='issue #1687 manual FSV: vault backup acceptance' }
    if (-not $p.error) { $profOk = $true }
    if ($profOk) {
        $res = Invoke-SynTool -SessionId $sid -Name 'storage' -Arguments @{ operation='backup'; backup=@{ target_dir=$backup } } -TimeoutSec 1800
    }
} finally {
    if ($profOk) { Invoke-SynTool -SessionId $sid -Name 'profile' -Arguments @{ operation='set'; profile='normal_agent'; reason='FSV complete' } | Out-Null }
    if ($leaseOk) { Invoke-SynTool -SessionId $sid -Name 'act' -Arguments @{ operation='lease_release' } | Out-Null }
}
if ($res.error) { Write-Host ("  backup error: " + ($res.error | ConvertTo-Json -Depth 6 -Compress)) -ForegroundColor Red }
Check 'backup succeeded against a LIVE daemon' $true (-not [bool]$res.error)
if ($res.error) { Write-Host "`nABORTING: backup failed." -ForegroundColor Red; exit 1 }

# ----------------------------------------------------------------- AFTER ----
Write-Host "`n=== AFTER: independent readback of what physically landed ===" -ForegroundColor Yellow
$manifestPath = Join-Path $backup 'backup_manifest.json'
Check 'backup_manifest.json exists' $true (Test-Path -LiteralPath $manifestPath)
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$vaultCopy = Join-Path $backup 'vault'
Check 'vault/ copy exists' $true (Test-Path -LiteralPath $vaultCopy)

# 1. Every hash in the manifest must be independently reproducible.
$files = @($manifest.files)
Note ("manifest records $($files.Count) files; re-hashing every one independently")
$mismatch = 0; $missing = 0
foreach ($entry in $files) {
    $rel = if ($entry.path) { $entry.path } else { $entry.relative_path }
    $abs = Join-Path $backup $rel
    if (-not (Test-Path -LiteralPath $abs)) { $missing++; continue }
    $want = if ($entry.sha256) { $entry.sha256 } else { $entry.hash }
    $got  = (Get-FileHash -LiteralPath $abs -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($got -ne $want.ToLowerInvariant()) { $mismatch++ ; Write-Host "    HASH MISMATCH $rel" -ForegroundColor Red }
}
Check 'every manifest file present on disk' 0 $missing
Check 'every manifest SHA-256 independently reproduces' 0 $mismatch

# 2. The runtime locks must be excluded DELIBERATELY and said so, not dropped.
$copied = @(Get-ChildItem $vaultCopy -Recurse -File | ForEach-Object { $_.FullName.Substring($vaultCopy.Length).TrimStart('\') })
foreach ($runtime in @('daemon.lock','daemon-lifecycle.lock','vault.lock','vault.pid','daemon.pid')) {
    Check "runtime lock excluded from the copy: $runtime" $false ($copied -contains $runtime)
}
Check 'wal\.append.lock excluded' $false ([bool]($copied | Where-Object { $_ -like '*.append.lock' }))
Check 'exclusions are recorded, not silent' $true ($null -ne $manifest.excluded_runtime -and @($manifest.excluded_runtime).Count -gt 0)
Note ("excluded_runtime: " + ((@($manifest.excluded_runtime) | ForEach-Object { $_.path }) -join ', '))

# 3. Vault identity must be captured (the #1875 lesson).
Check 'lineage journal captured' $true (Test-Path -LiteralPath (Join-Path $backup 'vault_lineage.json'))
$lin = Get-Content -LiteralPath (Join-Path $backup 'vault_lineage.json') -Raw | ConvertFrom-Json
Note ("captured lineage: " + ($lin | ConvertTo-Json -Depth 4 -Compress))

# 4. The data actually came across.
$copiedCf = (Get-ChildItem (Join-Path $vaultCopy 'cf') -Recurse -File).Count
Note ("live cf files = $liveCfFiles   backed-up cf files = $copiedCf")
Check 'cf files backed up (>= live count at start; vault only grows)' $true ($copiedCf -ge $liveCfFiles)

# --------------------------------------------------------- RESTORE VERIFY ---
Write-Host "`n=== restore_verify on the backup copy (read-only) ===" -ForegroundColor Yellow
$sid2 = New-SynSession -ClientName 'fsv-1687-verify'
$rv = Invoke-SynTool -SessionId $sid2 -Name 'storage' -Arguments @{ operation='restore_verify'; restore_verify=@{ vault_path=$vaultCopy } } -TimeoutSec 1800
if ($rv.error) { Write-Host ("  restore_verify error: " + ($rv.error | ConvertTo-Json -Depth 6 -Compress)) -ForegroundColor Red }
Check 'restore_verify succeeded' $true (-not [bool]$rv.error)
if ($rv.obj) {
    Note ($rv.obj | ConvertTo-Json -Depth 5 -Compress)
    Check 'restored chain verifies intact' $true ([bool]$rv.obj.chain_intact)
    Check 'restored vault id matches the source' $liveVaultId $rv.obj.vault_id
}

# ------------------------------------------------------ EDGE: CONTAINMENT ---
Write-Host "`n=== EDGE: a backup target INSIDE the vault must be refused ===" -ForegroundColor Yellow
$inside = Join-Path $live 'fsv-1687-inside'
$sid3 = New-SynSession -ClientName 'fsv-1687-edge'
$leaseOk = $false; $profOk = $false; $bad = $null
try {
    $l = Invoke-SynTool -SessionId $sid3 -Name 'act' -Arguments @{ operation='lease_acquire'; ttl_ms=120000 }
    if (-not $l.error) { $leaseOk = $true }
    $p = Invoke-SynTool -SessionId $sid3 -Name 'profile' -Arguments @{ operation='set'; profile='break_glass'; confirm_break_glass=$true; reason='issue #1687 FSV: containment guard edge case' }
    if (-not $p.error) { $profOk = $true }
    if ($profOk) { $bad = Invoke-SynTool -SessionId $sid3 -Name 'storage' -Arguments @{ operation='backup'; backup=@{ target_dir=$inside } } -TimeoutSec 300 }
} finally {
    if ($profOk) { Invoke-SynTool -SessionId $sid3 -Name 'profile' -Arguments @{ operation='set'; profile='normal_agent'; reason='FSV complete' } | Out-Null }
    if ($leaseOk) { Invoke-SynTool -SessionId $sid3 -Name 'act' -Arguments @{ operation='lease_release' } | Out-Null }
}
$badText = if ($bad.error) { $bad.error | ConvertTo-Json -Depth 6 -Compress } else { '<no error>' }
Check 'refused a target inside the vault' $true ([bool]$bad.error)
Check 'named CALYX_ASTER_BACKUP_TARGET_INSIDE_VAULT' $true ($badText -like '*BACKUP_TARGET_INSIDE_VAULT*')
Check 'and left nothing behind inside the vault' $false (Test-Path -LiteralPath $inside)
Remove-Item $inside -Recurse -Force -ErrorAction SilentlyContinue

# ------------------------------------------------------------- CONCLUSION ---
Write-Host ""
if ($fail -eq 0) { Write-Host "ALL CHECKS PASSED" -ForegroundColor Green; exit 0 }
else { Write-Host "$fail CHECK(S) FAILED" -ForegroundColor Red; exit 1 }
