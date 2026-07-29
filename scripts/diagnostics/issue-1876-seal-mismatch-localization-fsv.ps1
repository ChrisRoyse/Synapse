#requires -Version 7
<#
  FSV driver for issue #1876 — the raw-commitment seal mismatch must name the
  exact offending sequence, not just the cohort range.

  Everything here is real: a genuine Aster vault opened by the real
  SynapseCalyxVault, real WAL/MVCC commits, the real checkpoint sealer, the real
  verify_ledger_chain verifier, and physical SST byte patching that rewrites both
  CRC layers so the storage layer accepts the file as intact.
#>
$ErrorActionPreference = 'Stop'
$env:PATH = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\Llvm\x64\bin;$env:PATH"

$exe  = 'C:\code\synapse\target\debug\examples\provenance_tamper_fsv.exe'
if (-not (Test-Path $exe)) { throw "instrument not built: $exe" }
$work = Join-Path $env:TEMP 'synapse-fsv-1876'
$fail = 0

function Check {
    param([string]$Name, $Expected, $Actual)
    $ok = ($Expected -eq $Actual)
    if (-not $ok) { $script:fail++ }
    $tag = if ($ok) { 'PASS' } else { 'FAIL' }
    $col = if ($ok) { 'Green' } else { 'Red' }
    Write-Host ("  [{0}] {1}`n         expected={2}`n         actual  ={3}" -f $tag, $Name, $Expected, $Actual) -ForegroundColor $col
}
function Run { param([string[]]$Argv) return (& $exe @Argv 2>&1 | Out-String) }

function New-SeededVault {
    param([string]$Dir, [int]$Batches)
    if (Test-Path $Dir) { Remove-Item $Dir -Recurse -Force }
    # The lineage journal is a deliberate SIBLING of the vault (#1875) so it
    # survives deleting the vault. A disposable FSV vault must drop it too, or
    # the next open correctly refuses with SYNAPSE_CALYX_VAULT_RESET_UNACKNOWLEDGED.
    Remove-Item "$Dir.lineage.json" -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $out = Run @('seed', $Dir, "$Batches")
    if ($out -notmatch 'SEED_CLOSED') { throw "seed failed:`n$out" }
    return $out
}

# The physical raw-commitment row: CYXRAW01 | seq u64be | row_count u64be | batch_hash[32]
function Get-Commitments {
    param([string]$Dir)
    $out = Run @('list-commitments', $Dir)
    $rows = @()
    foreach ($line in ($out -split "`r?`n")) {
        if ($line -match '^COMMITMENT key_seq=(\d+) value_len=(\d+) magic_ok=(\w+) row_count=(\d+) batch_hash=([0-9a-f]+)') {
            $seq = [uint64]$Matches[1]; $rc = [uint64]$Matches[4]; $bh = $Matches[5]
            $seqHex = ($seq.ToString('x')).PadLeft(16,'0')
            $rcHex  = ($rc.ToString('x')).PadLeft(16,'0')
            $rows += [pscustomobject]@{
                seq = $seq; row_count = $rc; batch_hash = $bh
                value_hex = '4359584157303 1'.Replace(' ','')  # placeholder, replaced below
            }
            $rows[-1].value_hex = '4359585241573031' + $seqHex + $rcHex + $bh
        }
    }
    return $rows
}

function Invoke-Verify { param([string]$Dir) return (Run @('verify', $Dir)) }

function Get-VerifyJson {
    param([string]$Text)
    foreach ($line in ($Text -split "`r?`n")) {
        if ($line.StartsWith('VERIFY ')) { return ($line.Substring(7) | ConvertFrom-Json) }
    }
    throw "no VERIFY line in:`n$Text"
}

# Flip the low bit of the final batch-hash byte: the smallest change that leaves
# every structural field byte-identical, so only the Merkle leaf differs.
function Get-TamperedValueHex {
    param([string]$ValueHex)
    $lastByte = [Convert]::ToByte($ValueHex.Substring($ValueHex.Length - 2, 2), 16)
    $flipped  = ($lastByte -bxor 1)
    return $ValueHex.Substring(0, $ValueHex.Length - 2) + ('{0:x2}' -f $flipped)
}

function Invoke-Tamper {
    param([string]$Dir, [string[]]$ValueHexes)
    $cfDir = Join-Path $Dir 'cf\raw_commitment'
    if (-not (Test-Path $cfDir)) {
        $cfDir = (Get-ChildItem (Join-Path $Dir 'cf') -Directory | Where-Object { $_.Name -match 'commit' } | Select-Object -First 1).FullName
    }
    foreach ($v in $ValueHexes) {
        $out = Run @('sst-patch-value', $cfDir, $v, (Get-TamperedValueHex $v))
        if ($out -notmatch 'SST_PATCH_TOTAL patched_records=[1-9]') { throw "tamper failed for $v :`n$out" }
    }
    return $cfDir
}

Write-Host "=== #1876 FSV: exact offending sequence on a raw-commitment seal mismatch ===" -ForegroundColor Cyan
Write-Host "instrument: $exe"
Write-Host "workdir   : $work`n"

# ===========================================================================
Write-Host "=== CASE 0: cohort seals and verifies intact (baseline) ===" -ForegroundColor Yellow
$vault0 = Join-Path $work 'case0'
New-SeededVault -Dir $vault0 -Batches 12 | Out-Null
$rows0 = Get-Commitments -Dir $vault0
Write-Host ("  BEFORE: physical raw-commitment rows = {0}, seqs {1}" -f $rows0.Count, (($rows0.seq) -join ','))
$v0 = Get-VerifyJson (Invoke-Verify -Dir $vault0)
Write-Host ("  verdict: intact={0} raw_commitments={1} seal_count={2} sealed={3}" -f `
    $v0.intact, $v0.raw_commitment_count, $v0.raw_commitment_seal_count, $v0.raw_commitment_sealed_count)
Check 'baseline intact'          $true $v0.intact
Check 'raw commitments intact'   $true $v0.raw_commitments_intact
Check 'cohort large enough that a range is not an answer' $true ($rows0.Count -ge 12)
Check 'exactly one seal (one cohort)' 1 ([int]$v0.raw_commitment_seal_count)

# ===========================================================================
Write-Host "`n=== CASE 1: ONE tampered row in the middle of the cohort ===" -ForegroundColor Yellow
$vault1 = Join-Path $work 'case1'
New-SeededVault -Dir $vault1 -Batches 12 | Out-Null
$rows1 = Get-Commitments -Dir $vault1
$target1 = $rows1[6]
Write-Host ("  BEFORE: target seq={0} batch_hash={1}" -f $target1.seq, $target1.batch_hash)
Write-Host ("  cohort seqs: {0}" -f (($rows1.seq) -join ','))
Invoke-Tamper -Dir $vault1 -ValueHexes @($target1.value_hex) | Out-Null
$rows1After = Get-Commitments -Dir $vault1
$t1After = $rows1After | Where-Object { $_.seq -eq $target1.seq }
Write-Host ("  AFTER tamper (physical readback): seq={0} batch_hash={1}" -f $t1After.seq, $t1After.batch_hash)
Check 'the physical row really changed' $false ($t1After.batch_hash -eq $target1.batch_hash)
Check 'no other row changed' 0 (@(Compare-Object ($rows1 | Where-Object { $_.seq -ne $target1.seq } | ForEach-Object { "$($_.seq):$($_.batch_hash)" }) `
                                               ($rows1After | Where-Object { $_.seq -ne $target1.seq } | ForEach-Object { "$($_.seq):$($_.batch_hash)" })).Count)

$v1 = Get-VerifyJson (Invoke-Verify -Dir $vault1)
$reason1 = $v1.corrupt_reason
if (-not $reason1) { $reason1 = ($v1.raw_commitment_failure) }
Write-Host "  VERDICT:"
Write-Host "    intact=$($v1.intact) raw_commitments.intact=$($v1.raw_commitments_intact)"
Write-Host "    corrupt_reason: $reason1" -ForegroundColor DarkYellow
Check 'detection still fires (fail-closed)' $false $v1.raw_commitments_intact
Check "names the EXACT offending sequence $($target1.seq)" $true ($reason1 -match "offending_sequences=\[$($target1.seq)\]")
Check 'declares localization exact'  $true ($reason1 -match 'localization=exact')
Check 'still reports the cohort range' $true ($reason1 -match 'rows \d+\.\.=\d+')
Check 'names the diverging field'    $true ($reason1 -match 'merkle_root sealed=')

# ===========================================================================
Write-Host "`n=== CASE 2: TWO tampered rows ===" -ForegroundColor Yellow
$vault2 = Join-Path $work 'case2'
New-SeededVault -Dir $vault2 -Batches 12 | Out-Null
$rows2 = Get-Commitments -Dir $vault2
$t2a = $rows2[2]; $t2b = $rows2[9]
Write-Host ("  BEFORE: targets seq={0} and seq={1}" -f $t2a.seq, $t2b.seq)
Invoke-Tamper -Dir $vault2 -ValueHexes @($t2a.value_hex, $t2b.value_hex) | Out-Null
$v2 = Get-VerifyJson (Invoke-Verify -Dir $vault2)
$reason2 = $v2.corrupt_reason
Write-Host "    corrupt_reason: $reason2" -ForegroundColor DarkYellow
Check 'detection fires' $false $v2.raw_commitments_intact
Check "names BOTH sequences $($t2a.seq),$($t2b.seq)" $true ($reason2 -match "offending_sequences=\[$($t2a.seq),$($t2b.seq)\]")
Check 'localization exact' $true ($reason2 -match 'localization=exact')

# ===========================================================================
Write-Host "`n=== CASE 3 (boundary): FIRST row of the cohort ===" -ForegroundColor Yellow
$vault3 = Join-Path $work 'case3'
New-SeededVault -Dir $vault3 -Batches 12 | Out-Null
$rows3 = Get-Commitments -Dir $vault3
$t3 = $rows3[0]
Write-Host ("  BEFORE: first cohort row seq={0}" -f $t3.seq)
Invoke-Tamper -Dir $vault3 -ValueHexes @($t3.value_hex) | Out-Null
$v3 = Get-VerifyJson (Invoke-Verify -Dir $vault3)
$reason3 = $v3.corrupt_reason
Write-Host "    corrupt_reason: $reason3" -ForegroundColor DarkYellow
Check 'detection fires' $false $v3.raw_commitments_intact
Check "names the first sequence $($t3.seq)" $true ($reason3 -match "offending_sequences=\[$($t3.seq)\]")

# ===========================================================================
Write-Host "`n=== CASE 4 (boundary): LAST row of the cohort ===" -ForegroundColor Yellow
$vault4 = Join-Path $work 'case4'
New-SeededVault -Dir $vault4 -Batches 12 | Out-Null
$rows4 = Get-Commitments -Dir $vault4
$t4 = $rows4[-1]
Write-Host ("  BEFORE: last cohort row seq={0}" -f $t4.seq)
Invoke-Tamper -Dir $vault4 -ValueHexes @($t4.value_hex) | Out-Null
$v4 = Get-VerifyJson (Invoke-Verify -Dir $vault4)
$reason4 = $v4.corrupt_reason
Write-Host "    corrupt_reason: $reason4" -ForegroundColor DarkYellow
Check 'detection fires' $false $v4.raw_commitments_intact
Check "names the last sequence $($t4.seq)" $true ($reason4 -match "offending_sequences=\[$($t4.seq)\]")

# ===========================================================================
Write-Host "`n=== CASE 5 (edge): a row DROPPED from the cohort ===" -ForegroundColor Yellow
$vault5 = Join-Path $work 'case5'
New-SeededVault -Dir $vault5 -Batches 12 | Out-Null
$rows5 = Get-Commitments -Dir $vault5
$t5 = $rows5[5]
Write-Host ("  BEFORE: dropping seq={0} from the physical commitment CF" -f $t5.seq)
$cfDir5 = Join-Path $vault5 'cf\raw_commitment'
if (-not (Test-Path $cfDir5)) {
    $cfDir5 = (Get-ChildItem (Join-Path $vault5 'cf') -Directory | Where-Object { $_.Name -match 'commit' } | Select-Object -First 1).FullName
}
Run @('sst-drop-value', $cfDir5, $t5.value_hex) | Out-Null
$rows5After = Get-Commitments -Dir $vault5
Write-Host ("  AFTER : physical rows {0} -> {1}" -f $rows5.Count, $rows5After.Count)
Check 'the row really is gone' $false ([bool]($rows5After | Where-Object { $_.seq -eq $t5.seq }))
$v5 = Get-VerifyJson (Invoke-Verify -Dir $vault5)
$reason5 = $v5.corrupt_reason
Write-Host "    corrupt_reason: $reason5" -ForegroundColor DarkYellow
Check 'detection fires' $false $v5.raw_commitments_intact
Check 'names the count divergence exactly' $true ($reason5 -match 'claims 12 rows but only 11 remain')
Check 'pins WHERE the drop happened'       $true ($reason5 -match 'dropped_row_count=1 dropped_at_sealed_index=5')
Check "brackets the gap between seq $($rows5[4].seq) and seq $($rows5[6].seq)" $true `
    ($reason5 -match "between_physical_seq=$($rows5[4].seq) and_physical_seq=$($rows5[6].seq)")
Check 'gives the missing row''s sealed leaf digest' $true ($reason5 -match 'missing_sealed_leaf_digests=\[[0-9a-f]{64}\]')
Check 'declares localization exact'        $true ($reason5 -match 'localization=exact')

# ===========================================================================
Write-Host "`n=== CASE 6 (regression): a PRE-#1876 (v1) seal must still verify intact ===" -ForegroundColor Yellow
Write-Host "  Source of truth: the LIVE production vault, whose seals were all written"
Write-Host "  by pre-#1876 builds and carry the Merkle root alone."
$live = Join-Path $env:LOCALAPPDATA 'synapse\db-daemon'
Write-Host ("  live vault: {0}" -f $live)
$daemon = @(Get-Process -Name 'synapse-mcp' -ErrorAction SilentlyContinue)
Write-Host ("  daemon live: {0}" -f ($daemon.Count -gt 0))
Write-Host "  (verified read-only against a copy so the live daemon is never disturbed)"
$liveCopy = Join-Path $work 'live-copy'
if (Test-Path $liveCopy) { Remove-Item $liveCopy -Recurse -Force }
Copy-Item $live $liveCopy -Recurse -Force -ErrorAction SilentlyContinue
foreach ($stale in @('daemon.lock','vault.lock','daemon.pid','vault.pid','daemon-lifecycle.lock')) {
    Remove-Item (Join-Path $liveCopy $stale) -Force -ErrorAction SilentlyContinue
}
$vLiveText = Invoke-Verify -Dir $liveCopy
$vLive = Get-VerifyJson $vLiveText
Write-Host ("  verdict: intact={0} seal_count={1} commitment_count={2} sealed={3} pending={4}" -f `
    $vLive.intact, $vLive.raw_commitment_seal_count, $vLive.raw_commitment_count, `
    $vLive.raw_commitment_sealed_count, $vLive.raw_commitment_pending_count)
Check 'v1 seals still verify intact'          $true $vLive.raw_commitments_intact
Check 'v1 seals still verify (whole chain)'   $true $vLive.intact
Check 'this really is the v1 seal population' $true ([int]$vLive.raw_commitment_seal_count -gt 100)

# ===========================================================================
Write-Host "`n=== CASE 7 (honesty): a tampered PRE-#1876 (v1) cohort must say it cannot localize ===" -ForegroundColor Yellow
Write-Host "  A v1 seal stores only the Merkle root. RFC 6962 localizes a leaf only via an"
Write-Host "  inclusion proof, so no amount of bisection can name the row. The verifier must"
Write-Host "  say so plainly rather than guess."
$vault7 = Join-Path $work 'v1-tampered'
if (Test-Path $vault7) { Remove-Item $vault7 -Recurse -Force }
Copy-Item $liveCopy $vault7 -Recurse -Force
# The lineage journal records the vault DIRECTORY it belongs to, so a copy at a
# new path must seed its own rather than inherit one (SYNAPSE_CALYX_VAULT_LINEAGE_PATH_MISMATCH).
Remove-Item "$vault7.lineage.json" -Force -ErrorAction SilentlyContinue
$rows7 = Get-Commitments -Dir $vault7
Write-Host ("  BEFORE: v1 vault has {0} physical commitment rows" -f $rows7.Count)
$t7 = $rows7[[int]($rows7.Count / 2)]
Write-Host ("  tampering ONE row: seq={0} batch_hash={1}" -f $t7.seq, $t7.batch_hash)
Invoke-Tamper -Dir $vault7 -ValueHexes @($t7.value_hex) | Out-Null
$v7 = Get-VerifyJson (Invoke-Verify -Dir $vault7)
$reason7 = $v7.corrupt_reason
Write-Host "    corrupt_reason: $reason7" -ForegroundColor DarkYellow
Check 'detection still fires on a v1 seal' $false $v7.raw_commitments_intact
Check 'says localization is unavailable'   $true ($reason7 -match 'offending_sequences=unavailable')
Check 'explains WHY (root alone, RFC 6962)' $true ($reason7 -match 'RFC 6962')
Check 'does NOT fabricate a sequence'       $false ($reason7 -match 'localization=exact')
Check 'still names the diverging field'     $true ($reason7 -match 'merkle_root sealed=')
Check 'hands over physical leaf digests to diff against a backup' $true ($reason7 -match 'physical_leaf_digests=\[')

Write-Host ""
if ($fail -eq 0) { Write-Host "ALL CHECKS PASSED" -ForegroundColor Green; exit 0 }
else { Write-Host "$fail CHECK(S) FAILED" -ForegroundColor Red; exit 1 }
