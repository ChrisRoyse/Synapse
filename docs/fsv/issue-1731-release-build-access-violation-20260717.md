# Issue #1731 FSV - release build STATUS_ACCESS_VIOLATION

Date: 2026-07-17
Agent: Codex
Issue: https://github.com/ChrisRoyse/Synapse/issues/1731

## Result

Accepted for the setup release-build crash scope. `scripts/synapse-setup.ps1`
now owns the release compiler environment by setting `RUST_MIN_STACK` to at
least 8 MiB before `cargo build --release -p synapse-mcp`, logs the effective
value, and writes it into release-build failure diagnostics. Existing larger
operator values are honored.

No workaround/fallback was added: a failed build still fails setup and preserves
the release-build diagnostics archive. No automated tests, FSV harnesses,
benchmarks, or CI were created or run. Compile/lint commands are structural
checks only and are not FSV.

## Root Cause

The two real setup failures in
`%LOCALAPPDATA%\synapse\logs\setup-build-failures` were `rustc.exe` crashes
under the Windows MSVC release compiler path:

```text
cargo build --release -p synapse-mcp
rustc toolchain=1.96.1-x86_64-pc-windows-msvc
rustflags=-C lto=thin -C codegen-units=16 -C linker=rust-lld -C linker-flavor=lld-link
rustc_exit=0xc0000005 STATUS_ACCESS_VIOLATION
cargo_exit=101
failure_diagnostics=
  %LOCALAPPDATA%\synapse\logs\setup-build-failures\release-build-20260717T170109959Z-pid83820.diagnostics.json
  %LOCALAPPDATA%\synapse\logs\setup-build-failures\release-build-20260717T174112653Z-pid72052.diagnostics.json
```

This was not a Synapse runtime failure and not a valid state to retry over or
mask with an older artifact. The setup script was depending on the ambient
Windows compiler process environment for a large ThinLTO/lld release build.

The fix makes the required compiler stack explicit and observable:

```text
schema=synapse_setup_release_build_compiler_environment/v1
minimum_RUST_MIN_STACK=8388608
absent_env -> RUST_MIN_STACK=8388608 source=synapse_setup_default
too_small_or_invalid_env -> RUST_MIN_STACK=8388608 source=raised_by_synapse_setup
large_existing_env -> source=preexisting_env
```

## Best-Practice Research Inputs

Exa and browser research were used before finalizing the fix:

- rust-lang/rust #125765 documents Windows `lto = "thin"` release aborts and
  the Rust compiler-team guidance to use `RUST_MIN_STACK=8388608` so LLVM
  worker threads get an 8 MiB stack.
  https://github.com/rust-lang/rust/issues/125765
- rust-lang/rust #111480 documents `STATUS_ACCESS_VIOLATION` with Windows MSVC,
  LLD, and ThinLTO.
  https://github.com/rust-lang/rust/issues/111480
- rust-lang/rust #81408 documents the same Windows ThinLTO/lld access-violation
  class and identifies the Windows/MSVC + rust-lld + ThinLTO combination.
  https://github.com/rust-lang/rust/issues/81408
- Microsoft describes `0xc0000005` as an access violation caused by an
  application reading, writing, or executing an invalid memory address.
  https://learn.microsoft.com/en-us/shows/inside/c0000005

Applied decisions:

- Do not change the release profile away from ThinLTO/lld in setup.
- Do not retry failed release builds or keep using a stale candidate.
- Normalize and log the compiler environment before the release build.
- Persist the normalized compiler environment into failure diagnostics so any
  future crash has a concrete setup state to inspect.

## Sources Of Truth

- Release-build failure diagnostics:
  `%LOCALAPPDATA%\synapse\logs\setup-build-failures\release-build-*.diagnostics.json`.
- Setup logs:
  `%LOCALAPPDATA%\synapse\logs\issue-1731-*.stdout.log` and
  `%LOCALAPPDATA%\synapse\logs\issue-1731-*.stderr.log`.
- Installed binary:
  `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`.
- Daemon process/socket:
  `Get-Process synapse-mcp` and `Get-NetTCPConnection 127.0.0.1:7700`.
- Runtime health:
  authenticated `GET http://127.0.0.1:7700/health?detail=compact` and real
  wired `mcp__synapse.health`.

## Happy Path - Empty Parent Environment

Synthetic input:

```text
parent_RUST_MIN_STACK=<empty>
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1731
```

Expected output: setup sets `RUST_MIN_STACK=8388608`, completes the real release
build, installs the candidate, waits up to 600 seconds for installed health, and
leaves a real daemon serving on `127.0.0.1:7700`.

Before:

```text
time=2026-07-17T14:15:17.6110669-05:00
parent_RUST_MIN_STACK=<empty>
installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
daemon_pid=70960
listener=127.0.0.1:7700 owner=70960
```

After separate Source-of-Truth read:

```text
setup_exit=0
setup_log=%LOCALAPPDATA%\synapse\logs\issue-1731-1733-happy-setup.stdout.log
env_line=[synapse-setup] Release build compiler environment: RUST_MIN_STACK=8388608 source=synapse_setup_default
installed_health_line=[synapse-setup] Daemon OK: pid=60584 version=0.1.0 db=C:\Users\hotra\AppData\Local\synapse\db-daemon
installed_sha256=A2C29E8FF1D68C62B28D7E915C7890D96E8317DADC6571A259DE6A2B849B4C3A
daemon_pid=60584
listener=127.0.0.1:7700 owner=60584
mcp_health_ok=true pid=60584 tool_count=40 storage=ok calyx_vault=ok chrome_bridge=ok
```

## Edge 1 - Existing Larger Stack Is Honored

Synthetic input:

```text
parent_RUST_MIN_STACK=16777216
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1731
```

Expected output: setup does not lower the operator-provided value and still
installs a healthy daemon.

Before:

```text
installed_sha256=A2C29E8FF1D68C62B28D7E915C7890D96E8317DADC6571A259DE6A2B849B4C3A
daemon_pid=60584
listener=127.0.0.1:7700 owner=60584
```

After:

```text
setup_exit=0
setup_log=%LOCALAPPDATA%\synapse\logs\issue-1731-edge-high-stack.stdout.log
env_line=[synapse-setup] Release build compiler environment: RUST_MIN_STACK=16777216 source=preexisting_env
installed_health_line=[synapse-setup] Daemon OK: pid=24800 version=0.1.0 db=C:\Users\hotra\AppData\Local\synapse\db-daemon
installed_sha256=4322592DAAE13B1567A7968C3DD2DCF497BF825D062C878D668BFAB28960777F
daemon_pid=24800
listener=127.0.0.1:7700 owner=24800
mcp_health_ok=true pid=24800 tool_count=40 storage=ok calyx_vault=ok chrome_bridge=ok
```

## Edge 2 - Too-Small Stack Is Raised

Synthetic input:

```text
parent_RUST_MIN_STACK=1
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1731
```

Expected output: setup raises the value to 8 MiB, logs
`raised_by_synapse_setup`, and leaves a healthy daemon.

Before:

```text
installed_sha256=4322592DAAE13B1567A7968C3DD2DCF497BF825D062C878D668BFAB28960777F
daemon_pid=24800
listener=127.0.0.1:7700 owner=24800
```

After:

```text
setup_exit=0
setup_log=%LOCALAPPDATA%\synapse\logs\issue-1731-edge-low-stack.stdout.log
env_line=[synapse-setup] Release build compiler environment: RUST_MIN_STACK=8388608 source=raised_by_synapse_setup
installed_health_line=[synapse-setup] Daemon OK: pid=39352 version=0.1.0 db=C:\Users\hotra\AppData\Local\synapse\db-daemon
installed_sha256=3D311F6EE3EBF84855A11A3B80049E7F1B12AE6478E3143C9B8063D8A0119C5F
daemon_pid=39352
listener=127.0.0.1:7700 owner=39352
mcp_health_ok=true pid=39352 tool_count=40 storage=ok calyx_vault=ok chrome_bridge=ok
```

## Edge 3 - Malformed Stack Is Raised

Synthetic input:

```text
parent_RUST_MIN_STACK=not-a-number
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1731
```

Expected output: setup rejects the malformed environment as unusable, raises it
to 8 MiB, logs `raised_by_synapse_setup`, and leaves a healthy daemon.

Before:

```text
installed_sha256=3D311F6EE3EBF84855A11A3B80049E7F1B12AE6478E3143C9B8063D8A0119C5F
daemon_pid=39352
listener=127.0.0.1:7700 owner=39352
```

After:

```text
setup_exit=0
setup_log=%LOCALAPPDATA%\synapse\logs\issue-1731-edge-invalid-stack.stdout.log
env_line=[synapse-setup] Release build compiler environment: RUST_MIN_STACK=8388608 source=raised_by_synapse_setup
installed_health_line=[synapse-setup] Daemon OK: pid=85360 version=0.1.0 db=C:\Users\hotra\AppData\Local\synapse\db-daemon
installed_sha256=FAD10EF1A7F0BF19830536846F1EFC69A5207691BE352DF0013FE7A1C045B0E6
daemon_pid=85360
listener=127.0.0.1:7700 owner=85360
mcp_health_ok=true pid=85360 tool_count=40 storage=ok calyx_vault=ok chrome_bridge=ok
```

## Final Evidence Of Success

```text
installed_sha256=FAD10EF1A7F0BF19830536846F1EFC69A5207691BE352DF0013FE7A1C045B0E6
daemon_pid=85360
listener=127.0.0.1:7700 owner=85360
authenticated_http_health_ok=true pid=85360 tool_count=241 storage=ok calyx_vault=ok chrome_bridge=ok
wired_mcp_health_ok=true pid=85360 tool_count=40 tool_surface_sha256=d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb
latest_release_build_failure_diagnostics_still_preserved=
  %LOCALAPPDATA%\synapse\logs\setup-build-failures\release-build-20260717T174112653Z-pid72052.diagnostics.json
```
