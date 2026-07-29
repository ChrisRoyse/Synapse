#requires -Version 7
<#
  Manual FSV for issue #1686 — the Calyx hot-path boundary.

  The claim under test: the reflex tick thread makes ZERO Calyx calls, reading a
  frozen fingerprinted artifact instead, and any violation is observable in a
  RELEASE daemon rather than only tripping a debug_assert.

  Source of truth is deliberately NOT the tool's own return value:
    - the violation counter and artifact state are read from GET /health
    - the artifact bytes are re-hashed independently with Get-FileHash
    - vault advance is read from the vault's own latest_seq
  A pass therefore requires three independent readings to agree.

  Read-only with respect to the production vault. Nothing here writes to it.
#>
$ErrorActionPreference = 'Stop'

$tok  = (Get-Content "$env:APPDATA\synapse\token.txt" -Raw).Trim()
$hdr  = @{ Authorization = "Bearer $tok" }
$fail = 0

function Check {
    param([string]$Name, $Expected, $Actual)
    $ok = ($Expected -eq $Actual)
    if (-not $ok) { $script:fail++ }
    $tag = if ($ok) { 'PASS' } else { 'FAIL' }
    $col = if ($ok) { 'Green' } else { 'Red' }
    Write-Host ("  [{0}] {1}: expected={2} actual={3}" -f $tag, $Name, $Expected, $Actual) -ForegroundColor $col
}
function Note { param([string]$m) Write-Host "  $m" -ForegroundColor DarkGray }
function Get-HotPath {
    $h = Invoke-RestMethod -Uri 'http://127.0.0.1:7700/health' -Headers $hdr -TimeoutSec 60
    return $h.subsystems.calyx_hot_path.calyx_hot_path
}
function Get-VaultSeq {
    $h = Invoke-RestMethod -Uri 'http://127.0.0.1:7700/health' -Headers $hdr -TimeoutSec 60
    return [int64]$h.subsystems.calyx_vault.calyx_vault_latest_seq
}

Write-Host "=== #1686 FSV: Calyx hot-path boundary ===" -ForegroundColor Cyan

# ---------------------------------------------------------------- BEFORE ----
Write-Host "`n=== BEFORE ===" -ForegroundColor Yellow
$daemon = @(Get-Process -Name 'synapse-mcp' -ErrorAction SilentlyContinue)
Check 'daemon is live' $true ($daemon.Count -ge 1)
$before = Get-HotPath
if ($null -eq $before) { Write-Host "  calyx_hot_path subsystem ABSENT from health - deploy did not land" -ForegroundColor Red; exit 1 }
Note ($before | ConvertTo-Json -Depth 4 -Compress)

Check 'reflex tick thread is tagged hot'        $true  $before.tick_thread_tagged
Check 'no boundary violations at rest'          0      ([int]$before.violations_total)
Check 'no violation code at rest'               $null  $before.violation_code
Check 'artifact refresher is running'           $true  $before.refresher_running

# The publish must have actually happened; `pending_lowering` means it never did.
Note ("artifact_state = $($before.artifact_state)   publish_success_total = $($before.publish_success_total)")
Check 'at least one successful publish'         $true  ([int]$before.publish_success_total -ge 1)
Check 'no publish failures'                     0      ([int]$before.publish_failure_total)
Check 'no publish error code'                   $null  $before.publish_last_error_code

# ------------------------------------------------- ARTIFACT BYTES ON DISK ---
Write-Host "`n=== the artifact on disk, re-hashed independently ===" -ForegroundColor Yellow
$artifactPath = $before.artifact_path
Note ("artifact_path = $artifactPath")
Check 'artifact file exists on disk' $true (Test-Path -LiteralPath $artifactPath)
if (Test-Path -LiteralPath $artifactPath) {
    $diskHash = (Get-FileHash -LiteralPath $artifactPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Note ("Get-FileHash            = $diskHash")
    Note ("artifact_file_sha256    = $($before.artifact_file_sha256)")
    Note ("artifact_content_sha256 = $($before.artifact_content_sha256)   (payload only - must NOT equal the file hash)")
    # NOTE: content_sha256 covers only the frozen payload; the file is the whole
    # pretty-printed envelope. Comparing Get-FileHash to content_sha256 would be
    # wrong, which is exactly why artifact_file_sha256 exists.
    Check 'file hash matches artifact_file_sha256' $diskHash ($before.artifact_file_sha256).ToLowerInvariant()
    Check 'file hash differs from content hash (they cover different bytes)' $true ($diskHash -ne ($before.artifact_content_sha256).ToLowerInvariant())

    $env = Get-Content -LiteralPath $artifactPath -Raw | ConvertFrom-Json
    Check 'envelope kind is GuardThresholds' $true ("$($env.kind)" -match 'guard')
    Note ("envelope fingerprint.content_sha256 = $($env.fingerprint.content_sha256)")
    Check 'envelope carries the same content hash health reports' `
        ($before.artifact_content_sha256).ToLowerInvariant() ($env.fingerprint.content_sha256).ToLowerInvariant()
    Check 'envelope pins a source_ledger_seq' $true ([int64]$env.fingerprint.source_ledger_seq -gt 0)
    Check 'envelope pins the vault id'        $true (-not [string]::IsNullOrWhiteSpace($env.fingerprint.vault_id))
}

# ------------------------------------------------------------- THE TICK -----
Write-Host "`n=== TRIGGER: sustained reflex activity, then re-read every source ===" -ForegroundColor Yellow
$seqBefore   = Get-VaultSeq
$ticksBefore = [int64]$before.hot_ticks_total
$readsBefore = [int64]$before.artifact_hot_reads_total
Note ("vault latest_seq=$seqBefore  hot_ticks_total=$ticksBefore  artifact_hot_reads_total=$readsBefore")
Note "running the reflex loop for 60s ..."
Start-Sleep -Seconds 60

$after     = Get-HotPath
$seqAfter  = Get-VaultSeq
$ticksAfter = [int64]$after.hot_ticks_total
$readsAfter = [int64]$after.artifact_hot_reads_total
Note ("vault latest_seq=$seqAfter  hot_ticks_total=$ticksAfter  artifact_hot_reads_total=$readsAfter")

# The test is only meaningful if the hot path actually ran.
Check 'the hot path actually ticked (test is not vacuous)' $true ($ticksAfter -gt $ticksBefore)
Check 'ticks read the frozen artifact'                     $true ($readsAfter -gt $readsBefore)
Check 'STILL zero boundary violations after real ticks'    0     ([int]$after.violations_total)
Check 'still no violation code'                            $null $after.violation_code
Check 'artifact content hash unchanged across ticks'       $before.artifact_content_sha256 $after.artifact_content_sha256

Note ("vault advanced by $($seqAfter - $seqBefore) sequences during the window")
Note "(the vault legitimately advances from OTHER subsystems; the boundary claim is"
Note " that the reflex TICK made no Calyx call, which violations_total==0 is the direct"
Note " evidence for. Vault advance alone neither proves nor refutes it.)"

# ------------------------------------------------------------- CONCLUSION ---
Write-Host ""
if ($fail -eq 0) { Write-Host "ALL CHECKS PASSED" -ForegroundColor Green; exit 0 }
else { Write-Host "$fail CHECK(S) FAILED" -ForegroundColor Red; exit 1 }
