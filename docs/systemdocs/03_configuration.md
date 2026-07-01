# 03. Configuration

**Source files covered:**
- `crates/synapse-mcp/src/main.rs` (CLI parser, mode dispatch, telemetry init, env list parsing)
- `crates/synapse-mcp/src/m3.rs` (`M3ServiceConfig`, `default_db_path`, audio loopback, env constants)
- `crates/synapse-mcp/src/m2/config.rs` (`M2ServiceConfig`)
- `crates/synapse-mcp/src/m4.rs` (shell/launch allowlist env, shell session/working dirs, run-shell inline await)
- `crates/synapse-mcp/src/m1.rs`, `crates/synapse-mcp/src/m1/sources.rs` (perception force-toggles, timeline source env)
- `crates/synapse-capture/src/config.rs` (`CaptureConfig`)
- `crates/synapse-telemetry/src/lib.rs` (tracing/subscriber init, log dir, GC)
- `crates/synapse-mcp/src/http/auth.rs`, `crates/synapse-mcp/src/http/session.rs`, `crates/synapse-mcp/src/http/sse.rs`
- `crates/synapse-mcp/src/safety.rs`, `crates/synapse-action/src/hotkey.rs`, `crates/synapse-action/src/recovery.rs`
- `crates/synapse-mcp/src/m3/activity_recorder.rs`, `m3/intent_events.rs`, `m3/intent.rs`, `m3/routine_miner_job.rs`, `m3/suggestions.rs`, `m3/timeline_control.rs`, `m3/permissions.rs`
- `crates/synapse-mcp/src/server/ambient_agents.rs`, `server/agent_transcripts.rs`, `server/suggestions.rs`, `server/permission_gate.rs`, `server/codex_app_server_bridge.rs`, `server/target_policy.rs`, `server/m4_tools.rs`
- `crates/synapse-mcp/src/chrome_debugger_bridge.rs`, `crates/synapse-mcp/src/bin/synapse-chrome-native-host.rs`
- `crates/synapse-models/src/download.rs`, `crates/synapse-a11y/src/cdp.rs`, `crates/synapse-overlay/src/main.rs`
- `scripts/synapse-setup.ps1`

Synapse has **no TOML/JSON config file** for the daemon itself. All configuration is via (1) CLI flags, (2) environment variables, and (3) on-disk locations derived from `%LOCALAPPDATA%` / `%APPDATA%`. The one file read for config is the bearer-token file `%APPDATA%\synapse\token.txt`. Many flags accept a matching `env =` fallback through `clap`.

See [04_storage_and_persistence.md](04_storage_and_persistence.md) for the RocksDB store, and [15_mcp_server_architecture.md](15_mcp_server_architecture.md) for the server/daemon model.

---

## 1. Configuration sources and load/merge order

The daemon binary is `synapse-mcp` (`crates/synapse-mcp/src/main.rs`). Config is assembled in this precedence (highest wins):

| Order | Source | Notes |
|------|--------|-------|
| 1 | **CLI flag** (explicit `--flag`) | Parsed by `clap` (`Cli` struct, `main.rs`). |
| 2 | **Env var bound to that flag** | `clap` `env = "..."` attribute; used only if the flag is absent. |
| 3 | **Env var read directly** (modules calling `std::env::var`) | For settings with no CLI flag (most M3 tuning, audio, timeline, intent, etc.). |
| 4 | **Compiled default** | `default_value` / `default_value_t` on the flag, or a module `DEFAULT_*` constant. |

Two special merge rules:
- **`SYNAPSE_ALLOW_SHELL` / `SYNAPSE_ALLOW_LAUNCH`** are merged *additively* with their repeatable CLI flags: env (comma-separated) entries are parsed first, then CLI `--allow-shell`/`--allow-launch` entries are appended (`Cli::m4_config`, `parse_env_list`, `main.rs`).
- **`allow_unknown_profile`** is the inverse of the `--restrict-unknown-profile` flag / `SYNAPSE_RESTRICT_UNKNOWN_PROFILE` env, and is *permissive by default* (unknown apps actionable) unless explicitly restricted (`m3.rs`, `M3ServiceConfig`).

Config is split per "milestone" service struct, each with `from_env()` / `from_cli_parts()`:
- `M1State::from_env()` (`m1.rs`) — perception toggles.
- `M2ServiceConfig::from_env()` (`m2/config.rs`) — recording backend.
- `M3ServiceConfig` (`m3.rs`) — daemon bind, db, profiles, audio, permissions, reflex.
- `M4ServiceConfig` (`m4.rs`) — shell/launch allowlists.

---

## 2. CLI flags (binary `synapse-mcp`, `crates/synapse-mcp/src/main.rs`)

Each flag below has the listed `env` fallback. Type is the parsed Rust type.

| Flag | Env fallback | Type | Default | Description |
|------|--------------|------|---------|-------------|
| `--mode` | `SYNAPSE_MODE` | enum | `stdio` | One of `stdio`, `http`, `connect`, `chrome-native-host`, `approval-protocol`, `desktop-worker`, `doctor`, `local-agent`. |
| `--bind` | `SYNAPSE_BIND` | string | `127.0.0.1:7700` | HTTP server bind address (host:port). Also the single-daemon port. |
| `--allow-non-loopback` | `SYNAPSE_ALLOW_NON_LOOPBACK` | bool | `false` | Permit binding/serving on a non-loopback address. |
| `--db` | `SYNAPSE_DB` | path | (derived, see §4) | RocksDB store path. |
| `--profile-dir` | `SYNAPSE_PROFILE_DIR` | path | bundled dir (see [11]) | Profile package directory. See profiles doc (11) for layout. |
| `--log-level` | `SYNAPSE_LOG_LEVEL` | string | `info` | Tracing level (`off`/`error`/`warn`/`info`/`debug`/`trace`). Parsed as `LevelFilter`. |
| `--reflex-disabled` | `SYNAPSE_REFLEX_DISABLED` | bool | `false` | Disable the reflex runtime. |
| `--kill-stray` | — | bool | `false` | In `--mode doctor`, kill stray `synapse-mcp` processes. |
| `--enable-audio` | `SYNAPSE_ENABLE_AUDIO` | bool | `false` | Enable audio capture runtime. |
| `--restrict-unknown-profile` | `SYNAPSE_RESTRICT_UNKNOWN_PROFILE` | bool | `false` | Fail closed on unprofiled foreground apps. Default off = unknown apps actionable. |
| `--allowed-permissions` | `SYNAPSE_MCP_ALLOWED_PERMISSIONS` | string (LIST) | none | Restrict granted permission set. |
| `--reflex-force-degraded` | `SYNAPSE_REFLEX_FORCE_DEGRADED` | bool | `false` | Force reflex into degraded mode (testing/diagnostic). |
| `--storage-pressure-free-bytes-sample` | `SYNAPSE_STORAGE_PRESSURE_FREE_BYTES_SAMPLE` | u64 (BYTES) | none | Inject a synthetic free-bytes value for disk-pressure logic. |
| `--max-subscriptions` | `SYNAPSE_MAX_SUBSCRIPTIONS` | NonZeroUsize | `synapse_reflex::DEFAULT_MAX_SUBSCRIPTIONS_NONZERO` | Cap on event-bus subscriptions. |
| `--allow-shell` (repeatable) | `SYNAPSE_ALLOW_SHELL` (comma-sep) | regex list | empty | Allowlist regexes for `act_run_shell` command lines (merged with env, §1). |
| `--allow-launch` (repeatable) | `SYNAPSE_ALLOW_LAUNCH` (comma-sep) | regex list | empty | Allowlist regexes for `act_launch` targets (merged with env, §1). |
| `--run-shell-inline-await-limit-ms` | `SYNAPSE_RUN_SHELL_INLINE_AWAIT_LIMIT_MS` | u64 (ms) | `90000` (`m4::DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS`) | Inline await budget before `act_run_shell` returns a durable job handle. `0` backgrounds every request. |
| `--chrome-native-origin` | `SYNAPSE_CHROME_NATIVE_ORIGIN` | string | `chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/` | Origin for `--mode chrome-native-host` diagnostics. |
| `--approval-uri` | — | string (hidden) | none | Internal: approval-protocol child activation URI. |
| `--desktop-worker-*` (op/hwnd/region/client-region/depth/json/bgra) | — | various (hidden) | none | Internal: hidden-desktop worker child args. |
| `--local-agent-model` | `SYNAPSE_LOCAL_AGENT_MODEL` | string | none | Registry model name for `--mode local-agent`. |
| `--local-agent-task` | `SYNAPSE_LOCAL_AGENT_TASK` | string | none | Inline task text. |
| `--local-agent-task-file` | `SYNAPSE_LOCAL_AGENT_TASK_FILE` | path | none | Task text from file. |
| `--local-agent-mcp-url` | `SYNAPSE_LOCAL_AGENT_MCP_URL` | string | `http://127.0.0.1:7700/mcp` | MCP endpoint the local agent connects to. |
| `--local-agent-spawn-id` | `SYNAPSE_LOCAL_AGENT_SPAWN_ID` | string | none | Spawn correlation id. |
| `--local-agent-log-dir` | `SYNAPSE_LOCAL_AGENT_LOG_DIR` | path | none | Local-agent log directory. |
| `--local-agent-target-json` | `SYNAPSE_LOCAL_AGENT_TARGET_JSON` | string (JSON) | none | Target spec JSON. |
| `--local-agent-max-turns` | `SYNAPSE_LOCAL_AGENT_MAX_TURNS` | u32 | `40` | Max model turns. |
| `--local-agent-timeout-ms` | `SYNAPSE_LOCAL_AGENT_TIMEOUT_MS` | u64 | `120000` | Per-turn wall-clock timeout. |
| `--local-agent-hold-open-ms` | `SYNAPSE_LOCAL_AGENT_HOLD_OPEN_MS` | u64 | `0` | Hold connection open after completion. |
| `--local-agent-context-char-limit` | `SYNAPSE_LOCAL_AGENT_CONTEXT_CHAR_LIMIT` | usize | `120000` | Context window char cap. |
| `--local-agent-tool-parse-retry-limit` | `SYNAPSE_LOCAL_AGENT_TOOL_PARSE_RETRY_LIMIT` | u32 | `2` | Retries on tool-call parse failure. |
| `--local-agent-no-stream` | `SYNAPSE_LOCAL_AGENT_NO_STREAM` | bool | `false` | Disable streaming responses. |
| `--local-agent-allow-non-loopback` | `SYNAPSE_LOCAL_AGENT_ALLOW_NON_LOOPBACK` | bool | `false` | Allow non-loopback model base URL. |
| `--local-agent-trusted-unattended-exact-contract` | `SYNAPSE_LOCAL_AGENT_TRUSTED_UNATTENDED_EXACT_CONTRACT` | bool | `false` | Trusted unattended exact-contract mode. |

---

## 3. Environment variables (read directly via `std::env::var`/`var_os`)

Variables already listed as CLI `env` fallbacks in §2 are not repeated here. The following are read directly (no CLI flag), grouped by area. "Truthy" parsing accepts `1/true/yes/on`; "falsey" `0/false/no/off` (per-module; some accept only `1`/`true`).

### 3.1 Network / auth / HTTP

| Name | Read in | Type | Default | Description |
|------|---------|------|---------|-------------|
| `SYNAPSE_BEARER_TOKEN` | `m3.rs`, `http/auth.rs` (`TOKEN_ENV`), `chrome_debugger_bridge.rs`, `overlay/main.rs` | string | none | HTTP bearer token. Used only if `token.txt` is absent (see §4). |
| `APPDATA` | `http/auth.rs`, `chrome_debugger_bridge.rs`, `bin/synapse-chrome-native-host.rs` | path | (Windows-provided) | Root for `synapse\token.txt` and other roaming files. |
| `SYNAPSE_HTTP_SESSION_IDLE_TIMEOUT_SECS` | `http/session.rs` | u64 (s) | `86400` (24 h) | MCP HTTP session idle timeout. |
| `SYNAPSE_HTTP_SSE_MANUAL` | `http/sse.rs` (`MANUAL_ENV`) | bool (`1`/`true`) | unset (off) | Manual SSE test hook. |
| `SYNAPSE_ALLOW_NON_LOOPBACK` | (also CLI) | bool | `false` | See §2. |
| `SYNAPSE_TRAY_BASE_URL` | `overlay/main.rs` | string | `DEFAULT_BASE_URL` (overlay) | Base URL the tray/overlay talks to. |

### 3.2 Storage / paths

| Name | Read in | Type | Default | Description |
|------|---------|------|---------|-------------|
| `LOCALAPPDATA` | many (telemetry, m3, m4, models, etc.) | path | (Windows-provided) | Root for db/logs/models/runs/shell dirs (see §4). |
| `SYNAPSE_DB` | `m3.rs` (`DB_ENV`), `synapse-action/recovery.rs` | path | derived (§4) | RocksDB store path (also CLI `--db`). |
| `SYNAPSE_ACTION_RECOVERY_FILE` | `synapse-action/recovery.rs` (`RECOVERY_FILE_ENV`) | path | derived (§4) | Held-input crash-recovery JSONL ledger path. |
| `SYNAPSE_SHELL_SESSION_DIR` | `m4.rs` (`SHELL_SESSION_DIR_ENV`) | path | derived (§4) | Per-session shell working/session dir override. |
| `SYNAPSE_SHELL_WORKING_DIR` | `m4.rs` (`SHELL_WORKING_DIR_ENV`) | path | none | Working dir for shell jobs. |
| `XDG_STATE_HOME`, `HOME` | telemetry, m4 (non-Windows) | path | none | Non-Windows fallbacks for log/state dirs. |

### 3.3 Logging / telemetry

| Name | Read in | Type | Default | Description |
|------|---------|------|---------|-------------|
| `SYNAPSE_LOG_DIR` | `main.rs`, `bin/synapse-chrome-native-host.rs` | path | `default_log_dir()` (§4) | Override log directory. |
| `SYNAPSE_LOG_LEVEL` | `main.rs` (CLI/env), native host | string | `info` | File + console level (see §5). |
| `RUST_LOG` | `synapse-telemetry/src/lib.rs` | env-filter | console level directive | Overrides the **console** layer's filter (standard tracing `EnvFilter` syntax). |
| `SYNAPSE_LOG_GC_INTERVAL_S` | `synapse-telemetry/src/lib.rs` (`GC_INTERVAL_ENV`) | u64 (s) | configured interval; `0` disables | Periodic log-GC interval override. |

### 3.4 Perception / capture / a11y

| Name | Read in | Type | Default | Description |
|------|---------|------|---------|-------------|
| `SYNAPSE_CAPTURE_FORCE_DXGI` | `synapse-capture/config.rs` | string | unset (Auto backend) | Force the DXGI capture backend preference. |
| `SYNAPSE_MCP_SYNTHETIC_FIXTURE` | `m1.rs` | string | unset | `notepad` injects a synthetic observation fixture. |
| `SYNAPSE_MCP_FORCE_NO_PERCEPTION` | `m1.rs` | bool (`1`/`true`) | off | Force perception off. |
| `SYNAPSE_MCP_FORCE_OBSERVE_INTERNAL` | `m1.rs` | bool (`1`/`true`) | off | Force internal observe path. |
| `SYNAPSE_MCP_FORCE_NO_FOREGROUND` | `m1.rs` | bool (`1`/`true`) | off | Force no-foreground behavior. |
| `SYNAPSE_CDP_PORTS` | `synapse-a11y/cdp.rs` (`CDP_PORTS_ENV`) | comma list | `9222` (`DEFAULT_CDP_PORT`) | Chrome DevTools ports probed for attach. |
| `SYNAPSE_CDP_USER_DATA_DIR` | `m4.rs` | path | (Chrome default, §4) | Chrome `--user-data-dir` override for CDP launches. |

### 3.5 Timeline / activity recorder

| Name | Read in | Type | Default | Description |
|------|---------|------|---------|-------------|
| `SYNAPSE_TIMELINE_IDLE_TIMEOUT_MS` | `m3/activity_recorder.rs` (`IDLE_TIMEOUT_ENV`) | u64 (ms) | `180000` | Idle threshold. Set-but-invalid value is a hard startup error. |
| `SYNAPSE_TIMELINE_CLIPBOARD` | `m1/sources.rs` (`TIMELINE_CLIPBOARD_ENV`) | toggle | Not determined from source (parsing not shown) | Enable clipboard timeline source. |
| `SYNAPSE_TIMELINE_FILE_ACTIVITY` | `m1/sources.rs` (`TIMELINE_FILE_ACTIVITY_ENV`) | toggle | Not determined from source | Enable file-activity timeline source. |
| `SYNAPSE_FS_WATCH_ROOT` | `m1/sources.rs` (`FS_WATCH_ROOT_ENV`) | path | `%USERPROFILE%` fallback | Single FS-watch root. |
| `SYNAPSE_FS_WATCH_ROOTS` | `m1/sources.rs` (`FS_WATCH_ROOTS_ENV`) | path list | none | Multiple FS-watch roots. |
| `SYNAPSE_TIMELINE_EXCLUDE` | `m3/timeline_control.rs` (`TIMELINE_EXCLUDE_ENV`) | list | none | Timeline exclusion patterns. |
| `SYNAPSE_ASSIST_UNDO_BURST_COUNT` | `m3/activity_recorder.rs` | u64 | `3` | Assist detector: undo-burst count threshold. |
| `SYNAPSE_ASSIST_UNDO_BURST_WINDOW_MS` | same | u64 (ms) | `10000` | Undo-burst window. |
| `SYNAPSE_ASSIST_RETYPE_DELETE_COUNT` | same | u64 | `3` | Retype delete count. |
| `SYNAPSE_ASSIST_RETYPE_TEXT_COUNT` | same | u64 | `12` | Retype text count. |
| `SYNAPSE_ASSIST_RETYPE_WINDOW_MS` | same | u64 (ms) | `20000` | Retype window. |
| `SYNAPSE_ASSIST_REPEATED_CLICK_COUNT` | same | u64 | `5` | Repeated-click count. |
| `SYNAPSE_ASSIST_REPEATED_CLICK_WINDOW_MS` | same | u64 (ms) | `8000` | Repeated-click window. |
| `SYNAPSE_ASSIST_DIALOG_REOPEN_COUNT` | same | u64 | `3` | Dialog-reopen count. |
| `SYNAPSE_ASSIST_DIALOG_REOPEN_WINDOW_MS` | same | u64 (ms) | `60000` | Dialog-reopen window. |
| `SYNAPSE_ASSIST_COOLDOWN_MS` | same | u64 (ms) | `60000` | Assist-opportunity cooldown. |

### 3.6 Intent / routines / suggestions / ambient

| Name | Read in | Type | Default | Description |
|------|---------|------|---------|-------------|
| `SYNAPSE_INTENT_DETECT_INTERVAL_SECS` | `m3/intent_events.rs` | u64 (s) | `60` | Periodic intent-detect tick interval. |
| `SYNAPSE_INTENT_DETECT_STARTUP_DELAY_SECS` | same | u64 (s) | `45` | Delay before first intent tick. |
| `SYNAPSE_INTENT_DETECT_MIN_CONFIDENCE` | same | f64 `[0,1]` | `0.30` | Detection confidence floor. |
| `SYNAPSE_INTENT_DETECT_LOOKBACK_HOURS` | same / `m3/intent.rs` | u32 | `6` (`DEFAULT_LOOKBACK_HOURS`); valid `1..=168` | Recent-activity lookback. Out-of-range is a loud error. |
| `SYNAPSE_ROUTINE_MINE_INTERVAL_SECS` | `m3/routine_miner_job.rs` | u64 (s) | `21600` (6 h) | Routine-mining interval. |
| `SYNAPSE_ROUTINE_MINE_STARTUP_DELAY_SECS` | same | u64 (s) | `300` | Delay before first mining run. |
| `SYNAPSE_SUGGEST_QUIET_START_MIN` | `m3/suggestions.rs` | u32 (min) | none | Quiet-hours start (minutes-of-day, `<1440`). Both start+end required to enable. |
| `SYNAPSE_SUGGEST_QUIET_END_MIN` | same | u32 (min) | none | Quiet-hours end. |
| `SYNAPSE_ARMED_ROUTINE_INTERVAL_SECS` | `server/suggestions.rs` | u64 (s) | `60` | Armed-routine tick interval. |
| `SYNAPSE_ARMED_ROUTINE_STARTUP_DELAY_SECS` | same | u64 (s) | `60` | Armed-routine startup delay. |
| `SYNAPSE_AMBIENT_INGEST_INTERVAL_SECS` | `server/ambient_agents.rs` | u64 (s) | `5` | Ambient (Claude transcript) ingest cycle interval. |
| `SYNAPSE_AMBIENT_INGEST_STARTUP_DELAY_SECS` | same | u64 (s) | `8` | Delay before first ambient cycle. |
| `SYNAPSE_AMBIENT_MAX_IDLE_SECS` | same (`MAX_IDLE_ENV`) | u64 (s) | `86400` (24 h) | Only ingest sessions modified within this window. |
| `SYNAPSE_AMBIENT_CLAUDE_PROJECTS_DIR` | same (`ROOT_ENV`) | path | discovered only for configured daemon DBs | Explicit Claude `projects` dir for custom/scratch DB runs. Without this override, host-home discovery is enabled only for `%LOCALAPPDATA%\synapse\db-daemon` and the normal M3 default `%LOCALAPPDATA%\synapse\db`; custom DBs log `AMBIENT_INGEST_CUSTOM_DB_UNSCOPED` and skip ambient ingest. |
| `CLAUDE_CONFIG_DIR` | `server/ambient_agents.rs` | path | none | Claude config dir for transcript discovery. |
| `SYNAPSE_TRANSCRIPT_INGEST_INTERVAL_SECS` | `server/agent_transcripts.rs` | u64 (s) | `15` | Agent-transcript ingest interval. |
| `SYNAPSE_TRANSCRIPT_INGEST_STARTUP_DELAY_SECS` | same | u64 (s) | `10` | Delay before first transcript ingest. |

### 3.7 Action / safety / approvals

| Name | Read in | Type | Default | Description |
|------|---------|------|---------|-------------|
| `SYNAPSE_OPERATOR_HOTKEY` | `synapse-action/hotkey.rs` (`OPERATOR_HOTKEY_ENV`) | chord | `ctrl+alt+shift+p` (`DEFAULT_OPERATOR_HOTKEY`) | Operator panic-kill hotkey. |
| `SYNAPSE_MCP_OPERATOR_HOTKEY` | same (`OPERATOR_HOTKEY_COMPAT_ENV`) | chord | — | Compat alias for above (checked if primary unset). |
| `SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY` | `safety.rs` | bool (1/true/yes/on) | off | Run without the kill-switch hotkey. |
| `SYNAPSE_MCP_REQUIRE_OPERATOR_HOTKEY` | `safety.rs` | bool | off | Hard startup failure if hotkey cannot arm. |
| `SYNAPSE_APPROVAL_GATE_TIMEOUT_MS` | `server/permission_gate.rs` | u64 (ms, `>=1000`) | `1500000` (25 min) | Approval-gate blocking timeout. |
| `SYNAPSE_ENFORCE_SUPPORTED_USE` | `server/target_policy.rs` | bool (1/true/yes/y/on) | off | Restore legacy supported-use (game-profile) gating. |
| `SYNAPSE_ALLOW_SHELL_ANY` | `m4.rs` | bool (falsey to restrict) | **on** (permissive) | Allow any shell command unless explicitly disabled. |
| `SYNAPSE_ALLOW_LAUNCH_ANY` | `m4.rs` | bool (falsey to restrict) | **on** (permissive) | Allow any launch target unless explicitly disabled. |
| `SYNAPSE_AGENT_SPAWN_SHELL` | `server/m4_tools.rs` (`AGENT_SPAWN_SHELL_ENV_VAR`) | string | none | Override shell used for spawned agents. |
| `SYNAPSE_MCP_SESSION_ID` | `m4.rs` (`SHELL_SESSION_ID_ENV`) | string | none | Reserved env key injected into shell child env. |

### 3.8 Audio

| Name | Read in | Type | Default | Description |
|------|---------|------|---------|-------------|
| `SYNAPSE_ENABLE_AUDIO` | `m3.rs` (also CLI) | bool | `false` | Enable audio runtime. |
| `SYNAPSE_AUDIO_LOOPBACK` | `m3.rs` (`AUDIO_LOOPBACK_ENV`) | `1`/`0`/`true`/`false` | `1` (on) | System-audio loopback capture. Invalid value = hard error. |

### 3.9 Recording / Codex bridge

| Name | Read in | Type | Default | Description |
|------|---------|------|---------|-------------|
| `SYNAPSE_MCP_RECORDING_BACKEND` | `m2/config.rs` (`RECORDING_BACKEND_ENV`) | string | none | Demo-recording backend selector. |
| `SYNAPSE_CODEX_APP_SERVER_REQUEST_TIMEOUT_MS` | `server/codex_app_server_bridge.rs` | u64 (ms, `>=1000`) | `1500000` (25 min) | Codex app-server request timeout. |

### 3.10 Codex client-wiring env (set by `scripts/synapse-setup.ps1`, consumed by the generated Codex launcher)

These are written by setup/launcher scripts, not read by the daemon Rust code:

| Name | Set in | Description |
|------|--------|-------------|
| `SYNAPSE_BEARER_TOKEN` | setup + Codex launcher | Synced from `token.txt`. |
| `SYNAPSE_LOG_DIR` | daemon launcher (`synapse-setup.ps1` line ~436) | Points the launched daemon at the configured log dir. |
| `SYNAPSE_TOOL_SURFACE_HASH_AT_CODEX_START` | Codex launcher | Daemon tools/list fingerprint at start. |
| `SYNAPSE_TOOL_SURFACE_TOOL_COUNT_AT_CODEX_START` | Codex launcher | Tool count at start. |
| `SYNAPSE_TOOL_SURFACE_SNAPSHOT_AT_CODEX_START` | Codex launcher | Path to the start snapshot. |
| `SYNAPSE_TOOL_SURFACE_OUT` | `server/tool_profiles.rs` | Output path for the tool-surface dump. |
| `SYNAPSE_DEBUG_TOOLS` | `server.rs` | Truthy enables debug tools in the surface. |
| `SYNAPSE_ENABLE_EVERQUEST` | `server.rs` | Truthy enables EverQuest domain tools. |

### 3.11 Test / benchmark only (not production config)

Not part of normal operation; listed for completeness: `SYNAPSE_CAPTURE_FORCE_DXGI` (tests), `SYNAPSE_CAPTURE_BENCH_SECONDS`, `SYNAPSE_A11Y_MANUAL_BENCH`, `SYNAPSE_A11Y_BENCH_ITERS`, `SYNAPSE_ACTION_VIGEM_PAD_REAL`, `SYNAPSE_ACTION_SOFTWARE_PRESS_REAL`, `SYNAPSE_ACTION_SOFTWARE_CLICK_REAL`, `SYNAPSE_MCP_FORCE_PANIC_DURING_ACT`, `SYNAPSE_PTY_TRACE`, `SYNAPSE_MCP_BIN`, `SYNAPSE_LOCAL_AGENT_ITEST_*`, `SYNAPSE_LOCAL_MODEL_TOOL_PROBE_TIMEOUT_MS`, `SYNAPSE_LOCAL_MODEL_NON_TOOL_PROBE_TIMEOUT_MS`.

---

## 4. Data / working directories

All derivations assume Windows (the supported platform). Non-Windows fallbacks use `$XDG_STATE_HOME` → `$HOME/.local/state` → `./.synapse-state`.

| Logical name | Path / derivation | Source | Contents | Tier |
|--------------|-------------------|--------|----------|------|
| RocksDB store | `--db` / `SYNAPSE_DB`, else `%LOCALAPPDATA%\synapse\db` (falls back to `std::env::temp_dir()\synapse\db` if `LOCALAPPDATA` unset) | `m3.rs::default_db_path` | Timeline, episodes, agent events/transcripts, routines, KV, profiles history. | sacred (regenerable only by re-observing) |
| Setup daemon db | `%LOCALAPPDATA%\synapse\db-daemon` (setup `-DbPath` default) | `synapse-setup.ps1` | Same as above for the installed daemon. | sacred |
| Log dir | `%LOCALAPPDATA%\synapse\logs` (or `SYNAPSE_LOG_DIR`) | `synapse-telemetry::default_log_dir` | `synapse.log` (daily-rolled) + GC'd rotations; `daemon-launcher.log`. | regenerable |
| Models dir | `%LOCALAPPDATA%\synapse\models` (falls back to `.\synapse\models`) | `synapse-models::default_model_dir` | Side-loaded ONNX models (e.g. `yolov10n_general.onnx`). See [13_models_subsystem.md]. | sacred (manually side-loaded; downloads disabled) |
| Bearer token file | `%APPDATA%\synapse\token.txt` | `http/auth.rs::token_file_path` | HTTP bearer token (preferred over env). | sacred (secret) |
| Codex tool-surface snapshot | `%APPDATA%\synapse\codex-tool-surface.json` | `synapse-setup.ps1` | tools/list fingerprint for Codex start-guard. | regenerable |
| Codex start snapshots | `%LOCALAPPDATA%\synapse\codex-start-snapshots` | `synapse-setup.ps1` | Per-start tool-surface snapshots. | ephemeral |
| Action recovery ledger | `SYNAPSE_ACTION_RECOVERY_FILE` → daemon DB dir → `SYNAPSE_DB` → `%LOCALAPPDATA%\synapse\action_recovery.jsonl` | `synapse-action/recovery.rs` | JSONL of currently-held inputs for crash recovery (`action_recovery.jsonl`). | ephemeral |
| Shell job logs | `%LOCALAPPDATA%\Synapse\shell-jobs` (and `\jobs`) | `m4.rs::shell_job_root_dir` | Durable `act_run_shell` job stdout/stderr/status. | ephemeral |
| Shell sessions | `%LOCALAPPDATA%\Synapse\shell-sessions` | `m4.rs::shell_session_root_dir` | Per-session shell working dirs. | ephemeral |
| Replays | `%LOCALAPPDATA%\synapse\replays` (temp_dir fallback) | `m3/permissions.rs::replay_root` | Replay artifacts. | ephemeral |
| Setup build target | `%LOCALAPPDATA%\synapse\build-target` | `synapse-setup.ps1` | Cargo build output during install. | regenerable |
| Maintenance lock | `%LOCALAPPDATA%\synapse\setup-maintenance.lock.json` | `synapse-setup.ps1` | Single-setup mutex. | ephemeral |
| Runs dir | `%LOCALAPPDATA%\synapse\runs\` (and `.runs/<id>/`) | `.gitignore` references | Run artifacts. | ephemeral |
| Profiles dir | `--profile-dir` / `SYNAPSE_PROFILE_DIR`, else bundled (`%USERPROFILE%\.cargo\bin\profiles` in setup) | `m3.rs`, `synapse-setup.ps1` | Profile packages. See [11] for layout. | sacred |
| Chrome user-data (CDP) | `SYNAPSE_CDP_USER_DATA_DIR`, else `%LOCALAPPDATA%\Google\Chrome\User Data` | `m4.rs::default_chrome_user_data_dir` | Chrome profile for CDP control. | external |

> **Capitalization note:** `db`/`logs`/`models`/`replays`/`runs` live under `%LOCALAPPDATA%\synapse\` (lowercase), while shell job/session dirs use `%LOCALAPPDATA%\Synapse\` (capital S) — this is exactly as written in the respective source files.

See [04_storage_and_persistence.md](04_storage_and_persistence.md) for the RocksDB internals.

---

## 5. Logging configuration (`crates/synapse-telemetry/src/lib.rs`)

- Two `tracing_subscriber` layers via `Registry`:
  - **File layer:** JSON, daily-rolled `synapse.log` in the log dir (`tracing_appender::rolling::daily`), non-blocking, with target/file/line/thread/span metadata. Filtered at `cfg.file_level` (from `--log-level`/`SYNAPSE_LOG_LEVEL`), wrapped by a payload-safe filter.
  - **Console layer:** stderr, no ANSI. Filtered by `RUST_LOG` if set, else `cfg.console_level`.
- **Payload-safe filter** (`payload_safe_filter`): forces dependency/payload log targets (`PAYLOAD_LOG_TARGETS`) down to a capped dependency level so sensitive payloads aren't logged at debug/trace. Empty directive defaults to `info`; parse failure falls back to `info,rmcp=info`.
- **Log GC** (`run_log_gc`): on init and on a periodic worker. Deletes files older than `keep_days`; if total size still exceeds `max_dir_bytes`, deletes oldest-first until under cap. Interval from `SYNAPSE_LOG_GC_INTERVAL_S` (or configured `gc_interval`); `0` disables periodic GC.
- **Panic hook:** forwards panic payload + location to `tracing` (code `TELEMETRY_PANIC_HOOK_FIRED`) before the previous hook. Idempotent.
- Log-dir writability is probed at startup with a per-PID probe file; failure aborts startup (`TelemetryError::LogDirNotWritable`).

---

## 6. Network / ports / auth

| Item | Value | Source |
|------|-------|--------|
| Default bind / daemon port | `127.0.0.1:7700` (`DEFAULT_BIND`) | `m3.rs`, `main.rs`, `synapse-setup.ps1` |
| Single-daemon invariant | Embedded stdio daemon and HTTP daemon both open RocksDB; a single-instance guard prevents a second parallel daemon (#717). | `main.rs` (`run_stdio`), `single_instance.rs` |
| Local-agent MCP URL | `http://127.0.0.1:7700/mcp` | `main.rs` |
| Bearer-token source order | `%APPDATA%\synapse\token.txt` (if present & non-empty) → `SYNAPSE_BEARER_TOKEN` env | `http/auth.rs::load_token` |
| Auth header | `Authorization: Bearer <token>`; scheme case-insensitive, empty token rejected | `http/auth.rs::bearer_token` |
| Host/Origin validation | `validate_host` checks request Host header | `http/auth.rs` |
| Non-loopback bind | Refused unless `--allow-non-loopback` / `SYNAPSE_ALLOW_NON_LOOPBACK` | `main.rs`, `http::serve` |
| MCP session header | `Mcp-Session-Id`; max request body `1 MiB` (`MAX_MCP_REQUEST_BYTES`) | `http/session.rs` |
| Chrome bridge token | `SYNAPSE_BEARER_TOKEN` via `%APPDATA%\synapse\token.txt` | `chrome_debugger_bridge.rs`, `bin/synapse-chrome-native-host.rs` |

See [15_mcp_server_architecture.md](15_mcp_server_architecture.md).

---

## 7. Validation rules (observed in source)

| Rule | Where | Behavior on violation |
|------|-------|-----------------------|
| `--log-level` must parse as `LevelFilter` | `main.rs::configure_telemetry_from_level` | startup error `invalid log level {x}` |
| `SYNAPSE_TIMELINE_IDLE_TIMEOUT_MS` must be positive integer if set | `m3/activity_recorder.rs` | hard startup error (refuses to record) |
| `SYNAPSE_AUDIO_LOOPBACK` must be `1/0/true/false` | `m3.rs::audio_loopback_enabled` | `AudioError::LoopbackInitFailed` |
| Operator-hotkey bool envs must be `1/true/yes/on` or `0/false/no/off` | `safety.rs::parse_bool_env` | `ActionError::BackendUnavailable` |
| Boolean envs (`SYNAPSE_REFLEX_DISABLED`, `SYNAPSE_ENABLE_AUDIO`, `SYNAPSE_ALLOW_UNKNOWN_PROFILE`, etc.) parsed strictly | `m3.rs::parse_bool_env` | error returned from `from_env` |
| `SYNAPSE_INTENT_DETECT_LOOKBACK_HOURS` must be `1..=168` | `m3/intent_events.rs`, `m3/intent.rs` | loud error |
| `SYNAPSE_APPROVAL_GATE_TIMEOUT_MS` / `SYNAPSE_CODEX_APP_SERVER_REQUEST_TIMEOUT_MS` must be `>= 1000` | `permission_gate.rs`, `codex_app_server_bridge.rs` | value ignored, default used |
| `SYNAPSE_MAX_SUBSCRIPTIONS` must be NonZeroUsize | `m3.rs::parse_max_subscriptions_env` | error from `from_env` |
| Quiet-hours minutes must be `< 1440` and both set | `m3/suggestions.rs` | quiet hours disabled (`None`) |
| allow-shell/allow-launch overly-broad regex | `m4.rs` (`BroadAllowPatternError`, `SHELL_PATTERN_TOO_BROAD`/`LAUNCH_PATTERN_TOO_BROAD`) | `main.rs` logs `CONFIG_INVALID`, exits code `2` |
| Log dir must be writable | `synapse-telemetry::prepare_log_dir` | `LogDirNotWritable`, startup aborts |
| Bearer token must be non-empty (trimmed) | `http/auth.rs::normalize_token` | error |

---

## 8. Cross-references
- Storage layout & RocksDB column families: [04_storage_and_persistence.md](04_storage_and_persistence.md)
- Profiles directory layout & loading: profiles doc (11)
- Models directory & ONNX side-loading: [13_models_subsystem.md](13_models_subsystem.md)
- Server/daemon architecture, single-instance, HTTP transport: [15_mcp_server_architecture.md](15_mcp_server_architecture.md)
- Telemetry/overlay internals: [14_core_telemetry_overlay.md](14_core_telemetry_overlay.md)
