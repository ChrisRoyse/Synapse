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
#    `relative_path` is relative to the vault/ copy, not the backup root.
$files = @($manifest.files)
Note ("manifest records $($files.Count) files; re-hashing every one independently")
$mismatch = 0; $missing = 0; $hashed = 0
foreach ($entry in $files) {
    $abs = Join-Path $vaultCopy $entry.relative_path
    if (-not (Test-Path -LiteralPath $abs)) {
        $missing++
        if ($missing -le 5) { Write-Host "    MISSING $($entry.relative_path)" -ForegroundColor Red }
        continue
    }
    $got = (Get-FileHash -LiteralPath $abs -Algorithm SHA256).Hash.ToLowerInvariant()
    $hashed++
    if ($got -ne "$($entry.sha256)".ToLowerInvariant()) {
        $mismatch++
        Write-Host "    HASH MISMATCH $($entry.relative_path)" -ForegroundColor Red
    }
}
Check 'every manifest file present on disk' 0 $missing
Check 'every manifest SHA-256 independently reproduces' 0 $mismatch
# Guard against the vacuous pass: a `continue` on every entry would leave zero
# mismatches while having verified nothing at all.
Check 'the hash check was not vacuous (all entries actually hashed)' $files.Count $hashed
Check 'manifest file_count agrees with its own files array' ([int]$manifest.file_count) $files.Count

# 2. The runtime locks must be excluded DELIBERATELY and said so, not dropped.
$copied     = @(Get-ChildItem $vaultCopy -Recurse -File -Force | ForEach-Object { $_.FullName.Substring($vaultCopy.Length).TrimStart('\') })
$manifested = @($files | ForEach-Object { $_.relative_path -replace '/', '\' })
$excluded   = @($manifest.excluded_runtime)
foreach ($runtime in @('daemon.lock','daemon-lifecycle.lock','vault.lock','vault.pid','daemon.pid')) {
    Check "runtime lock not copied: $runtime" $false ($copied -contains $runtime)
    Check "runtime lock not manifested: $runtime" $false ($manifested -contains $runtime)
}
# A 0-byte .append.lock can legitimately reappear in the copy, because opening
# the backup vault read-only during restore_verify recreates the lock token.
# The property that matters is that the backup never CLAIMS it as vault data.
Check 'wal/.append.lock is not manifested as data' $false ([bool]($manifested | Where-Object { $_ -like '*.append.lock' }))
Check 'exclusions are recorded, not silent' $true ($excluded.Count -gt 0)
Check 'wal/.append.lock exclusion is recorded with a reason' $true `
    ([bool]($excluded | Where-Object { $_.relative_path -like '*append.lock*' -and $_.reason }))
Note ("excluded_runtime (" + $excluded.Count + "): " + ((@($excluded) | ForEach-Object { "$($_.relative_path)" }) -join ', '))

# 2b. The pinned-manifest snapshot is the fix for the rotation race. Assert it.
$pin = $manifest.pinned_manifest
Check 'a manifest generation was pinned' $true ($null -ne $pin -and -not [string]::IsNullOrWhiteSpace($pin.pointer))
Note ("pinned pointer=$($pin.pointer) manifest_seq=$($pin.manifest_seq) durable_seq=$($pin.durable_seq) retained=$($pin.generations_retained)")
Check 'retention window matches the measured 32' 32 ([int]$pin.generations_retained)
Check 'the pinned manifest itself is in the copy' $true (Test-Path -LiteralPath (Join-Path $vaultCopy $pin.pointer))
Check 'CURRENT in the copy names the pinned generation' $pin.pointer ((Get-Content -LiteralPath (Join-Path $vaultCopy 'CURRENT') -Raw).Trim())
$refMissing = 0
foreach ($ref in @($pin.referenced_paths)) {
    if (-not (Test-Path -LiteralPath (Join-Path $vaultCopy ($ref -replace '/', '\')))) { $refMissing++; Write-Host "    REFERENCED-BUT-MISSING $ref" -ForegroundColor Red }
}
Note ("pinned manifest references $(@($pin.referenced_paths).Count) paths")
Check 'every referenced path was copied (mandatory, never tolerated)' 0 $refMissing
Note ("tolerated_absences = $(@($manifest.tolerated_absences).Count) (superseded generations reclaimed mid-copy; each must carry a justification)")
foreach ($ta in @($manifest.tolerated_absences)) {
    Check "tolerated absence carries a justification: $($ta.relative_path)" $true (-not [string]::IsNullOrWhiteSpace($ta.reason))
}

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
    $v = $rv.obj.restore_verify.verify
    Note ("success=$($v.success) chain_intact=$($v.chain_intact) constellations=$($v.constellation_count) anchors=$($v.anchor_count) ledger_entries=$($v.ledger_entry_count)")
    Note ("ledger_tip_hash=$($v.ledger_tip_hash)")
    Check 'restore_verify reports success'        $true ([bool]$v.success)
    Check 'restored chain verifies intact'        $true ([bool]$v.chain_intact)
    Check 'no failure reasons'                    0     (@($v.failure_reasons).Count)
    Check 'restored vault holds constellations'   $true ([int]$v.constellation_count -gt 0)
    Check 'restored vault holds ledger entries'   $true ([int]$v.ledger_entry_count -gt 0)
    Check 'verify ran against the backup copy'    $vaultCopy $v.vault_path
    # The manifest recorded the vault identity independently of this verify.
    Check 'backup manifest records the source vault id' $liveVaultId $manifest.vault_id
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
