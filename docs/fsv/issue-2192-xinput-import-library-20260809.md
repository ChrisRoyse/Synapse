# Issue #2192 — unused XInput import library

Date: 2026-08-09 (America/Chicago)

Code commit verified: `393724267a0bd2a74243a638f2d160f7be2187a1`

## Acceptance and Sources of Truth

The acceptance Sources of Truth were:

1. `%LOCALAPPDATA%\synapse\logs\setup-build.log`, produced by the real
   optimized setup build;
2. the PE import directories of the linked release binaries and a separately
   relinked debug example, read with `llvm-readobj --coff-imports`;
3. the installed `%USERPROFILE%\.cargo\bin\synapse-mcp.exe`, its SHA-256, and
   the Windows process table;
4. authenticated `/health`, the physical Calyx vault identity/sequence it
   reports, and `%APPDATA%\synapse\codex-tool-surface.json`;
5. the durable setup maintenance-lock record and Task Scheduler state.

Every trigger was followed by a separate file, PE, process, or HTTP read.
Compiler return values were not accepted as proof. No automated test, mock,
harness, alternate target directory, linker-warning suppression, or linker
fallback was used.

## Root cause

The warning was not an LLD false positive and did not originate in a Synapse
XInput call.

The released `vigem-client 0.1.4` imports
`winapi::um::xinput::XINPUT_GAMEPAD` only to expose ABI-compatible conversion
implementations. Its mandatory `winapi/xinput` feature nevertheless makes
winapi's build script emit `cargo:rustc-link-lib=dylib=xinput`. That adds the
Windows SDK `xinput.lib` to every final link even though no XInput function is
referenced.

Physical SDK inspection established that the host's
`Windows Kits\10\lib\10.0.26100.0\um\x64\xinput.lib` contains imported
`DllMain` and XInput members. LLD correctly scans the archive and warns while
skipping that imported DLL entry point. PE inspection established that the
old example and old installed release did not retain an XInput DLL import, so
the defect was an unnecessary native link input rather than a runtime XInput
dependency.

A controlled rebuild removed Synapse's separate, unused
`windows/Win32_UI_Input_XboxController` feature. The warning remained exactly
once, independently isolating `vigem-client -> winapi/xinput` as the source.

## Independent research after diagnosis

`pwsh -File scripts/check-research-lane.ps1` performed real Exa MCP
`initialize`, `tools/list`, and `tools/call` requests and reported Exa MCP 3.4.0
`live`. Exa was used as the supplemental lane. The built-in web lane also read
primary sources:

- LLVM added the `importeddllmain` warning deliberately because an import
  library's DLL entry point must not become the executable entry point:
  <https://github.com/llvm/llvm-project/pull/146610> and
  <https://github.com/llvm/llvm-project/pull/147152>.
- The current upstream crate remains `vigem-client 0.1.4`; its latest commit is
  the 2022 release and its manifest still enables `winapi/xinput`:
  <https://github.com/CasualX/vigem-client>.
- Upstream PR #6 replaces winapi with windows-sys and remains clean but
  unmerged: <https://github.com/CasualX/vigem-client/pull/6>.
- windows-rs documents feature-gated API selection:
  <https://github.com/microsoft/windows-rs/blob/master/docs/crates/windows.md>.
- Microsoft's XInput API documentation confirms that actual XInput function
  callers require the XInput import library:
  <https://learn.microsoft.com/en-us/windows/win32/api/xinput/nf-xinput-xinputgetstate>.

The issue explicitly prohibited `/ignore:importeddllmain`; research confirmed
that option only suppresses the diagnostic. Switching back to slower
`link.exe` would likewise hide the signal rather than remove the unnecessary
dependency.

## Implemented behavior

- Removed Synapse's unused `Win32_UI_Input_XboxController` windows-rs feature.
- Pinned the exact commit from upstream PR #6:
  `c24e58a258d56bc8cc68156face90e330ecbe155`.
- Kept the explicit crate version `0.1.4` so cargo-deny's wildcard ban remains
  enforceable.
- Added the exact git source to cargo-deny's allow-list with a written removal
  condition: return to crates.io when upstream PR #6 is merged and published.
- Retained `rust-lld` in `lld-link` mode and the portable `x86-64-v2` codegen
  baseline. No warning or lint was allowed or suppressed.

The first full lint run correctly failed because the prototype git source was
not approved and lacked an explicit version. The final manifest fixed both
supply-chain facts. The complete gate was rerun; no policy was weakened.

## Compile and lint

The exact committed source passed:

```text
cargo check --workspace
Finished dev profile; exit 0

pwsh -File scripts/lint.ps1
Gate 0 zero-test/zero-harness doctrine — OK
Gate 0b production surface invariants — OK
Gate 1 shared lint contract — OK
Gate 2 toolchain 1.97.1 agreement — OK
Gate 3 lock graph — OK
Gate 4 fmt, root and calyx — OK
Gate 5 cargo deny, root and calyx — OK
Gate 6 clippy --workspace --all-targets -D warnings, root and calyx — OK
Gate 7 public Calyx API ratchet — OK
LINT OK
```

`cargo tree -e features -i winapi` contained zero `xinput` feature matches after
the fix. Other real winapi consumers remain; only the accidental XInput native
link was removed.

## Full State Verification

### Before state

The physical pre-trigger state was:

```text
commit=0daa351bf51950bcafec3d9a89794ff7a4ca3941
installed_exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
installed_bytes=256731977
installed_sha256=3039EB567278DFA8B6990F215392D7CE065250A034DB9159614CFB546076CCA4
live_pid=20684
setup_build_log_bytes=663
setup_build_log_sha256=9B53E72FE94F01B51FE598F934CC3235E0204A9BCDD0867EAD49B43DE2106CED
setup_build_log_xinput_or_importeddllmain_matches=1
```

The old log named both production binaries as warning-bearing:

```text
warning: linker stderr: rust-lld: ...\xinput.lib: skipping imported DllMain symbol [importeddllmain]
warning: synapse-mcp (bin "synapse-chrome-native-host") generated 1 warning
warning: synapse-mcp (bin "synapse-mcp") generated 1 warning (1 duplicate)
```

### Real optimized build and deployment trigger

`scripts\synapse-setup.ps1` built clean commit `39372426` with the canonical
checkout target, `CARGO_BUILD_JOBS=12`, and
`CMAKE_BUILD_PARALLEL_LEVEL=12`. It packaged and verified every registered
model and linked the release in 14m47s.

The first full setup invocation installed and started the verified candidate,
then intentionally returned nonzero at
`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE`: this already-running Codex
process began with tool-surface hash `72d406...8846`, while the daemon publishes
`80c43d...c573`. Setup wrote the required restart handoff instead of pretending
the process-local schemas had refreshed.

That invocation also showed that omitting the existing deployment's audio
flags would remove audio authorization. Acceptance stopped. The corrected,
supported `-SkipBuild -EnableAudio -AllowedPermissions ... READ_AUDIO
-SkipClientWiring` invocation independently revalidated the installed model
bundle and candidate hash, detected the command-line drift, performed a real
handoff, restored audio, wrote the stale-client handoff nonfatally, and exited
0. It did not rebuild or substitute another executable.

### After state — independent physical reads

```text
installed_exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
installed_bytes=256725833
installed_sha256=FBE78669CE3847F176D14B64E2C9BD178B5B7143AC226E8E3AE73FB78CB126DC
live_pid=13504
task_state=Running
maintenance_lock_state=released
setup_build_log_bytes=1660
setup_build_log_sha256=892708514FD5E809BC84BFAFDC9C7987D5A24EFF73661A5867C849456D5085A2
setup_build_log_xinput_or_importeddllmain_matches=0
```

The final live command line independently proved the preserved capability:

```text
--enable-audio
--allowed-permissions READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE,READ_AUDIO
```

PE import-directory reads:

```text
target\release\synapse-mcp.exe
  bytes=256725833
  sha256=FBE78669CE3847F176D14B64E2C9BD178B5B7143AC226E8E3AE73FB78CB126DC
  xinput_import_matches=0

target\release\synapse-chrome-native-host.exe
  bytes=5933568
  sha256=406463EF673576EA7F36A7B7BB249713A6909158F62134178AADCFF7E7B51DD9
  xinput_import_matches=0
```

The installed file and release `synapse-mcp.exe` hashes are identical.

Authenticated live health independently reported:

```text
ok=true
pid=13504
build_commit=393724267a0bd2a74243a638f2d160f7be2187a1
build_profile=release
build_tree_state=clean
build_matches_checkout=true
audio_status=initializing
audio_model_available=true
storage_status=ok
calyx_vault_status=ok
calyx_vault_id=01KYJPGWATPD4XNMZY3ERGTKQW
calyx_vault_latest_seq=1304360
chrome_bridge_status=ok
math_backend=cpu
math_simd=avx2
math_probe=ok
execution_speed_throttling_disabled=true
```

The separate physical tool-surface snapshot was 2,135,034 bytes with file
SHA-256 `9BE81980159B949087CE8E0D684637DC37A1CCB9B66C163423C49B7055AD234B`.
It names daemon PID 13504, 40 public tools, and surface SHA-256
`80c43d2b9c6f43587901eab53ac544569816a6cda5b9fbcb0428e0235145c573`.

The CPU AVX2 path remains the fastest supported Calyx path on this host: setup
proved there is no NVIDIA PnP device, NVCC, or usable CUDA device. The real math
probe passed, and Windows execution-speed throttling is disabled. The linker
fix did not trade away build or runtime performance.

### Happy path — installed executable loader/runtime

Before and after `synapse-mcp.exe --help`, the configured daemon remained PID
13504 and the installed SHA-256 remained `FBE786...26DC`. The real installed
executable exited 0 and produced 5,414 bytes of help text with SHA-256
`ACF294272D902FA39E985E4973DC4BD37D97787EA3AF967E17E1BC71C0535361`.
This exercises Windows PE loading from the installed path independently of the
already-running process.

### Link boundary 1 — real debug example relink

The diagnostic example was explicitly relinked through Cargo/rust-lld with a
benign `/NOLOGO` link argument so Cargo could not answer from the existing final
binary. State before and after:

```text
before_sha256=CEE1388C7270033617543C6F9D60D05C3B13628BDE1F20F932ADBE63E6248B91
before_xinput_import_matches=0
trigger_exit=0
link_log_xinput_or_importeddllmain_matches=0
after_sha256=76C6242E19BFED6476190754CD3A9306EECB163D0ADDB9D23CA4A350762C167B
after_xinput_import_matches=0
```

The hash and modification time changed, proving a real final link occurred.

### Edge 1 — invalid CLI enum

Trigger: installed executable with `--mode definitely-invalid-mode`.

It exited 2 with the exact accepted enum list and `try --help` remediation.
Before and after, the live PID was 13504, installed SHA-256 was
`FBE786...26DC`, and the action recovery file remained absent. Clap rejected
the value before daemon initialization.

### Edge 2 — audio/authorization mismatch

Trigger: real setup with `-EnableAudio` but without `READ_AUDIO`.

It exited 1 with structured code
`SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID`, named the actual grant, and gave
both valid remediations. Independent before/after reads proved:

```text
pid=13504 -> 13504
installed_sha256=FBE786...26DC -> FBE786...26DC
maintenance_lock_state=released -> released
live_command_line still contains --enable-audio and READ_AUDIO
```

The invalid deployment never drained or replaced the daemon.

### Edge 3 — malformed HTTP bind audit

Trigger: installed executable with `--mode http --bind not-a-socket-address`.

The authoritative configured daemon again remained PID 13504 with the same
installed hash, and the rejected process emitted the correct primary parse
error. The audit also exposed a separate defect: `main` ran global action crash
recovery, the real input release sweep, and started the input watchdog before
`http::serve` parsed the bind, then top-level error reporting added a misleading
`daemon lifecycle ledger is not configured` message. This edge is explicitly
not reported as a success. Root cause and physical evidence are tracked in
issue #2193: <https://github.com/ChrisRoyse/Synapse/issues/2193>.

That new issue does not invalidate #2192's linker acceptance: the production
build log and all three final PE reads are warning-free, the configured release
daemon is healthy, and its import directory contains no XInput DLL.

## Cleanup

Both setup runs removed their issue-owned candidate and staging directories.
The setup staging root was empty after deployment. The three issue-owned
diagnostic logs under `%TEMP%` were deleted after their evidence was transcribed;
no alternate Cargo target, worktree, branch, test database, or mock artifact was
created. Five small setup-candidate directories dated 2026-08-04 through
2026-08-06 predated this work and were preserved as unrelated operator state.

