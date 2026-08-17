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

    0. D1 zero-test / zero-harness doctrine (#2037, #2042, #2045, #2153,
       #2154, #2164). A pure text, manifest, lockfile, and executable-path scan
       — no compilation, no execution, nothing behavioural.
       Rejects `#[test]` / `#[tokio::test]` / `#[bench]` / `#[cfg(test)]`,
       `[dev-dependencies]` / `[[test]]` / `[[bench]]` manifest sections, and
       FSV executable targets/scripts, plus JavaScript/TypeScript test scripts,
       direct runner dependencies, runner configs, and test source paths. Runs
       first because it is the cheapest gate and because gate 6
       (`clippy --all-targets`) would otherwise spend a full compile building
       the very targets this one forbids.
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
    6. `cargo clippy --workspace --all-targets -- -D warnings`, per workspace.

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

.PARAMETER PolicyOnly
  Run only Gate 0, print its complete fail-closed verdict, and exit. The
  pre-push hook uses this cheap path for pushes that do not need the Rust/Cargo
  gates, so D1 enforcement cannot be bypassed by adding only a script or another
  non-Rust file.

.EXAMPLE
  pwsh -File scripts/lint.ps1
.EXAMPLE
  pwsh -File scripts/lint.ps1 -Fix
.EXAMPLE
  pwsh -File scripts/lint.ps1 -PolicyOnly
#>
[CmdletBinding()]
param(
    [switch]$Fix,
    [switch]$SkipClippy,
    [switch]$PolicyOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$CalyxRoot = Join-Path $RepoRoot 'calyx'

$script:Failures = @()

if ($PolicyOnly -and ($Fix -or $SkipClippy)) {
    throw 'SYNAPSE_LINT_POLICY_ONLY_CONFLICT: -PolicyOnly cannot be combined with -Fix or -SkipClippy; run the requested gate shape explicitly'
}

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
# Gate 0 — D1 zero-test / zero-harness doctrine (#2037, #2042, #2045,
# #2153, #2154, #2164)
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
# modules, zero `[dev-dependencies]`, zero benches, zero FSV harness binaries,
# examples, or scripts.
# Verification is manual Full State Verification against a physical source of
# truth.
#
# By 2026-08-06 that invariant was false again and nothing had noticed. Four
# inline `#[cfg(test)] mod` blocks (#2037), two `[dev-dependencies]` sections
# (#2042) and two auto-discovered `*_fsv` bin targets (#2045) had all come back
# through ordinary feature commits. The invariant depended on memory, and memory
# is not a gate.
#
# Cargo's target discovery is wider than manifest tables: `examples/*.rs`,
# `tests/*.rs`, and `benches/*.rs` become executable targets without a manifest
# line. Gate 0 therefore reads manifests, the filesystem layout, and Cargo's own
# authoritative `metadata --no-deps` target inventory. JavaScript package
# managers likewise make every package script directly invocable and expose
# direct runner dependencies as commands. Gate 0 therefore parses every
# package.json plus the root workspace records of every text bun.lock; it does
# not confuse transitive Storybook implementation packages with a repository-
# owned test entry point. Metadata parses target
# declarations without compiling or executing them; this is structural policy
# verification, never behavioral verification.
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

Write-Gate 'Gate 0     D1 zero-test / zero-harness doctrine (#2037, #2042, #2045, #2153, #2154, #2164)'
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

    # --- 0b/0c: forbidden manifest sections and FSV executable targets ------
    $devDepRe = [regex]'^\s*\[\s*(?:[A-Za-z0-9_."''\-\*\(\)= ]*\.)?dev[-_]dependencies\s*\]'
    $testTargetRe = [regex]'^\s*\[\[\s*(test|bench)\s*\]\]'
    $devDepHits = [System.Collections.Generic.List[string]]::new()
    $testTargetHits = [System.Collections.Generic.List[string]]::new()
    $fsvTargetHits = [System.Collections.Generic.List[string]]::new()
    $fsvTargetSourceHits = [System.Collections.Generic.List[string]]::new()
    $testBenchSourceHits = [System.Collections.Generic.List[string]]::new()
    $fsvScriptHits = [System.Collections.Generic.List[string]]::new()
    $packageScriptHits = [System.Collections.Generic.List[string]]::new()
    $packageRunnerDependencyHits = [System.Collections.Generic.List[string]]::new()
    $packageExecutablePathHits = [System.Collections.Generic.List[string]]::new()
    $packageManifestInvalidHits = [System.Collections.Generic.List[string]]::new()
    $packageLockInvalidHits = [System.Collections.Generic.List[string]]::new()
    $unsupportedPackageLockHits = [System.Collections.Generic.List[string]]::new()
    $manifests = Get-D1Files -Root $RepoRoot -Names @('Cargo.toml')
    foreach ($file in $manifests) {
        $lines = [IO.File]::ReadAllLines($file)
        for ($i = 0; $i -lt $lines.Count; $i++) {
            $line = $lines[$i]
            if ($devDepRe.IsMatch($line)) {
                $devDepHits.Add("        $(& $RelativeTo $file):$($i + 1)  $($line.Trim())")
            }
            if ($testTargetRe.IsMatch($line)) {
                $testTargetHits.Add("        $(& $RelativeTo $file):$($i + 1)  $($line.Trim())")
            }
        }
    }

    # Cargo owns target semantics, so Cargo's parsed inventory is the authority
    # for what is executable. This catches auto-discovered targets and explicit
    # TOML targets regardless of quoting, key order, whitespace, or path shape;
    # the old regex parser could be bypassed by valid single-quoted TOML.
    $metadataSpecs = @(
        [pscustomobject]@{ Label = 'root'; Manifest = Join-Path $RepoRoot 'Cargo.toml' },
        [pscustomobject]@{ Label = 'calyx'; Manifest = Join-Path $CalyxRoot 'Cargo.toml' }
    )
    $cargoTargetsScanned = 0
    foreach ($spec in $metadataSpecs) {
        $metadataRaw = (& cargo metadata --manifest-path $spec.Manifest --no-deps `
                --format-version 1 --locked --offline 2>&1 | Out-String)
        if ($LASTEXITCODE -ne 0) {
            throw "SYNAPSE_LINT_D1_CARGO_METADATA_FAILED: $($spec.Label) cargo metadata exited $LASTEXITCODE; $($metadataRaw.Trim())"
        }
        try {
            $metadata = $metadataRaw | ConvertFrom-Json -ErrorAction Stop
        }
        catch {
            throw "SYNAPSE_LINT_D1_CARGO_METADATA_INVALID: $($spec.Label) cargo metadata returned invalid JSON; $($_.Exception.Message)"
        }
        foreach ($package in $metadata.packages) {
            foreach ($target in $package.targets) {
                $cargoTargetsScanned++
                $kinds = @($target.kind)
                if (($kinds -contains 'test') -or ($kinds -contains 'bench')) {
                    $testTargetHits.Add("        $($spec.Label):$($package.name):$($target.name)  cargo kind=$($kinds -join ',') source=$($target.src_path)")
                }
                if ((($kinds -contains 'bin') -or ($kinds -contains 'example')) -and
                    ($target.name -match '(^|[_\-])fsv([_\-.]|$)' -or
                    $target.src_path -match '(^|[\\/_\-])fsv([_\.\\/\-]|$)')) {
                    $fsvTargetHits.Add("        $($spec.Label):$($package.name):$($target.name)  cargo kind=$($kinds -join ',') source=$($target.src_path)")
                }
            }
        }
    }

    # Auto-discovered targets: Cargo treats src/bin/*.rs and examples/*.rs as
    # executable targets, and tests/*.rs / benches/*.rs as forbidden behavioral
    # targets, even when no manifest mentions them. #2153 proved that checking
    # only src/bin reproduced the same omission for 118 example targets.
    foreach ($root in $D1RustRoots) {
        foreach ($file in Get-D1Files -Root (Join-Path $RepoRoot $root) -Extensions @('.rs')) {
            $relative = & $RelativeTo $file
            $stem = [System.IO.Path]::GetFileNameWithoutExtension($file)
            if ($file -match '[\\/](tests|benches)[\\/]') {
                $testBenchSourceHits.Add("        $relative  (autodiscovered $($Matches[1]) target)")
            }
            if ($file -match '[\\/](src[\\/]bin|examples)[\\/]' -and
                $stem -match '(^|[_\-])fsv([_\-]|$)') {
                $targetSurface = if ($file -match '[\\/]examples[\\/]') { 'example' } else { 'src/bin' }
                $fsvTargetSourceHits.Add("        $relative  ($targetSurface executable source)")
            }
        }
    }

    # D1 also forbids script-based FSV drivers. Scan executable source names,
    # never their output and never the historical Markdown evidence corpus.
    $scriptExtensions = @('.ps1', '.psm1', '.sh', '.bash', '.py', '.rb', '.pl',
        '.lua', '.js', '.mjs', '.cjs', '.ts', '.cmd', '.bat', '.go', '.cs',
        '.c', '.cc', '.cpp')
    foreach ($file in Get-D1Files -Root $RepoRoot -Extensions $scriptExtensions) {
        $stem = [System.IO.Path]::GetFileNameWithoutExtension($file)
        if ($stem -match '(^|[_\-])fsv([_\-]|$)') {
            $fsvScriptHits.Add("        $(& $RelativeTo $file)  (executable FSV driver source)")
        }
    }

    # Package scripts are executable surface, not descriptive metadata. Parse
    # manifests instead of grepping JSON so whitespace, key order, and escaped
    # command strings cannot bypass the policy. Dependency matching is limited
    # to direct manifest/workspace-root entries: Storybook may carry internal
    # testing libraries transitively without creating a repository-owned test
    # command, while a direct runner dependency does create one.
    $testScriptNameRe = [regex]'(?i)^(?:pre|post)?(?:test(?:[:._-].*)?|bench(?:mark)?(?:[:._-].*)?|coverage(?:[:._-].*)?)$'
    $testRunnerCommandRe = [regex]'(?i)(?:^|[\s;&|])(?:(?:bunx|npx)\s+)?(?:playwright\s+test|vitest(?:\s|$)|jest(?:\s|$)|mocha(?:\s|$)|ava(?:\s|$)|tap(?:\s|$)|tape(?:\s|$)|jasmine(?:\s|$)|cypress\s+(?:run|open)|node\s+--test|bun\s+test|cargo\s+(?:test|bench)|(?:npm|pnpm|yarn|bun)\s+(?:run\s+)?test(?=[:\s]|$)|storybook\s+test)'
    $testRunnerDependencyRe = [regex]'(?i)^(?:@playwright/test|@axe-core/playwright|playwright|playwright-core|@storybook/test-runner|vitest|@vitest/.+|jest|jest-.+|@jest/.+|mocha|ava|tap|tape|jasmine|jasmine-core|cypress|@testing-library/.+)$'
    $packageDependencySections = @('dependencies', 'devDependencies', 'optionalDependencies', 'peerDependencies')
    $packageManifests = Get-D1Files -Root $RepoRoot -Names @('package.json')
    $packageManifestsScanned = 0
    foreach ($file in $packageManifests) {
        $packageManifestsScanned++
        try {
            $manifest = [IO.File]::ReadAllText($file) | ConvertFrom-Json -AsHashtable -ErrorAction Stop
        }
        catch {
            $packageManifestInvalidHits.Add("        $(& $RelativeTo $file)  invalid JSON: $($_.Exception.Message)")
            continue
        }
        if ($manifest.ContainsKey('scripts')) {
            if ($manifest.scripts -isnot [System.Collections.IDictionary]) {
                $packageManifestInvalidHits.Add("        $(& $RelativeTo $file)  scripts must be a JSON object")
            }
            else {
                foreach ($entry in $manifest.scripts.GetEnumerator()) {
                    $name = [string]$entry.Key
                    $command = [string]$entry.Value
                    if ($testScriptNameRe.IsMatch($name) -or $testRunnerCommandRe.IsMatch($command)) {
                        $packageScriptHits.Add("        $(& $RelativeTo $file)  script=$name command=$command")
                    }
                }
            }
        }
        foreach ($section in $packageDependencySections) {
            if (-not $manifest.ContainsKey($section)) { continue }
            if ($manifest[$section] -isnot [System.Collections.IDictionary]) {
                $packageManifestInvalidHits.Add("        $(& $RelativeTo $file)  $section must be a JSON object")
                continue
            }
            foreach ($name in $manifest[$section].Keys) {
                if ($testRunnerDependencyRe.IsMatch([string]$name)) {
                    $packageRunnerDependencyHits.Add("        $(& $RelativeTo $file)  $section.$name=$($manifest[$section][$name])")
                }
            }
        }
    }

    # Bun's text lock is JSON with trailing commas, which PowerShell's parser
    # accepts. Inspect only direct workspace dependency maps; scanning every
    # transitive package would reject production authoring tools such as
    # Storybook merely because their internals reuse Vitest utilities.
    $bunLocks = Get-D1Files -Root $RepoRoot -Names @('bun.lock')
    $packageLockWorkspacesScanned = 0
    foreach ($file in $bunLocks) {
        try {
            $lock = [IO.File]::ReadAllText($file) | ConvertFrom-Json -AsHashtable -ErrorAction Stop
        }
        catch {
            $packageLockInvalidHits.Add("        $(& $RelativeTo $file)  invalid Bun text lock: $($_.Exception.Message)")
            continue
        }
        if (-not $lock.ContainsKey('workspaces') -or $lock.workspaces -isnot [System.Collections.IDictionary]) {
            $packageLockInvalidHits.Add("        $(& $RelativeTo $file)  missing object-valued workspaces map")
            continue
        }
        foreach ($workspace in $lock.workspaces.GetEnumerator()) {
            $packageLockWorkspacesScanned++
            if ($workspace.Value -isnot [System.Collections.IDictionary]) {
                $packageLockInvalidHits.Add("        $(& $RelativeTo $file)  workspace '$($workspace.Key)' must be an object")
                continue
            }
            foreach ($section in $packageDependencySections) {
                if (-not $workspace.Value.ContainsKey($section)) { continue }
                if ($workspace.Value[$section] -isnot [System.Collections.IDictionary]) {
                    $packageLockInvalidHits.Add("        $(& $RelativeTo $file)  workspace '$($workspace.Key)' $section must be an object")
                    continue
                }
                foreach ($name in $workspace.Value[$section].Keys) {
                    if ($testRunnerDependencyRe.IsMatch([string]$name)) {
                        $packageRunnerDependencyHits.Add("        $(& $RelativeTo $file)  workspace='$($workspace.Key)' $section.$name=$($workspace.Value[$section][$name])")
                    }
                }
            }
        }
    }

    # Fail closed when a future package manager introduces a lock format this
    # gate does not parse. Silent non-coverage is how #2164 survived.
    foreach ($name in @('bun.lockb', 'package-lock.json', 'npm-shrinkwrap.json', 'pnpm-lock.yaml', 'yarn.lock')) {
        foreach ($file in Get-D1Files -Root $RepoRoot -Names @($name)) {
            $unsupportedPackageLockHits.Add("        $(& $RelativeTo $file)  unsupported package lock format")
        }
    }

    $packageSourceExtensions = @('.js', '.jsx', '.mjs', '.cjs', '.ts', '.tsx', '.mts', '.cts')
    $testSourceLeafRe = [regex]'(?i)(?:^|[-_.])(?:test|tests|spec|coverage|benchmark|bench)(?:[-_.]|$)'
    $testConfigLeafRe = [regex]'(?i)^(?:playwright(?:\.[^.]+)*|vitest|jest|cypress)\.config\.(?:js|jsx|mjs|cjs|ts|tsx|mts|cts)$'
    foreach ($file in Get-D1Files -Root $RepoRoot -Extensions $packageSourceExtensions) {
        $relative = (& $RelativeTo $file).Replace('\', '/')
        $leaf = Split-Path -Leaf $file
        if ($relative -match '/(?:test|tests|__tests__|spec|specs)/' -or
            $testSourceLeafRe.IsMatch($leaf) -or $testConfigLeafRe.IsMatch($leaf)) {
            $packageExecutablePathHits.Add("        $relative  (package test/config executable source)")
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
    if ($testBenchSourceHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_TEST_BENCH_SOURCE_PRESENT' `
        ("$($testBenchSourceHits.Count) auto-discovered tests/ or benches/ source(s), which directive D1 deleted repo-wide:" + [Environment]::NewLine + ($testBenchSourceHits -join [Environment]::NewLine)) `
            'delete the tests/ or benches/ source and verify behavior manually against its physical source of truth.'
    }
    if ($fsvTargetHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_FSV_EXECUTABLE_TARGET_PRESENT' `
        ("$($fsvTargetHits.Count) Cargo-inventoried FSV binary/example target(s), which directive D1 forbids (#2045, #2153):" + [Environment]::NewLine + ($fsvTargetHits -join [Environment]::NewLine)) `
            'delete the executable target and every dependency/support surface reached only by it; manual FSV must trigger the real production surface and independently read its physical source of truth.'
    }
    if ($fsvTargetSourceHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_FSV_EXECUTABLE_SOURCE_PRESENT' `
        ("$($fsvTargetSourceHits.Count) FSV-named binary/example source(s), which directive D1 forbids even if Cargo auto-discovery is disabled:" + [Environment]::NewLine + ($fsvTargetSourceHits -join [Environment]::NewLine)) `
            'delete the executable source; disabling Cargo target discovery does not turn an automated FSV driver into an allowed repository artifact.'
    }
    if ($fsvScriptHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_FSV_SCRIPT_DRIVER_PRESENT' `
        ("$($fsvScriptHits.Count) executable FSV script driver(s), which directive D1 forbids (#2154):" + [Environment]::NewLine + ($fsvScriptHits -join [Environment]::NewLine)) `
            'delete the driver and perform the trigger/readback sequence manually through the real production surface.'
    }
    if ($packageManifestInvalidHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_PACKAGE_MANIFEST_INVALID' `
        ("$($packageManifestInvalidHits.Count) package manifest(s) could not be structurally inventoried:" + [Environment]::NewLine + ($packageManifestInvalidHits -join [Environment]::NewLine)) `
            'repair the JSON/object shape. Gate 0 will not claim the package surface is test-free when it cannot parse the manifest.'
    }
    if ($packageLockInvalidHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_PACKAGE_LOCK_INVALID' `
        ("$($packageLockInvalidHits.Count) Bun lock workspace surface(s) could not be structurally inventoried:" + [Environment]::NewLine + ($packageLockInvalidHits -join [Environment]::NewLine)) `
            'regenerate the text lock with bun install --lockfile-only, then re-run Gate 0. An unreadable lock is not accepted as dependency absence.'
    }
    if ($unsupportedPackageLockHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_PACKAGE_LOCK_UNSUPPORTED' `
        ("$($unsupportedPackageLockHits.Count) package lockfile(s) use an unparsed format:" + [Environment]::NewLine + ($unsupportedPackageLockHits -join [Environment]::NewLine)) `
            'use the repository-standard text bun.lock, or extend Gate 0 with a structural parser for the new lock format in the same change. Do not add an uninspected dependency surface.'
    }
    if ($packageScriptHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_PACKAGE_TEST_SCRIPT_PRESENT' `
        ("$($packageScriptHits.Count) executable automated-test/benchmark package script(s) remain:" + [Environment]::NewLine + ($packageScriptHits -join [Environment]::NewLine)) `
            'delete the package script and its runner/config/source/dependency surface. UI acceptance is manual Synapse FSV in the real application.'
    }
    if ($packageRunnerDependencyHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_PACKAGE_TEST_RUNNER_DEPENDENCY_PRESENT' `
        ("$($packageRunnerDependencyHits.Count) direct automated-test runner/support dependency entry or lock workspace entry remains:" + [Environment]::NewLine + ($packageRunnerDependencyHits -join [Environment]::NewLine)) `
            'remove the direct runner dependency and regenerate its lockfile. Do not replace it with another automated runner or mock surface.'
    }
    if ($packageExecutablePathHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_D1_PACKAGE_TEST_SOURCE_PRESENT' `
        ("$($packageExecutablePathHits.Count) JavaScript/TypeScript test, coverage, benchmark, or runner-config source path(s) remain:" + [Environment]::NewLine + ($packageExecutablePathHits -join [Environment]::NewLine)) `
            'delete the automated source/config and manually verify the real production trigger plus a separate physical Source-of-Truth readback.'
    }
    if ($devDepHits.Count -eq 0 -and $testTargetHits.Count -eq 0 -and
        $testBenchSourceHits.Count -eq 0 -and $fsvTargetHits.Count -eq 0 -and
        $fsvTargetSourceHits.Count -eq 0 -and $fsvScriptHits.Count -eq 0 -and
        $packageManifestInvalidHits.Count -eq 0 -and $packageLockInvalidHits.Count -eq 0 -and
        $unsupportedPackageLockHits.Count -eq 0 -and $packageScriptHits.Count -eq 0 -and
        $packageRunnerDependencyHits.Count -eq 0 -and $packageExecutablePathHits.Count -eq 0) {
        Write-Host "   OK   $($manifests.Count) Cargo manifests, $cargoTargetsScanned Cargo targets, $packageManifestsScanned package manifests, $packageLockWorkspacesScanned Bun lock workspaces, and executable source paths expose no automated-test/bench/FSV surface" -ForegroundColor Green
    }
}
catch {
    Add-Failure 'SYNAPSE_LINT_D1_SWEEP_FAILED' $_.Exception.Message `
        'the doctrine sweep itself failed; this gate fails closed rather than reporting an invariant it did not check'
}

# ---------------------------------------------------------------------------
# Gate 0b — production tool-surface invariants
# ---------------------------------------------------------------------------
#
# `storage_put_probe_rows` is an intentionally debug-gated raw diagnostic. It
# escaped through the always-public `storage` facade twice (#1595), including
# after the first fix, because compilation cannot distinguish a legitimate
# debug route from an illegitimate public projection. Keep that distinction as
# a cheap source invariant checked on every push.

Write-Gate 'Gate 0b    production tool-surface invariants (#1595)'
try {
    $probeFacadeFiles = @(
        'crates/synapse-mcp/src/server/operational_facades/types.rs',
        'crates/synapse-mcp/src/server/operational_facades/validation.rs',
        'crates/synapse-mcp/src/server/operational_facades/response.rs',
        'crates/synapse-mcp/src/server/operational_facades/storage.rs'
    )
    $probeFacadeHits = [System.Collections.Generic.List[string]]::new()
    foreach ($relative in $probeFacadeFiles) {
        $path = Join-Path $RepoRoot $relative
        if (-not (Test-Path -LiteralPath $path)) {
            throw "SYNAPSE_LINT_PUBLIC_SURFACE_SOURCE_MISSING: required facade source is absent: $relative"
        }
        $lines = [IO.File]::ReadAllLines($path)
        for ($i = 0; $i -lt $lines.Count; $i++) {
            if ($lines[$i] -match 'put_probe_rows') {
                $probeFacadeHits.Add("        $relative`:$($i + 1)  $($lines[$i].Trim())")
            }
        }
    }
    $profilePath = Join-Path $RepoRoot 'crates/synapse-mcp/src/server/tool_profiles.rs'
    if (-not (Test-Path -LiteralPath $profilePath)) {
        throw 'SYNAPSE_LINT_PUBLIC_SURFACE_SOURCE_MISSING: required tool profile source is absent: crates/synapse-mcp/src/server/tool_profiles.rs'
    }
    $profileLines = [IO.File]::ReadAllLines($profilePath)
    for ($i = 0; $i -lt $profileLines.Count; $i++) {
        if ($profileLines[$i] -match '"put_probe_rows"') {
            $probeFacadeHits.Add("        crates/synapse-mcp/src/server/tool_profiles.rs`:$($i + 1)  $($profileLines[$i].Trim())")
        }
    }
    if ($probeFacadeHits.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_SYNTHETIC_WRITER_PUBLIC' `
        ("$($probeFacadeHits.Count) production storage-facade projection(s) expose the debug-only synthetic writer (#1595):" + [Environment]::NewLine + ($probeFacadeHits -join [Environment]::NewLine)) `
            'remove put_probe_rows from StorageOperation, StorageParams, StorageResponse, facade validation/dispatch, and the public storage operation contract. Keep storage_put_probe_rows only behind SYNAPSE_DEBUG_TOOLS.'
    }
    else {
        Write-Host '   OK   production storage facade has no put_probe_rows projection; raw diagnostic remains independently debug-gated' -ForegroundColor Green
    }
}
catch {
    Add-Failure 'SYNAPSE_LINT_PUBLIC_SURFACE_SWEEP_FAILED' $_.Exception.Message `
        'repair the source inventory or gate implementation; the public tool surface cannot be certified when its structural sweep did not complete'
}

# ---------------------------------------------------------------------------
# Gate 0c — authenticated Chrome error/body-budget contract (#2217/#2219)
# ---------------------------------------------------------------------------
#
# The extension and daemon are different languages joined by string-valued
# machine error identifiers. A missing Rust allowlist arm used to collapse the
# extension's precise CAPTURE_PLAN_EXCEEDS_LIMIT into A11Y_CDP_ATTACH_FAILED.
# This is a structural contract check, not a behavioural test: compare the two
# source registries, require one sorted occurrence of each value, require every
# value in synapse_core::error_codes, and reject any code-shaped extension
# literal that is absent from the public command registry. Runtime enforcement
# independently turns any missing/dynamic code into an explicit contract error.

Write-Gate 'Gate 0c    authenticated Chrome error/body-budget contract (#2217/#2219)'
try {
    $contractFailureCountBefore = $script:Failures.Count
    $workerPath = Join-Path $RepoRoot 'extensions/synapse-chrome-debugger/service_worker.js'
    $bridgePath = Join-Path $RepoRoot 'crates/synapse-chrome-bridge/src/lib.rs'
    $coreErrorPath = Join-Path $RepoRoot 'crates/synapse-core/src/error_codes.rs'
    foreach ($path in @($workerPath, $bridgePath, $coreErrorPath)) {
        if (-not (Test-Path -LiteralPath $path)) {
            throw "SYNAPSE_LINT_CHROME_ERROR_CONTRACT_SOURCE_MISSING: $path"
        }
    }

    function Get-ChromeErrorContractValues {
        param([string]$Path)
        $lines = [IO.File]::ReadAllLines($Path)
        $begin = -1
        $end = -1
        for ($i = 0; $i -lt $lines.Count; $i++) {
            if ($lines[$i] -match '>>> SHARED-CHROME-ERROR-CODE-CONTRACT') {
                if ($begin -ge 0) { throw "duplicate contract begin marker in $Path" }
                $begin = $i
            }
            if ($lines[$i] -match '<<< SHARED-CHROME-ERROR-CODE-CONTRACT <<<') {
                if ($end -ge 0) { throw "duplicate contract end marker in $Path" }
                $end = $i
            }
        }
        if ($begin -lt 0 -or $end -le $begin) {
            throw "missing or inverted SHARED-CHROME-ERROR-CODE-CONTRACT markers in $Path"
        }
        $values = [System.Collections.Generic.List[string]]::new()
        for ($i = $begin + 1; $i -lt $end; $i++) {
            $match = [regex]::Match($lines[$i], '^\s*"([A-Z][A-Z0-9_]+)",?\s*$')
            if (-not $match.Success) {
                throw "unparseable Chrome error contract line $Path`:$($i + 1): $($lines[$i])"
            }
            $values.Add($match.Groups[1].Value)
        }
        if ($values.Count -eq 0) { throw "empty Chrome error contract in $Path" }
        return $values
    }

    function Get-ChromeNativeMessageBudgetValues {
        param([string]$Path)
        $lines = [IO.File]::ReadAllLines($Path)
        $begin = -1
        $end = -1
        for ($i = 0; $i -lt $lines.Count; $i++) {
            if ($lines[$i] -match '>>> SHARED-CHROME-NATIVE-MESSAGE-BUDGET-CONTRACT') {
                if ($begin -ge 0) { throw "duplicate native-message budget contract begin marker in $Path" }
                $begin = $i
            }
            if ($lines[$i] -match '<<< SHARED-CHROME-NATIVE-MESSAGE-BUDGET-CONTRACT <<<') {
                if ($end -ge 0) { throw "duplicate native-message budget contract end marker in $Path" }
                $end = $i
            }
        }
        if ($begin -lt 0 -or $end -le $begin) {
            throw "missing or inverted SHARED-CHROME-NATIVE-MESSAGE-BUDGET-CONTRACT markers in $Path"
        }
        $values = [System.Collections.Generic.List[string]]::new()
        for ($i = $begin + 1; $i -lt $end; $i++) {
            $match = [regex]::Match(
                $lines[$i],
                '^\s*(?:pub\s+)?const\s+([A-Z][A-Z0-9_]+)(?:\s*:\s*(?:usize|u64))?\s*=\s*(\d+);\s*$'
            )
            if (-not $match.Success) {
                throw "unparseable Chrome native-message budget contract line $Path`:$($i + 1): $($lines[$i])"
            }
            $values.Add("$($match.Groups[1].Value)=$($match.Groups[2].Value)")
        }
        return $values
    }

    $workerContract = @(Get-ChromeErrorContractValues -Path $workerPath)
    $bridgeContract = @(Get-ChromeErrorContractValues -Path $bridgePath)
    $workerJoined = $workerContract -join "`n"
    $bridgeJoined = $bridgeContract -join "`n"
    if ($workerJoined -ne $bridgeJoined) {
        $onlyWorker = @($workerContract | Where-Object { $_ -notin $bridgeContract })
        $onlyBridge = @($bridgeContract | Where-Object { $_ -notin $workerContract })
        Add-Failure 'SYNAPSE_LINT_CHROME_ERROR_CONTRACT_DIVERGED' `
            "extension and daemon trusted error registries differ; extension_only=$($onlyWorker -join ',') daemon_only=$($onlyBridge -join ',')" `
            'update PUBLIC_COMMAND_ERROR_CODES and TRUSTED_EXTENSION_ERROR_CODES together, add every value to synapse_core::error_codes, and bump the bridge build ID/SHA'
    }

    $workerBudgets = @(Get-ChromeNativeMessageBudgetValues -Path $workerPath)
    $bridgeBudgets = @(Get-ChromeNativeMessageBudgetValues -Path $bridgePath)
    $expectedBudgets = @(
        'NATIVE_MESSAGE_HTTP_BODY_LIMIT_MIB=64',
        'PAGE_SCREENSHOT_NATIVE_MESSAGE_BUDGET_MIB=60'
    )
    if (($workerBudgets -join "`n") -ne ($bridgeBudgets -join "`n")) {
        Add-Failure 'SYNAPSE_LINT_CHROME_NATIVE_MESSAGE_BUDGET_DIVERGED' `
            "extension and daemon native-message budget contracts differ; extension=$($workerBudgets -join ',') daemon=$($bridgeBudgets -join ',')" `
            'keep the exact HTTP envelope limit and screenshot payload budget synchronized across service_worker.js and synapse-chrome-bridge/src/lib.rs, preserving explicit envelope headroom'
    }
    if (($workerBudgets -join "`n") -ne ($expectedBudgets -join "`n")) {
        Add-Failure 'SYNAPSE_LINT_CHROME_NATIVE_MESSAGE_BUDGET_INVALID' `
            "native-message budget contract must retain the documented 64 MiB body ceiling and 60 MiB screenshot payload ceiling; actual=$($workerBudgets -join ',')" `
            'restore NATIVE_MESSAGE_HTTP_BODY_LIMIT_MIB=64 and PAGE_SCREENSHOT_NATIVE_MESSAGE_BUDGET_MIB=60 in both languages; the 4 MiB difference is required envelope/evidence headroom'
    }

    $sortedUnique = @($workerContract | Sort-Object -Unique)
    if (($sortedUnique -join "`n") -ne $workerJoined) {
        Add-Failure 'SYNAPSE_LINT_CHROME_ERROR_CONTRACT_UNSORTED_OR_DUPLICATED' `
            'PUBLIC_COMMAND_ERROR_CODES must contain each code exactly once in ordinal sorted order so source diffs and binary search remain authoritative' `
            'sort the registry values, remove duplicates, and copy the exact sequence to the Rust trusted registry'
    }

    $coreText = [IO.File]::ReadAllText($coreErrorPath)
    $coreValues = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($match in [regex]::Matches(
            $coreText,
            'pub\s+const\s+[A-Z][A-Z0-9_]*\s*:\s*&str\s*=\s*"([A-Z][A-Z0-9_]+)"\s*;',
            [Text.RegularExpressions.RegexOptions]::Singleline)) {
        [void]$coreValues.Add($match.Groups[1].Value)
    }
    $missingCore = @($workerContract | Where-Object { -not $coreValues.Contains($_) })
    if ($missingCore.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_CHROME_ERROR_CONTRACT_CORE_MISSING' `
            "registered Chrome command error code(s) have no synapse_core::error_codes definition: $($missingCore -join ',')" `
            'define each exact machine identifier in crates/synapse-core/src/error_codes.rs; error identities are shared protocol, not bridge-local prose'
    }

    $workerText = [IO.File]::ReadAllText($workerPath)
    $codeLiterals = @([regex]::Matches(
            $workerText,
            '"([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)"') |
        ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique)
    $unregistered = @($codeLiterals | Where-Object { $_ -notin $workerContract })
    if ($unregistered.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_CHROME_ERROR_LITERAL_UNREGISTERED' `
            "extension source contains code-shaped machine identifier(s) outside PUBLIC_COMMAND_ERROR_CODES: $($unregistered -join ',')" `
            'classify each identifier explicitly: register real command/readback errors across all three contract surfaces, or stop encoding non-error data as an uppercase underscore-delimited error token'
    }

    if ($script:Failures.Count -eq $contractFailureCountBefore) {
        Write-Host "   OK   $($workerContract.Count) registered Chrome error codes and the 64/60 MiB native-message budget contract agree across JavaScript/Rust/synapse-core" -ForegroundColor Green
    }
}
catch {
    Add-Failure 'SYNAPSE_LINT_CHROME_ERROR_CONTRACT_SWEEP_FAILED' $_.Exception.Message `
        'repair the contract markers/parser inputs; canonical lint cannot certify machine error identity while the cross-language source contract is unreadable'
}

# ---------------------------------------------------------------------------
# Gate 0d -- setup source encoding and dual-parser contract (#2216)
# ---------------------------------------------------------------------------
#
# The public setup facade deliberately launches the Windows-inbox PowerShell
# 5.1 executable. That engine decodes BOM-less source through the active ANSI
# code page, while PowerShell 7 decodes the same bytes as UTF-8. In #2216 one
# UTF-8 em dash decoded as three Windows-1252 characters ending in a typographic
# quote. That last character closed a string, so the production repair child
# failed before setup could update its durable repair manifest while a
# PowerShell 7 parser check stayed green.
#
# Keep this shipping script 7-bit ASCII so its bytes mean the same thing under
# every Windows ANSI code page and UTF-8, then parse those physical bytes with
# both the current PowerShell engine and the exact Windows PowerShell launcher
# used by the MCP facade. This is a structural source gate only: it executes no
# setup behavior and is not FSV.

Write-Gate 'Gate 0d    setup ASCII and PowerShell 5.1/7 parser contract (#2216)'
try {
    $setupContractFailureCountBefore = $script:Failures.Count
    $setupPath = Join-Path $RepoRoot 'scripts/synapse-setup.ps1'
    if (-not (Test-Path -LiteralPath $setupPath -PathType Leaf)) {
        throw "SYNAPSE_LINT_SETUP_SOURCE_MISSING path=$setupPath"
    }

    $setupBytes = [IO.File]::ReadAllBytes($setupPath)
    $nonAsciiOffsets = [System.Collections.Generic.List[int]]::new()
    for ($i = 0; $i -lt $setupBytes.Length; $i++) {
        if ($setupBytes[$i] -gt 0x7f) {
            $nonAsciiOffsets.Add($i)
            if ($nonAsciiOffsets.Count -ge 16) { break }
        }
    }
    if ($nonAsciiOffsets.Count -gt 0) {
        Add-Failure 'SYNAPSE_LINT_SETUP_SOURCE_NOT_ASCII' `
            "scripts/synapse-setup.ps1 contains non-ASCII bytes; first_byte_offsets=$($nonAsciiOffsets -join ',')" `
            'replace non-ASCII source characters with exact ASCII equivalents; the shipping script must decode identically under Windows PowerShell ANSI code pages and PowerShell 7 UTF-8'
    }

    $tokens = $null
    $parseErrors = $null
    [void][Management.Automation.Language.Parser]::ParseFile(
        $setupPath,
        [ref]$tokens,
        [ref]$parseErrors
    )
    if ($parseErrors.Count -gt 0) {
        $diagnostics = @($parseErrors | ForEach-Object {
                '{0}:{1}:{2} {3}' -f $setupPath, $_.Extent.StartLineNumber, $_.Extent.StartColumnNumber, $_.Message
            })
        Add-Failure 'SYNAPSE_LINT_SETUP_POWERSHELL_CURRENT_PARSE_FAILED' `
            ($diagnostics -join [Environment]::NewLine) `
            'repair every reported syntax error in scripts/synapse-setup.ps1 under the current PowerShell engine'
    }

    $windowsPowerShell = if ([string]::IsNullOrWhiteSpace($env:SystemRoot)) {
        $null
    }
    else {
        Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
    }
    if ([string]::IsNullOrWhiteSpace($windowsPowerShell) -or
        -not (Test-Path -LiteralPath $windowsPowerShell -PathType Leaf)) {
        Add-Failure 'SYNAPSE_LINT_SETUP_WINDOWS_POWERSHELL_MISSING' `
            "the production setup-repair parser is unavailable; SystemRoot=$($env:SystemRoot) expected_path=$windowsPowerShell" `
            'run the structural gate on Windows with the inbox Windows PowerShell 5.1 installation intact; the shipping parser contract cannot be inferred from PowerShell 7'
    }
    else {
        $windowsParserCommand = @'
$path = $env:SYNAPSE_LINT_SETUP_PARSE_PATH
if ($PSVersionTable.PSVersion.Major -ne 5 -or $PSVersionTable.PSVersion.Minor -ne 1) {
    [Console]::Error.WriteLine(('expected Windows PowerShell 5.1, found {0}' -f $PSVersionTable.PSVersion))
    exit 2
}
$tokens = $null
$errors = $null
[void][Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
if ($errors.Count -gt 0) {
    foreach ($errorRecord in $errors) {
        [Console]::Error.WriteLine(
            ('{0}:{1}:{2} {3}' -f $path, $errorRecord.Extent.StartLineNumber, $errorRecord.Extent.StartColumnNumber, $errorRecord.Message)
        )
    }
    exit 1
}
exit 0
'@
        $encodedParserCommand = [Convert]::ToBase64String(
            [Text.Encoding]::Unicode.GetBytes($windowsParserCommand)
        )
        $priorParsePath = $env:SYNAPSE_LINT_SETUP_PARSE_PATH
        try {
            $env:SYNAPSE_LINT_SETUP_PARSE_PATH = $setupPath
            $windowsParseOutput = @(& $windowsPowerShell `
                    -NoLogo `
                    -NoProfile `
                    -NonInteractive `
                    -EncodedCommand $encodedParserCommand 2>&1)
            $windowsParseExitCode = $LASTEXITCODE
        }
        finally {
            $env:SYNAPSE_LINT_SETUP_PARSE_PATH = $priorParsePath
        }
        if ($windowsParseExitCode -ne 0) {
            Add-Failure 'SYNAPSE_LINT_SETUP_WINDOWS_POWERSHELL_PARSE_FAILED' `
                "Windows PowerShell parser exited $windowsParseExitCode`n$($windowsParseOutput -join [Environment]::NewLine)" `
                'repair every reported Windows PowerShell 5.1 syntax/encoding error in scripts/synapse-setup.ps1; do not substitute a PowerShell 7-only parser check'
        }
    }

    if ($script:Failures.Count -eq $setupContractFailureCountBefore) {
        Write-Host "   OK   $($setupBytes.Length) ASCII bytes parse under PowerShell $($PSVersionTable.PSVersion) and Windows PowerShell 5.1" -ForegroundColor Green
    }
}
catch {
    Add-Failure 'SYNAPSE_LINT_SETUP_PARSER_CONTRACT_SWEEP_FAILED' $_.Exception.Message `
        'repair the setup source inventory or dual-parser gate; canonical lint cannot certify the production setup launcher while this structural sweep is incomplete'
}

if ($PolicyOnly) {
    Write-Host ''
    if ($script:Failures.Count -eq 0) {
        Write-Host 'POLICY OK: Gate 0 found no automated-test/FSV-driver surface, Gate 0b found no forbidden public tool projection, Gate 0c proved the Chrome error/body-budget contract, and Gate 0d proved the setup ASCII/dual-parser contract.' -ForegroundColor Green
        exit 0
    }

    Write-Host "POLICY FAILED — $($script:Failures.Count) Gate 0 failure(s):" -ForegroundColor Red
    foreach ($failure in $script:Failures) {
        Write-Host ''
        Write-Host "  code        : $($failure.Code)" -ForegroundColor Red
        Write-Host "  detail      : $($failure.Detail)"
        Write-Host "  remediation : $($failure.Remediation)"
    }
    Write-Host ''
    exit 1
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
    # Denying only clippy::all in Cargo.toml leaves rustc, pedantic, nursery,
    # and future toolchain warnings non-fatal. The two workspaces pin the exact
    # same toolchain, so make the entire warning surface fail closed (#2175).
    $clippyArgs = @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
    [void](Invoke-CargoGate -WorkspaceLabel 'root' -WorkingDirectory $RepoRoot -CargoArgs $clippyArgs `
            -Code 'SYNAPSE_LINT_CLIPPY_ROOT_FAILED' -Remediation 'fix every reported rustc/Clippy warning at the source; do not lower severity or add a blanket allow. A narrow #[expect(lint, reason = "...")] is acceptable only for a named architectural invariant, and stale expectations are fatal.')
    # NOT `-p calyx-*` from the repo root: that compiles without linting, because
    # calyx crates are excluded path dependencies rather than workspace members.
    # This must run with calyx/ as the current directory (#1928).
    [void](Invoke-CargoGate -WorkspaceLabel 'calyx' -WorkingDirectory $CalyxRoot -CargoArgs $clippyArgs `
            -Code 'SYNAPSE_LINT_CLIPPY_CALYX_FAILED' -Remediation 'fix every reported rustc/Clippy warning at the source; do not lower severity or add a blanket allow. A narrow #[expect(lint, reason = "...")] is acceptable only for a named architectural invariant, and stale expectations are fatal.')
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
