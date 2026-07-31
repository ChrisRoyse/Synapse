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

    1. SHARED-LINT-CONTRACT agreement. `clippy.toml` is resolved per workspace,
       so a `disallowed-methods` rule at the root reaches no calyx crate. The
       rule is therefore duplicated into `calyx/clippy.toml`, and the two copies
       are compared byte-for-byte here. Divergence is a hard failure with a diff.
    2. Toolchain pin agreement. `rust-toolchain.toml` is resolved from the
       *current directory*, so `cd calyx; cargo clippy` used to run clippy 0.1.95
       while the root — the compiler that actually builds calyx into the shipped
       binary — ran 0.1.97. Two lint universes over one tree. The pins must match.
    3. `cargo fmt --all --check`, per workspace.
    4. `cargo clippy --workspace --all-targets`, per workspace.

  It also REPORTS (does not fail on) the `[workspace.lints]` policy divergence
  between the two workspaces, so "I ran lint" never implies the two trees are
  held to one standard when they are not.

.PARAMETER Fix
  Run `cargo fmt --all` (write mode) instead of `--check` before the clippy
  gates. Formatting is the one gate whose repair is unambiguous.

.PARAMETER SkipClippy
  Run only the cheap config gates and `fmt`. For a fast pre-commit pass.

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

Write-Gate 'Gate 1/4  shared clippy.toml contract agreement (#1928)'
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

Write-Gate 'Gate 2/4  rust-toolchain pin agreement (#1928)'
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
# Gates 3 and 4 — fmt and clippy, per workspace
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

Write-Gate 'Gate 3/4  cargo fmt, both workspaces'
[void](Invoke-CargoGate -WorkspaceLabel 'root' -WorkingDirectory $RepoRoot -CargoArgs $fmtArgs `
        -Code 'SYNAPSE_LINT_FMT_ROOT_FAILED' -Remediation 'run scripts/lint.ps1 -Fix, then review the diff')
[void](Invoke-CargoGate -WorkspaceLabel 'calyx' -WorkingDirectory $CalyxRoot -CargoArgs $fmtArgs `
        -Code 'SYNAPSE_LINT_FMT_CALYX_FAILED' -Remediation 'run scripts/lint.ps1 -Fix, then review the diff')

if (-not $SkipClippy) {
    Write-Gate 'Gate 4/4  cargo clippy, both workspaces'
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
    Write-Gate 'Gate 4/4  cargo clippy — SKIPPED by -SkipClippy'
    Write-Host '   NOTE this run proves nothing about lint cleanliness in either workspace.' -ForegroundColor Yellow
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
