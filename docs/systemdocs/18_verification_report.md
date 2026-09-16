# 18. Verification Report

**Source files covered:**
- Whole-workspace counts gathered by enumerating `crates/**/*.rs`
- `Cargo.toml` (workspace), `clippy.toml`, `deny.toml`
- `crates/synapse-core/src/defaults.rs` (`SCHEMA_VERSION`)
- `.githooks/pre-push`, `README.md`
- Reconciled against companion documents 02, 04, 16, 17

> This document is a measured snapshot. Counts were produced by scanning the source tree on the documentation date (workspace version `0.1.0`). They are reproducible with the commands listed in §6.

---

## 1. Codebase metrics

| Metric | Value | How measured |
|---|---|---|
| Workspace crates | 14 | `Cargo.toml` `[workspace].members` |
| Rust source files (`crates/**/*.rs`) | 586 | `find crates -name '*.rs' | wc -l` |
| Lines of Rust (approx, `crates/**/*.rs`) | ~47,805 | `wc -l` total |
| MCP tool macros (`#[tool(` occurrences) | 238 | grep over `crates/synapse-mcp/src` |
| Distinct client-exposed MCP tools | 206 | enumerated in [16_api_tools_reference.md](16_api_tools_reference.md) (after gating/dedup) |
| RocksDB column families (named) | 17 (+ implicit `default`) | [04_storage_and_persistence.md](04_storage_and_persistence.md) |
| Storage `SCHEMA_VERSION` | 1 | `crates/synapse-core/src/defaults.rs` |
| Error codes (catalog) | ~120 across 9 groups | `crates/synapse-core/src/error_codes.rs` |
| Telemetry metrics | 19 (12 counter / 5 gauge / 2 histogram) | [14_core_telemetry_overlay.md](14_core_telemetry_overlay.md) |
| Test functions (`#[test]`) | 1,922 | grep over `crates` |
| Async test functions (`#[tokio::test]`) | 241 | grep over `crates` |
| Total test functions | 2,163 | sum of the above |
| Test files (`tests/*.rs` + `tests.rs`) | 160 | `find` over `crates` |
| Binaries / entry points | 3 | `synapse-mcp`, `synapse-overlay`, `synapse-chrome-native-host` |

### 1.1 Source files per crate

| Crate | `.rs` files |
|---|---|
| synapse-mcp | 252 |
| synapse-action | 86 |
| synapse-core | 51 |
| synapse-reflex | 39 |
| synapse-a11y | 37 |
| synapse-storage | 30 |
| synapse-capture | 21 |
| synapse-perception | 19 |
| synapse-profiles | 13 |
| synapse-audio | 12 |
| synapse-models | 8 |
| synapse-telemetry | 5 |
| synapse-test-utils | 3 |
| synapse-overlay | 1 |

---

## 2. Count reconciliations (discrepancies noted)

| Item | Raw signal | Documented figure | Explanation |
|---|---|---|---|
| MCP tools | 238 `#[tool(` macros | 206 distinct tools (doc 16) | Some tools are feature-gated (`storage_*` debug tools behind `SYNAPSE_DEBUG_TOOLS`); macro count also includes router plumbing. |
| MCP tools (README) | 81 (badge) | 206 | The README badge is **stale**. |
| Test functions | 2,163 (`#[test]`+`#[tokio::test]`) | "~1,590" estimate in an earlier draft of doc 17 | The grep-measured **2,163** is authoritative; doc 17's prose figure was a conservative estimate. |

---

## 3. Lint & dependency configuration

| Control | Status |
|---|---|
| Clippy | `clippy::unwrap_used` / `clippy::expect_used` **denied** workspace-wide in production code; allowed in tests via `clippy.toml` (`allow-unwrap-in-tests = true`, `allow-expect-in-tests = true`). |
| Pre-push gate | `.githooks/pre-push` runs `cargo clippy --workspace --all-targets`. |
| Dependency/license gate | `cargo-deny` (`deny.toml`): targets x86_64 linux-gnu + windows-msvc; allowed licenses MIT, Apache-2.0 (+LLVM-exception), BSD-2/3-Clause, MPL-2.0; advisories version 2. |
| CI | None (no `.github/` workflows, Makefile, justfile, or nextest config). |
| Lint status at snapshot | Not executed in this pass (`cargo clippy` not run here) — "Not determined from source". |

---

## 4. Test results

| Item | Result |
|---|---|
| Test run executed in this pass | No — tests were not executed (building requires `libclang.dll` on PATH per project memory; Windows-gated tests require an interactive desktop). |
| Pass/fail | Not determined from source. |
| Run command | `cargo test --workspace` (Windows-only tests are `#[ignore]`d; run with `-- --ignored`). |
| Shipping gate | Manual Full State Verification ("FSV") performed by the agent on the configured Windows host; surfaces in code only as `minimum_manual_fsv` manifest metadata. It is never automated. |

See [17_test_suite.md](17_test_suite.md) for the test inventory and categories.

---

## 5. Notable constants & magic numbers

| Constant | Value | Source |
|---|---|---|
| `SCHEMA_VERSION` (storage) | 1 (stored as 4-byte BE `u32` under `__schema_version`) | `crates/synapse-core/src/defaults.rs` |
| `DEFAULT_BIND` (daemon) | `127.0.0.1:7700` | mcp config |
| `DEFAULT_AIM_TRACK_EMA_ALPHA` | exported reflex EMA alpha | `crates/synapse-core/src/defaults.rs` |
| Reflex scheduler target | 1 ms (MMCSS waitable timer; 2 ms tokio fallback) | [10](10_reflex_subsystem.md) |
| Reflex starvation threshold | `STARVATION_AFTER = 2 s` | `crates/synapse-reflex/src/conflict.rs` |
| GC eviction per pass | 25% of byte budget | [04](04_storage_and_persistence.md) |
| Disk-pressure state machine | 5 levels (fs2) | [04](04_storage_and_persistence.md) |
| Audio ring | 30 s, 48 kHz, f32, stereo | [08](08_audio_subsystem.md) |
| STT | Whisper tiny INT8, 16 kHz mono, English | [08](08_audio_subsystem.md) |
| Template match | NCC clamped [-1,1]; 10 slots, min conf 0.85 | [07](07_perception_subsystem.md) |
| Frame channel | capacity 2, drop-oldest | [05](05_capture_subsystem.md) |
| Double-click delay | clamp(`GetDoubleClickTime`/4, 30..150) ms | [09](09_action_subsystem.md) |
| Bigram typing speedup | 0.75× over 50 common English bigrams | [09](09_action_subsystem.md) |
| Registered ML models | 1 (RT-DETRv2-S COCO) | [13](13_models_subsystem.md) |
| ORT version | 2.0.0-rc.12 | `Cargo.toml` |
| rmcp version | 1.7.0 | `Cargo.toml` |
| Single-instance duplicate exit code | 3 | `crates/synapse-mcp/src/main.rs` |

---

## 6. Reproducing these counts

```bash
# from repo root C:\code\synapse (Git Bash)
find crates -name '*.rs' | wc -l                              # source files (586)
find crates -name '*.rs' | xargs wc -l | tail -1              # LOC (~47,805)
grep -rE '#\[test\]' crates --include='*.rs' | wc -l          # 1922
grep -rE '#\[tokio::test' crates --include='*.rs' | wc -l     # 241
grep -rE '#\[tool\(' crates/synapse-mcp/src --include='*.rs' | wc -l   # 238
find crates -path '*/tests/*.rs' -o -name 'tests.rs' | wc -l  # 160
```

---

## 7. Documentation series completeness

| Doc | File | Status |
|---|---|---|
| 01 | system_overview.md | ✅ |
| 02 | source_code_map.md | ✅ |
| 03 | configuration.md | ✅ |
| 04 | storage_and_persistence.md | ✅ |
| 05 | capture_subsystem.md | ✅ |
| 06 | accessibility_and_cdp_subsystem.md | ✅ |
| 07 | perception_subsystem.md | ✅ |
| 08 | audio_subsystem.md | ✅ |
| 09 | action_subsystem.md | ✅ |
| 10 | reflex_subsystem.md | ✅ |
| 11 | profiles_subsystem.md | ✅ |
| 13 | models_subsystem.md | ✅ |
| 14 | core_telemetry_overlay.md | ✅ |
| 15 | mcp_server_architecture.md | ✅ |
| 16 | api_tools_reference.md | ✅ |
| 17 | test_suite.md | ✅ |
| 18 | verification_report.md | ✅ (this file) |
