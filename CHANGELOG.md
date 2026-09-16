# Changelog

## Unreleased

- Added the shared-daemon architecture so many agents can use Synapse at once
  against one RocksDB without leaked/duplicate instances: a single `--mode http`
  daemon owns the DB; stdio-only clients connect through a new `--mode connect`
  bridge (auto-spawns the daemon, parent-death watchdog); a single-instance
  guard (`<db>/daemon.lock` + `daemon.pid`) makes a duplicate daemon exit 3;
  storage opens eagerly at startup (fail-fast `STORAGE_LOCK_CONTENDED`/
  `STORAGE_OPEN_FAILED`, exit 4); `/health` reports `pid`; and a new
  `--mode doctor [--kill-stray]` enumerates/classifies synapse-mcp processes and
  cleans matching strays only after identifying the live lock-holder daemon. See
  `docs/shared-daemon.md`. (#661)
- Added the repository agent doctrine: manual FSV must be performed by the
  agent with direct source-of-truth readback; automated tests, scripts,
  benchmarks, GitHub Actions, and CI are supporting evidence only.
- Retired the physical hardware-HID path after the software-only input decision
  in #588. The active documented action backend is now `software`; the legacy
  `hardware` token remains only as a fail-closed compatibility value that
  returns `ACTION_BACKEND_UNAVAILABLE`.

## v0.1.0-m4 - 2026-05-28

M4 adds the hardware-HID profile surface for the configured Windows host, with
manual configured-host FSV remaining the release gate rather than GitHub
Actions/CI.

- Added the three M4 MCP tools: `act_combo`, `act_run_shell`, and
  `act_launch`, bringing the M4 baseline to 33 tools before later
  profile-registry and audit-data work expanded the live surface.
- Wired the hardware HID runtime path: `Backend::Hardware`,
  `synapse-hid-host` serial framing, RP2040 CDC auto-detect through
  `--hardware-hid auto`, IDENTIFY/version checks, reconnect handling, hardware
  release-all, and fail-closed software fallback when no hardware path is
  configured.
- Added `synapse-mcp hid identify` and `synapse-mcp hid flash` plus the bundled
  release firmware asset `scripts/release/firmware/pico-hid-0.1.0-m4.uf2`.
- Added hardware-HID consent state in `%APPDATA%/synapse/agreement.json`,
  including supported-use scopes and the explicit
  `I AUTHORIZE HARDWARE INPUT` acknowledgement.
- Added the M4 HID and safety error families:
  `HID_PORT_NOT_FOUND`, `HID_PORT_OPEN_FAILED`,
  `HID_PROTOCOL_HANDSHAKE_FAILED`, `HID_FIRMWARE_VERSION_MISMATCH`,
  `HID_COMMAND_REJECTED`, `HID_LINK_TIMEOUT`,
  `SAFETY_SHELL_DENIED_BY_POLICY`, and `SAFETY_LAUNCH_DENIED_BY_POLICY`.
- Recorded ADR-0008 through ADR-0012 for Synapse Pico VID/PID, default
  detection model, aim-track EMA smoothing, and hardware action coalescing.
## v0.1.0-m3 - 2026-05-25

M3 adds the reflex, storage, profile, audio, replay, and HTTP/SSE runtime
surface for Synapse, shipping from manual configured-host FSV rather than CI.

- Added the M3 MCP tools: `subscribe`, `subscribe_cancel`,
  `reflex_register`, `reflex_cancel`, `reflex_list`, `reflex_history`,
  `profile_list`, `profile_activate`, `replay_record`, `audio_tail`,
  `audio_transcribe`, `storage_inspect`, `storage_put_probe_rows`,
  `storage_gc_once`, and `storage_pressure_sample`.
- Filled the M3 crates `synapse-reflex`, `synapse-storage`,
  `synapse-profiles`, and `synapse-audio` with the runtime implementations
  used by the daemon.
- Added streamable HTTP transport and manual SSE event routes with bearer-token
  auth plus Origin and Host enforcement.
- Added the M3 error families `REFLEX_RECURSION_LIMIT`, `HTTP_*`,
  `STORAGE_DISK_PRESSURE_LEVEL_1` through `STORAGE_DISK_PRESSURE_LEVEL_4`, and
  `REPLAY_*`.
- Carried forward M2 fixes for #244, #234, #233, #231, and #239, including the
  LoC splits needed to keep M2 action modules maintainable.
- Recorded the M3 operating doctrine: manual FSV must use the real Synapse
  runtime surface where available, then separately inspect the physical SoT.

## v0.1.0-m2 - 2026-05-24

M2 adds the action MVP for the configured Windows host with manual FSV as the
release gate.

- Added the M2 MCP action tools: `act_click`, `act_type`, `act_press`,
  `act_aim`, `act_drag`, `act_scroll`, `act_clipboard`, and `release_all`.
- Wired real Windows input paths for keyboard, mouse, and UIA InvokePattern.
- Added ReleaseAll safety coverage for explicit cleanup, shutdown, SIGINT,
  stdio disconnect, and panic paths.
- Clarified that M2 ships from manual configured-host FSV, not GitHub
  Actions/CI or missing-dependency portability tests.

## v0.1.0-m0 - 2026-05-23

M0 bootstraps Synapse as a Rust MCP server with a single `health` tool over stdio.

- Added the Rust workspace, crate skeletons, dual license files, cargo-deny configuration, CI workflow, and helper scripts.
- Implemented `synapse-core` shared types, schema version, and M0 error-code constants.
- Implemented `synapse-telemetry` JSON file logging, console logging, and log-dir validation.
- Implemented `synapse-mcp` stdio startup, CLI flags, graceful shutdown logging, and the `health` MCP tool.
- Implemented `synapse-test-utils` raw stdio JSON-RPC client and M0 end-to-end health demo tests.
- Added root README quick start, current Rust/dependency ADR, documentation link checking, and WSL-global Codex/Claude Code MCP configuration guidance.
