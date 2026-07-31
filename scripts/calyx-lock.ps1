<#
.SYNOPSIS
  Makes `calyx/Cargo.lock` a DERIVED artifact of the root `Cargo.lock`, and gates
  the two against each other so the calyx lint workspace compiles what ships.
  Issue #1929.

.DESCRIPTION
  Two lock files exist and both are authoritative for something:

    Cargo.lock        the root workspace. This is what `synapse-mcp.exe` is built
                      from, because every calyx crate is a *path dependency* of
                      it rather than a member (`exclude = ["calyx"]`).
    calyx/Cargo.lock  `cd calyx; cargo {check,clippy,test,fmt}` — the gate that
                      #1928 made mandatory, because a root-level clippy lints no
                      calyx crate.

  Before this script the two resolved independently. Measured at 9daea16f:
  **43 package versions in calyx/Cargo.lock were absent from the root graph and
  were genuinely reachable from calyx's own crates** — bytemuck_derive 1.10.2 vs
  1.11.0 (the derive macro behind calyx-forge's `Pod`/`Zeroable` SIMD types),
  hyper 1.10.1 vs 1.9.0, shlex 2.0.1 vs 1.3.0, the whole windows-targets 0.53.1
  family, log, memchr, uuid, zerocopy. So the calyx gate compiled calyx sources
  against a dependency graph the shipped binary never sees, and a green run there
  was not evidence about the binary. The workspace rule is CPU/GPU bit-parity
  within tolerance; proving parity under one derive-macro version and shipping
  another is not a proof.

  Of the three options #1929 laid out this is option 2, done with a real
  mechanism rather than per-package `cargo update --precise`.

  MODES

    -Sync    copy the root lock over calyx/Cargo.lock, then let cargo re-resolve
             inside the calyx workspace. Cargo keeps every seeded version that
             still satisfies a requirement, prunes what the calyx graph does not
             need, and freshly resolves the edges root never sees. The root's
             choices therefore win everywhere the two graphs overlap.
    -Check   (default) evaluate the invariant below. Run by scripts/lint.ps1
             gate 3. Reads only the two lock files — no cargo, no network.

  THE INVARIANT

      A version that calyx resolves and root does not is a VIOLATION unless it is
      unreachable from calyx's own crates except by passing through a package the
      root graph does not contain at all.

  Equality between the two locks is not the rule and cannot be: the graphs are
  genuinely different. Root holds 618 packages to calyx's 451 (it pulls the whole
  capture/perception/action stack), and calyx holds 13 packages root never
  resolves — calyx-oracle, cuvs-sys, cudaforge, candle-kernels, candle-ug, ug,
  ug-cuda, cmake-package, winsafe, env_home, glob, num, num-iter — CUDA and
  dev-only edges no synapse crate enables.

  Those calyx-only packages drag in their own old majors: `ug` requires
  gemm 0.18.2 and safetensors 0.4.5, `ug-cuda` requires cudarc 0.17.8,
  `cudaforge` requires which 7.0.3. Each sits ALONGSIDE the root's version
  (gemm 0.19.0, cudarc 0.19.8, which 8.0.2), and nothing calyx builds reaches
  them. Failing on those would make the gate unsatisfiable; ignoring them by name
  would need a hand-maintained allowlist that rots. So the exemption is DERIVED
  from the lock's own dependency edges instead: reachability from the calyx
  workspace members with every calyx-only package treated as a wall. An extra
  version outside that reachable set is exempt; one inside it is a violation.

  Measured with this rule:
      at 9daea16f (before)  : 43 violations, 14 exempt
      after -Sync (after)   :  0 violations, 13 exempt

  Fails closed: any violation is a hard error naming every offending package with
  both version sets and the one command that repairs it.

.PARAMETER Sync
  Rewrite calyx/Cargo.lock from the root lock. Prints before/after violation
  counts so the run is measured rather than asserted.

.PARAMETER Check
  Verify the invariant. The default when no switch is given.

.EXAMPLE
  pwsh -File scripts/calyx-lock.ps1
.EXAMPLE
  pwsh -File scripts/calyx-lock.ps1 -Sync
#>
[CmdletBinding()]
param(
    [switch]$Sync,
    [switch]$Check
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RootLock = Join-Path $RepoRoot 'Cargo.lock'
$CalyxRoot = Join-Path $RepoRoot 'calyx'
$CalyxLock = Join-Path $CalyxRoot 'Cargo.lock'

# ---------------------------------------------------------------------------
# Lock parsing
# ---------------------------------------------------------------------------

function Get-Lock {
    <#
      Cargo.lock is machine-generated with a fixed shape: a `[[package]]` header
      followed by `name`, `version`, an optional `source`, and an optional
      `dependencies = [ ... ]` array of `"name"` or `"name version"` strings.
      Parsed with a line state machine rather than a TOML reader so this gate has
      no dependency of its own to keep in step with the thing it is gating.

      Returns @{ Nodes = @{ "name`0version" = @{Name;Version;HasSource;Deps} }
                 Names = HashSet[string]
                 Versions = @{ name = HashSet[string] } }
    #>
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) {
        throw "SYNAPSE_CALYX_LOCK_MISSING: $Path does not exist"
    }

    $nodes = @{}
    $names = [System.Collections.Generic.HashSet[string]]::new()
    $versions = @{}

    $curName = $null; $curVersion = $null; $curSource = $false
    $curDeps = [System.Collections.Generic.List[string]]::new()
    $inDeps = $false

    function Complete-Node {
        # Closes the package block currently being accumulated. Declared inside
        # Get-Lock so the accumulators are in scope without passing six refs.
        if ($null -eq $script:__cn) { return }
        $key = "$($script:__cn)`0$($script:__cv)"
        $nodes[$key] = [pscustomobject]@{
            Name      = $script:__cn
            Version   = $script:__cv
            HasSource = $script:__cs
            Deps      = @($script:__cd)
        }
        [void]$names.Add($script:__cn)
        if (-not $versions.ContainsKey($script:__cn)) {
            $versions[$script:__cn] = [System.Collections.Generic.HashSet[string]]::new()
        }
        [void]$versions[$script:__cn].Add($script:__cv)
        $script:__cn = $null
    }

    $script:__cn = $null; $script:__cv = $null; $script:__cs = $false
    $script:__cd = [System.Collections.Generic.List[string]]::new()

    foreach ($line in [System.IO.File]::ReadAllLines($Path, [System.Text.UTF8Encoding]::new($false))) {
        if ($inDeps) {
            if ($line -match '^\]') { $inDeps = $false; continue }
            if ($line -match '"(.+)"') { $script:__cd.Add($Matches[1]) }
            continue
        }
        if ($line -eq '[[package]]') {
            Complete-Node
            $script:__cn = $null; $script:__cv = $null; $script:__cs = $false
            $script:__cd = [System.Collections.Generic.List[string]]::new()
            continue
        }
        if ($line -match '^name = "(.+)"$') { $script:__cn = $Matches[1]; continue }
        if ($line -match '^version = "(.+)"$') { $script:__cv = $Matches[1]; continue }
        if ($line -match '^source = ') { $script:__cs = $true; continue }
        if ($line -match '^dependencies = \[') { $inDeps = $true; continue }
    }
    Complete-Node

    if ($nodes.Count -eq 0) {
        throw "SYNAPSE_CALYX_LOCK_UNPARSEABLE: $Path yielded zero packages; the lock format is not what this parser expects"
    }
    # A lock with no dependency edges at all would silently make every extra
    # version look unreachable, i.e. exempt. That is the one parser failure that
    # turns this gate into a rubber stamp, so it is checked rather than assumed.
    $withDeps = @($nodes.Values | Where-Object { $_.Deps.Count -gt 0 }).Count
    if ($withDeps -eq 0) {
        throw "SYNAPSE_CALYX_LOCK_NO_EDGES: $Path parsed $($nodes.Count) packages but zero dependency edges; the reachability rule would exempt everything, so this is treated as a parse failure rather than a clean run"
    }

    return [pscustomobject]@{ Nodes = $nodes; Names = $names; Versions = $versions }
}

function Test-LockInvariant {
    param($Root, $Calyx)

    # Resolve a `dependencies` entry to a concrete node key. Cargo writes the
    # version only when the name is ambiguous, so a bare name means "the single
    # version of that package in this lock".
    function Resolve-Dep {
        param([string]$Dep)
        $parts = $Dep.Split(' ')
        $n = $parts[0]
        if ($parts.Count -gt 1) { return "$n`0$($parts[1])" }
        if (-not $Calyx.Versions.ContainsKey($n)) { return $null }
        if ($Calyx.Versions[$n].Count -ne 1) { return $null }
        $only = @($Calyx.Versions[$n])[0]
        return "$n`0$only"
    }

    # Roots of the traversal: the calyx workspace's own crates. A package with no
    # `source` field in a lock is a path/workspace member.
    $members = @($Calyx.Nodes.Values | Where-Object { -not $_.HasSource } | ForEach-Object { "$($_.Name)`0$($_.Version)" })
    if ($members.Count -eq 0) {
        throw "SYNAPSE_CALYX_LOCK_NO_MEMBERS: no source-less packages found in $CalyxLock; the traversal would start from nowhere and exempt every divergence"
    }

    # Reachable from calyx's own crates WITHOUT entering any package the root
    # graph does not contain. Everything outside this set exists only because of
    # an edge root never has.
    $seen = [System.Collections.Generic.HashSet[string]]::new()
    $stack = [System.Collections.Generic.Stack[string]]::new()
    foreach ($m in $members) { $stack.Push($m) }
    while ($stack.Count -gt 0) {
        $cur = $stack.Pop()
        if (-not $seen.Add($cur)) { continue }
        if (-not $Calyx.Nodes.ContainsKey($cur)) { continue }
        foreach ($d in $Calyx.Nodes[$cur].Deps) {
            $key = Resolve-Dep -Dep $d
            if ($null -eq $key -or -not $Calyx.Nodes.ContainsKey($key)) { continue }
            # Wall: never traverse INTO a package absent from the root graph.
            if (-not $Root.Names.Contains($Calyx.Nodes[$key].Name)) { continue }
            $stack.Push($key)
        }
    }

    $violations = @(); $exempt = @(); $calyxOnly = @()
    foreach ($name in ($Calyx.Versions.Keys | Sort-Object)) {
        if (-not $Root.Names.Contains($name)) {
            $calyxOnly += [pscustomobject]@{ Name = $name; Versions = (@($Calyx.Versions[$name]) | Sort-Object) -join ', ' }
            continue
        }
        foreach ($v in (@($Calyx.Versions[$name]) | Sort-Object)) {
            if ($Root.Versions[$name].Contains($v)) { continue }
            $row = [pscustomobject]@{
                Name         = $name
                Version      = $v
                RootVersions = (@($Root.Versions[$name]) | Sort-Object) -join ', '
            }
            if ($seen.Contains("$name`0$v")) { $violations += $row } else { $exempt += $row }
        }
    }

    return [pscustomobject]@{
        Violations = $violations
        Exempt     = $exempt
        CalyxOnly  = $calyxOnly
        RootCount  = $Root.Nodes.Count
        CalyxCount = $Calyx.Nodes.Count
        Reachable  = $seen.Count
    }
}

# ---------------------------------------------------------------------------
# Sync
# ---------------------------------------------------------------------------

if ($Sync) {
    Write-Host '== calyx/Cargo.lock <- Cargo.lock  (#1929)' -ForegroundColor Cyan

    $before = Test-LockInvariant -Root (Get-Lock -Path $RootLock) -Calyx (Get-Lock -Path $CalyxLock)
    Write-Host "   before : $($before.Violations.Count) violation(s), $($before.Exempt.Count) exempt"

    Copy-Item -LiteralPath $RootLock -Destination $CalyxLock -Force

    Write-Host '   -> cargo metadata (re-resolve in calyx/)'
    Push-Location -LiteralPath $CalyxRoot
    try {
        $output = & cargo metadata --format-version 1 2>&1 | Out-String
        $exit = $LASTEXITCODE
    }
    finally {
        Pop-Location
    }
    if ($exit -ne 0) {
        Write-Host $output
        throw "SYNAPSE_CALYX_LOCK_RESOLVE_FAILED: cargo metadata exited $exit in $CalyxRoot after seeding the lock from the root. The seeded lock is left in place for inspection; 'git checkout calyx/Cargo.lock' restores it."
    }

    $after = Test-LockInvariant -Root (Get-Lock -Path $RootLock) -Calyx (Get-Lock -Path $CalyxLock)
    Write-Host "   after  : $($after.Violations.Count) violation(s), $($after.Exempt.Count) exempt" -ForegroundColor Green
    if ($after.Exempt.Count -gt 0) {
        Write-Host ''
        Write-Host '   Exempt — an extra version alongside the root version, reachable only by' -ForegroundColor Yellow
        Write-Host '   crossing a package the root graph does not contain:' -ForegroundColor Yellow
        foreach ($e in $after.Exempt) {
            Write-Host ("      {0,-22} {1,-10} root=[{2}]" -f $e.Name, $e.Version, $e.RootVersions)
        }
    }
    Write-Host ''
    Write-Host "   packages: root=$($after.RootCount) calyx=$($after.CalyxCount) calyx-only=$($after.CalyxOnly.Count)"
    if ($after.Violations.Count -gt 0) {
        Write-Host 'SYNC INCOMPLETE — violations remain after re-resolution; see scripts/lint.ps1 gate 3.' -ForegroundColor Red
        exit 1
    }
    Write-Host 'SYNC OK — commit calyx/Cargo.lock alongside Cargo.lock.' -ForegroundColor Green
    exit 0
}

# ---------------------------------------------------------------------------
# Check (default)
# ---------------------------------------------------------------------------

$result = Test-LockInvariant -Root (Get-Lock -Path $RootLock) -Calyx (Get-Lock -Path $CalyxLock)

Write-Host "   packages: root=$($result.RootCount) calyx=$($result.CalyxCount) calyx-only=$($result.CalyxOnly.Count) reachable-shared=$($result.Reachable)"

if ($result.Violations.Count -eq 0) {
    Write-Host "   OK   0 violations, $($result.Exempt.Count) exempt (extra versions reachable only via calyx-only packages)" -ForegroundColor Green
    exit 0
}

Write-Host ''
Write-Host "SYNAPSE_CALYX_LOCK_DRIFTED: $($result.Violations.Count) package version(s) resolve in calyx/Cargo.lock," -ForegroundColor Red
Write-Host '   are ABSENT from the root graph, and ARE reachable from calyx crates. The calyx' -ForegroundColor Red
Write-Host '   lint gate is not compiling what ships, so a green run there proves nothing' -ForegroundColor Red
Write-Host '   about the binary.' -ForegroundColor Red
Write-Host ''
foreach ($v in $result.Violations) {
    Write-Host ("   {0,-24} calyx={1,-12} root=[{2}]" -f $v.Name, $v.Version, $v.RootVersions)
}
Write-Host ''
Write-Host '   remediation : pwsh -File scripts/calyx-lock.ps1 -Sync' -ForegroundColor Yellow
Write-Host '                 then re-run scripts/lint.ps1 and commit both lock files together.'
exit 1
