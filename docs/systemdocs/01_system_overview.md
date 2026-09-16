# 01. System Overview

**Source files covered:**
- `Cargo.toml` (workspace root), `clippy.toml`, `deny.toml`
- `README.md`
- `crates/synapse-mcp/src/main.rs`, `server.rs`, `m1.rs`, `m2.rs`, `m3.rs`, `m4.rs`, `single_instance.rs`, `daemon_lifecycle.rs`
- `crates/synapse-core/src/lib.rs`, `defaults.rs`, `error_codes.rs`
- The 14 workspace crates' `lib.rs` / `main.rs` module declarations
- Synthesized from companion documents 02–17 in this series

> Scope note: every claim below is derived from source in the `C:\code\synapse` workspace. Where a fact is documented in detail elsewhere in this series it is cross-referenced rather than repeated.

---

## 1. What the system is

Synapse is a local **Model Context Protocol (MCP) server**, written in Rust (edition 2024, `rust-version = 1.95`), that gives an LLM agent a structured interface to a **Windows** PC: it perceives the screen (capture + OCR + object detection + accessibility tree), emits human-like mouse/keyboard/gamepad input, runs sub-millisecond reflex loops, drives the browser over the Chrome DevTools Protocol, and orchestrates a fleet of agents that share one machine. It speaks MCP over stdio and over HTTP/SSE, and is designed to plug into Claude Code, Codex, and the Claude Desktop app (`README.md`).

The system is structured as a Cargo workspace of **14 crates** (`Cargo.toml`). `synapse-mcp` is the server/daemon and the sink of the dependency graph; `synapse-core` is the shared-vocabulary root with no internal dependencies. See [02_source_code_map.md](02_source_code_map.md) for the full crate tree and dependency graph.

---

## 2. Runtime / process & port topology

The product runs as a single canonical daemon plus subordinate processes spawned by CLI `--mode`. The daemon enforces single-instance ownership via an `fs2` advisory lock on `<db>/daemon.lock` (PID sidecar `daemon.pid`); a duplicate launch exits with code 3 and is told to use `--mode connect` (`crates/synapse-mcp/src/single_instance.rs`, `main.rs`).

| Process / role | How launched | Transport / port | Technology | Purpose |
|---|---|---|---|---|
| MCP daemon (stdio) | `--mode stdio` (default) | stdio JSON-RPC | `rmcp` 1.7.0 | Primary MCP server for a directly-attached client |
| MCP daemon (HTTP) | `--mode http` | `127.0.0.1:7700` (`DEFAULT_BIND`) | axum 0.8 + `rmcp` StreamableHttpService at `/mcp`; SSE at `/events` | Shared daemon for multiple agents; bearer-token auth |
| Connect shim | `--mode connect` | → 7700 | — | Bridges a stdio client into the already-running shared daemon |
| Desktop worker | `--mode desktop-worker` | IPC to daemon | Win32 | Runs in an interactive desktop session for input/capture |
| Local agent | `--mode local-agent` | IPC | — | Hosts a locally-spawned sub-agent |
| Chrome native host | `--mode chrome-native-host` (bin `synapse-chrome-native-host`) | Chrome native messaging (stdio) | MV3 extension bridge | Backs the CDP bridge for the installed Chrome extension |
| Approval protocol helper | `--mode approval-protocol` | URI handler | — | Handles `synapse://` approval callbacks |
| Doctor | `--mode doctor` | — | — | Diagnostics; kills stray `synapse-mcp` processes |
| Overlay | separate binary `synapse-overlay` | polls daemon HTTP every 2 s | Win32 `Shell_NotifyIconW` tray | System-tray status companion (recorder/demo/approvals/sessions/lease) |

Default `--mode` is `Stdio` (env `SYNAPSE_MODE`). See [15_mcp_server_architecture.md](15_mcp_server_architecture.md) for the entry-point trace, transport details, and lifecycle; [03_configuration.md](03_configuration.md) for ports, binds, and env vars.

---

## 3. Technology stack

| Layer | Technology | Version / constraint (from `Cargo.toml`) |
|---|---|---|
| Language / edition | Rust | edition 2024, rust-version 1.95 |
| Async runtime | tokio (full) | 1.52.3 |
| MCP protocol | `rmcp` (server, transport-io, streamable-http-server, macros, schemars) | 1.7.0 |
| HTTP server | axum (ws) / hyper / tower | 0.8.9 / 1.9.0 / 0.5.3 |
| Serialization | serde / serde_json / toml | 1.0.228 / 1.0.150 / 1.1.2 |
| Errors | thiserror / anyhow | 2.0.18 / 1.0.102 |
| Windows APIs | `windows` crate (Win32 Foundation, UI Automation, HiDPI, WindowsAndMessaging, Input KeyboardAndMouse/XboxController, DXGI, Graphics.Capture, JobObjects, Registry, StationsAndDesktops, …) | 0.62.2 |
| Storage | RocksDB (`rocksdb`, lz4/zstd, multi-threaded CF) | see [04_storage_and_persistence.md](04_storage_and_persistence.md) |
| ML inference | ONNX Runtime via `ort` | 2.0.0-rc.12 (feature api-24) |
| Metrics | `metrics` + `metrics-exporter-prometheus`; `opentelemetry` / `opentelemetry-otlp` | 0.24.6 / 0.18.3 / 0.32.0 |
| Tracing/logging | tracing + tracing-subscriber (env-filter, json) + tracing-appender | 0.1.44 / 0.3.23 / 0.2.5 |
| Concurrency | crossbeam, arc-swap | 0.8.4 / 1.9.1 |
| Clipboard | arboard | 3.6.1 |

Lint posture: `clippy::unwrap_used` / `clippy::expect_used` are **denied workspace-wide** in production paths, allowed only in test code (`clippy.toml`, `[workspace.lints.clippy]`). Dependency/license gating via `cargo-deny` (`deny.toml`).

---

## 4. Milestone layering (tool families)

Tools and subsystems are organized into milestones (`crates/synapse-mcp/src/m1.rs`–`m4.rs`):

| Milestone | Theme | Representative capability |
|---|---|---|
| **M1** | Perception | observe/screenshot, OCR (`read_text`), object detection, accessibility snapshot, find/locate |
| **M2** | Action | human-like mouse/keyboard input emission, click/type/scroll/press, set-field-text |
| **M3** | Memory & control | reflexes, episodes/timeline, intents/plans/routines, profiles, approvals, suggestions, audio, agent orchestration |
| **M4** | Effector | shell execution, app launch, sub-agent spawning |

See [16_api_tools_reference.md](16_api_tools_reference.md) for every tool grouped by domain.

---

## 5. Public surface (MCP tools)

The server registers MCP tools as `rmcp` `#[tool(...)]` functions on `SynapseService`, summed across per-module `#[tool_router]` impls in `tool_router()` (`crates/synapse-mcp/src/server.rs`). Client-visible names are `mcp__synapse__<fn_name>`. Domain groups (full reference in [16_api_tools_reference.md](16_api_tools_reference.md)):

| Domain | Examples |
|---|---|
| Perception / M1 | `observe`, `observe_delta`, `read_text`, `find`, `capture_screenshot`, `set_target`, `set_perception_mode` |
| Action / M2 | `target_act` (verb router for click/type/scroll/press/drag), set-field/value |
| Action / M4 | `act_run_shell` (+ start/status/cancel), `act_launch`, `act_spawn_agent` |
| Reflex / M3 + audio | reflex install/list/stop, `read_text`, audio capture/STT |
| Profiles / registry / authoring | `profile_list`, registry & quality tools |
| Hygiene / local models | `hygiene_scan_text`/`scan_storage`/`flags`, `local_model_*` |
| Storage | `storage_inspect`, debug probe tools (gated) |
| Approvals / escalation | `approval_request`/`decide`/`gate`/`list`, `escalation_*` |
| Agents & orchestration | `agent_spawn`/`kill`/`pause`/`resume`/`steer`/`send`/`wait`, cost/stats/query/templates/tasks/sessions/leases/claims, `fleet_stop` |
| Browser / CDP | `browser_*` (navigate, evaluate, snapshot, network, emulation, dialog, drag/drop, storage, frames, …) |
| Intent / plans / routines | `intent_*`, `plan_*`, `armed_routine_*`, `suggestion_*` |
| Reality / timeline | `reality_audit`/`baseline`, `timeline_get`/`search`/`digest`/`stats` |
| Workspace blackboard | `workspace_get`/`put`/`list`/`subscribe` |

Total registered tool macros: **238** `#[tool(` occurrences (the reference doc enumerates **206** distinct client-exposed tools after accounting for feature-gated/debug tools; the README badge value of 81 is stale). See [18_verification_report.md](18_verification_report.md).

---

## 6. Entry points

| Entry point | Crate | Invocation | Notes |
|---|---|---|---|
| `synapse-mcp` daemon | `crates/synapse-mcp/src/main.rs` | `synapse-mcp [--mode <stdio\|http\|connect\|doctor\|desktop-worker\|local-agent\|chrome-native-host\|approval-protocol>]` | Default mode stdio; 8 mode branches |
| `synapse-overlay` | `crates/synapse-overlay/src/main.rs` | `synapse-overlay` | Win32 system-tray companion |
| `synapse-chrome-native-host` | `crates/synapse-mcp/src/bin/synapse-chrome-native-host.rs` | launched by Chrome native messaging | CDP bridge backend |

`default-members` in the workspace are `synapse-mcp` and `synapse-overlay`. Setup/installation is scripted via `scripts/synapse-setup.ps1` and `scripts/install-synapse-chrome-debugger.ps1`.

---

## 7. Storage / data tiers

Data lives under `%LOCALAPPDATA%\synapse` (db, logs, models, replays, runs); the daemon auth token at `%APPDATA%\synapse\token.txt` (see [03_configuration.md](03_configuration.md)). The store is RocksDB with **17 named column families** plus the implicit `default` CF; values are JSON; storage `SCHEMA_VERSION = 1`. Full schema in [04_storage_and_persistence.md](04_storage_and_persistence.md).

| Tier | What | Behavior |
|---|---|---|
| Sacred | user profiles (TOML on disk), registered models | not regenerated by the system |
| Regenerable | RocksDB timeline/episodes/agent events, OCR/detection caches | TTL compaction + byte-budget GC (25%/pass) + 5-level disk-pressure write-shedding |
| Ephemeral | in-memory rings (audio 30 s), capacity-2 frame channel, replay temp | dropped on restart |

---

## 8. Error hierarchy

Each crate defines a `thiserror` error enum, surfaced to clients through a shared **error-code catalog** (`crates/synapse-core/src/error_codes.rs`, ~120 codes across 9 groups: perception, action, reflex, profile/config, MCP/session, storage, models, notifications, safety). Per-crate base error types:

| Crate | Error type(s) |
|---|---|
| synapse-core | error-code catalog (string codes) |
| synapse-capture | `CaptureError` (e.g. `CAPTURE_GRAPHICS_API_UNSUPPORTED`) |
| synapse-a11y | a11y error (e.g. `A11Y_NOT_AVAILABLE`) |
| synapse-perception | `PerceptionError` |
| synapse-audio | `AudioError` |
| synapse-action | `ActionError` |
| synapse-reflex | `ReflexError` |
| synapse-storage | `StorageError` |
| synapse-profiles | `ProfileError`, `ProfileLoadError` |
| synapse-models | model error enum (download/verify/session) |

See [14_core_telemetry_overlay.md](14_core_telemetry_overlay.md) for the full code catalog.

---

## 9. Subsystem summaries

| Subsystem (crate) | What it does | Key algorithm / fact | Doc |
|---|---|---|---|
| **Capture** (`synapse-capture`) | Windows screen/window capture | WGC (Windows.Graphics.Capture) + DXGI Desktop Duplication backends, `Auto` falls WGC→DXGI; BGRA8 D3D11 textures over a capacity-2 drop-oldest channel; GDI for one-shot grabs. Windows-only; non-Windows fails loud. | [05](05_capture_subsystem.md) |
| **Accessibility & CDP** (`synapse-a11y`) | Native UIA tree + Chrome DevTools Protocol | Windows UI Automation via `uiautomation` (MTA worker thread, RuntimeId/FNV ids); CDP domains: target lifecycle, DOM/AX, input actions, actionability, runtime bindings, clock, console, dialog, emulation, page lifecycle, network/fetch. | [06](06_accessibility_and_cdp_subsystem.md) |
| **Perception** (`synapse-perception`) | OCR, object detection, HUD detection | OCR via `Windows.Media.Ocr` (WinRT; WSL shells to host PowerShell); template match = grayscale normalized cross-correlation (NCC) clamped [-1,1]; slotted counter extraction (10 slots, min conf 0.85). | [07](07_perception_subsystem.md) |
| **Audio** (`synapse-audio`) | Loopback capture + STT + direction | WASAPI loopback (48 kHz/f32/stereo, MMCSS Pro Audio thread, 30 s ring); STT = Whisper tiny INT8 ONNX (16 kHz mono, English, greedy). | [08](08_audio_subsystem.md) |
| **Action** (`synapse-action`) | Human-like input emission | Win32 `SendInput` + `SetPhysicalCursorPos`, ViGEm virtual gamepad; deterministic SplitMix64-seeded Bézier/min-jerk paths, Fitts'-law durations, WindMouse strokes, Gaussian inter-key timing with 0.75× bigram speedup; token-bucket rate limits, input lease, panic hotkey. | [09](09_action_subsystem.md) |
| **Reflex** (`synapse-reflex`) | Sub-ms event-driven control loops | 7 kinds (aim_track, combo, hold_button, hold_move, on_event, path_follow, + hold_lifetime); 1 ms MMCSS waitable-timer scheduler thread (2 ms tokio fallback); aim_track EMA; starvation after 2 s. | [10](10_reflex_subsystem.md) |
| **Profiles** (`synapse-profiles`) | Per-app/game profile matching | TOML profiles, `deny_unknown_fields`; foreground-window match ranked exe > title_regex > steam_appid > window_class, tie-break newest mtime; 200 ms debounced hot-reload, fail-open. | [11](11_profiles_subsystem.md) |
| **Models** (`synapse-models`) | ONNX model registry/verify/session | 1 registered model (RT-DETRv2-S COCO); SHA-256 streaming verify (64 KiB chunks); EP order CUDA→DirectML→CPU; downloads disabled in M1 (manual side-load to `%LOCALAPPDATA%\synapse\models`). | [13](13_models_subsystem.md) |
| **Core/Telemetry/Overlay** (`synapse-core`, `-telemetry`, `-overlay`) | Shared types, metrics, tray UI | ~120 error codes; 19 Prometheus metrics (12 counters/5 gauges/2 histograms); overlay = Win32 tray polling daemon every 2 s. | [14](14_core_telemetry_overlay.md) |
| **MCP server** (`synapse-mcp`) | Daemon, transports, orchestration | stdio + HTTP/SSE (rmcp 1.7.0, axum); single-instance fs2 lock on port 7700; permission gate + target claims + approvals; `target_act` verb router; agent fleet lifecycle. | [15](15_mcp_server_architecture.md) |

---

## 10. What is NOT covered by the system

- **Non-Windows runtime.** Real capture, action, accessibility, and audio are Windows-only; non-Windows builds compile but the corresponding subsystems fail loud (`CAPTURE_GRAPHICS_API_UNSUPPORTED`, `A11Y_NOT_AVAILABLE`). See [05](05_capture_subsystem.md), [06](06_accessibility_and_cdp_subsystem.md).
- **Automated model download.** Disabled in the current milestone; models must be side-loaded manually. See [13](13_models_subsystem.md).
- **CI test execution.** No GitHub Actions/Makefile/justfile/nextest; the only automated gate is a `.githooks/pre-push` clippy run, and the shipping gate ("FSV" = manual Full State Verification) is performed by the agent on the configured Windows host and is never automated. See [17_test_suite.md](17_test_suite.md).
- **Multi-language OCR confidence.** Per-word OCR confidence is hardcoded to 1.0. See [07](07_perception_subsystem.md).
