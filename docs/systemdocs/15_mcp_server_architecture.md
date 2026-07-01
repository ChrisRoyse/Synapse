# 15. MCP Server Architecture

**Source files covered:**

- `crates/synapse-mcp/src/main.rs` — binary entry point, CLI parsing, mode dispatch, stdio runtime.
- `crates/synapse-mcp/src/server.rs` — `SynapseService` definition, tool-router assembly, per-session state.
- `crates/synapse-mcp/src/server/handler.rs` — rmcp `ServerHandler` impl, `call_tool`/`list_tools`/`get_info`.
- `crates/synapse-mcp/src/server/context.rs`, `server/health.rs`, `server/background_router.rs`.
- `crates/synapse-mcp/src/server/session_registry.rs`, `server/session_lifecycle.rs`, `server/drain.rs`, `server/schema_sanitize.rs`, `server/tool_profiles.rs`.
- `crates/synapse-mcp/src/server/permission_gate.rs`, `server/permission_policy.rs`, `server/target_claims.rs`, `server/target_policy.rs`, `server/escalation/mod.rs`.
- `crates/synapse-mcp/src/m1.rs`, `m2.rs`, `m3.rs`, `m4.rs` (milestone layers; tool params in doc 16).
- `crates/synapse-mcp/src/http/{mod,auth,session,transport,sse}.rs` and `http/sse/{stream,ring,replay,lossy}.rs`.
- `crates/synapse-mcp/src/single_instance.rs`, `daemon_lifecycle.rs`, `stdio_eof.rs`, `connect.rs`, `desktop_worker.rs`, `local_agent.rs`, `doctor.rs`, `approval_protocol.rs`, `chrome_debugger_bridge.rs`, `safety.rs`, `secret_crypto.rs`.

See [01_system_overview.md](01_system_overview.md) for where the MCP server sits in the wider Synapse system. See [16_api_tools_reference.md](16_api_tools_reference.md) for individual tool schemas and parameters.

---

## 1. Overview

The `synapse-mcp` binary is the Synapse daemon: a Model Context Protocol (MCP) server that exposes Windows perception, action, memory, and effector capabilities as MCP tools to AI agents (Claude, Codex, local models). It is built on the **`rmcp` crate, version `1.7.0`** (workspace pin in `crates/synapse-mcp/Cargo.toml` → root `Cargo.toml`), with features `server`, `transport-io`, `transport-streamable-http-server`, `macros`, `schemars`.

The server runs in one of two transport modes:

- **stdio** (`--mode stdio`, default) — an embedded full daemon speaking JSON-RPC over stdin/stdout via `rmcp::transport::stdio()`. This is a complete daemon: it opens RocksDB and owns its own input lease and session registries.
- **HTTP / SSE** (`--mode http`) — an axum HTTP server (default bind `127.0.0.1:7700`) exposing the rmcp Streamable HTTP transport at `/mcp`, plus a separate daemon-owned SSE event channel at `/events`, a `/health` endpoint, dashboard routes, and Chrome native-host bridge routes.

Because a daemon owns a process-global input lease and a single RocksDB directory, **only one daemon may own a given DB path at a time** (enforced via `single_instance.rs`, section 5). Stdio-only clients that need to reach the shared HTTP daemon use the thin `--mode connect` bridge rather than spawning a second daemon.

The MCP server identifies itself (`get_info`, `server/handler.rs`) as:

- `ServerCapabilities`: `enable_tools()` + `enable_tool_list_changed()` (advertises `tools` and `tools.listChanged`).
- `Implementation`: name `"synapse-mcp"`, version `env!("CARGO_PKG_VERSION")`.
- `instructions`: a dynamic string from `context.rs::instructions()` (e.g. `"Synapse M1 perception MCP server with M2 action scaffold and M3 scaffold"`, with "(recording enabled)" appended when recording is active).
- Protocol version: not set explicitly — uses rmcp's `ServerInfo::new` default (`LATEST`).

---

## 2. Entry Point Trace

`main()` (`main.rs`) builds a multi-threaded Tokio runtime, calls `run()`, and applies a 5-second `shutdown_timeout`. Top-level errors are recorded into the daemon-lifecycle ledger via `daemon_lifecycle::record_top_level_error` and exit with code 1.

`run()` dispatch order:

1. **Chrome native-host short-circuit** — if argv looks like a Chrome native-messaging invocation (`chrome_debugger_bridge::native_host_invocation_from_args`), it runs the native host and returns before normal CLI parsing.
2. `Cli::parse()` (clap) → telemetry configured from `--log-level`.
3. **Mode-specific early returns** that skip daemon setup: `Connect`, `Doctor`, `ChromeNativeHost`, `ApprovalProtocol`, `DesktopWorker`, `LocalAgent`.
4. For `Stdio`/`Http`: initialize per-monitor DPI awareness, configure the action crash-recovery ledger and recover stale inputs, build `M2`/`M3`/`M4` service configs, then run `run_stdio(...)` or `http::serve(...)`.

### 2.1 CLI subcommands (`--mode`)

The `Mode` enum (`main.rs`) selects behavior. There are no clap subcommands; mode is a flag (`--mode`, env `SYNAPSE_MODE`).

| Mode | Purpose | Handler |
|------|---------|---------|
| `stdio` (default) | Embedded full daemon over stdin/stdout | `run_stdio` (`main.rs`) |
| `http` | axum HTTP/SSE daemon | `http::serve` |
| `connect` | Thin stdio↔HTTP bridge to the shared daemon | `connect::run_connect` |
| `chrome-native-host` | Chrome native-messaging host / CDP bridge | `chrome_debugger_bridge::run_native_host` |
| `approval-protocol` | `synapse-approval://` URI-activation child for toast buttons | `approval_protocol::run_protocol_activation` |
| `desktop-worker` | Hidden-desktop UIA/PrintWindow child process | `desktop_worker::run_worker_from_cli` |
| `doctor` | Enumerate/classify/clean stray synapse-mcp processes | `doctor::run_doctor` |
| `local-agent` | Run a registry-backed local model as an MCP client/agent | `local_agent::run_from_cli` |

### 2.2 Key CLI flags

| Flag | Env | Default | Purpose |
|------|-----|---------|---------|
| `--mode` | `SYNAPSE_MODE` | `stdio` | Transport / sub-process mode |
| `--bind` | `SYNAPSE_BIND` | `127.0.0.1:7700` | HTTP bind address |
| `--allow-non-loopback` | `SYNAPSE_ALLOW_NON_LOOPBACK` | false | Permit non-loopback HTTP bind |
| `--db` | `SYNAPSE_DB` | `%LOCALAPPDATA%\synapse\db` | RocksDB directory |
| `--profile-dir` | `SYNAPSE_PROFILE_DIR` | — | App profile directory |
| `--log-level` | `SYNAPSE_LOG_LEVEL` | `info` | Tracing level |
| `--reflex-disabled` | `SYNAPSE_REFLEX_DISABLED` | false | Disable M3 reflex runtime |
| `--reflex-force-degraded` | `SYNAPSE_REFLEX_FORCE_DEGRADED` | false | Force reflex degraded mode (test) |
| `--kill-stray` | — | false | (doctor) kill matching strays once a live holder is found |
| `--enable-audio` | `SYNAPSE_ENABLE_AUDIO` | false | Enable audio capture / `ReadAudio` permission |
| `--restrict-unknown-profile` | `SYNAPSE_RESTRICT_UNKNOWN_PROFILE` | false | Fail-closed on unprofiled apps (default: actionable) |
| `--allowed-permissions` | `SYNAPSE_MCP_ALLOWED_PERMISSIONS` | — | Permission grant allowlist (section 6) |
| `--max-subscriptions` | `SYNAPSE_MAX_SUBSCRIPTIONS` | `synapse_reflex::DEFAULT_MAX_SUBSCRIPTIONS_NONZERO` | SSE subscription cap |
| `--allow-shell` (repeatable) | `SYNAPSE_ALLOW_SHELL` (comma) | — | `act_run_shell` allow regex |
| `--allow-launch` (repeatable) | `SYNAPSE_ALLOW_LAUNCH` (comma) | — | `act_launch` allow regex |
| `--run-shell-inline-await-limit-ms` | `SYNAPSE_RUN_SHELL_INLINE_AWAIT_LIMIT_MS` | `90_000` | Inline await before `act_run_shell` returns a durable job |
| `--storage-pressure-free-bytes-sample` | `SYNAPSE_STORAGE_PRESSURE_FREE_BYTES_SAMPLE` | — | Injected free-bytes sample (test) |
| `--chrome-native-origin` | `SYNAPSE_CHROME_NATIVE_ORIGIN` | bundled extension origin | Diagnostic origin for chrome-native-host |
| `--approval-uri` | — (hidden) | — | (approval-protocol) activation URI |
| `--desktop-worker-*` | — (hidden) | — | (desktop-worker) op/hwnd/region/depth/json/bgra |
| `--local-agent-*` | `SYNAPSE_LOCAL_AGENT_*` | various | (local-agent) model, task, mcp-url, max-turns (40), timeout (120s), etc. |

Two additional `act_*` allow inputs are read directly from env at config time: `SYNAPSE_ALLOW_SHELL` / `SYNAPSE_ALLOW_LAUNCH` (comma-separated lists merged with the repeated flags). Permissive bypass toggles `SYNAPSE_ALLOW_SHELL_ANY` / `SYNAPSE_ALLOW_LAUNCH_ANY` default to **true** when unset (see section 3, M4).

### 2.3 stdio bootstrap (`run_stdio`)

1. Acquire the **single-instance lock** for the DB path (`SingleInstanceGuard::acquire`). On `AlreadyRunning`, log `MCP_DAEMON_ALREADY_RUNNING`, print guidance to use `--mode connect`, and exit code **3**.
2. `daemon_lifecycle::configure(mode="stdio", ...)` and install the panic hook.
3. Build the `SynapseService` (`try_with_m2_shutdown_reason_and_m3_config`) wiring shutdown / connection-closed cancellation tokens.
4. Install the operator panic hotkey (`safety::install_operator_hotkey`).
5. Wrap stdin in `CancelOnEofRead` (`stdio_eof.rs`) so stdin EOF cancels the transport, then `service.serve_with_ct((stdin, stdout), token)`.
6. Race the running service against `wait_for_shutdown_signal` (Ctrl-C / Ctrl-Break on Windows); on signal, cancel tokens, drain the M2 emitter, record graceful exit.

---

## 3. Milestone Layering

The tool surface is organized into four milestone layers (`m1`/`m2`/`m3`/`m4`), each with its own `*ServiceConfig` (`from_env` / `from_cli_parts`) and, for M1–M3, a `Shared*State = Arc<Mutex<*State>>`. M4 is policy-only (no separate runtime state; it depends on M2's action handle and M3's permissions). The full tool router is assembled in `server.rs::tool_router()`.

| Milestone | Theme | Source | Representative tool families |
|-----------|-------|--------|------------------------------|
| **M1** | Perception / observation | `m1.rs` + `m1/{detection,ocr,search,sources}.rs` | `observe`, `read_text`, `find`, `capture_screenshot`, `set_target`/`set_perception_mode`/`set_capture_target`, `window_list`, browser/CDP perception (`browser_*`, `cdp_*`) |
| **M2** | Action / input emission | `m2.rs` + `m2/{click,press,type_text,scroll,stroke,set_value,set_field_text,clipboard,pad,focus_window,release_all,...}` | `act_click`, `act_type`, `act_press`, `act_scroll`, `act_stroke`, `act_set_value`, `act_clipboard`, `act_pad`, `act_focus_window`, `release_all` |
| **M3** | Memory / reflexes / orchestration | `m3.rs` + ~30 `m3/*` submodules | reflexes (`reflex_register/list/cancel`, armed routines), timeline & episodes, routines/suggestions, approvals, audio, hygiene, local-model registry, profiles (+ authoring/registry/quality), replay, storage, subscribe, intent, plan, audit export/retention |
| **M4** | Effector / shell / agent spawn | `m4.rs` | `act_run_shell` (+ `_start`/`_status`/`_cancel` durable jobs), `act_launch`, `act_spawn_agent`, `act_combo` |

### M1 — Perception

`M1State::from_env` reads `SYNAPSE_MCP_SYNTHETIC_FIXTURE`, `SYNAPSE_MCP_FORCE_NO_PERCEPTION`, `SYNAPSE_MCP_FORCE_OBSERVE_INTERNAL`, `SYNAPSE_MCP_FORCE_NO_FOREGROUND`. Submodules: `detection.rs` (object-detection runtime + entity tracking), `ocr.rs` (text reading / OCR backend selection), `search.rs` (`find` element/entity matching), `sources.rs` (clipboard, filesystem-recent watcher, foreground assembly, synthetic fixtures, hidden-desktop worker snapshots).

### M2 — Action

`M2ServiceConfig { recording_backend: Option<String> }` (env `SYNAPSE_MCP_RECORDING_BACKEND`). `M2State` holds the action emitter actor, backend-resolution policy, recording backend, and the **foreground input lease** machinery (`ForegroundInputLeaseGuard` / `ForegroundInputContextSnapshot`) that captures and restores cursor + foreground HWND/PID after an action.

### M3 — Memory / Reflexes

`M3ServiceConfig` fields: `db_path`, `profile_dir`, `reflex_disabled`, `bind`, `bearer_token` (from `SYNAPSE_BEARER_TOKEN`), `max_subscriptions`, `enable_audio`, `allow_unknown_profile`, `allowed_permissions`, `reflex_force_degraded`, `storage_pressure_free_bytes_sample`. `default_db_path()` → `%LOCALAPPDATA%\synapse\db`. `M3State` adds the shared RocksDB handle, profile/reflex runtimes, `SseState`, activity recorder, audio runtime, intent tracker, and audit-session maps. M3 exposes the largest tool surface (~58 tool stubs).

### M4 — Effector

`M4ServiceConfig` (private): `allow_shell: Vec<AllowPattern>`, `allow_launch: Vec<AllowPattern>`, `allow_shell_any: bool`, `allow_launch_any: bool`, `run_shell_inline_await_limit_ms`. Key constants: `DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS = 90_000`, `DEFAULT_SHELL_TIMEOUT_MS = 30_000`, `DEFAULT_LAUNCH_TIMEOUT_MS = 10_000`, `DEFAULT_AGENT_SPAWN_WAIT_TIMEOUT_MS = 120_000`, `MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS = 1_800_000`, `MAX_COMBO_STEPS = 256`.

Allowlist gating: each `--allow-shell`/`--allow-launch` pattern is compiled to a regex via `compile_allow_patterns`; over-broad patterns are rejected (`BroadAllowPatternError`, exit code 2, codes `SHELL_PATTERN_TOO_BROAD`/`LAUNCH_PATTERN_TOO_BROAD`). **Permissive-by-default:** `allow_shell_any`/`allow_launch_any` come from `SYNAPSE_ALLOW_SHELL_ANY`/`SYNAPSE_ALLOW_LAUNCH_ANY`, which default to **true** when unset (allowlist bypassed; only `0`/`false`/`no`/`off` re-enables per-pattern enforcement). Both emit a startup warning; every command is still recorded in the action log.

### Tool-router assembly notes (`server.rs`)

`tool_router()` sums dozens of per-family sub-routers (M1/M2/M3/M4, agent control/cost/stats/query/templates/tasks, leases, target-claims, reality, intent, plan, suggestions, timeline, notify, hygiene, escalation, `target_act`, browser families, tool-profile). Two gating switches:

- The **EverQuest domain pack** (~25 tools) is only added when `SYNAPSE_ENABLE_EVERQUEST` is set.
- Test-only storage probes (`storage_put_probe_rows`, `storage_pressure_sample`) are removed unless `SYNAPSE_DEBUG_TOOLS` is set.

---

## 4. Transports

### 4.1 stdio (`stdio_eof.rs`)

The stdio transport wraps stdin in `CancelOnEofRead<R>`, an `AsyncRead` adapter. On the first zero-byte read (EOF), it cancels both the connection-closed token and the service token and logs `MCP_STDIO_EOF_CONNECTION_CLOSED`, so a closed stdin reliably tears down the embedded daemon.

### 4.2 HTTP server (`http/transport.rs`, axum)

`http::serve(bind, allow_non_loopback, m2_config, m3_config, m4_config)` startup (exit codes in parentheses):

1. Parse `bind` into a `SocketAddr`.
2. **Loopback enforcement** — non-loopback bind without `--allow-non-loopback` → `HTTP_BIND_NON_LOOPBACK_REFUSED`, exit **2** (checked before the lock).
3. `SingleInstanceGuard::acquire` — `AlreadyRunning` → `MCP_DAEMON_ALREADY_RUNNING`, exit **3**.
4. `daemon_lifecycle::configure(mode="http", ...)` + panic hook.
5. `TcpListener::bind`, create shutdown/connection-closed cancellation tokens, build `SseState`.
6. Eager RocksDB open (failure → exit **4**, distinguishing `STORAGE_LOCK_CONTENDED` vs `STORAGE_OPEN_FAILED`) and eager activity-recorder start (failure → exit **4**).
7. Spawn background tasks (routine miner, intent detector, armed-routine runner, transcript ingester, ambient-agent discovery) and the operator hotkey.
8. Build the axum `Router`, then `axum::serve(...).with_graceful_shutdown(...)`, racing server completion against the shutdown signal.

The axum router merges two groups:

**`protected_routes`** — wrapped by middleware (outermost→innermost): `auth::require_http_security` → `session::release_held_inputs_on_delete` → `session::require_mcp_session`.

| Method | Path | Notes |
|--------|------|-------|
| `nest /mcp` | `/mcp`, `/mcp/*` | rmcp `StreamableHttpService` (POST=requests, GET=stream, DELETE=teardown) |
| GET | `/health` | Health payload (section 8) |
| POST | `/shutdown` | Marks draining, returns `202`, cancels after 2s grace |
| GET / POST | `/events` | SSE open / manual publish (publish gated on `SYNAPSE_HTTP_SSE_MANUAL`) |
| GET | `/events/stats` | Subscription stats |
| POST | `/agent-events` | Agent event ingress (body-limited) |
| GET | `/agent-events/stats` | — |
| POST | `/codex-app-server/request` | Codex app-server bridge (body-limited) |
| GET | `/agent-transcripts/stats` | — |
| POST | `/chrome-debugger/native/register` | Chrome native-host registration |
| POST | `/chrome-debugger/native/message` | — |
| GET | `/chrome-debugger/native/next` | — |
| GET | `/chrome-debugger/native/ws` | WebSocket |

**`dashboard_routes`** — merged separately, **not** behind the auth/session middleware: `GET /dashboard`, `/dashboard/assets/*`, `/dashboard/state.json`, `/dashboard/events`, plus many `POST /dashboard/...` control endpoints (spawn-agent, tasks, timeline, routines, storage, templates, approval/decide), and `GET /approval/activate` (consumed by the approval-protocol child, section 5).

### 4.3 Auth (`http/auth.rs`)

- **Scheme:** HTTP Bearer — `Authorization: Bearer <token>` (scheme match case-insensitive).
- **Token source** (`load_token`): file `%APPDATA%\synapse\token.txt` if present, else env `SYNAPSE_BEARER_TOKEN`. Empty token is an error.
- **Validation:** token SHA-256 digested at load and compared in constant time (`subtle::ConstantTimeEq`); raw token is never stored.
- **Origin/Host validation** (`validate_origin_and_host`): `Host` must resolve to `127.0.0.1`/`localhost`/`::1`; `Origin`, if present, must be `http` to the same allowlist (absent + loopback is allowed; absent + non-loopback rejected).
- **Middleware `require_http_security`:** (1) loopback chrome-extension bridge carve-out — whitelisted extension ID hitting only `/chrome-debugger/native/*` bypasses bearer auth; (2) origin/host check → `403` `HTTP_ORIGIN_REFUSED`; (3) authorize → `401` `HTTP_TOKEN_INVALID` with `WWW-Authenticate: Bearer`. There is no general loopback auth exemption.

### 4.4 Sessions over HTTP (`http/session.rs`)

- Header **`Mcp-Session-Id`** identifies a session; exposed to handlers via task-local `CURRENT_MCP_SESSION_ID` (`current_mcp_session_id()`).
- Backed by rmcp `LocalSessionManager` with `keep_alive` from `SYNAPSE_HTTP_SESSION_IDLE_TIMEOUT_SECS` (default 86400 s / 24 h).
- `require_mcp_session` middleware (acts only on `/mcp*`): POST without a session id is allowed only if the body is a JSON-RPC `initialize`; GET/DELETE without a session → `404`. Request body cap `MAX_MCP_REQUEST_BYTES = 1 MiB` (`413` on overflow). Terminated sessions allow idempotent DELETE, else `404 UnknownOrExpired`.
- **Persistent session store** (`SynapseMcpSessionStore`): a custom rmcp `SessionStore` persisting wrapped `PersistedMcpSessionState` rows in RocksDB `CF_KV`, TTL = `keep_alive`. It also drives the session registry and journals `session_initialized`/`session_restored`/`exited` agent events.

### 4.5 SSE (`http/sse.rs` + `http/sse/*`)

The daemon-owned `/events` channel is distinct from the rmcp `/mcp` GET stream.

- **Resume header `Last-Event-ID`** (u64 stream-seq); malformed → `400`. Response carries `Synapse-Subscription-Id`; optional `?subscription_id=` query to attach to an existing subscription.
- **`SseState`** holds an `EventBus`, a subscription map, owner-session mapping (for per-session cleanup), and `manual_routes_enabled` (env `SYNAPSE_HTTP_SSE_MANUAL`). Constructed with `max_subscriptions`; exceeding the cap → `SUBSCRIPTION_CAP_REACHED`.
- **Ring buffer** (`ring.rs`): each subscription has a bounded `VecDeque` (`SUBSCRIBER_QUEUE_CAPACITY`) and a monotonic `next_stream_seq` (the SSE event `id`). Overflow drops the oldest, increments `dropped_total`, and sets a `lossy_pending` flag.
- **Replay** (`replay.rs`): `frames_after(last_event_id)` returns events with `stream_seq > last_event_id`. If the resume point fell off the ring (gap), it prepends a `subscription_started` frame with `lossy:true` so the client knows it missed events.
- **Streaming** (`stream.rs`): polls every `SSE_POLL_INTERVAL = 20 ms`; event frames are JSON-RPC `synapse/event` notifications with `id = stream_seq`.

---

## 5. Lifecycle

### 5.1 Single-instance lock (`single_instance.rs`)

`SingleInstanceGuard::acquire(db_path)` takes an **OS advisory exclusive file lock** (`fs2`) on `<db>/daemon.lock`, acquired *before* RocksDB opens so a duplicate launch fails fast naming the holder PID instead of dying on a cryptic RocksDB `LOCK` error. The holder PID is stored in a separate unlocked sidecar `<db>/daemon.pid` (on Windows the exclusive lock is a mandatory whole-file lock, so the PID must live outside it). Dropping the guard releases the lock and removes the PID file; the OS also releases the lock if the process dies. Both stdio and HTTP modes acquire it; the **canonical daemon binds port 7700** and owns this lock. The lock is scoped per-DB-path, so daemons on different `--db` paths coexist.

### 5.2 Daemon lifecycle ledger (`daemon_lifecycle.rs`)

A process-global ledger written to four files under the DB directory: `daemon-run-current.json`, `daemon-tool-last.json`, `daemon-tool-events.jsonl`, `daemon-exit.jsonl`. `configure(...)` records the current run and, if the previous `run-current` had no `ended_at`, appends a `previous_run_unclean` exit event. `begin_tool_call`/`ToolCallGuard::finish_ok|error|panic` bracket every tool call; `record_context_event` logs out-of-band context events; `record_graceful_exit`/`record_top_level_error` close out a run. The installed panic hook records panics here. `health_subsystem()` and `in_flight_tool_calls_for_session()` expose state to health and session tools.

### 5.3 Drain / shutdown (`server/drain.rs`)

`DaemonDrainState` holds an `Option<DaemonDrainReason>` plus a `CancellationToken`. `mark_draining(source)` idempotently sets the reason (`reason_code = DAEMON_RESTARTING`) and cancels the token. In `handler.rs::call_tool`, each running tool is raced against the drain and shutdown tokens; if drain wins, the call returns a **retryable** `DAEMON_RESTARTING` error (rmcp `-32099`). A poisoned drain lock fails toward "draining". On HTTP shutdown the server marks draining, waits a 2 s grace, cancels, releases held inputs/leases (`session_lifecycle`), and waits for the M2 emitter to finish.

### 5.4 Session registry & lifecycle (`server/session_registry.rs`, `server/session_lifecycle.rs`)

`SessionRegistry` is an in-memory ledger of MCP sessions: client name/version, protocol version, inferred `agent_kind` (`local-model`/`codex`/`claude`/`unknown`), timestamps, last action, and spawned-agent attribution. `record_initialized`/`record_closed` return `true` only on the live↔closed transition so exactly one journal event is emitted; lifecycle states derive to `live`/`stale`/`closed`.

`session_lifecycle::teardown_session` is the authoritative per-session resource reclaim (`SharedSessionProcessResources`, `SharedTerminatedSessions`): mark terminated → release held keys/buttons/pads and the foreground input lease → clear target/continuity/audit/clipboard → close owned CDP targets → release target claims → cancel shell jobs → drop owned process jobs (kill-on-close) → cancel subscriptions → delete the session store row → record registry close. A daemon-shutdown variant *disarms* kill-on-close so spawned agents survive a restart; live spawned-agent sessions are protected from idle eviction via a real OS process probe.

### 5.5 Desktop worker (`desktop_worker.rs`)

`--mode desktop-worker` is a short-lived Windows child launched with `STARTUPINFOW.lpDesktop` set to a (hidden) desktop name, so UIA/capture run on that desktop. `DesktopWorkerOp`: `Context` (foreground context), `Snapshot` (UIA subtree, depth ≤16), `Capture` (PrintWindow BGRA). Results marshal back to the daemon via temp JSON/`.bgra` files; the daemon-side launchers (`hidden_desktop_window_context/snapshot/capture/hwnds`) spawn it via `CreateProcessW` with a 10 s timeout.

### 5.6 Connect bridge (`connect.rs`)

`--mode connect` is a pure JSON-RPC stdio↔HTTP pump letting a stdio-only client reach the shared daemon. `run_connect` installs a parent-death watchdog (force-exits if the launching client dies), ensures a daemon is running (probes `GET /health`, spawning a detached `--mode http` daemon if absent), then forwards messages between an stdio transport and a `StreamableHttpClientTransport` to `http://{bind}/mcp`. It saves the `initialize`/`initialized` handshake so it can transparently reconnect/replay if the daemon stream drops. The spawned daemon uses non-inheritable handles so it never inherits the client's stdio pipes.

### 5.7 Local agent (`local_agent.rs`)

`--mode local-agent` runs a registry-backed local LLM as an autonomous MCP client/agent against the daemon: opens an MCP session, lists tools, resolves the model's chat-completions endpoint, then drives a turn loop (default `max_turns = 40`, per-turn timeout 120 s) routing the model's tool calls back into Synapse MCP tools. It gates-but-bypasses human approval (local agents are trusted autonomous workers), feeds recoverable tool errors back to the model, and journals lifecycle events/logs consumed by the fleet/agent subsystem.

### 5.8 Doctor (`doctor.rs`)

`--mode doctor` enumerates all `synapse-mcp` processes, identifies the single legitimate daemon via the recorded lock-holder PID, and classifies the rest (`Daemon`/`Bridge`/`Doctor`/`Orphan`/`StrayStdio`/`Unknown`). With `--kill-stray` it refuses (exit 2) unless a live lock-holder exists, then kills bridges/stray-stdio/orphan/unknown processes (never the daemon or itself; exit 3 if any kill fails).

### 5.9 Approval-protocol child (`approval_protocol.rs`)

`--mode approval-protocol` handles the Windows `synapse-approval://` URI scheme fired by actionable approval-toast buttons. It parses the activation token, validates that `bind` is loopback, and forwards a `GET` to the daemon's `/approval/activate` endpoint (the durable RocksDB approval row is validated there). `ensure_protocol_handler_registered` registers the URL-protocol handler under `HKCU\Software\Classes\synapse-approval`.

### 5.10 Chrome debugger bridge (`chrome_debugger_bridge.rs`)

Provides the bundled MV3 extension's native-messaging host and the CDP bridge backing the `browser_*`/`cdp_*` tools. `run_native_host` registers with the daemon and pumps the native-messaging length-prefixed protocol between Chrome and the daemon. `set_browser_navigation_sink` installs a process-global sink (wired in `server.rs::install_chrome_browser_navigation_sink`) that forwards Chrome navigation events into the M3 activity recorder/timeline.

---

## 6. Permission & Safety Model

### 6.1 M3 permission grants (`m3/permissions.rs`)

`Permission` enum (12 variants): `ReadEvents`, `WriteReflex`, `ReadReflex`, `ReadProfile`, `WriteProfileActive`, `WriteReplay`, `ReadAudio`, `ReadStorage`, `WriteStorage`, `InputKeyboard`, `InputMouse`, `InputPad`. `RequiredPermissions = BTreeSet<Permission>`. `PermissionGrants::from_config(allowed_permissions, audio_enabled)` parses `--allowed-permissions` or uses `default_grants` (all but `ReadAudio` unless audio is enabled). A tool whose `required(...)` set is not satisfied fails with an authorization error naming the first missing permission. `ReadAudio` additionally requires `--enable-audio`.

### 6.2 Permission gate (`server/permission_gate.rs`) + classifier (`server/permission_policy.rs`)

Spawned Claude agents use the public `approval` facade as `--permission-prompt-tool` when a tool falls outside their static allow rules. The facade accepts Claude's direct permission-prompt payload and delegates internally to hidden `approval_gate`, so the production <=40 tool surface does not expose implementation tools. `permission_policy::classify(tool, input)` returns a `GateDecision` (`AutoAllow` or `Gate { destructive }`); it is **fail-safe** — anything not provably read-only/low-consequence is gated. The classifier recognizes built-in tools, shell command structure (splitting on `&&`/`||`/`|`/`;` so destructive parts can't be smuggled), git/cargo subcommands, and MCP tool name heuristics (read-only suffixes auto-allow; a `DESTRUCTIVE_MCP_TOOLS` set like `agent_kill`, `timeline_purge`, `session_end` is destructive). On a gate, `run_gate` writes a durable `Pending` `AgentPermission` approval row in `CF_KV` and blocks (`DEFAULT_GATE_TIMEOUT_MS = 25 min`, env-overridable) until a human decides; the returned top-level `{"behavior":"allow"|"deny"}` (optionally with `updatedInput` from operator edits) is the agent's resume. `agent_ask_operator` uses the same durable queue and waiter path for needs-input pauses: it writes an `AgentQuestion` row with `allow.respond=true`, blocks until `approval_decide` records a response/decline/timeout, and returns the operator response as the tool result. It is fail-closed: storage/internal errors return an MCP error, never a silent allow. Spawn attribution via header `x-synapse-spawn-id`.

### 6.3 Target claims (`server/target_claims.rs`)

Advisory, in-memory ownership leases over targets (`window:0x{hwnd}` or `cdp:0x{hwnd}:{id}`) so concurrent agent sessions don't clobber each other. A live claim makes other sessions' **mutating** actions fail closed with `TARGET_CO_OWNED` (`-32099`) while read-only observe stays allowed. Tools: `target_claim` (TTL 1–600 s, default 120 s), `target_claim_adopt` (fail-closed takeover of an older same-agent session after client churn), `target_release` (own claim only), `target_claim_status`. `prune_inactive` drops expired/dead-owner claims; teardown releases all claims for a session.

### 6.4 Target action policy (`server/target_policy.rs`)

`ensure_supported_use_allows` gates action dispatch against a profile's `supported_use.*` metadata — **opt-in** via `SYNAPSE_ENFORCE_SUPPORTED_USE` (default off: every profiled foreground app is actionable). When enforced it supports two modes (local-world validation and operator-attended live-server attestations); denials surface as `SAFETY_PROFILE_ACTION_DENIED` (`-32099`). Functional safety (operator hotkey, release-all, rate limits) is independent and always active.

### 6.5 Operator panic hotkey (`safety.rs`)

`install_operator_hotkey` registers a daemon-owned global hotkey (default `Ctrl+Alt+Shift+<key>`). On fire it seizes the foreground input lease, disables all reflexes, releases held keys/buttons within a 50 ms budget, logs `SAFETY_OPERATOR_HOTKEY_FIRED`, and spawns the fleet kill of all agents. Registration failure is degraded (logged, surfaced in `/health`) unless `SYNAPSE_MCP_REQUIRE_OPERATOR_HOTKEY` forces a hard fail; `SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY` skips it.

### 6.6 Secret crypto (`secret_crypto.rs`)

At-rest protection for cloud-model API keys / tokens before they touch RocksDB, using **Windows DPAPI (CurrentUser scope)** — `CryptProtectData`/`CryptUnprotectData` with secondary entropy `b"synapse/local-model-api-key/v1"`. Ciphertext is bound to the Windows account; `unprotect` fails loudly on foreign/tampered bytes; non-Windows builds `bail!` rather than persisting plaintext.

### 6.7 Escalation (`server/escalation/mod.rs`)

AFK attention-escalation: Tier 0 always-on on-PC toast + Tier 1 opt-in webhook ladder (HMAC-SHA256 signed `X-Synapse-Signature`). Severities `Low`/`Medium`/`Critical`; truth in `CF_KV`. MCP surface: `escalation_ack` (stops the off-machine ladder), `escalation_list`, plus `escalation_config_set/get`. Opening an escalation writes an `AgentEscalation` approval row; any approval decision acks the escalation.

---

## 7. Server Context, Dispatch & Routing

### 7.1 `SynapseService` (`server.rs`)

The single rmcp service struct, cloneable, holding: the assembled `ToolRouter`, `m1_state`/`m2_state`/`m3_state`, `m4_config`, `drain_state`, and per-session registries — `session_targets` (`SharedSessionTargets`, keyed by `Mcp-Session-Id`; `SessionTarget::Window{hwnd}` or `::Cdp{window_hwnd,cdp_target_id}`), `cdp_target_owners`, `session_clipboards`, `session_registry`, `target_claims`, `session_processes`, `terminated_sessions`, and a `mailbox_notify`. Action target resolution (`action_session_target_override`) lets an explicit per-call `window_hwnd`/`cdp_target_id` override the bound session target (a `cdp_target_id` requires `window_hwnd`).

### 7.2 Context (`server/context.rs`)

Per-call plumbing: resolving M1/M2/M3 state handles, the MCP session id (`MCP_SESSION_ID_HEADER`), and the action preflight/scope gate `ensure_supported_use_allows_action`. Defines the approval event-kind constants (`APPROVAL_REQUEST_EVENT_KIND`, `APPROVAL_DECISION_EVENT_KIND`, `APPROVAL_TIMEOUT_EVENT_KIND`), `AgentTranscriptSnapshotRow`, and the dynamic `instructions()` string. Audit/foreground context themselves (`current_action_audit_context`, `current_audit_foreground`) live in sibling `audit_context.rs`.

### 7.3 Handler dispatch (`server/handler.rs`)

`call_tool` flow: extract tool name + session id → `begin_daemon_lifecycle_tool_call` (captures audit context, foreground, session target, each with a read-error fallback) → reject terminated sessions (`HTTP_SESSION_INVALID`) → tool-profile admission gate → drain/shutdown check (`DAEMON_RESTARTING`, retryable) → run the tool via `tool_router.call(...)` wrapped in `catch_unwind` and raced against drain/shutdown tokens → finish the lifecycle guard (`finish_ok`/`finish_error`/`finish_panic`). Errors are normalized (`tool not found`→`TOOL_NOT_FOUND`, invalid params→`TOOL_PARAMS_INVALID`); panics return `MCP_TOOL_PANIC` (`-32099`). `list_tools` delegates to `tools_for_session_profile(session_id)`, so the same profile row gates both discovery and execution.

### 7.4 `target_act` router (`server/background_router.rs`)

Despite the module name, this implements the single high-level computer-use verb `target_act` (not a background-job subsystem). It dispatches `params.verb` (`read`, `screenshot`, `navigate`, `set_field`, `click`, `type`, `key`, `press`, `run_shell`, `focus_window`, ...) to the underlying primitive method, inheriting that tool's target resolution, action audit, and lease/foreground guards. (Durable backgrounding of shell work lives in the M4 `act_run_shell_start/_status/_cancel` family, not here.)

### 7.5 Schema sanitize (`server/schema_sanitize.rs`)

`sanitize_tools(Vec<Tool>)` normalizes tool schemas at the `tools/list` boundary for strict (Zod/Ajv) MCP clients: replaces bare boolean subschemas in `properties`/`oneOf`/etc. with explicit permissive/never objects, and strips non-standard `format` annotations (`uint32`, `int64`, ...) via a standard-format allowlist. Booleans in `additionalProperties`/`additionalItems`/`unevaluated*` are preserved.

### 7.6 Tool profiles (`server/tool_profiles.rs`)

Per-session durable tool-profile gating (RocksDB `CF_SESSIONS`, key `mcp/tool-profile/v1/<session_id>`). `ToolProfileKind`: `NormalAgent` (least-privilege default — hides raw `act_*` input primitives, keeps capability via `target_act`/browser/CDP), `BrowserControl`, `BreakGlass` (full raw surface incl. hazardous tools), `FullCapability` (full surface, auto-assigned to trusted Synapse-spawned local-model harnesses). Unscoped stdio (no session id) → `BreakGlass`. `admit_tool_call_for_profile` denies hidden tools with `TOOL_PROFILE_POLICY_DENIED` (attaching a `capability_route` alternative). `tool_profile_set` to `BreakGlass`/`FullCapability` requires `confirm_break_glass=true`, a reason, **and** that the session currently holds the foreground input lease (prevents self-escalation); a visible-surface change pushes `notifications/tools/list_changed`.

---

## 8. Health & Doctor

### 8.1 Health (`server/health.rs`)

The `health` tool / `GET /health` returns a `Health` payload: `ok` (true iff no subsystem is `"error"`), `version`, `build` (git SHA or `"dev"`), `pid`, `uptime_s`, `tool_count`, `tool_surface_sha256`, `tool_names`, and a `subsystems` map. Subsystems aggregated: `storage` (RocksDB CF sizes + disk-pressure level + schema version), `reflex` (active/sample counts, tick jitter, degraded/disabled), `profiles`, `perception`, `action` (emitter availability, recording, operator-hotkey label, allow_shell/launch counts, input-lease owner/expiry, backend-resolution policy), `audio`, `chrome_bridge`, `http` (bind addr, active sessions, SSE subscriber count; `disabled` in stdio), `daemon_drain`, and `daemon_lifecycle`. `tool_surface_sha256` is a deterministic SHA-256 of the session-profile-gated, canonically key-sorted tool list, for drift detection.

### 8.2 Doctor

See section 5.8 — `--mode doctor` / `--kill-stray` is the operational triage that proves which process owns the RocksDB lock and removes the rest.
