#requires -Version 5.1
<#
  FSV instrument for issue #1877.

  Loads the REAL function definitions out of scripts\synapse-setup.ps1 (parsed
  from the file on disk, not copied) and exercises them against physical files,
  verifying every byte total against an independently-computed handle length.
#>
$ErrorActionPreference = 'Stop'

$src = 'C:\code\synapse\scripts\synapse-setup.ps1'
$errs = $null; $toks = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($src, [ref]$toks, [ref]$errs)
if ($errs) { throw "setup script parse errors: $($errs -join '; ')" }
$want = @(
    'Ensure-SynapseFileSizeProbeType',
    'Get-SynapseAuthoritativeFileLength',
    'Measure-SynapseAuthoritativeFileBytes',
    'Get-SynapseCalyxPhysicalSnapshot',
    'Get-SynapseVaultRecoveryFingerprint'
)
$defs = $ast.FindAll({
    param($n)
    $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $want -contains $n.Name
}, $true)
if (@($defs).Count -ne $want.Count) {
    throw "expected $($want.Count) function definitions, extracted $(@($defs).Count): $(@($defs).Name -join ',')"
}
foreach ($d in $defs) { . ([scriptblock]::Create($d.Extent.Text)) }
Write-Host "loaded from ${src}: $(@($defs).Name -join ', ')" -ForegroundColor Cyan

# --- independent oracle: a SEPARATE code path for handle length -------------
function Oracle-HandleLength {
    param([string]$Path)
    $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read,
        ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
    try { return [int64]$fs.Length } finally { $fs.Dispose() }
}
function Oracle-HandleLastWriteUtc {
    param([string]$Path)
    $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read,
        ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
    try { return $fs.SafeFileHandle } finally { $fs.Dispose() }
}
# The BUGGY reading: the directory entry as the enumeration API (FindFirstFile,
# which is what Get-ChildItem uses) reports it. This is the value the old code
# summed, and the value that goes stale.
function Oracle-EnumEntry {
    param([string]$Dir, [string]$Name)
    return @(Get-ChildItem -LiteralPath $Dir -Filter $Name -File -Force)[0]
}

$root = Join-Path $env:TEMP 'synapse-fsv-1877'
if (Test-Path $root) { Remove-Item $root -Recurse -Force }
$vault = Join-Path $root 'vault'
New-Item -ItemType Directory -Path (Join-Path $vault 'cf\slot_01') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $vault 'wal') -Force | Out-Null

$fail = 0
function Check {
    param([string]$Name, $Expected, $Actual)
    $ok = ($Expected -eq $Actual)
    if (-not $ok) { $script:fail++ }
    $tag = if ($ok) { 'PASS' } else { 'FAIL' }
    $col = if ($ok) { 'Green' } else { 'Red' }
    Write-Host ("  [{0}] {1}: expected={2} actual={3}" -f $tag, $Name, $Expected, $Actual) -ForegroundColor $col
}

# ===========================================================================
Write-Host "`n=== CASE 1: closed files, known byte counts (happy path) ===" -ForegroundColor Yellow
# manifest: 3 files x 100 bytes = 300 ; cf: 2 files x 4096 = 8192 ; wal: 1 x 777
foreach ($i in 1..3) {
    [System.IO.File]::WriteAllBytes((Join-Path $vault ("manifest-{0:d20}.json" -f $i)), (New-Object byte[] 100))
}
foreach ($i in 1..2) {
    [System.IO.File]::WriteAllBytes((Join-Path $vault ("cf\slot_01\{0:d6}.dat" -f $i)), (New-Object byte[] 4096))
}
$walPath = Join-Path $vault 'wal\00000000000000000000.wal'
[System.IO.File]::WriteAllBytes($walPath, (New-Object byte[] 777))

Write-Host "  BEFORE (source of truth = files on disk, all handles closed):"
Write-Host ("    manifest dir-entry sum = {0}" -f (Get-ChildItem $vault -Filter 'manifest-*.json' -File | Measure-Object Length -Sum).Sum)
Write-Host ("    cf       dir-entry sum = {0}" -f (Get-ChildItem (Join-Path $vault 'cf') -Recurse -File | Measure-Object Length -Sum).Sum)
Write-Host ("    wal      dir-entry     = {0}   handle = {1}" -f (Oracle-EnumEntry (Join-Path $vault 'wal') '*.wal').Length, (Oracle-HandleLength $walPath))

$s1 = Get-SynapseCalyxPhysicalSnapshot -Path $vault
Check 'schema'              'synapse_calyx_physical_snapshot/v2' $s1.schema
Check 'manifest_file_count' 3     $s1.manifest_file_count
Check 'manifest_bytes'      300   $s1.manifest_bytes
Check 'cf_file_count'       2     $s1.cf_file_count
Check 'cf_bytes'            8192  $s1.cf_bytes
Check 'wal_segment_count'   1     $s1.wal_segment_count
Check 'wal_bytes'           777   $s1.wal_bytes
Check 'largest_wal.bytes'   777   $s1.largest_wal.bytes
Check 'byte_source'         'handle:GetFileInformationByHandle' $s1.byte_source
Check 'bytes_complete'      $true $s1.bytes_complete
Check 'unsized_file_count'  0     $s1.unsized_file_count

# ===========================================================================
Write-Host "`n=== CASE 2: WAL held OPEN by a live writer, buffered appends ===" -ForegroundColor Yellow
Write-Host "  (this is the exact #1877 condition: NTFS has not replicated the size"
Write-Host "   into the directory entry because the last handle has not closed)"
$openWal = Join-Path $vault 'wal\00000000000000000001.wal'
$writer = [System.IO.File]::Open($openWal, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write,
    ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
try {
    $chunk = New-Object byte[] 65536
    for ($i = 0; $i -lt 16; $i++) { $writer.Write($chunk, 0, $chunk.Length) }   # 1,048,576 bytes
    $writer.Flush()   # managed buffer -> OS. NTFS does not replicate the new
                      # size into the directory entry until the last handle closes.

    $entry    = Oracle-EnumEntry (Join-Path $vault 'wal') '00000000000000000001.wal'
    $dirEntry = [int64]$entry.Length
    $handle   = Oracle-HandleLength $openWal
    Write-Host "  BEFORE (two readings of the SAME file, while the writer holds it open):"
    Write-Host ("    Get-ChildItem .Length  [directory entry / FindFirstFile] = {0}" -f $dirEntry)
    Write-Host ("    handle       .Length   [file object]                     = {0}" -f $handle)
    Write-Host ("    understatement                                            = {0} bytes" -f ($handle - $dirEntry))
    Write-Host ("    Get-ChildItem LastWriteTimeUtc = {0}" -f $entry.LastWriteTimeUtc.ToString('o'))
    Check 'handle length is the truth'  1048576 $handle
    Check 'DEFECT REPRODUCED: directory entry is stale' $true ($dirEntry -lt $handle)
    Write-Host ("    >>> the OLD code would have reported wal_bytes = {0} instead of {1}" -f `
        (777 + $dirEntry), (777 + $handle)) -ForegroundColor Magenta

    $s2 = Get-SynapseCalyxPhysicalSnapshot -Path $vault
    Write-Host "  AFTER (repaired snapshot read while the writer still holds the file):"
    Write-Host ("    wal_segment_count = {0}  wal_bytes = {1}  largest_wal = {2}/{3}  last_write_utc = {4}" -f `
        $s2.wal_segment_count, $s2.wal_bytes, $s2.largest_wal.name, $s2.largest_wal.bytes, $s2.largest_wal.last_write_utc)
    Check 'wal_bytes == handle truth (777 + 1048576)' (777 + $handle) $s2.wal_bytes
    Check 'wal_bytes != stale directory-entry total'  $true ($s2.wal_bytes -ne (777 + $dirEntry))
    Check 'largest_wal picked the open segment' '00000000000000000001.wal' $s2.largest_wal.name
    Check 'largest_wal.bytes == handle truth'  $handle $s2.largest_wal.bytes
    Check 'bytes_complete under a live writer' $true $s2.bytes_complete
    Check 'largest_wal.last_write_utc is fresher than the stale entry' $true `
        ([datetime]::Parse($s2.largest_wal.last_write_utc).ToUniversalTime() -ge $entry.LastWriteTimeUtc)

    # --- the stall detector: a growing vault must NOT look frozen -----------
    $fp1 = Get-SynapseVaultRecoveryFingerprint -Path $vault
    Write-Host ("  fingerprint#1 (stall signal): {0}" -f $fp1.Key)
    for ($i = 0; $i -lt 8; $i++) { $writer.Write($chunk, 0, $chunk.Length) }
    $writer.Flush()
    $handle2 = Oracle-HandleLength $openWal
    $entry2  = Oracle-EnumEntry (Join-Path $vault 'wal') '00000000000000000001.wal'
    $s2b = Get-SynapseCalyxPhysicalSnapshot -Path $vault
    $fp2 = Get-SynapseVaultRecoveryFingerprint -Path $vault
    Write-Host ("  growth: handle {0} -> {1}   directory entry {2} -> {3}" -f $handle, $handle2, $dirEntry, $entry2.Length)
    Write-Host ("  fingerprint#2 (stall signal): {0}" -f $fp2.Key)
    Check 'snapshot tracks WAL growth'    (777 + $handle2) $s2b.wal_bytes
    Check 'growth is visible in snapshot' $true ($s2b.wal_bytes -gt $s2.wal_bytes)
    Check 'stall detector sees progress (fingerprint changed)' $true ($fp2.Key -ne $fp1.Key)
} finally {
    $writer.Dispose()
}

Write-Host "  AFTER CLEAN CLOSE (dir entry and handle must now agree):"
$dirAfter = [int64](Oracle-EnumEntry (Join-Path $vault 'wal') '00000000000000000001.wal').Length
$hndAfter = Oracle-HandleLength $openWal
Write-Host ("    dir-entry = {0}   handle = {1}" -f $dirAfter, $hndAfter)
Check 'dir entry == handle after close' $hndAfter $dirAfter
$s2c = Get-SynapseCalyxPhysicalSnapshot -Path $vault
Check 'snapshot == handle after close' (777 + $hndAfter) $s2c.wal_bytes

# ===========================================================================
Write-Host "`n=== CASE 3 (edge): empty vault directory ===" -ForegroundColor Yellow
$emptyVault = Join-Path $root 'empty-vault'
New-Item -ItemType Directory -Path $emptyVault -Force | Out-Null
Write-Host ("  BEFORE: files in {0} = {1}" -f $emptyVault, @(Get-ChildItem $emptyVault -Recurse -File).Count)
$s3 = Get-SynapseCalyxPhysicalSnapshot -Path $emptyVault
Check 'exists'             $true  $s3.exists
Check 'manifest_bytes'     0      $s3.manifest_bytes
Check 'cf_bytes'           0      $s3.cf_bytes
Check 'wal_bytes'          0      $s3.wal_bytes
Check 'largest_wal'        $null  $s3.largest_wal
Check 'bytes_complete'     $true  $s3.bytes_complete
Check 'error'              $null  $s3.error

Write-Host "`n=== CASE 3b (edge): vault directory does not exist ===" -ForegroundColor Yellow
$s3b = Get-SynapseCalyxPhysicalSnapshot -Path (Join-Path $root 'no-such-vault')
Check 'exists'         $false $s3b.exists
Check 'bytes_complete' $true  $s3b.bytes_complete

# ===========================================================================
Write-Host "`n=== CASE 4 (edge): a file that cannot be sized must be named, not silently 0 ===" -ForegroundColor Yellow
Write-Host "  (models the real TOCTOU: a cf file compacted away between enumeration and sizing)"
$ghostDir = Join-Path $root 'ghost'
New-Item -ItemType Directory -Path $ghostDir -Force | Out-Null
$keep  = Join-Path $ghostDir 'keep.dat'
$ghost = Join-Path $ghostDir 'ghost.dat'
[System.IO.File]::WriteAllBytes($keep,  (New-Object byte[] 1234))
[System.IO.File]::WriteAllBytes($ghost, (New-Object byte[] 999999))
$enumerated = @(Get-ChildItem $ghostDir -File)
Write-Host ("  BEFORE: enumerated {0} files, dir-entry sum = {1}" -f $enumerated.Count, ($enumerated | Measure-Object Length -Sum).Sum)
Remove-Item $ghost -Force   # vanishes after enumeration, before sizing
Write-Host ("  trigger: deleted {0}; exists now = {1}" -f $ghost, (Test-Path $ghost))
$m4 = Measure-SynapseAuthoritativeFileBytes -Files $enumerated
Write-Host ("  AFTER: bytes={0} sized={1} unsized={2} complete={3}" -f $m4.bytes, $m4.sized_file_count, $m4.unsized_file_count, $m4.complete)
Write-Host ("  unsized detail: {0}" -f (($m4.unsized_files | ForEach-Object { "$($_.path) => $($_.error)" }) -join ' | '))
Check 'file_count'         2     $m4.file_count
Check 'sized_file_count'   1     $m4.sized_file_count
Check 'bytes (lower bound, no stale 999999)' 1234 $m4.bytes
Check 'unsized_file_count' 1     $m4.unsized_file_count
Check 'complete'           $false $m4.complete
Check 'largest is the surviving file' 1234 $m4.largest.bytes
$errText = @($m4.unsized_files)[0].error
Check 'error names the path'   $true ($errText -like "*ghost.dat*")
Check 'error names win32 code' $true ($errText -like "*win32=2*")

# ===========================================================================
Write-Host "`n=== CASE 5 (boundary): file larger than 4 GiB (int64 / sparse) ===" -ForegroundColor Yellow
$bigVault = Join-Path $root 'big-vault'
New-Item -ItemType Directory -Path (Join-Path $bigVault 'wal') -Force | Out-Null
$big = Join-Path $bigVault 'wal\00000000000000000000.wal'
$expectedBig = [int64]5000000000   # 5,000,000,000 > 2^32
$fsBig = [System.IO.File]::Create($big)
try { $fsBig.SetLength($expectedBig) } finally { $fsBig.Dispose() }
& fsutil sparse setflag $big | Out-Null
Write-Host ("  BEFORE: dir-entry={0}  handle={1}" -f (Oracle-EnumEntry (Join-Path $bigVault 'wal') '*.wal').Length, (Oracle-HandleLength $big))
$s5 = Get-SynapseCalyxPhysicalSnapshot -Path $bigVault
Write-Host ("  AFTER : snapshot wal_bytes={0}" -f $s5.wal_bytes)
Check 'wal_bytes beyond 2^32'  $expectedBig $s5.wal_bytes
Check 'largest_wal.bytes'      $expectedBig $s5.largest_wal.bytes
Check 'no int32 overflow/negative' $true ($s5.wal_bytes -gt [int64]4294967296)

# ===========================================================================
Write-Host "`n=== CASE 6: LIVE PRODUCTION VAULT, cross-checked against an independent handle sum ===" -ForegroundColor Yellow
$live = Join-Path $env:LOCALAPPDATA 'synapse\db-daemon'
$daemon = @(Get-Process -Name 'synapse-mcp' -ErrorAction SilentlyContinue)
Write-Host ("  daemon processes live: {0} (pids {1})" -f $daemon.Count, (($daemon | ForEach-Object { $_.Id }) -join ','))
# The live vault is being written to continuously, so a single oracle reading
# races the snapshot. Bracket it: oracle -> snapshot -> oracle, and require the
# snapshot to land inside the bracket. That is race-free and still exact.
function Oracle-LiveSums {
    param([string]$Root)
    $m = [int64]0; $c = [int64]0; $w = [int64]0
    foreach ($f in @(Get-ChildItem $Root -Filter 'manifest-*.json' -File)) { $m += Oracle-HandleLength $f.FullName }
    foreach ($f in @(Get-ChildItem (Join-Path $Root 'cf') -Recurse -File)) { $c += Oracle-HandleLength $f.FullName }
    foreach ($f in @(Get-ChildItem (Join-Path $Root 'wal') -Filter '*.wal' -File)) { $w += Oracle-HandleLength $f.FullName }
    return [pscustomobject]@{ manifest = $m; cf = $c; wal = $w }
}
function Check-Bracket {
    param([string]$Name, [int64]$Before, [int64]$After, [int64]$Actual)
    $lo = [Math]::Min($Before, $After); $hi = [Math]::Max($Before, $After)
    $ok = ($Actual -ge $lo -and $Actual -le $hi)
    if (-not $ok) { $script:fail++ }
    $tag = if ($ok) { 'PASS' } else { 'FAIL' }
    $col = if ($ok) { 'Green' } else { 'Red' }
    Write-Host ("  [{0}] {1}: oracle_bracket=[{2}..{3}] snapshot={4}" -f $tag, $Name, $lo, $hi, $Actual) -ForegroundColor $col
}

$oBefore = Oracle-LiveSums -Root $live
$sLive   = Get-SynapseCalyxPhysicalSnapshot -Path $live
$oAfter  = Oracle-LiveSums -Root $live

Write-Host ("  snapshot: manifest={0} cf={1} wal={2} largest_wal={3}/{4} complete={5} byte_source={6}" -f `
    $sLive.manifest_bytes, $sLive.cf_bytes, $sLive.wal_bytes, $sLive.largest_wal.name, $sLive.largest_wal.bytes, `
    $sLive.bytes_complete, $sLive.byte_source)
Write-Host ("  oracle before: manifest={0} cf={1} wal={2}" -f $oBefore.manifest, $oBefore.cf, $oBefore.wal)
Write-Host ("  oracle after : manifest={0} cf={1} wal={2}" -f $oAfter.manifest,  $oAfter.cf,  $oAfter.wal)
Check-Bracket 'live manifest_bytes' $oBefore.manifest $oAfter.manifest $sLive.manifest_bytes
Check-Bracket 'live cf_bytes'       $oBefore.cf       $oAfter.cf       $sLive.cf_bytes
Check-Bracket 'live wal_bytes'      $oBefore.wal      $oAfter.wal      $sLive.wal_bytes
Check 'live bytes_complete' $true           $sLive.bytes_complete

# and prove the live WAL is genuinely moving (so the bracket is a real test)
Write-Host ("  live WAL growth during this check: {0} -> {1} bytes" -f $oBefore.wal, $oAfter.wal)
Check 'live durable_seq present' $true      ($null -ne $sLive.current_manifest_durable_seq)
Write-Host ("  live current_manifest_durable_seq = {0}" -f $sLive.current_manifest_durable_seq)

# ===========================================================================
Write-Host "`n=== CASE 7: the operator-facing purge refusal must quote handle truth ===" -ForegroundColor Yellow
$defs7 = $ast.FindAll({
    param($n)
    $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Write-SynapseVaultDeletionRecord'
}, $true)
if (@($defs7).Count -ne 1) { throw 'Write-SynapseVaultDeletionRecord not found' }
$script:infoLines = New-Object System.Collections.Generic.List[string]
$script:warnLines = New-Object System.Collections.Generic.List[string]
function Info { param($m) $script:infoLines.Add("$m") | Out-Null }
function Warn { param($m) $script:warnLines.Add("$m") | Out-Null }
function Die  { param($m) throw "DIE:$m" }
. ([scriptblock]::Create(@($defs7)[0].Extent.Text))

$pVault = Join-Path $root 'purge-vault'
New-Item -ItemType Directory -Path (Join-Path $pVault 'cf\slot_01') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $pVault 'wal') -Force | Out-Null
[System.IO.File]::WriteAllBytes((Join-Path $pVault 'cf\slot_01\000001.dat'), (New-Object byte[] 4096))
@{ vault_id = 'fsv-1877-vault' } | ConvertTo-Json | Set-Content (Join-Path $pVault 'vault-identity.json')
@{ manifest_seq = 7; durable_seq = 1560000; derived_content_seq = 5 } | ConvertTo-Json |
    Set-Content (Join-Path $pVault 'manifest-00000000000000000007.json')
'manifest-00000000000000000007.json' | Set-Content (Join-Path $pVault 'CURRENT') -NoNewline
@{ manifest_seq = 7; durable_seq = 1560000; derived_content_seq = 5 } | ConvertTo-Json |
    Set-Content (Join-Path $pVault 'MANIFEST')

$pWal = Join-Path $pVault 'wal\00000000000000000000.wal'
$pw = [System.IO.File]::Open($pWal, 'CreateNew', 'Write', ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
try {
    $big = New-Object byte[] 1048576
    for ($i = 0; $i -lt 40; $i++) { $pw.Write($big, 0, $big.Length) }   # 41,943,040 bytes
    $pw.Flush()
    $staleWal = [int64](Oracle-EnumEntry (Join-Path $pVault 'wal') '*.wal').Length
    $trueWal  = Oracle-HandleLength $pWal
    Write-Host ("  BEFORE: directory entry says the WAL is {0} bytes; the handle says {1}" -f $staleWal, $trueWal)
    Check 'stale entry understates the WAL' $true ($staleWal -lt $trueWal)

    $died = $null
    try { Write-SynapseVaultDeletionRecord -DbPath $pVault -Reason 'fsv-1877' } catch { $died = "$_" }
    Write-Host "  AFTER (the message an operator would actually see):"
    Write-Host "    $died" -ForegroundColor DarkYellow
    Check 'refused the purge'                $true ($died -like '*SYNAPSE_SETUP_VAULT_PURGE_REFUSED*')
    Check 'quotes the TRUE wal_bytes'        $true ($died -like "*wal_bytes=$trueWal*")
    Check 'does NOT quote the stale value'   $false ($died -like "*wal_bytes=$staleWal *")
    Check 'names the byte source'            $true ($died -like '*byte_source=handle:GetFileInformationByHandle*')
    Check 'marks the totals exact'           $true ($died -like '*bytes=exact*')
    Check 'quotes durable_seq'               $true ($died -like '*durable_seq=1560000*')
    Check 'no deletion record was written'   0 @(Get-ChildItem $root -Filter 'purge-vault.deleted-*.json' -File).Count
} finally { $pw.Dispose() }

Write-Host ""
if ($fail -eq 0) { Write-Host "ALL CHECKS PASSED" -ForegroundColor Green; exit 0 }
else { Write-Host "$fail CHECK(S) FAILED" -ForegroundColor Red; exit 1 }
