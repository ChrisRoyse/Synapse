# Issue #2175 — fatal Rust warning gate

Date: 2026-08-09 (UTC)

## Scope and source of truth

Issue #2175 was accepted against two independent authorities:

1. The structural authority is the real `scripts/lint.ps1` process, its exit
   status, and the exact source bytes before and after hand-planted warnings.
   The script is the only repository lint entry point and invokes both Cargo
   workspaces.
2. The runtime authority is the installed release process plus the physical
   Calyx vault. Authenticated MCP readback was cross-checked by a separate
   read-only process opening the same vault; `/health` return values alone were
   not treated as evidence.

The accepted source commit is
`8b25b6b39e1092ad07e513a12ad7e9ce27ae073f`. A fresh `git fetch origin main`
reported `origin/main...HEAD = 0 1`, so the verified checkout contains all
upstream work and one local issue commit.

## Diagnosis and research

Both `rust-toolchain.toml` files already pinned Rust 1.97.1. Cargo's
`rust-version = "1.95"` is the declared MSRV; it is not the active toolchain
selector. The root cause was that the canonical script ran Clippy without
`-- -D warnings`. The manifests denied `clippy::all`, but Rust warnings and
Clippy pedantic/nursery warnings remained non-fatal. Main could therefore
accumulate warning debt while the lint script returned success.

Research was performed after that diagnosis. The configured Exa MCP lane was
exercised through a real initialize/tools-list/tools-call exchange and reported
`live` (`exa-search-server` 3.4.0). The built-in web lane independently read the
primary documentation:

- [Clippy CI guidance](https://doc.rust-lang.org/clippy/continuous_integration/)
  recommends making Clippy warnings fatal and keeping the Clippy version
  consistent.
- [Clippy usage](https://doc.rust-lang.org/clippy/usage.html) documents that a
  Clippy invocation also emits normal compiler warnings.
- [rustup overrides](https://rust-lang.github.io/rustup/overrides.html) defines
  directory `rust-toolchain.toml` selection.
- [Cargo rust-version](https://doc.rust-lang.org/cargo/reference/rust-version.html)
  defines the field as the package's supported minimum Rust version.

The resulting policy is exact and fail-closed: both workspaces run
`cargo clippy --workspace --all-targets -- -D warnings`. Remediation text
forbids lowering severity or adding blanket suppression. Intentional atomic or
reporting functions use only narrow, reasoned `#[expect]` attributes; the fatal
gate rejects an expectation that becomes stale.

## Clean happy path

Trigger:

```text
pwsh -File scripts/lint.ps1
```

The process exited 0. Its separately printed authorities were:

```text
Rust files: 1355
Cargo manifests: 32
Cargo targets: 52
Automated test/bench/FSV executable surface: 0
Shared lint-contract lines: 21 (root and calyx matched)
Root toolchain: 1.97.1
Calyx toolchain: 1.97.1
Lock graph violations: 0
Root fmt/deny/clippy: passed
Calyx fmt/deny/clippy: passed
Public API count: 357; baseline: 357
LINT OK: every gate passed in BOTH workspaces (root and calyx).
```

Independent exact fatal invocations in the repository root and in `calyx/`
both reported zero unique errors. `cargo check --workspace` also exited 0.
After every mutation below was restored, the complete canonical script was run
again and returned the same clean verdict.

## Boundary and edge-case audit

These were manual source mutations, not automated tests or a probe harness.
For each case the file bytes were hashed before, the real canonical script was
triggered, then the source was restored and independently hashed/read again.

### 1. Root-workspace compiler warning

Source: `crates/synapse-audio/src/stt.rs`

```text
BEFORE sha256=A2E24660439E5FD5318B391393BF59E9213CFF64A17C941DB15CBB9AB0827333
TRIGGER hand-added one unused constant in a root workspace member
MUTATED sha256=224A2CCDCB8F7E4EE2BA39307FF1E3F01D93666FFEBE3BFA82D23C470EC3787F
AFTER script_exit=1
AFTER diagnostic=dead_code; -D dead-code implied by -D warnings
AFTER structured_code=SYNAPSE_LINT_CLIPPY_ROOT_FAILED
RESTORED sha256=A2E24660439E5FD5318B391393BF59E9213CFF64A17C941DB15CBB9AB0827333
RESTORED marker_matches=0
```

### 2. Calyx-only compiler warning

Source: `calyx/crates/calyx-core/src/lib.rs`

```text
BEFORE sha256=4D42B9A353ADFBDC6EDE8A08DADE8F5171EFE1C1F0E38345813E0189C6A1FB14
TRIGGER hand-added one unused constant in calyx-core
MUTATED sha256=0ACBB56635898E2014F95F0532E6F798AB0C12F00FC2DD43ADB32B213CFB0B73
AFTER root_clippy=passed
AFTER calyx_clippy_exit=101
AFTER diagnostic=dead_code; -D dead-code implied by -D warnings
AFTER structured_code=SYNAPSE_LINT_CLIPPY_CALYX_FAILED
RESTORED sha256=4D42B9A353ADFBDC6EDE8A08DADE8F5171EFE1C1F0E38345813E0189C6A1FB14
RESTORED marker_matches=0
```

This proves the second workspace cannot be silently skipped by a green root
workspace run.

### 3. Stale narrow lint expectation

Source: `crates/synapse-calyx/src/autonomy.rs`

```text
BEFORE sha256=E6B1549DF90C0BA40563BFDD5A3F7A2AE449A3FA7A9D3ECEE87F172C02E37A7F
TRIGGER hand-added #[expect(clippy::needless_return)] where the lint did not apply
MUTATED sha256=BCAC36A84BAAF73A6ACD115B88558D8A60D2B694C632711B5A37E4FE109ADD8D
AFTER script_exit=1
AFTER diagnostic=unfulfilled lint expectation; -D unfulfilled-lint-expectations implied by -D warnings
AFTER structured_code=SYNAPSE_LINT_CLIPPY_ROOT_FAILED
RESTORED sha256=E6B1549DF90C0BA40563BFDD5A3F7A2AE449A3FA7A9D3ECEE87F172C02E37A7F
RESTORED marker_matches=0
```

The final tracked `scripts/lint.ps1` SHA-256 was
`63457B38DDFBFD25E7D766A1DF01C672474994166AC2BFDA164E19902D481884`.

## Installed runtime and physical-state readback

The issue commit was deployed through `scripts/synapse-setup.ps1` to the normal
installed path with audio and the existing explicit permission set preserved.
The OS process table, read after setup exited, contained exactly the live daemon
process below (no release-target image was run):

```text
pid=18412
image=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
args=--mode http --bind 127.0.0.1:7700 --db C:\Users\hotra\AppData\Local\synapse\db-daemon --profile-dir C:\Users\hotra\.cargo\bin\profiles --log-level info --enable-audio --allowed-permissions READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE,READ_AUDIO
image_bytes=256648009
image_sha256=81EEA46BFDB7A8CC8F8C491B593A657C927A4692D470893B608DF3A9325EDD30
```

A real authenticated MCP `health` tools/call independently reported:

```text
ok=true
pid=18412
build_commit=8b25b6b39e1092ad07e513a12ad7e9ce27ae073f
build_checkout_commit=8b25b6b39e1092ad07e513a12ad7e9ce27ae073f
build_matches_checkout=true
build_tree_state=clean
build_profile=release
build_exe_path=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
previous_shutdown=clean (graceful, phase=calyx_vault_close)
process_qos.execution_speed_throttling_disabled=true
storage_backend=calyx
storage.status=ok
```

Hardware dispatch was read from the live vault runtime rather than inferred:
auto selection proved no NVIDIA driver/device, selected CPU, selected AVX2
SIMD, and the startup math probe matched the portable implementation
bit-for-bit over 19 elements. All 11 reported Calyx tuning knobs were
load-bearing and the inert-knob count was zero.

Two real MCP storage tools/calls then scanned the production vault. `summary`
reported 17 logical CFs with no missing count or size estimate and normal disk
pressure. A separate `inspect` snapshot reported:

```text
vault_id=01KYJPGWATPD4XNMZY3ERGTKQW
census_atomic=true
census_snapshot_seq_first=1303374
census_snapshot_seq_last=1303374
live_rows=79286
raw_rows=81253
expired_rows=1967
logical_bytes=93243278
```

Finally, a separate read-only process opened
`%LOCALAPPDATA%\synapse\db-daemon` rather than trusting MCP return values:

```text
native_family=TimeSeries rows=1823 bytes=94282 ordered_rows_sha256=e525f3da805790b74018e2356f911540a25324fae219b604bee12af676fb7c54
native_family=Collections rows=5 bytes=403 ordered_rows_sha256=4244a95353013fb02c9b5ed8b0f60392d5e440656faf7889d89792a04cdec543
```

No automated test, benchmark, CI job, worktree, branch, mock data, fallback, or
FSV harness was created or run. The temporary warning mutations were removed;
their exact original hashes and zero marker counts prove the source was restored.
