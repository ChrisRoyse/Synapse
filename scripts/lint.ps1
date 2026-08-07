<#
.SYNOPSIS
  The single lint entry point for this repository. Covers BOTH workspaces.

.DESCRIPTION
  Issue #1928. The repository root `Cargo.toml` declares `exclude = ["calyx"]`,
  so every calyx crate is a *path dependency* of the root workspace, not a
  member. Cargo applies `RUSTC_WORKSPACE_WRAPPER` — which is how `cargo clippy`
  actually runs clippy-driver — to workspace members only. The measured
  consequence, reproduced on 2026-07-31 with a deliberate
  `clippy::manual_range_contains` violation planted in `calyx-core/src/cosine.rs`:

      cargo clippy -p calyx-core                    (repo root) -> exit 0, SILENT
      cd calyx; cargo clippy -p calyx-core          (calyx)     -> exit 101, error

  Both exit codes are "success" to anyone reading only the first one. So
  "I ran clippy" was never a statement about the whole tree, and the
  `clamp_cosine_quotient` finding that #1928 was filed over had been sitting in
  the tree unreported.

  This script makes the boundary explicit instead of accidental. It runs every
  gate over both workspaces and FAILS CLOSED, naming exactly which gate in which
  workspace failed. There is no partial-success exit path.

  Gates, in order (cheapest first, so a config mistake is not paid for with a
  full compile):

    0. D1 zero-test / zero-harness doctrine (#2037, #2042, #2045). A pure text
       and manifest scan — no compilation, no execution, nothing behavioural.
       Rejects `#[test]` / `#[tokio::test]` / `#[bench]` / `#[cfg(test)]`,
       `[dev-dependencies]` / `[[test]]` / `[[bench]]` manifest sections, and
       `*_fsv` binary targets, in both workspaces. Runs first because it is the
       cheapest gate and because gate 6 (`clippy --all-targets`) would otherwise
       spend a full compile building the very targets this one forbids.
    1. SHARED-LINT-CONTRACT agreement. `clippy.toml` is resolved per workspace,
       so a `disallowed-methods` rule at the root reaches no calyx crate. The
       rule is therefore duplicated into `calyx/clippy.toml`, and the two copies
       are compared byte-for-byte here. Divergence is a hard failure with a diff.
    2. Toolchain pin agreement. `rust-toolchain.toml` is resolved from the
       *current directory*, so `cd calyx; cargo clippy` used to run clippy 0.1.95
       while the root — the compiler that actually builds calyx into the shipped
       binary — ran 0.1.97. Two lint universes over one tree. The pins must match.
    3. Lock-graph agreement (#1929). Gates 1 and 2 make the two workspaces lint
       under one config and one compiler; this one makes them lint the same
       DEPENDENCY GRAPH. `calyx/Cargo.lock` used to resolve independently, and 46
       package versions reachable from calyx's own crates were absent from the
       root graph — so a green calyx gate was not evidence about the shipped
       binary. Delegated to scripts/calyx-lock.ps1, which reads both lock files
       and invokes no cargo.
    4. `cargo fmt --all --check`, per workspace.
    5. `cargo deny check`, per workspace (#1930). Advisories, licenses, bans and
       sources. FAILS CLOSED when the binary is absent rather than skipping — a
       gate that reports success while checking nothing is the exact failure mode
       this script exists to end.
    6. `cargo clippy --workspace --all-targets`, per workspace.

  It also REPORTS (does not fail on) the `[workspace.lints]` policy divergence
  between the two workspaces, so "I ran lint" never implies the two trees are
  held to one standard when they are not.

.PARAMETER Fix
  Run `cargo fmt --all` (write mode) instead of `--check` before the clippy
  gates. Formatting is the one gate whose repair is unambiguous.

.PARAMETER SkipClippy
  Skip gate 6 only. The config gates, `fmt` and `cargo deny` still run, so a
  `-SkipClippy` pass is still a real statement about dependency policy and lock
  agreement — just not about lint cleanliness. For a fast pre-commit pass.

.EXAMPLE
  pwsh -File scripts/lint.ps1
.EXAMPLE
  pwsh -File scripts/lint.ps1 -Fix
#>
[CmdletBinding()]
param(
    [switch]$Fix,
    [switch]$SkipClippy
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$CalyxRoot = Join-Path $RepoRoot 'calyx'

$script:Failures = @()

function Write-Gate {
    param([string]$Name)
    Write-Host ''
    Write-Host "== $Name" -ForegroundColor Cyan
}

function Add-Failure {
    param([string]$Code, [string]$Detail, [string]$Remediation)
    $script:Failures += [pscustomobject]@{
        Code        = $Code
        Detail      = $Detail
        Remediation = $Remediation
    }
    Write-Host "   FAIL $Code" -ForegroundColor Red
    Write-Host "        $Detail" -ForegroundColor Red
}

# ---------------------------------------------------------------------------
# Gate 0 — D1 zero-test / zero-harness doctrine (#2037, #2042, #2045)
# ---------------------------------------------------------------------------
#
# Numbered 0, not 8, on purpose: it is inserted BEFORE the seven existing gates
# rather than appended after them, so the numbering of gates 1-7 (which the
# pre-push hook, the docs and several issues name by number) does not shift.
#
# WHAT THIS IS
#
# Operator directive D1 (2026-07-15) deleted the entire automated test surface
# of this repository: zero `#[test]`, zero `#[tokio::test]`, zero `#[cfg(test)]`
# modules, zero `[dev-dependencies]`, zero benches, zero FSV harness binaries.
# Verification is manual Full State Verification against a physical source of
# truth.
#
# By 2026-08-06 that invariant was false again and nothing had noticed. Four
# inline `#[cfg(test)] mod` blocks (#2037), two `[dev-dependencies]` sections
# (#2042) and two auto-discovered `*_fsv` bin targets (#2045) had all come back
# through ordinary feature commits. The invariant depended on memory, and memory
# is not a gate.
#
# WHAT THIS IS NOT
#
# It is not a test harness, and it must never become one. It compiles nothing,
# runs no binary, and asserts nothing about behaviour. It reads text and
# manifests and reports what it finds — the same category of check as gate 1's
# byte comparison of two clippy.toml blocks. A gate that enforces "no automated
# tests" by running an automated test would be self-refuting.
#
# KNOWN LIMIT, stated so a green result is not over-read: the Rust scan strips
# `//` line comments and `/* */` block comments before matching, so doctrine
# prose that quotes `#[test]` does not trip it — and, symmetrically, an
# attribute smuggled inside a string literal on a line that also opens a comment
# would be missed. Attributes live on their own line in real code; this gate
# catches the regression that actually happens, not an adversary.

$D1RustRoots = @('crates', 'calyx')
$D1PruneDirs = @('target', '.git', 'node_modules', '.cargo', 'vendor')

function Get-D1Files {
    param([string]$Root, [string[]]$Extensions, [string[]]$Names)

    $found = [System.Collections.Generic.List[string]]::new()
    if (-not (Test-Path -LiteralPath $Root)) { return $found }
    $stack = [System.Collections.Generic.Stack[string]]::new()
    $stack.Push((Resolve-Path -LiteralPath $Root).Path)
    while ($stack.Count -gt 0) {
        $dir = $stack.Pop()
        foreach ($entry in [System.IO.Directory]::EnumerateDirectories($dir)) {
            $leaf = Split-Path -Leaf $entry
            if ($D1PruneDirs -contains $leaf) { continue }
            $stack.Push($entry)
        }
        foreach ($entry in [System.IO.Directory]::EnumerateFiles($dir)) {
            $leaf = Split-Path -Leaf $entry
            if ($Names -and ($Names -contains $leaf)) { $found.Add($entry); continue }
            if ($Extensions -and ($Extensions -contains [System.IO.Path]::GetExtension($leaf))) {
                $found.Add($entry)
            }
        }
    }
    return $found
}

# Strips `//` line comments and `/* */` block comments from Rust source, keeping
# the line count intact so reported line numbers stay true to the file.
function Remove-RustComments {
    param([string[]]$Lines)

    $out = [string[]]::new($Lines.Count)
    $inBlock = $false
    for ($i = 0; $i -lt $Lines.Count; $i++) {
        $line = $Lines[$i]
        $kept = ''
        $j = 0
        while ($j -lt $line.Length) {
            if ($inBlock) {
                $close = $line.IndexOf('*/', $j)
                if ($close -lt 0) { $j = $line.Length; break }
                $inBlock = $false
                $j = $close + 2
                continue
            }
            $open = $line.IndexOf('/*', $j)
            $slash = $line.IndexOf('//', $j)
            if ($slash -ge 0 -and ($open -lt 0 -or $slash -lt $open)) {
                $kept += $line.Substring($j, $slash - $j)
                $j = $line.Length
                break
            }
            if ($open -ge 0) {
                $kept += $line.Substring($j, $open - $j)
                $inBlock = $true
                $j = $open + 2
                continue
            }
            $kept += $line.Substring($j)
            break
        }
        $out[$i] = $kept
    }
    return $out
}

Write-Gate 'Gate 0     D1 zero-test / zero-harness doctrine (#2037, #2042, #2045)'
try {
    $RelativeTo = { param([string]$Path) $Path.Substring($RepoRoot.Length).TrimStart('\', '/') }

    # --- 0a: Rust test attributes -------------------------------------------
    # `#[test]`, `#[tokio::test]`, `#[bench]` and any `path::to::test`.
    $attrRe = [regex]'^\s*#\[\s*(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*(test|bench)\s*[\]\(]'
    # `#[cfg(...)]` / `#[cfg_attr(...)]` whose predicate list contains a bare
    # `test` token. Quoted strings are removed first so `feature = "test-only"`
    # is not mistaken for the `test` cfg.
    $cfgRe = [regex]'^\s*#\[\s*cfg(_attr)?\s*\('
    $quoteRe = [regex]'"(?:[^"\\]|\\.)*"'
    $bareTestRe = [regex]'(?<![A-Za-z0-9_])test(?![A-Za-z0-9_])'

    $rustHits = [System.Collections.Generic.List[string]]::new()
    $rustScanned = 0
    foreach ($root in $D1RustRoots) {
        foreach ($file in Get-D1Files -Root (Join-Path $RepoRoot $root) -Extensions @('.rs')) {
            $rustScanned++
            $raw = [IO.File]::ReadAllText($file)
            if ($raw -notmatch '#\[') { continue }
            $stripped = Remove-RustComments -Lines ($raw -split "\r?\n")
            for ($i = 0; $i -lt $stripped.Count; $i++) {
                $line = $stripped[$i]
                if ($line -notmatch '#\[') { continue }
                $what = $null
                $m = $attrRe.Match($line)
                if ($m.Success) {
                    $what = "#[...$($m.Groups[1].Value)]"
                }
                elseif ($cfgRe.IsMatch($line) -and $bareTestRe.IsMatch($quoteRe.Replace($line, '""'))) {
                    $what = '#[cfg(test)]'
                }
                if ($what) {
                    $rustHits.Add("        $(& $RelativeTo $file):$($i + 1)  $what  ->  $($line.Trim())")
                }
            }
        }
    }
    if ($rustHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_TEST_ATTRIBUTE_PRESENT' `
        ("$($rustHits.Count) automated-test attribute(s) in tracked Rust source, which directive D1 deleted repo-wide on 2026-07-15 (#2037):" + [Environment]::NewLine + ($rustHits -join [Environment]::NewLine)) `
            'delete the test module. If it was protecting a real invariant, express that invariant as ordinary fail-closed production code (a runtime check that logs a structured code, or a debug_assert) and verify it by manual FSV against a physical source of truth — never by re-adding a libtest target.'
    }
    else {
        Write-Host "   OK   $rustScanned .rs files carry no #[test]/#[tokio::test]/#[bench]/#[cfg(test)]" -ForegroundColor Green
    }

    # --- 0b/0c: forbidden manifest sections and *_fsv bin targets -----------
    $devDepRe = [regex]'^\s*\[\s*(?:[A-Za-z0-9_."''\-\*\(\)= ]*\.)?dev[-_]dependencies\s*\]'
    $testTargetRe = [regex]'^\s*\[\[\s*(test|bench)\s*\]\]'
    $binOpenRe = [regex]'^\s*\[\[\s*bin\s*\]\]'
    $sectionRe = [regex]'^\s*\['
    $nameRe = [regex]'^\s*(name|path)\s*=\s*"([^"]*)"'

    $devDepHits = [System.Collections.Generic.List[string]]::new()
    $testTargetHits = [System.Collections.Generic.List[string]]::new()
    $fsvBinHits = [System.Collections.Generic.List[string]]::new()
    $manifests = Get-D1Files -Root $RepoRoot -Names @('Cargo.toml')
    foreach ($file in $manifests) {
        $lines = [IO.File]::ReadAllLines($file)
        $inBin = $false
        $binStart = 0
        $binFlagged = $false
        for ($i = 0; $i -lt $lines.Count; $i++) {
            $line = $lines[$i]
            if ($devDepRe.IsMatch($line)) {
                $devDepHits.Add("        $(& $RelativeTo $file):$($i + 1)  $($line.Trim())")
            }
            if ($testTargetRe.IsMatch($line)) {
                $testTargetHits.Add("        $(& $RelativeTo $file):$($i + 1)  $($line.Trim())")
            }
            if ($binOpenRe.IsMatch($line)) {
                $inBin = $true
                $binStart = $i + 1
                $binFlagged = $false
                continue
            }
            if ($inBin -and $sectionRe.IsMatch($line)) { $inBin = $false }
            # One row per [[bin]] block, not one per key: a target whose name and
            # path both say fsv is one forbidden target, not two.
            if ($inBin -and -not $binFlagged) {
                $m = $nameRe.Match($line)
                if ($m.Success -and $m.Groups[2].Value -match '(^|[/_\-])fsv([_\-.]|$)') {
                    $binFlagged = $true
                    $fsvBinHits.Add("        $(& $RelativeTo $file):$binStart  [[bin]] $($line.Trim())")
                }
            }
        }
    }

    # Autodiscovered bins: a file under any `src/bin/` whose stem names fsv is a
    # shipping binary even though no manifest mentions it. That is exactly how
    # #2045 happened, so the file layout is checked, not only the manifests.
    foreach ($root in $D1RustRoots) {
        foreach ($file in Get-D1Files -Root (Join-Path $RepoRoot $root) -Extensions @('.rs')) {
            if ($file -notmatch '[\\/]src[\\/]bin[\\/]') { continue }
            $stem = [System.IO.Path]::GetFileNameWithoutExtension($file)
            if ($stem -match '(^|[_\-])fsv([_\-]|$)') {
                $fsvBinHits.Add("        $(& $RelativeTo $file)  (autodiscovered src/bin target)")
            }
        }
    }

    if ($devDepHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_DEV_DEPENDENCIES_PRESENT' `
        ("$($devDepHits.Count) [dev-dependencies] section(s), which directive D1 bans outright (#2042):" + [Environment]::NewLine + ($devDepHits -join [Environment]::NewLine)) `
            'a dev-only dependency surface is the manifest half of a test surface. Cargo has no example-only dependency table, so either declare the crate as an ordinary [dependencies] entry with a comment naming its example consumer, or delete the dependency together with the code that reached it.'
    }
    if ($testTargetHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_TEST_TARGET_SECTION_PRESENT' `
        ("$($testTargetHits.Count) [[test]]/[[bench]] target section(s), which directive D1 deleted repo-wide:" + [Environment]::NewLine + ($testTargetHits -join [Environment]::NewLine)) `
            'delete the section and the tests/ or benches/ directory it points at; measurement is manual FSV against a physical source of truth.'
    }
    if ($fsvBinHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_FSV_BIN_TARGET_PRESENT' `
        ("$($fsvBinHits.Count) *_fsv binary target(s), which every release links (#2045):" + [Environment]::NewLine + ($fsvBinHits -join [Environment]::NewLine)) `
            'delete the target and any support surface it exclusively reached. Set autobins = false in the owning [package] so a file dropped into src/ bin builds nothing until someone writes a reviewable [[bin]] block for it.'
    }
    if ($devDepHits.Count -eq 0 -and $testTargetHits.Count -eq 0 -and $fsvBinHits.Count -eq 0) {
        Write-Host "   OK   $($manifests.Count) Cargo.toml files declare no [dev-dependencies], [[test]], [[bench]] or *_fsv bin" -ForegroundColor Green
    }
}
catch {
    Add-Failure 'SYNAPSE_LINT_D1_SWEEP_FAILED' $_.Exception.Message `
        'the doctrine sweep itself failed; this gate fails closed rather than reporting an invariant it did not check'
}

# ---------------------------------------------------------------------------
# Gate 1 — the shared clippy.toml contract must be byte-identical
# ---------------------------------------------------------------------------

$SharedBeginMarker = '# >>> SHARED-LINT-CONTRACT'
$SharedEndMarker = '# <<< SHARED-LINT-CONTRACT <<<'

function Get-SharedContract {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) {
        throw "SYNAPSE_LINT_CLIPPY_TOML_MISSING: $Path does not exist; both workspaces must carry a clippy.toml holding the shared contract"
    }
    # Read as raw bytes decoded UTF-8 so a BOM or CRLF difference cannot be
    # mistaken for a rule difference, and so a real rule difference cannot hide
    # behind whitespace normalization.
    $lines = [System.IO.File]::ReadAllLines($Path, [System.Text.UTF8Encoding]::new($false))
    $begin = -1
    $end = -1
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($begin -lt 0 -and $lines[$i].StartsWith($SharedBeginMarker)) { $begin = $i; continue }
        if ($begin -ge 0 -and $lines[$i].StartsWith($SharedEndMarker)) { $end = $i; break }
    }
    if ($begin -lt 0) {
        throw "SYNAPSE_LINT_SHARED_CONTRACT_BEGIN_MISSING: $Path has no '$SharedBeginMarker' marker; the shared rule block cannot be located, so its agreement with the other workspace cannot be proven"
    }
    if ($end -lt 0) {
        throw "SYNAPSE_LINT_SHARED_CONTRACT_END_MISSING: $Path has a begin marker but no '$SharedEndMarker'; the block is unterminated"
    }
    # Exclusive of both marker lines: the markers name their own file's path in
    # prose, the rules between them must not.
    if ($end -le ($begin + 1)) { return @() }
    return $lines[($begin + 1)..($end - 1)]
}

Write-Gate 'Gate 1/7  shared clippy.toml contract agreement (#1928)'
$rootClippy = Join-Path $RepoRoot 'clippy.toml'
$calyxClippy = Join-Path $CalyxRoot 'clippy.toml'
try {
    $rootShared = @(Get-SharedContract -Path $rootClippy)
    $calyxShared = @(Get-SharedContract -Path $calyxClippy)

    if ($rootShared.Count -eq 0) {
        Add-Failure 'SYNAPSE_LINT_SHARED_CONTRACT_EMPTY' `
            "$rootClippy declares an empty SHARED-LINT-CONTRACT block" `
            'put the cross-workspace rules between the markers, or remove the markers entirely'
    }

    $diff = Compare-Object -ReferenceObject $rootShared -DifferenceObject $calyxShared -SyncWindow 0
    if ($null -ne $diff) {
        $rendered = ($diff | ForEach-Object {
                $side = if ($_.SideIndicator -eq '<=') { 'root-only ' } else { 'calyx-only' }
                "        $side | $($_.InputObject)"
            }) -join [Environment]::NewLine
        Add-Failure 'SYNAPSE_LINT_SHARED_CONTRACT_DIVERGED' `
        ("clippy.toml SHARED-LINT-CONTRACT blocks differ between the two workspaces:" + [Environment]::NewLine + $rendered) `
            'edit both clippy.toml files together so the blocks are byte-identical; a rule present in only one file applies to only one tree'
    }
    else {
        Write-Host "   OK   $($rootShared.Count) shared lines identical in both clippy.toml files" -ForegroundColor Green
    }
}
catch {
    Add-Failure 'SYNAPSE_LINT_SHARED_CONTRACT_UNREADABLE' $_.Exception.Message `
        'restore the SHARED-LINT-CONTRACT markers in both clippy.toml files'
}

# ---------------------------------------------------------------------------
# Gate 2 — the two rust-toolchain pins must match
# ---------------------------------------------------------------------------

function Get-ToolchainChannel {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "SYNAPSE_LINT_TOOLCHAIN_FILE_MISSING: $Path does not exist"
    }
    $match = Select-String -LiteralPath $Path -Pattern '^\s*channel\s*=\s*"([^"]+)"' | Select-Object -First 1
    if ($null -eq $match) {
        throw "SYNAPSE_LINT_TOOLCHAIN_CHANNEL_UNPARSEABLE: $Path declares no `channel = ""...""` line"
    }
    return $match.Matches[0].Groups[1].Value
}

Write-Gate 'Gate 2/7  rust-toolchain pin agreement (#1928)'
try {
    $rootChannel = Get-ToolchainChannel -Path (Join-Path $RepoRoot 'rust-toolchain.toml')
    $calyxChannel = Get-ToolchainChannel -Path (Join-Path $CalyxRoot 'rust-toolchain.toml')
    if ($rootChannel -ne $calyxChannel) {
        Add-Failure 'SYNAPSE_LINT_TOOLCHAIN_PIN_DIVERGED' `
            "root pins $rootChannel but calyx/ pins $calyxChannel; rust-toolchain.toml resolves from the CURRENT DIRECTORY, so 'cd calyx; cargo clippy' lints with a different clippy than the compiler that builds those same crates into the shipped binary" `
            'set both rust-toolchain.toml channels to the same version and re-run this script'
    }
    else {
        Write-Host "   OK   both workspaces pin $rootChannel" -ForegroundColor Green
    }
}
catch {
    Add-Failure 'SYNAPSE_LINT_TOOLCHAIN_UNREADABLE' $_.Exception.Message `
        'restore a parseable [toolchain] channel in both rust-toolchain.toml files'
}

# ---------------------------------------------------------------------------
# Report — is the pre-push hook actually armed on this clone? (#1931)
# ---------------------------------------------------------------------------
#
# `.githooks/pre-push` calls itself the CI replacement and delegates to this
# script, but it only runs if `core.hooksPath` points at it, and that is a manual
# `git config` nobody is prompted to perform. Measured 2026-07-31 on the primary
# clone: unset, `.git/hooks/` holding only samples — so the backstop had never
# run there.
#
# REPORTED, not enforced. Arming the hook is per-clone local config; failing a
# lint run over it would make this script unusable in a fresh checkout and in
# any context that legitimately has no hook (a CI-less scratch clone, a bisect
# worktree). What was missing was not enforcement, it was that an unarmed clone
# looked exactly like an armed one.

Write-Gate 'Report    pre-push hook armed on this clone? (#1931)'
$hooksPath = (& git -C $RepoRoot config --get core.hooksPath 2>$null)
if ($LASTEXITCODE -ne 0) { $hooksPath = '' }
$hooksPath = "$hooksPath".Trim()
if ($hooksPath -eq '.githooks') {
    Write-Host "   OK   core.hooksPath = .githooks; git push runs the gates above first" -ForegroundColor Green
}
elseif ([string]::IsNullOrWhiteSpace($hooksPath)) {
    Write-Host '   NOT ARMED  core.hooksPath is unset, so .githooks/pre-push never runs and' -ForegroundColor Yellow
    Write-Host '              `git push` gates nothing on this clone.' -ForegroundColor Yellow
    Write-Host '              arm it with:  git config core.hooksPath .githooks' -ForegroundColor Yellow
}
else {
    Write-Host "   NOT ARMED  core.hooksPath = '$hooksPath', which is not .githooks; the" -ForegroundColor Yellow
    Write-Host '              repository pre-push gate is being bypassed by another hook dir.' -ForegroundColor Yellow
}

# ---------------------------------------------------------------------------
# Report — [workspace.lints] policy divergence (informational, never a failure)
# ---------------------------------------------------------------------------

function Get-WorkspaceLints {
    param([string]$ManifestPath)
    $lines = [System.IO.File]::ReadAllLines($ManifestPath, [System.Text.UTF8Encoding]::new($false))
    $out = [ordered]@{}
    $section = $null
    foreach ($line in $lines) {
        $trimmed = $line.Trim()
        if ($trimmed.StartsWith('[')) {
            $section = if ($trimmed -match '^\[workspace\.lints\.([a-z]+)\]$') { $Matches[1] } else { $null }
            continue
        }
        if ($null -eq $section) { continue }
        if ($trimmed.Length -eq 0 -or $trimmed.StartsWith('#')) { continue }
        if ($trimmed -match '^([A-Za-z0-9_]+)\s*=\s*(.+)$') {
            $out["$section::$($Matches[1])"] = $Matches[2].Trim()
        }
    }
    return $out
}

Write-Gate 'Report    [workspace.lints] policy divergence (#1928 ask 3)'
$rootLints = Get-WorkspaceLints -ManifestPath (Join-Path $RepoRoot 'Cargo.toml')
$calyxLints = Get-WorkspaceLints -ManifestPath (Join-Path $CalyxRoot 'Cargo.toml')
$allKeys = @($rootLints.Keys) + @($calyxLints.Keys) | Sort-Object -Unique
$divergent = 0
foreach ($key in $allKeys) {
    $r = if ($rootLints.Contains($key)) { $rootLints[$key] } else { '<unset>' }
    $c = if ($calyxLints.Contains($key)) { $calyxLints[$key] } else { '<unset>' }
    if ($r -ne $c) {
        $divergent++
        Write-Host "   DIVERGES  $key : root=$r  calyx=$c" -ForegroundColor Yellow
    }
}
if ($divergent -eq 0) {
    Write-Host '   OK   both workspaces declare identical [workspace.lints]' -ForegroundColor Green
}
else {
    Write-Host "   $divergent lint key(s) differ. This is REPORTED, not enforced: calyx carries real" -ForegroundColor Yellow
    Write-Host '   `unsafe` (SIMD intrinsics, mmap, CUDA FFI), so `unsafe_code = "forbid"` cannot' -ForegroundColor Yellow
    Write-Host '   apply there. The unwrap_used/expect_used escalation is tracked separately.' -ForegroundColor Yellow
}

# ---------------------------------------------------------------------------
# Gate 3 — the two lock graphs must agree (#1929)
# ---------------------------------------------------------------------------

Write-Gate 'Gate 3/7  Cargo.lock graph agreement, root vs calyx (#1929)'
$lockScript = Join-Path $PSScriptRoot 'calyx-lock.ps1'
if (-not (Test-Path -LiteralPath $lockScript)) {
    Add-Failure 'SYNAPSE_LINT_LOCK_GATE_MISSING' `
        "$lockScript does not exist, so the calyx lock graph cannot be checked against the root's" `
        'restore scripts/calyx-lock.ps1 (issue #1929)'
}
else {
    # Run in-process rather than as a nested pwsh: the child would not share
    # $script:Failures, and a non-zero exit there must land in THIS script's
    # verdict rather than being printed and forgotten.
    & $lockScript -Check
    if ($LASTEXITCODE -ne 0) {
        Add-Failure 'SYNAPSE_LINT_CALYX_LOCK_DRIFTED' `
            "calyx/Cargo.lock resolves package versions that the root graph does not contain and that calyx crates actually reach; the calyx clippy gate below would not be linting what ships" `
            'pwsh -File scripts/calyx-lock.ps1 -Sync, then re-run this script and commit both lock files together'
    }
}

# ---------------------------------------------------------------------------
# Gates 4, 5 and 6 — fmt, cargo-deny and clippy, per workspace
# ---------------------------------------------------------------------------

function Invoke-CargoGate {
    param(
        [string]$WorkspaceLabel,
        [string]$WorkingDirectory,
        [string[]]$CargoArgs,
        [string]$Code,
        [string]$Remediation
    )
    $rendered = "cargo $($CargoArgs -join ' ')"
    Write-Host "   -> [$WorkspaceLabel] $rendered"
    Push-Location -LiteralPath $WorkingDirectory
    try {
        $output = & cargo @CargoArgs 2>&1 | Out-String
        $exit = $LASTEXITCODE
    }
    finally {
        Pop-Location
    }
    if ($exit -ne 0) {
        Write-Host $output
        Add-Failure $Code "[$WorkspaceLabel] $rendered exited $exit" $Remediation
        return $false
    }
    Write-Host "   OK   [$WorkspaceLabel] $rendered" -ForegroundColor Green
    return $true
}

$fmtArgs = if ($Fix) { @('fmt', '--all') } else { @('fmt', '--all', '--check') }

Write-Gate 'Gate 4/7  cargo fmt, both workspaces'
[void](Invoke-CargoGate -WorkspaceLabel 'root' -WorkingDirectory $RepoRoot -CargoArgs $fmtArgs `
        -Code 'SYNAPSE_LINT_FMT_ROOT_FAILED' -Remediation 'run scripts/lint.ps1 -Fix, then review the diff')
[void](Invoke-CargoGate -WorkspaceLabel 'calyx' -WorkingDirectory $CalyxRoot -CargoArgs $fmtArgs `
        -Code 'SYNAPSE_LINT_FMT_CALYX_FAILED' -Remediation 'run scripts/lint.ps1 -Fix, then review the diff')

# ---------------------------------------------------------------------------
# Gate 5 — cargo-deny, both workspaces (#1930)
# ---------------------------------------------------------------------------
#
# The root `deny.toml` sat in this repository from initial scaffolding and NOTHING ever
# ran it, while two docs listed it as an active gate. The first real run
# (2026-07-31) failed all three checks with 45 findings. So the one thing this
# gate must never do is report success without having run: an absent binary is a
# HARD FAILURE, not a skip.
#
# It runs over BOTH workspaces for the reason gate 3 exists. The root and calyx
# graphs are not identical even after the lock sync — calyx resolves 13 packages
# root never sees — and RUSTSEC-2026-0186 (memmap2) was found only in the calyx
# run before `unsound = "all"` was set in both workspace policies. Synapse and
# standalone Calyx intentionally resolve different optional capability graphs,
# so each lock file has a colocated policy. Both retain the same fail-closed
# baseline; only graph-specific licenses, sources, and written advisory
# exceptions differ.

$DenyMinVersion = [version]'0.20.2'

Write-Gate 'Gate 5/7  cargo deny check, both workspaces (#1930)'
$denyCmd = Get-Command 'cargo-deny' -ErrorAction SilentlyContinue
if ($null -eq $denyCmd) {
    Add-Failure 'SYNAPSE_LINT_CARGO_DENY_ABSENT' `
        'cargo-deny is not installed on this host, so the advisory/license/ban policy in deny.toml is enforced by nothing (#1930)' `
        'pwsh -File scripts/install-cargo-deny.ps1'
}
else {
    $denyVersionRaw = (& cargo deny --version 2>&1 | Out-String).Trim()
    $parsed = $null
    if ($denyVersionRaw -match 'cargo-deny\s+(\d+\.\d+\.\d+)') { $parsed = [version]$Matches[1] }

    if ($null -eq $parsed) {
        Add-Failure 'SYNAPSE_LINT_CARGO_DENY_VERSION_UNPARSEABLE' `
            "'cargo deny --version' returned '$denyVersionRaw', which does not match 'cargo-deny X.Y.Z'" `
            'pwsh -File scripts/install-cargo-deny.ps1 -Force'
    }
    elseif ($parsed -lt $DenyMinVersion) {
        # deny.toml uses `unsound`/`unmaintained = "all"` and
        # `unused-ignored-advisory`, which older cargo-deny does not understand.
        # Catch it by version rather than by a confusing unknown-field error.
        Add-Failure 'SYNAPSE_LINT_CARGO_DENY_TOO_OLD' `
            "found cargo-deny $parsed but deny.toml requires >= $DenyMinVersion (it uses the advisories fields unsound/unmaintained = 'all' and unused-ignored-advisory, which older releases reject as unknown)" `
            "pwsh -File scripts/install-cargo-deny.ps1 -Version $DenyMinVersion -Force"
    }
    else {
        Write-Host "   using $denyVersionRaw" -ForegroundColor Green
        [void](Invoke-CargoGate -WorkspaceLabel 'root' -WorkingDirectory $RepoRoot `
                -CargoArgs @('deny', 'check') `
                -Code 'SYNAPSE_LINT_CARGO_DENY_ROOT_FAILED' `
                -Remediation 'read the reported RUSTSEC ids / licenses / bans. Fix at the source (cargo update, a manifest floor, a removed wildcard). Only add a deny.toml ignore with a written reason and a removal condition.')
        [void](Invoke-CargoGate -WorkspaceLabel 'calyx' -WorkingDirectory $CalyxRoot `
                -CargoArgs @('deny', 'check') `
                -Code 'SYNAPSE_LINT_CARGO_DENY_CALYX_FAILED' `
                -Remediation 'same as root. Inspect calyx/deny.toml for the standalone graph policy; every exception needs a written reason and removal condition.')
    }
}

if (-not $SkipClippy) {
    Write-Gate 'Gate 6/7  cargo clippy, both workspaces'
    $clippyArgs = @('clippy', '--workspace', '--all-targets')
    [void](Invoke-CargoGate -WorkspaceLabel 'root' -WorkingDirectory $RepoRoot -CargoArgs $clippyArgs `
            -Code 'SYNAPSE_LINT_CLIPPY_ROOT_FAILED' -Remediation 'fix the reported lints; the root workspace denies clippy::all')
    # NOT `-p calyx-*` from the repo root: that compiles without linting, because
    # calyx crates are excluded path dependencies rather than workspace members.
    # This must run with calyx/ as the current directory (#1928).
    [void](Invoke-CargoGate -WorkspaceLabel 'calyx' -WorkingDirectory $CalyxRoot -CargoArgs $clippyArgs `
            -Code 'SYNAPSE_LINT_CLIPPY_CALYX_FAILED' -Remediation 'fix the reported lints; the calyx workspace denies clippy::all')
}
else {
    Write-Gate 'Gate 6/7  cargo clippy — SKIPPED by -SkipClippy'
    Write-Host '   NOTE this run proves nothing about lint cleanliness in either workspace.' -ForegroundColor Yellow
}

# ---------------------------------------------------------------------------
# Gate 7 — unreached public calyx API ratchet (#1944)
# ---------------------------------------------------------------------------
#
# `calyx_assay::ensemble_card` had no caller anywhere in Synapse, and that is
# why three defects (#1942, #1943, and a whole-pass abort on one degenerate
# lens) accumulated on it unobserved. An unreached code path accumulates
# defects at full rate and reports none of them. It was found by accident.
#
# This is the sweep that finds the rest, run as a ratchet: the count may fall
# but never rise. Wiring a `pub fn` to a caller lowers it; adding another
# unreachable one fails the gate. Deliberately NOT a hard zero — there were 375
# at the time this was written, and failing the whole tree on a pre-existing
# number nobody can pay down in one change is how a gate gets disabled.
#
# Pure PowerShell on purpose: this must not add a ripgrep dependency to the one
# lint entry point.
#
# Known limits, stated so a number from this gate is not over-read: it is
# lexical, so it cannot see a function reached only through trait-object
# dispatch, and a few of the counted symbols are legitimate public API with no
# in-tree consumer. It is conservative in three directions — tests and examples
# are included in the occurrence count, a name declared in several places must
# be unreferenced at all of them, and same-name collisions across crates merge
# — so it under-reports rather than inventing findings.

Write-Gate 'Gate 7/7  unreached public calyx API ratchet (#1944)'

$BaselinePath = Join-Path $PSScriptRoot 'unreached-calyx-api-baseline.txt'
try {
    $declRe = [regex]'(?m)^\s*pub\s+(?:const\s+|async\s+|unsafe\s+)*fn\s+([a-z_][a-z0-9_]*)'
    $anyFnRe = [regex]'\bfn\s+([a-z_][a-z0-9_]*)'
    $skipRe = '[\\/](?:tests|examples|benches|target)[\\/]'

    $calyxCrates = Join-Path $CalyxRoot 'crates'
    $rootCrates = Join-Path $RepoRoot 'crates'

    # Candidate set: every `pub fn` a calyx crate declares in non-test code.
    $candidates = [System.Collections.Generic.HashSet[string]]::new()
    foreach ($file in Get-ChildItem $calyxCrates -Recurse -Filter *.rs -File) {
        if ($file.FullName -match $skipRe) { continue }
        foreach ($match in $declRe.Matches([IO.File]::ReadAllText($file.FullName))) {
            [void]$candidates.Add($match.Groups[1].Value)
        }
    }

    # Occurrences and declarations of those names across BOTH trees. `uses` is
    # occurrences minus declarations, so a symbol that appears only where it is
    # defined scores 0 and is unreached.
    $total = @{}
    $decls = @{}
    # Note the asymmetry, which is deliberate: candidates come only from
    # non-test calyx code, but occurrences are counted over EVERYTHING
    # including tests and examples. A `pub fn` exercised by a test is observed,
    # so it is not the silent defect reservoir this gate exists to find, and
    # counting it as one would be a false finding.
    $scanRoots = @($calyxCrates, $rootCrates) | Where-Object { Test-Path $_ }
    foreach ($file in Get-ChildItem $scanRoots -Recurse -Filter *.rs -File) {
        if ($file.FullName -match '[\\/]target[\\/]') { continue }
        $text = [IO.File]::ReadAllText($file.FullName)
        foreach ($word in [regex]::Split($text, '[^A-Za-z0-9_]+')) {
            if ($candidates.Contains($word)) { $total[$word] = 1 + $total[$word] }
        }
        foreach ($match in $anyFnRe.Matches($text)) {
            $name = $match.Groups[1].Value
            if ($candidates.Contains($name)) { $decls[$name] = 1 + $decls[$name] }
        }
    }

    $unreached = [System.Collections.Generic.List[string]]::new()
    foreach ($name in $candidates) {
        $used = [int]$total[$name] - [int]$decls[$name]
        if ($used -le 0) { $unreached.Add($name) }
    }
    $count = $unreached.Count

    if (-not (Test-Path $BaselinePath)) {
        Add-Failure 'SYNAPSE_LINT_UNREACHED_API_BASELINE_MISSING' `
            "no baseline at $BaselinePath; measured $count unreached pub fn" `
            "write the measured count to that file to adopt it as the baseline, after confirming $count is the real current number"
    }
    else {
        $raw = (Get-Content $BaselinePath -Raw).Trim()
        $baseline = 0
        if (-not [int]::TryParse($raw, [ref]$baseline)) {
            Add-Failure 'SYNAPSE_LINT_UNREACHED_API_BASELINE_UNPARSEABLE' `
                "baseline file $BaselinePath does not contain an integer (found '$raw')" `
                'the file must contain only the baseline count'
        }
        elseif ($count -gt $baseline) {
            $added = ($unreached | Sort-Object) -join ', '
            Add-Failure 'SYNAPSE_LINT_UNREACHED_API_INCREASED' `
                "$count public calyx fn have no caller anywhere, up from the baseline $baseline" `
                "a new pub fn was added with nothing calling it. Wire it to a caller, or if it is deliberately unreached for now say so where the code is and raise the baseline in $BaselinePath. Current set: $added"
        }
        else {
            Write-Host "   OK   $count unreached pub fn (baseline $baseline)" -ForegroundColor Green
            if ($count -lt $baseline) {
                Write-Host "   NOTE ratchet fell by $($baseline - $count); lower the baseline in $BaselinePath to hold the gain." -ForegroundColor Yellow
            }
        }
    }
}
catch {
    Add-Failure 'SYNAPSE_LINT_UNREACHED_API_SWEEP_FAILED' $_.Exception.Message `
        'the unreached-API sweep itself failed; this gate fails closed rather than reporting a count it did not compute'
}

# ---------------------------------------------------------------------------
# Verdict — fail closed
# ---------------------------------------------------------------------------

Write-Host ''
if ($script:Failures.Count -eq 0) {
    if ($SkipClippy) {
        Write-Host 'LINT PARTIAL: config + fmt gates passed in BOTH workspaces; clippy was skipped.' -ForegroundColor Yellow
        exit 0
    }
    Write-Host 'LINT OK: every gate passed in BOTH workspaces (root and calyx).' -ForegroundColor Green
    exit 0
}

Write-Host "LINT FAILED — $($script:Failures.Count) gate(s):" -ForegroundColor Red
foreach ($failure in $script:Failures) {
    Write-Host ''
    Write-Host "  code        : $($failure.Code)" -ForegroundColor Red
    Write-Host "  detail      : $($failure.Detail)"
    Write-Host "  remediation : $($failure.Remediation)"
}
Write-Host ''
exit 1
