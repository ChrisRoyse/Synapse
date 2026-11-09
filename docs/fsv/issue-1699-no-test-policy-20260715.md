# Issue 1699 Manual FSV - no-test policy

Date: 2026-07-15

## Source of Truth

- File tree in the clean worktree `C:\code\Synapse-issue1699-no-tests`.
- Root and nested workspace manifests: `Cargo.toml`, `Cargo.lock`, `calyx/Cargo.toml`, `calyx/Cargo.lock`, and per-crate `Cargo.toml` files.
- Git staged tree for the pending `[skip ci]` commit.
- GitHub issue state for #1699 and follow-up #1700.
- Synapse runtime liveness readback for host context: process `synapse-mcp.exe` PID `46828`, bind `127.0.0.1:7700`, and real `mcp__synapse.timeline` readback from `CF_TIMELINE`.

## Root Cause

The operator decision changed the project invariant from "automated checks support manual FSV" to "the repo carries zero automated tests." `origin/main` still carried a full Cargo/libtest test surface: integration-test paths, benchmark paths, inline `#[test]`/`#[tokio::test]` functions, `#[cfg(test)]` seams, manifest test/bench targets, `[dev-dependencies]`, and test-support crates. Cargo's official behavior makes that surface real because `cargo test` builds lib/bin test targets by default, enables `cfg(test)` for test builds, runs libtest against `#[test]`, and treats doctests/test flags as target-level behavior. Leaving any of those seams would make the no-test policy depend on human memory.

Research sources used:

- Exa MCP search result and official Cargo Book: `https://doc.rust-lang.org/cargo/reference/cargo-targets.html`
- Native web search and official Cargo Book: `https://doc.rust-lang.org/cargo/commands/cargo-test.html`

## Trigger

Manual tree edit in the clean worktree based on `origin/main` commit `26de2e0f0621416a670fb57ce6c2592cacaf3475`:

- Removed `tests/` and `benches/` paths across root Synapse and `calyx/`.
- Removed inline Rust test attributes and test modules.
- Removed manifest `[[test]]`, `[[bench]]`, and `[dev-dependencies]` sections.
- Removed `synapse-test-utils`, `calyx-testkit`, and `calyx-fsv`.
- Removed `cfg(test)` and `test-support` feature seams.
- Updated workflow/docs to say structural checks are `cargo fmt`, `cargo check`, and `cargo clippy`; manual FSV remains the only behavioral acceptance path.
- Filed #1700 for stale generated `calyx/CALYX_COMPLETE_DOCUMENTATION.md` content that still describes the old removed test surface.

## Before Read

Baseline file inventory from the clean worktree before applying the no-test transplant:

```text
test_path_files=872
bench_path_files=19
rust_test_attrs=5954
manifest_test_or_bench_targets=63
manifest_dev_dependency_sections=28
```

## After Read

Separate post-trigger read of the file tree and manifests:

```json
{
  "test_path_files": 0,
  "bench_path_files": 0,
  "rust_test_attrs": 0,
  "manifest_test_or_bench_targets": 0,
  "manifest_dev_dependency_sections": 0,
  "test_support_crate_files": 0,
  "fsv_or_test_named_rs_files": 0,
  "cfg_test_or_test_support_hits": 0,
  "unused_test_workspace_deps": 0
}
```

Explicit path scan after the trigger:

```text
rg --files | rg '(^|/)(tests|benches)(/|$)|(_tests|fsv_tests)\.rs$|(^|/)tests\.rs$|(^|/)(synapse-test-utils|calyx-testkit|calyx-fsv)(/|$)|fsv_driver|fsv_support'
result: no output, exit 1
```

Explicit code/manifest scan after the trigger:

```text
rg -n '#\[(tokio::test|test)\]|^\s*\[\[(test|bench)\]\]|^\s*\[dev-dependencies\]|cfg\(test\)|cfg_attr\(test|test-support' -g '*.rs' -g 'Cargo.toml'
result: no output, exit 1
```

## Manual Edge Audit

Happy path - complete automated test surface removal:

- Before: 872 test-path files, 19 bench-path files, 5,954 Rust test attributes, 63 manifest test/bench targets, 28 dev-dependency sections.
- Trigger: removed automated tests, benches, test-only crates, and policy-conflicting manifest sections.
- After: all corresponding inventory counts read back as `0`.

Edge 1 - empty discovered path set:

- Before: `tests/` and `benches/` directories existed in root, dashboard, extensions, and `calyx/`.
- Trigger: removed all matching paths.
- After: path scan for `tests`, `benches`, `_tests.rs`, `fsv_tests.rs`, `tests.rs`, `fsv_driver`, and `fsv_support` returned no rows.

Edge 2 - test-only support crates and automated FSV drivers:

- Before: `crates/synapse-test-utils`, `calyx/crates/calyx-testkit`, `calyx/crates/calyx-fsv`, and many `*_fsv` driver/support paths existed.
- Trigger: removed those crates and paths from the file tree and manifests.
- After: support-crate and FSV-driver inventory counts read `0`.

Edge 3 - hidden Cargo/Rust activation seams:

- Before: live Rust/Cargo surfaces included `cfg(test)`, `cfg_attr(test, ...)`, `test-support`, `[dev-dependencies]`, and workspace test dependencies such as `proptest`, `criterion`, `insta`, and `mockall`.
- Trigger: removed the feature gates, test-only globals, and unused test workspace dependencies.
- After: scans for `cfg(test)`, `cfg_attr(test`, `test-support`, `[dev-dependencies]`, and unused test workspace dependencies all read `0`.

Edge 4 - command surface drift:

- Before: docs and agent-spawn allow rules still referenced `cargo test` as an expected workflow.
- Trigger: changed those references to structural checks only and removed `Bash(cargo test:*)` from the Claude auto-allow list.
- After: the documented workflow is structural-only; `cargo test` was not run.

## Supporting Structural Checks

These are compile/format/lint checks only. They are not FSV and do not prove behavior.

```text
cargo fmt --all --check: PASS
cargo fmt --manifest-path calyx\Cargo.toml --all --check: PASS
cargo check --workspace: PASS
cargo check --manifest-path calyx\Cargo.toml --workspace: PASS
cargo clippy --workspace --all-targets: PASS
cargo clippy --manifest-path calyx\Cargo.toml --workspace --all-targets: PASS
```

Both `calyx-forge` build-script warnings were expected and non-fatal: `cuda feature not enabled, skipping kernel compilation`.

No `cargo test` command was run.

## Result

The file-tree and manifest Source of Truth now match the no-test operator decision: the root workspace and nested `calyx/` workspace contain no automated test/bench targets, test paths, Rust test attributes, test-only support crates, automated FSV drivers, or hidden `cfg(test)`/`test-support` seams. Behavioral acceptance remains manual FSV against physical SoT only.
