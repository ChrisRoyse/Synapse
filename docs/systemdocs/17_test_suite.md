# 17. Test Suite

**Source files covered:**
- `crates/synapse-test-utils/src/lib.rs` (re-exports `fixtures`, `stdio_mcp_client`)
- `crates/synapse-test-utils/src/fixtures.rs` (Notepad launch fixture, Windows + non-Windows stubs)
- `crates/synapse-test-utils/src/stdio_mcp_client.rs` (`StdioMcpClient` — raw JSON-RPC stdio MCP client)
- `crates/*/tests/*.rs` (per-crate integration tests)
- `crates/*/src/**/tests.rs` and inline `#[cfg(test)] mod tests` modules
- `tests/fixtures/audio/*` (workspace-level WAV fixtures + `README.md`)
- `CONTRIBUTING.md`, `.githooks/pre-push` (run commands / gate policy)
- `docs/foreground-capability-acceptance-runbook.md` (manual FSV runbook)

Cross-reference: see [18_verification_report.md](18_verification_report.md).

---

## 1. Overview

Tests live in three places:

1. **Inline unit tests** — `#[cfg(test)] mod tests` blocks inside `src/`, plus dedicated `src/**/tests.rs` and `src/**_tests.rs` files (e.g. `crates/synapse-storage/src/open_tests.rs`, `crates/synapse-mcp/src/m4.rs`).
2. **Per-crate integration tests** — `crates/<crate>/tests/*.rs`. These compile against the crate's public API; `synapse-mcp` integration tests additionally get the built binary path via `CARGO_BIN_EXE_synapse-mcp`.
3. **Workspace-level `tests/`** — `C:\code\synapse\tests\` contains **only fixtures** (`tests/fixtures/audio/*.wav` + `README.md`). There is no workspace-level test runner crate; there is no top-level `tests/*.rs`.

Approximate total: **~1,590 `#[test]` / `#[tokio::test]` functions** across ~250 source/test files (`Grep` count of `#[test]` and `#[tokio::test]`).

There is **no CI**: GitHub Actions are explicitly forbidden (per `.githooks/pre-push` comment referencing issue #351). The local pre-push hook + manual runs are the source of truth.

---

## 2. Per-crate test count

Counts below are `#[test]` + `#[tokio::test]` occurrences summed across each crate's `src/` and `tests/` (from `Grep`). Approximate (aggregation of per-file counts).

| Crate | ~`#[test]`/`#[tokio::test]` | Notable test files |
|---|---|---|
| `synapse-mcp` | ~860 | `src/local_agent.rs` (58), `src/m4.rs` (123), `src/chrome_debugger_bridge.rs` (31), `src/http/transport.rs` (29), `src/server/context.rs` (24), `src/m2/type_text.rs` (24), `src/m1.rs` (22), `src/server/agent_control/tests.rs` (35), `src/server/escalation/tests.rs` (21), `src/server/everquest_autocombat.rs` (20); plus ~50 `tests/*.rs` integration files (see §4) |
| `synapse-action` | ~155 | `tests/session_inputs.rs` (8), `tests/mouse_stroke.rs` (8), `tests/emitter_state.rs` (7), `src/rate_limit.rs` (8), `src/lease.rs` (13), `src/backend/vigem/tests.rs` (9), `src/backend/software/mouse.rs` (9), `src/invoke/tests.rs` (11) |
| `synapse-core` | ~115 | `tests/action_serde_proptest.rs` (18), `tests/types.rs` (13), `tests/snapshots.rs` (7), `src/episodes.rs` (15), `src/types/agent_cost.rs` (13), `src/routines.rs` (12), `src/intent.rs` (10) |
| `synapse-a11y` | ~120 | `src/tests.rs` (23), `src/cdp_action.rs` (19), `src/cdp_network.rs` (19), `src/cdp_dom.rs` (11), `tests/uwp_snapshot_regression.rs` (1, `#[ignore]`) |
| `synapse-storage` | ~55 | `src/open_tests.rs` (12), `src/batch_tests.rs` (6), `tests/timeline_cf.rs`, `tests/agent_events_cf.rs`, GC/compaction/disk-pressure proptests |
| `synapse-reflex` | ~65 | `tests/scheduler_behavior.rs` (26), `tests/hold_move_behavior.rs` (8), `tests/combo_behavior.rs` (7), `src/tests.rs` (7) |
| `synapse-perception` | ~45 | `tests/perception_regression.rs` (12), `tests/hud_extractor.rs` (7), `tests/template_match.rs` (5), `tests/hud_anchor.rs` (5), `tests/cdp_diagnostics_regression.rs` (2) |
| `synapse-profiles` | ~35 | `tests/parse_bundled.rs` (14), `tests/package_manifest.rs` (12), `tests/runtime_refresh.rs` (3), `src/resolver.rs` (5) |
| `synapse-models` | ~13 | `tests/model_loader.rs` (13) |
| `synapse-audio` | ~22 | `tests/ring_detectors.rs` (5), `tests/direction.rs` (4), `tests/stt.rs` (3), `tests/runtime_scaffold.rs` (3) |
| `synapse-capture` | ~22 | `src/tests.rs` (14), `src/platform/windows/bitmap.rs` (7) |
| `synapse-everquest` | ~24 | `src/log.rs` (11), `src/map.rs` (6), `src/zone_graph.rs` (5), `tests/map_inventory.rs` (2) |
| `synapse-telemetry` | ~7 | `src/metrics.rs` (3), `tests/file_sink.rs` (2), `tests/periodic_gc.rs` (1), `tests/periodic_gc_size_cap.rs` (1) |
| `synapse-test-utils` | ~11 | `src/fixtures.rs` (10) — self-tests of the Notepad selection logic |
| `synapse-overlay` | ~1 | `src/main.rs` (1) |

Totals are derived from the `Grep` per-file counts; treat as approximate. Per-crate exact counts: run `cargo test -p <crate> -- --list`.

---

## 3. Test categories

| Category | What it is | Where | How identified |
|---|---|---|---|
| **Unit** | In-process tests of one module's logic; no external process. | `#[cfg(test)] mod tests` in `src/`, `src/**/tests.rs`, `src/**_tests.rs` | inline modules |
| **Integration** | Compiled against a crate's public API in a separate binary. | `crates/<crate>/tests/*.rs` | files under `tests/` |
| **End-to-end (e2e)** | Spawn the real `synapse-mcp` binary over stdio and drive it via JSON-RPC. | `StdioMcpClient` in `crates/synapse-test-utils/src/stdio_mcp_client.rs`; ~45 callers (see §4) | use of `StdioMcpClient` |
| **Property tests** | `proptest`-style round-trip / invariant tests. | `synapse-core/tests/action_serde_proptest.rs`, `synapse-action/tests/dynamics_*_proptest.rs`, `synapse-storage/tests/compaction_ttl_proptest.rs` | `*proptest*` filenames |
| **Snapshot/regression** | Golden snapshots and regression guards. | `crates/synapse-mcp/tests/snapshots/*.snap`, `*_regression.rs` (a11y/perception) | `.snap` files, `regression` in name |
| **Manual FSV** | NOT a Rust test category — see §3.1. | docs + operator runbook | "FSV" references |

### 3.1 What "FSV" means here

**FSV = Full State Verification** (also written "Full State Verification (FSV)" in `CONTRIBUTING.md`). It is the project's **manual, human-at-the-machine acceptance gate** on the configured Windows host — *not* an automated test type. Key points from source:

- `CONTRIBUTING.md` §4: "the project uses manual Full State Verification (FSV) on the configured Windows host as the shipping gate ... Automated tests are supporting evidence, not a substitute for verifying real behavior."
- `docs/foreground-capability-acceptance-runbook.md` is "the **manual acceptance runbook** ... explicitly *not* an automated FSV harness." It directs the operator to trigger via the real wired MCP client, then read the Source-of-Truth (SoT) with a separate operation — "Do not use any script, helper client, or harness as acceptance."
- `manual-fsv-tmp/` is a gitignored scratch dir (`.gitignore` line 63: `/manual-fsv-tmp/`).
- `manual_fsv` / `minimum_manual_fsv` also appear as **data fields** in curated profile manifests (`crates/synapse-profiles/tests/fixtures/profile_registry/.../*.toml`, key `curated.minimum_manual_fsv`) and the `profile_quality` tool (`manual_fsv_evidence_ref`). These record the minimal manual-FSV checklist a curated profile must pass; the Rust tests only assert that these fields exist/parse, not that FSV was performed.

So FSV is a **process/gate**, surfaced in code only as manifest metadata and evidence references.

---

## 4. End-to-end (StdioMcpClient) tests

`StdioMcpClient` (`crates/synapse-test-utils/src/stdio_mcp_client.rs`) is a minimal raw JSON-RPC stdio client. It:
- Resolves the binary from `SYNAPSE_MCP_BIN` or `CARGO_BIN_EXE_synapse-mcp` (`mcp_binary_path`).
- Spawns `synapse-mcp --mode stdio` with `stdin/stdout/stderr` piped; sets `SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY=1`, `SYNAPSE_LOG_LEVEL=debug`.
- Defaults a synthetic foreground via `SYNAPSE_MCP_SYNTHETIC_FIXTURE=notepad` (so action-gated tools have a deterministic foreground) unless the caller sets one of `SYNAPSE_MCP_SYNTHETIC_FIXTURE` / `SYNAPSE_MCP_FORCE_NO_PERCEPTION` / `SYNAPSE_MCP_FORCE_OBSERVE_INTERNAL` / `SYNAPSE_MCP_FORCE_NO_FOREGROUND`.
- Defaults `SYNAPSE_DB` to a fresh `tempfile::TempDir` unless the caller supplies `SYNAPSE_DB`.
- `launch_and_init()` performs the MCP `initialize` handshake (protocolVersion `2025-11-25`) and asserts `serverInfo.name == "synapse-mcp"`.
- 10 s per-request timeout; helpers `tools_list`, `tools_call`, `tools_call_error`, `request`, `request_error`, `notify`, `shutdown`, `send_sigint_and_wait` (unix only), `stderr_tail`. `Drop` force-kills the child.

Used by ~45 files, all under `crates/synapse-mcp/tests/` (plus the client's own source). Representative e2e integration files and what each drives (one line, approx test count):

| File (`crates/synapse-mcp/tests/`) | Drives | ~tests |
|---|---|---|
| `cli_modes.rs` | `--mode` CLI selection / startup | 9 |
| `health_tools_list.rs` | `health` + `tools/list` baseline | 1 |
| `m3_tools_list.rs`, `m4_tools_list.rs` | tool catalogue per milestone | 1 each |
| `m2_act_stroke_tool.rs`, `m2_act_stroke_unified_motion_semantics.rs` | `act_*` stroke input semantics | 2 / 1 |
| `m2_notepad_type_save.rs` | full Notepad type+save (Windows-gated, `#[ignore]`) | 4 |
| `m3_timeline_*`, `m3_episode_*`, `m3_intent_*`, `m3_reflex_*`, `m3_routine_*` | M3 timeline/episode/intent/reflex/routine tools | 1–2 each |
| `m3_audio_tail_tool.rs`, `m3_audio_transcribe_tool.rs` | audio tools (use `tests/fixtures/audio` WAVs) | 1 each |
| `m3_hygiene_report_tool.rs`, `m3_data_cleaning_tool.rs`, `m3_permissions_tool.rs` | hygiene / cleaning / permissions | 1 each |
| `m3_profile_tools.rs`, `m5_profile_quality_tool.rs`, `m5_curated_registry_tool.rs`, `m5_registry_report_tool.rs` | profile registry / quality / curated registry | 1–6 each |
| `m4_agent_tasks_tool.rs`, `m4_agent_templates_tool.rs`, `m4_agent_query_tool.rs`, `m4_task_dispatch_tool.rs`, `agent_stats_tool.rs` | M4 agent/task tools | 1 each |
| `m4_no_foreground_action_gate.rs` | action gate denial when no foreground | 2 |
| `multi_agent_capability_matrix.rs` | multi-agent capability matrix tool | 1 |
| `notify_human_toast.rs` | human notification / toast | 3 |
| `drop_kills_child.rs`, `sigint_clean_exit.rs` | process lifecycle (drop kills child; SIGINT clean exit, unix) | 1 / 2 |
| `lifecycle_windows.rs` | Windows daemon lifecycle (incl. WSL parent test, `#[ignore]`) | 5 |
| `m0_demo_gate.rs` | demo gate | 1 |

---

## 5. How to run tests

No `Makefile`, no `justfile`, no `.config/nextest.toml`, no `.github/` workflows exist in the repo (confirmed by `Glob`). Cargo is the only runner.

From `CONTRIBUTING.md`:

```bash
cargo build --workspace
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

| Goal | Command |
|---|---|
| All tests | `cargo test --workspace` |
| One crate | `cargo test -p synapse-mcp` |
| One integration file | `cargo test -p synapse-mcp --test m3_tools_list` |
| One test by name | `cargo test -p synapse-core types` |
| Include `#[ignore]` tests | `cargo test --workspace -- --ignored` |
| List tests without running | `cargo test -p <crate> -- --list` |
| Lint gate (what pre-push runs) | `cargo clippy --workspace --all-targets` |

**Build prerequisite (from MEMORY):** the workspace links `librocksdb-sys`; the build/test compile can fail with `STATUS_DLL_NOT_FOUND` unless `libclang.dll` (VS BuildTools `VC\Tools\Llvm\x64\bin`) is on `PATH`.

**Pre-push gate** (`.githooks/pre-push`, enabled once via `git config core.hooksPath .githooks`): runs `cargo clippy --workspace --all-targets` on any push touching `.rs`/`Cargo.*`, blocks on failure, skips docs-only pushes. Warning-only clippy output is accepted by policy, captured to a local Git-private log, and summarized with a warning count so push output distinguishes accepted warnings from deny-level failures. It deliberately does **not** run `cargo test --workspace` (too slow). Bypass: `git push --no-verify`.

### Test-relevant environment variables (from `StdioMcpClient`)

| Env var | Effect |
|---|---|
| `SYNAPSE_MCP_BIN` | Override path to the `synapse-mcp` binary used by e2e tests |
| `CARGO_BIN_EXE_synapse-mcp` | Set by cargo for `synapse-mcp` integration tests; binary path |
| `SYNAPSE_DB` | Override DB dir (else a fresh temp dir per client) |
| `SYNAPSE_MCP_SYNTHETIC_FIXTURE` | Inject a synthetic foreground (default `notepad`) |
| `SYNAPSE_MCP_FORCE_NO_PERCEPTION` / `_FORCE_OBSERVE_INTERNAL` / `_FORCE_NO_FOREGROUND` | Override perception/foreground state for a test |
| `SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY` | Set to `1` by the client to avoid grabbing the operator hotkey |
| `SYNAPSE_LOG_DIR` / `SYNAPSE_LOG_LEVEL` | Log routing (client sets level `debug`) |

---

## 6. Fixtures and helpers

| Fixture / helper | Location | Purpose |
|---|---|---|
| `StdioMcpClient` | `crates/synapse-test-utils/src/stdio_mcp_client.rs` | Raw JSON-RPC stdio MCP client for e2e (see §4) |
| `launch_notepad()` / `NotepadHandle` | `crates/synapse-test-utils/src/fixtures.rs` | Launches a fixture-owned `notepad.exe`, waits for the expected window title (regex `^(?:Untitled - Notepad|Notepad)$`, 5 s timeout, 20 ms poll), handles Win11 session-restore via a Ctrl+N retry, and cleans up only the recorded fixture-owned UI/launcher PIDs via WM_CLOSE → taskkill → CIM `Win32_Process.Terminate`. Non-Windows build returns a stub that fails closed. |
| `wait_for_window_title_regex` | `crates/synapse-test-utils/src/fixtures.rs` | Poll for a window whose title matches a regex |
| Window-selection helpers | `crates/synapse-test-utils/src/fixtures.rs` | `select_window_title_match`, `select_new_notepad_window`, `is_notepad_window_title` — pure functions, unit-tested cross-platform |
| Audio WAVs | `tests/fixtures/audio/*.wav` (+ `README.md`) | Deterministic synthetic WAVs for STT/direction/transient tests: `hello_world_5s.wav` ("Hello world. This is Synapse."), `loud_transient_1s.wav`, `pan_minus60_0_plus60.wav`. Each documented with format + SHA-256. |
| Profile-registry fixtures | `crates/synapse-profiles/tests/fixtures/profile_registry/**` | TOML manifests + JSON CF rows for curated-registry / package-manifest / governance tests; include `minimum_manual_fsv` metadata |
| MCP snapshot fixtures | `crates/synapse-mcp/tests/snapshots/*.snap`, `crates/synapse-mcp/tests/fixtures/claude_stream_real.jsonl` | Golden snapshots + a recorded Claude stream for agent-event parsing |

---

## 7. Test gating

| Gate | Mechanism | Examples |
|---|---|---|
| **Windows-only** | `#[cfg(windows)]` on test fns / `mod platform` split | `fixtures.rs` Windows vs non-Windows `platform` module; ~68 Windows-gated test sites (`Grep`) |
| **Non-Windows fail-closed** | tests that assert the Windows-only path errors off-Windows | `synapse-action/tests/software_non_windows.rs`; `fixtures.rs` `launch_notepad_fails_closed_off_windows` |
| **`#[ignore]` (manual / hardware)** | 28 `#[ignore]` attributes across 13 files (`Grep`) | `a11y/tests/uwp_snapshot_regression.rs` (interactive desktop), `action/tests/vigem_xinput.rs` (ViGEmBus + XInput), `action/tests/auto_release_keyboard_hook.rs` (real keyboard hook + 30 s timer), `perception/tests/perception_regression.rs` (WinRT OCR), `mcp/tests/m2_notepad_type_save.rs` (Notepad+UIA), `mcp/tests/lifecycle_windows.rs` (configured Ubuntu-24.04 WSL distro) |
| **unix-only** | `#[cfg(unix)]` | `StdioMcpClient::send_sigint_and_wait`, `sigint_clean_exit.rs` |
| **env-gated behavior** | tests set `SYNAPSE_*` env to steer the daemon (not skip) | `lifecycle_windows.rs` sets `SYNAPSE_BEARER_TOKEN=...`; perception-force flags via `StdioMcpClient` |

`#[ignore]` tests are excluded from `cargo test --workspace` by default; run them explicitly with `-- --ignored` on a host that meets the stated requirement (interactive Windows desktop, ViGEmBus, WinRT OCR, WSL distro, etc.).

---

## 8. What is NOT covered (gaps observed from source)

- **Real Windows perception/action paths are not exercised in the default suite.** The heavyweight Windows behaviors (Notepad type+save, UIA snapshots, WinRT OCR, ViGEm XInput, real keyboard hook) are all `#[ignore]`d, so `cargo test --workspace` does not validate them. They depend on manual FSV (see §3.1).
- **Non-Windows is only "fail-closed" tested.** On non-Windows, the Windows surfaces have stub implementations whose tests merely assert they error ("requires Windows"); actual perception/action is untested off-Windows (and unsupported).
- **No CI.** GitHub Actions are forbidden (#351); the only automated gate is the local pre-push clippy hook, which does **not** run tests. Test execution depends on the developer/agent running `cargo test --workspace` manually.
- **e2e tests pin a synthetic foreground.** `StdioMcpClient` defaults `SYNAPSE_MCP_SYNTHETIC_FIXTURE=notepad`, so e2e action-gate tests run against an injected (not real) foreground — real foreground/SoT behavior is verified only by manual FSV.
- **FSV itself is unautomated by design.** Manifests carry `minimum_manual_fsv` metadata, but no Rust test performs the FSV checklist; tests only assert the metadata parses.
- **No workspace-level integration crate.** `C:\code\synapse\tests\` holds fixtures only; there is no cross-crate end-to-end runner outside `synapse-mcp`.

See [18_verification_report.md](18_verification_report.md) for verification-status reporting.
