# FSV — #2211/#2212 setup pure preflight and profile provenance

Date: 2026-08-10 (America/Chicago)  
Implementation: `81c3fa8b2599cb9213b37541350fa69a62c3e4d9` on `main`

Acceptance comments:

- #2211: <https://github.com/ChrisRoyse/Synapse/issues/2211#issuecomment-5245098376>
- #2212: <https://github.com/ChrisRoyse/Synapse/issues/2212#issuecomment-5245098794>

## Defects and first-principles diagnosis

Setup accepted all parameters, acquired setup state, built an optimized release,
and launched an isolated candidate before the first call to
`Get-SynapseNormalizedIssueRef`. A malformed issue reference is a property of
the input string, so no build or state mutation can add information needed to
validate it. The original `#2187,#2191` trigger consequently spent 1,025 seconds
before returning `SYNAPSE_ACTIVE_ISSUE_INVALID`.

The same ordering defect existed for source profiles. Candidate selection could
replace a missing
`<SourceDir>\crates\synapse-profiles\profiles` input with the deployed profile
directory from a previous release. That directory is output state, not a source
dependency of the current build, so the candidate could appear healthy while
the supplied checkout was incomplete.

The fix creates a pure-input/source-prerequisite fence at
`scripts/synapse-setup.ps1:14123`, before parent waiting, maintenance-lock
acquisition, cleanup, token mutation, build, or candidate launch. It validates
and normalizes the issue reference from either parameter or environment;
validates rollback flags and Calyx configuration; checks the source/Cargo/token
prerequisites; resolves the authorized target and hardware build capability; and
requires a nonempty source profile tree. The exact source profile path is then
carried to candidate validation (`:14873`) and manifest deployment (`:15215`).
Both deployed-profile fallback branches were removed.

## Independent best-practice research

Research occurred after diagnosis and before implementation.

- `scripts/check-research-lane.ps1` performed real MCP initialize, tools/list,
  and tools/call. The physical readback at
  `%TEMP%\synapse-research-lane-readback.json` was 1,626 bytes, SHA-256
  `C8C717E5AC342E21F11AB1D3A0E9964BB02AD437BEE02E2DC46E6B034347ABAA`,
  and reported Exa MCP `exa-search-server` 3.4.0 `live` with both search and
  fetch tools.
- Exa and the built-in web lane independently found the Microsoft contract that
  advanced-parameter validation rejects supplied invalid input before function
  invocation. Microsoft also documents that default values are not validated by
  parameter validation attributes, which is why the environment-backed default
  needs the explicit entry fence:
  <https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_functions_advanced_parameters?view=powershell-7.6>
- The same two lanes found the terminating-error contract used by the existing
  structured `Die` path:
  <https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_error_handling?view=powershell-7.6>
- Exa and built-in web independently found SLSA 1.2's provenance requirement
  that initialization/execution inputs be declared and its verification advice
  to reject unexpected source/build inputs. A stale deployed profile tree is
  therefore not an acceptable substitute for current checkout input:
  <https://slsa.dev/spec/v1.2/build-provenance> and
  <https://slsa.dev/spec/v1.2/verifying-artifacts>.

## Sources of Truth

Acceptance used independent reads, not setup return values:

1. installed and release executable bytes on disk;
2. the Windows process table and TCP listener for the configured daemon;
3. `%LOCALAPPDATA%\synapse\setup-maintenance.lock.json`;
4. release/candidate/staging directory inventories and compiler process table;
5. setup's physical phase ledger and build invocation/target readbacks;
6. the source and deployed profile files plus the persisted bundled-profile
   manifest;
7. the authenticated `/health` endpoint, public MCP tools/list, setup status,
   active-issue handoff, and a full public `audit verify_chain` walk of the
   physical Calyx vault.

## Boundary and edge-case audit

Each trigger printed state before and then independently printed it after the
failure. `phase_count=0` and null Cargo jobs are reads from the physical setup
phase ledger, not timing inference.

| Trigger | Expected/observed error | Elapsed in script | Before → after Source of Truth |
|---|---|---:|---|
| Explicit composite `-ActiveIssue '#2211,#2100'` | `SYNAPSE_ACTIVE_ISSUE_INVALID` | 0.106 s | installed/release SHA unchanged at `2D5B46…`; lock SHA unchanged at `EFEF6750…`; PID 15976 unchanged; candidate and staging sets identical; `phase_count=0`; zero Cargo/rustc/candidate processes |
| Environment default `SYNAPSE_ACTIVE_ISSUE=not-an-issue` | `SYNAPSE_ACTIVE_ISSUE_INVALID` | 0.087 s | same executable, lock, PID, and directory state; `phase_count=0`; zero build processes |
| Valid URL `https://github.com/ChrisRoyse/Synapse/issues/2211?edge=valid` plus rollback pause mode without its probe | issue URL accepted, then `SYNAPSE_MANUAL_INSTALL_HEALTH_ROLLBACK_PAUSE_MODE_WITHOUT_PROBE` | 0.060 s | same executable, lock, PID, candidate/staging state; `phase_count=0`; zero build processes |
| Real Cargo workspace `C:\code\synapse\calyx` (Cargo.toml exists; Synapse source profiles do not), `-SkipBuild -ActiveIssue #2212` | `SYNAPSE_SOURCE_PROFILES_MISSING` naming the exact path and repair | 0.050 s | installed/release/lock hashes, PID 15976, and candidate/staging sets unchanged; `phase_count=0`; zero compiler/candidate processes |

These exercise explicit input, an environment-backed default, accepted boundary
syntax followed by the next validation, and an incomplete but real local source
workspace. No temporary source copy, mock data, test harness, branch, worktree,
or alternate target directory was created.

## Happy path: real optimized build, candidate, deployment, and readback

The real trigger was:

```powershell
pwsh -NoProfile -File scripts\synapse-setup.ps1 `
  -SourceDir C:\code\synapse -ForceRestart -EnableAudio `
  -AllowedPermissions 'READ_EVENTS,READ_REFLEX,WRITE_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE,READ_AUDIO' `
  -ActiveIssue 2211 -SkipClientWiring
```

The persisted `setup-phase-timings.json` reports `outcome=completed`, 10 phases,
971.209 seconds total, and 12 Cargo/CMake jobs for the host's 12 logical CPUs.
The release build completed in 820.565 seconds; isolated candidate validation
completed in 26.357 seconds using
`C:\code\synapse\crates\synapse-profiles\profiles`, count 26. The candidate
exposed 40 public tools, built a real search generation, shut down gracefully,
and its current candidate/staging directories were independently absent after
cleanup. No Cargo, rustc, clippy, or candidate process remained.

Hardware selection was recorded before mutation in
`setup-build-target.json`: canonical checkout target
`C:\code\synapse\target`, no alternate target footprint, 12-way parallelism,
and no NVIDIA PnP device/NVCC/CUDA path. Live Calyx health selected the CPU AVX2
SIMD path and its fixed-vector probe agreed bit-for-bit with the portable path.
Process QoS independently reports execution-speed throttling disabled. This is
the fastest verified native path available on this i7-1355U / Intel Iris Xe
host without pretending a CUDA device exists.

### Executable and live process

The independent after-state was:

```text
release  C:\code\synapse\target\release\synapse-mcp.exe
install  C:\Users\hotra\.cargo\bin\synapse-mcp.exe
length   257759561 bytes (both)
SHA-256  E6AEB9CC91949B4ED1DAB9EAAEF726F9C8ABB1999186A8FFCBC6F71476B23ABA (both)
mtime    2026-08-10T19:31:40.4037999Z (both)
match    true
```

The Windows TCP table independently named PID 3268 as the listener on
`127.0.0.1:7700`. CIM read PID 3268 executing the installed path with the
requested audio flag and exact permission list. Authenticated health returned
`ok=true`, build `81c3fa8b2599`, PID 3268, clean `refs/heads/main` provenance,
1,451 build input files, zero changed inputs, and
`build_matches_checkout=true`. The public MCP session independently listed the
expected 40 facade tools; `setup operation=status` read PID 3268 and the durable
run file.

The maintenance lock was separately read after setup:

```text
path      %LOCALAPPDATA%\synapse\setup-maintenance.lock.json
length    874
SHA-256   7D8B3487CC40AA53882DBF68C6CC7C41BD9CD57E996A0FB77077AD74202A8789
state     released
reason    setup
released  2026-08-10T19:34:15.3889357Z
```

### Active issue persistence

The independently read handoff is
`codex-restart-handoff-22476-20260810T193415103Z.json`, 5,651 bytes, SHA-256
`EEB15BB8D217736BD444772D2D092A355A71BA134DDBABBC45D5E4B4AFECFE7D`.
It stores `issue_ref=#2211`, `issue_number=2211`, `status=provided`, and daemon
PID 3268 with the same 40-tool facade hash. This proves a valid input reached
and survived the final durable Source of Truth in normalized form.

### Profile provenance and physical bytes

Before deploy, the checkout contained 26 tracked TOMLs. After deploy, all 26
source relative paths existed in the installed directory and every length/hash
matched. The deployed directory contains 29 TOMLs because three user profiles
already existed. The physical manifest
`C:\Users\hotra\.cargo\bin\profiles\.synapse-bundled-profiles.manifest.json`
is 6,222 bytes, SHA-256
`8AA4EF93C235416C48CD659F933842768909B177DAA8142FEC6585C55E09D167`, and records:

```text
source_profiles_dir       C:\code\synapse\crates\synapse-profiles\profiles
deployed_profiles_dir     C:\Users\hotra\.cargo\bin\profiles
bundled_file_count        26
bundled_profile_count     26
source/deployed mismatch  []
quarantined_retired       []
preserved custom          everquest.live.toml, luanti.minetest.toml, minecraft.java.toml
legacy hash mismatches    []
```

Thus the candidate and installed bundled profiles came from this checkout; the
three additional files are explicitly classified and hash-recorded custom state,
not borrowed source or a concealed fallback.

### Durable vault integrity

A fresh authenticated MCP session called
`audit operation=verify_chain verify_chain={}`. The daemon independently
re-read and re-hashed the physical `CF_LEDGER` and raw-commitment cohorts:

```text
verdict/intact                  intact / true
entry_count/head_height        343363 / 343363
verified range                 0..343363
tip_hash                       eba52374186bddfe0767a35cdc7e6380e44275481bf140af5646c53370a07908
raw commitments                965732 total; 965729 sealed; 3 pending checkpoint tail
raw_commitments_intact         true
chain origin/history           lineage-seeded / partial-journal-seeded-over-pre-existing-vault
vault resets                   0
```

`covers_full_history=false` is the recorded lineage-seeded origin of this
pre-existing vault, not damage; the complete available chain and all sealed
commitments verified intact.

## Compile and lint gates

- PowerShell parser: zero errors.
- `cargo check --workspace`: pass, 54.95 seconds.
- Canonical `pwsh -File scripts/lint.ps1`: pass across the root and Calyx
  workspaces, 184.1 seconds.
- `git diff --check`: pass.

The early failures prove no expensive work or state change occurs for knowably
bad input. The full real deployment and independent disk/process/MCP/vault reads
prove valid setup remains operational and that profile provenance can no longer
silently depend on stale deployed files.
