# 02. Source Code Map

**Source files covered:** entire workspace tree across 14 crates plus top-level non-Rust components; key `lib.rs`/`main.rs`/`server.rs` and module-root files of each crate read in detail; one-line descriptions for remaining files derived from module names, doc-comments, and key declarations.

See [01_system_overview.md](01_system_overview.md) for the architectural narrative this map indexes.

---

## 1. Workspace Layout

Root: `C:\code\synapse\Cargo.toml` — `resolver = "2"`, `edition = "2024"`, `rust-version = "1.95"`, `version = "0.1.0"`, `license-file = "LICENSE.md"`.

**`[workspace] members`** (14 crates):
`synapse-mcp`, `synapse-core`, `synapse-capture`, `synapse-a11y`, `synapse-perception`, `synapse-audio`, `synapse-action`, `synapse-reflex`, `synapse-storage`, `synapse-profiles`, `synapse-models`, `synapse-telemetry`, `synapse-overlay`.

**`default-members`:** `synapse-mcp`, `synapse-overlay` (the two shipped binaries).

**Key shared `[workspace.dependencies]`** (selected):

| Area | Crates |
|---|---|
| Async runtime | `tokio` (full), `tokio-util`, `tokio-tungstenite`, `futures-util` |
| MCP / HTTP | `rmcp` 1.7 (server, stdio, streamable-http, macros, schemars), `axum` 0.8 (ws), `hyper`, `tower`, `reqwest` |
| Serialization | `serde`, `serde_json`, `toml`, `schemars`, `base64` |
| Storage | `rocksdb` 0.24 (lz4, zstd, multi-threaded-cf), `fs2` |
| Windows platform | `windows` 0.62 (Win32 Foundation/UI/Graphics/Media OCR/etc.), `windows-capture`, `uiautomation` 0.25 |
| Browser/CDP | `chromiumoxide` 0.9 |
| Input/HID | `enigo`, `vigem-client`, `arboard` (clipboard), `x11rb` (non-Windows) |
| ML / audio | `ort` 2.0-rc (ONNX Runtime), `wasapi` |
| Telemetry | `tracing`, `tracing-subscriber`, `tracing-appender`, `metrics`, `metrics-exporter-prometheus`, `opentelemetry`, `opentelemetry-otlp` |
| Crypto | `ed25519-dalek`, `sha2`, `subtle` |
| CLI / util | `clap`, `chrono`, `uuid`, `regex`, `sysinfo`, `notify`, `image` |
| Test/bench | `proptest`, `criterion`, `insta`, `tempfile`, `mockall` |

**Lints:** `unsafe_code = "forbid"` workspace-wide; clippy `all = "deny"`, `pedantic`/`nursery` warn, `unwrap_used`/`expect_used` deny. (Crates needing FFI opt in with `#![allow(unsafe_code)]`: `synapse-a11y`, `synapse-capture`, `synapse-action`, `synapse-overlay`.)

**Profiles:** `dev` incremental on (line-tables debug); `release` thin-LTO, codegen-units 16, stripped, `panic = "abort"` (the shipped daemon); `release-max` fat-LTO single-codegen for max runtime.

---

## 2. Per-Crate File Trees

### crates/synapse-core
Shared types, IDs, error codes, retention/filter logic. Foundation crate; depends on nothing internal.

```
crates/synapse-core/src/lib.rs              # crate root; re-exports all type modules + SCHEMA_VERSION
crates/synapse-core/src/defaults.rs         # default constants (HUD confidence, aim-track EMA alpha)
crates/synapse-core/src/episodes.rs         # episode (activity segment) domain helpers
crates/synapse-core/src/error_codes.rs      # canonical string error-code constants used across crates
crates/synapse-core/src/filter.rs           # event filter evaluation / predicate logic
crates/synapse-core/src/intent.rs           # operator-intent model types
crates/synapse-core/src/retention.rs        # per-CF data retention TTL policy
crates/synapse-core/src/routines.rs         # mined-routine domain types
crates/synapse-core/src/types.rs            # types module root, re-exports submodules
crates/synapse-core/src/types/action.rs         # Action, Key, MouseButton, ComboStep, stroke/path specs
crates/synapse-core/src/types/agent_cost.rs      # BillableUsage, CostBreakdown, ModelPrice
crates/synapse-core/src/types/agent_event.rs     # AgentEventRecord/Kind, end-state
crates/synapse-core/src/types/agent_transcript.rs # AgentTranscriptRecord, tool-call/usage types
crates/synapse-core/src/types/episode.rs         # episode record types
crates/synapse-core/src/types/event.rs           # Event, EventSource, EventFilter, EventExtension
crates/synapse-core/src/types/geometry.rs        # Point, Rect, Size, PathPoint
crates/synapse-core/src/types/health.rs          # Health, SubsystemHealth, SensorStatus
crates/synapse-core/src/types/observation.rs     # Observation, capture config/target, diagnostics
crates/synapse-core/src/types/profile.rs         # Profile, ProfileMatch, capture/detection/ocr config
crates/synapse-core/src/types/reality.rs         # RealityAudit/Baseline/Delta drift-detection types
crates/synapse-core/src/types/reflex.rs          # ReflexKind, ReflexRegistration, Trigger, ReflexThen
crates/synapse-core/src/types/routine.rs         # routine record types
crates/synapse-core/src/types/stored.rs          # StoredEvent/Observation/Session/ReflexAudit (DB rows)
crates/synapse-core/src/types/timeline.rs        # timeline actor/entry types
crates/synapse-core/src/types/web_perception.rs  # WebPerceptionPath / CDP perception types
```
Automated tests were removed by policy; see [17_test_suite.md](17_test_suite.md).

### crates/synapse-storage
RocksDB persistence: column families, batched writes, GC, disk-pressure shedding. Depends on `synapse-core`, `synapse-telemetry`.

```
crates/synapse-storage/src/lib.rs            # Db handle: open, put/delete batch, scan/compact, GC + pressure spawn
crates/synapse-storage/src/cf.rs             # column-family name constants + ALL_COLUMN_FAMILIES
crates/synapse-storage/src/codecs.rs         # encode_json / decode_json row codecs
crates/synapse-storage/src/batch.rs          # background Batcher write aggregation thread
crates/synapse-storage/src/compaction.rs     # TTL compaction filter install per CF
crates/synapse-storage/src/gc.rs             # garbage-collection pass (soft/hard caps, tombstone compaction)
crates/synapse-storage/src/pressure.rs       # disk free-space pressure levels + write shedding
crates/synapse-storage/src/episodes.rs       # episode CF read/write helpers
crates/synapse-storage/src/timeline.rs       # timeline CF read/write helpers
crates/synapse-storage/src/routines.rs       # routine CF read/write helpers
crates/synapse-storage/src/agent_events.rs   # agent-event CF read/write helpers
crates/synapse-storage/src/agent_transcripts.rs # agent-transcript CF read/write helpers
crates/synapse-storage/src/error.rs          # StorageError / StorageResult
crates/synapse-storage/src/{batch,compaction,gc,open,pressure}_tests.rs # in-crate unit tests
crates/synapse-storage/build.rs              # build script (rocksdb link config)
```
`examples/dump_cf.rs` dumps a column family; `benches/batch_throughput.rs` write-throughput bench.

### crates/synapse-a11y
Windows UI Automation + Chrome DevTools Protocol (CDP) accessibility. `#![allow(unsafe_code)]`. Depends on `synapse-core`.

```
crates/synapse-a11y/src/lib.rs               # crate root; gates platform + CDP modules
crates/synapse-a11y/src/cdp.rs               # CDP client / browser session core
crates/synapse-a11y/src/cdp_action.rs        # CDP-driven click/type/mouse-stroke actions
crates/synapse-a11y/src/cdp_actionability.rs # element actionability checks via CDP
crates/synapse-a11y/src/cdp_binding.rs       # exposeBinding / addInitScript JS bindings
crates/synapse-a11y/src/cdp_clock.rs         # CDP virtual-clock control
crates/synapse-a11y/src/cdp_console.rs       # console message capture
crates/synapse-a11y/src/cdp_dialog.rs        # JS dialog (alert/confirm) handling
crates/synapse-a11y/src/cdp_dom.rs           # DOM snapshot / node query over CDP
crates/synapse-a11y/src/cdp_emulation.rs     # device/viewport/geolocation/media emulation
crates/synapse-a11y/src/cdp_lifecycle.rs     # page lifecycle / load-state events
crates/synapse-a11y/src/cdp_network.rs       # network request/response/HAR capture
crates/synapse-a11y/src/error.rs             # a11y error types
crates/synapse-a11y/src/events.rs            # UIA event subscription model
crates/synapse-a11y/src/ids.rs              # ElementId construction / parsing
crates/synapse-a11y/src/re_resolve.rs        # re-resolve stale element handles
crates/synapse-a11y/src/snapshot.rs          # cross-platform a11y subtree snapshot model
crates/synapse-a11y/src/ui_element.rs        # UiElement abstraction
crates/synapse-a11y/src/window.rs            # window enumeration + millis_since_last_input
crates/synapse-a11y/src/platform/mod.rs      # platform dispatch (windows vs non_windows)
crates/synapse-a11y/src/platform/non_windows.rs # stub backend off Windows
crates/synapse-a11y/src/platform/windows/mod.rs      # Windows UIA backend root
crates/synapse-a11y/src/platform/windows/common.rs   # shared UIA COM helpers
crates/synapse-a11y/src/platform/windows/events.rs   # UIA event handlers
crates/synapse-a11y/src/platform/windows/resolve.rs  # element resolution from point/handle
crates/synapse-a11y/src/platform/windows/snapshot.rs # UIA subtree walk -> snapshot
crates/synapse-a11y/src/platform/windows/window.rs   # HWND enumeration / foreground
```
`examples/cdp_*_probe.rs` (7) manual CDP probes; `benches/uia_snapshot_depth2_60elem.rs`; `tests/uwp_snapshot_regression.rs`.

### crates/synapse-capture
Screen/window capture (Windows Graphics Capture + DXGI fallback), DPI, coordinate mapping. `#![allow(unsafe_code)]`. Depends on `synapse-core`, `synapse-telemetry`.

```
crates/synapse-capture/src/lib.rs            # crate root; CaptureController, capture-loop spawn, target resolve
crates/synapse-capture/src/backend.rs        # CaptureBackend preference + DXGI fallback decision
crates/synapse-capture/src/bitmap.rs         # screen_region_to_bgra_bitmap + WinRT SoftwareBitmap helpers
crates/synapse-capture/src/config.rs         # CaptureConfig / CaptureTarget / ResolvedCaptureTarget
crates/synapse-capture/src/controller.rs     # capture loop controller + metrics registration
crates/synapse-capture/src/coords.rs         # coordinate transforms
crates/synapse-capture/src/dpi.rs            # DPI awareness init + scaling
crates/synapse-capture/src/error.rs          # capture errors
crates/synapse-capture/src/frame.rs          # captured frame buffer type
crates/synapse-capture/src/stats.rs          # CaptureStats, thread-priority knobs
crates/synapse-capture/src/platform/mod.rs           # platform dispatch
crates/synapse-capture/src/platform/non_windows.rs   # off-Windows stub
crates/synapse-capture/src/platform/windows/bitmap.rs   # Windows bitmap conversion
crates/synapse-capture/src/platform/windows/capture.rs  # WGC / DXGI frame grab
crates/synapse-capture/src/platform/windows/common.rs   # shared Win32 helpers
crates/synapse-capture/src/platform/windows/coords.rs   # window-to-screen coordinate math
crates/synapse-capture/src/platform/windows/dpi.rs      # per-monitor DPI
crates/synapse-capture/src/platform/windows/target.rs   # HWND/monitor capture target
```
`benches/capture_loop.rs`.

### crates/synapse-perception
Assembles observations from capture + a11y + OCR + object detection + HUD/template reads. Depends on `synapse-a11y`, `synapse-capture`, `synapse-core`.

```
crates/synapse-perception/src/lib.rs            # crate root; observation assembly + perception-mode parse exports
crates/synapse-perception/src/observe.rs        # ObservationAssembler, auto-mode, a11y-tree summary
crates/synapse-perception/src/ocr.rs            # OcrProvider, SystemOcrProvider (Windows Media OCR), read_text
crates/synapse-perception/src/template_match.rs # HUD template/counter matching from frames
crates/synapse-perception/src/event_extensions.rs # profile-defined event-extension evaluation
crates/synapse-perception/src/error.rs          # perception errors
crates/synapse-perception/src/hud/mod.rs         # HUD parsing module root
crates/synapse-perception/src/hud/anchor.rs      # HUD anchor-region resolution
crates/synapse-perception/src/hud/extractor.rs   # HUD field extraction from regions
```
Tests + benches cover hud_anchor, hud_extractor, template_match, event_extensions, perception/CDP regression.

### crates/synapse-audio
WASAPI loopback capture, ring buffer, direction estimation, Whisper STT (ONNX). Depends on `synapse-core`, `synapse-models`.

```
crates/synapse-audio/src/lib.rs        # AudioRuntime: spawn loopback/detectors, tail, transcribe, direction
crates/synapse-audio/src/loopback.rs   # WASAPI loopback capture handle
crates/synapse-audio/src/ring.rs       # AudioRing buffer + AudioWindow tail
crates/synapse-audio/src/detectors.rs  # audio-cue detector processor + shared state
crates/synapse-audio/src/direction.rs  # stereo direction estimation
crates/synapse-audio/src/stt.rs        # WhisperTinyStt model load + transcribe
crates/synapse-audio/src/stt/window.rs # STT audio-window framing
crates/synapse-audio/src/error.rs      # audio errors
```
Automated tests were removed by policy; see [17_test_suite.md](17_test_suite.md).

### crates/synapse-action
Input emission: software (enigo), ViGEm gamepad, recording backends; humanized curves, leases, safety. `#![allow(unsafe_code)]`. Depends on `synapse-core`.

```
crates/synapse-action/src/lib.rs           # crate root; ActionEmitter/Backend/handle/lease exports
crates/synapse-action/src/emitter.rs        # ActionEmitter module root (state, dispatch, rate limits)
crates/synapse-action/src/emitter/backends.rs    # backend set wiring
crates/synapse-action/src/emitter/dispatch.rs     # action dispatch to resolved backend
crates/synapse-action/src/emitter/keyboard.rs     # keyboard emission + held-key tracking
crates/synapse-action/src/emitter/lifecycle.rs    # emitter lifecycle / shutdown
crates/synapse-action/src/emitter/rate_limits.rs  # per-backend token-bucket rate limiting
crates/synapse-action/src/emitter/routing.rs      # route action to software/vigem/recording
crates/synapse-action/src/emitter/state.rs        # EmitState / held-input snapshot
crates/synapse-action/src/backend/mod.rs          # ActionBackend trait + resolve_backend policy
crates/synapse-action/src/backend/software.rs     # software (enigo) backend root
crates/synapse-action/src/backend/software/input.rs    # low-level synthesized input
crates/synapse-action/src/backend/software/keyboard.rs # software keyboard
crates/synapse-action/src/backend/software/mouse.rs    # software mouse
crates/synapse-action/src/backend/software/text.rs     # software unicode text entry
crates/synapse-action/src/backend/software/utils.rs    # software-backend helpers
crates/synapse-action/src/backend/software_non_windows.rs # off-Windows software stub
crates/synapse-action/src/backend/mouse_coordinates.rs # mouse absolute/relative coord math
crates/synapse-action/src/backend/text_dispatch.rs     # text dispatch strategy
crates/synapse-action/src/backend/recording.rs         # RecordingBackend (capture inputs, no emit)
crates/synapse-action/src/backend/recording/state.rs   # recorded-input state
crates/synapse-action/src/backend/unavailable.rs       # HardwareUnavailableBackend (fails loud)
crates/synapse-action/src/backend/vigem.rs             # ViGEm virtual-gamepad backend root
crates/synapse-action/src/backend/vigem/client.rs      # ViGEm bus client
crates/synapse-action/src/backend/vigem/pad.rs         # virtual pad lifecycle
crates/synapse-action/src/backend/vigem/reports.rs     # XInput report building
crates/synapse-action/src/backend/vigem/state.rs       # pad state
crates/synapse-action/src/backend/vigem/error.rs       # vigem errors
crates/synapse-action/src/handle.rs        # ActionHandle queue + combo scheduler + session input snapshot
crates/synapse-action/src/invoke.rs         # invoke-element module root (click element or coord fallback)
crates/synapse-action/src/invoke/dispatch.rs    # element-invoke dispatch
crates/synapse-action/src/invoke/resolver.rs    # resolve element click target
crates/synapse-action/src/lease.rs          # process-global input lease (TTL, preempt, handoff)
crates/synapse-action/src/hotkey.rs         # operator panic-hotkey (release-all) guard
crates/synapse-action/src/safety.rs         # panic hook -> release all inputs
crates/synapse-action/src/recovery.rs       # crash-recovery ledger (recover stale held inputs)
crates/synapse-action/src/clipboard.rs      # clipboard snapshot/restore (arboard)
crates/synapse-action/src/curve.rs          # mouse-path curve sampling
crates/synapse-action/src/dynamics.rs       # humanized keystroke timing (bigram model)
crates/synapse-action/src/humanize.rs       # humanize a timed path
crates/synapse-action/src/path.rs           # spatial path + arc-length parameterization
crates/synapse-action/src/stroke.rs         # timed mouse-stroke planning
crates/synapse-action/src/velocity.rs       # Fitts-law velocity profile / timing
crates/synapse-action/src/click_timing.rs   # double-click timing cache
crates/synapse-action/src/rate_limit.rs     # TokenBucket primitive
crates/synapse-action/src/validation.rs     # validate_action (drag distance, bounds)
crates/synapse-action/src/error.rs          # ActionError / ActionResult
```
Extensive `tests/*.rs` (curve, dynamics, path, stroke, velocity, lease/handle, safety, vigem) + 5 benches.

### crates/synapse-reflex
Low-latency reactive automations (combos, hold, aim-track, path-follow) driven by an event bus + scheduler. Depends on `synapse-action`, `synapse-core`, `synapse-storage`.

```
crates/synapse-reflex/src/lib.rs            # crate root; runtime, scheduler, bus, reflex-kind exports
crates/synapse-reflex/src/runtime.rs        # ReflexRuntime (register/cancel/tick orchestration)
crates/synapse-reflex/src/bus.rs            # EventBus + bounded subscriber queues
crates/synapse-reflex/src/dispatch.rs       # ReflexActionGate (permission-gated action dispatch)
crates/synapse-reflex/src/conflict.rs       # reflex priority conflict + starvation detection
crates/synapse-reflex/src/lifecycle.rs      # reflex lifetime/expiry
crates/synapse-reflex/src/listing.rs        # list registered reflexes
crates/synapse-reflex/src/storage.rs        # reflex audit persistence to storage
crates/synapse-reflex/src/audit.rs          # write_audit reflex-step audit
crates/synapse-reflex/src/audit_state.rs    # audit state tracking
crates/synapse-reflex/src/action_combo_bridge.rs # install action combo scheduler bridge
crates/synapse-reflex/src/scheduler.rs      # ReflexScheduler core (priority, tick)
crates/synapse-reflex/src/scheduler_combo.rs    # combo scheduling
crates/synapse-reflex/src/scheduler_handle.rs   # scheduler handle
crates/synapse-reflex/src/scheduler_loop.rs     # tick loop driver
crates/synapse-reflex/src/scheduler_stateful.rs # stateful reflex scheduling
crates/synapse-reflex/src/scheduler_stats.rs    # tick jitter / p99 stats
crates/synapse-reflex/src/scheduler_tick.rs     # per-tick step execution
crates/synapse-reflex/src/scheduler_windows.rs  # high-res Windows timer integration
crates/synapse-reflex/src/kinds/mod.rs          # reflex-kind module root
crates/synapse-reflex/src/kinds/aim_track.rs    # aim-tracking controller (EMA correction)
crates/synapse-reflex/src/kinds/combo.rs        # multi-step combo controller
crates/synapse-reflex/src/kinds/hold_button.rs  # hold-button controller
crates/synapse-reflex/src/kinds/hold_lifetime.rs # hold-with-lifetime release
crates/synapse-reflex/src/kinds/hold_move.rs    # hold-and-move controller
crates/synapse-reflex/src/kinds/on_event.rs     # on-event firing (debounce, recursion guard)
crates/synapse-reflex/src/kinds/path_follow.rs  # path-follow controller
crates/synapse-reflex/src/error.rs          # reflex errors
```
6 behavior `tests/*.rs` + 4 benches (tick jitter, combo interval, event-to-subscriber).

### crates/synapse-profiles
Per-application profile parsing (TOML), matching/resolution, hot-reload watcher, signed profile packages. Depends on `synapse-core`.

```
crates/synapse-profiles/src/lib.rs        # crate root; parser/resolver/watcher/package exports
crates/synapse-profiles/src/parser.rs      # parse profile TOML -> LoadedProfile, bundled dir
crates/synapse-profiles/src/resolver.rs    # resolve active profile from foreground window
crates/synapse-profiles/src/watcher.rs     # ProfileRuntime hot-reload + foreground transitions
crates/synapse-profiles/src/toml_format.rs # TOML (de)serialization format glue
crates/synapse-profiles/src/error.rs       # ProfileError / ProfileLoadError
crates/synapse-profiles/src/package/mod.rs        # signed profile-package manifest root
crates/synapse-profiles/src/package/types.rs      # package manifest type definitions
crates/synapse-profiles/src/package/digest.rs     # manifest digest computation
crates/synapse-profiles/src/package/validation.rs # package permission/signature validation
```
Automated tests were removed by policy; see [17_test_suite.md](17_test_suite.md).

### crates/synapse-models
ONNX model registry, download, verification (sha256), ORT session loading (DirectML EP). Depends on `synapse-core`.

```
crates/synapse-models/src/lib.rs       # Detector trait, DetectionFrame/DetectOpts, registry exports
crates/synapse-models/src/registry.rs   # registered models (RT-DETRv2-S COCO), class map, defaults
crates/synapse-models/src/download.rs   # ModelDescriptor + model download/dir resolution
crates/synapse-models/src/session.rs    # ModelLoader / ORT session factory + LoadedModel
crates/synapse-models/src/ep.rs         # execution-provider order (DirectML/CPU)
crates/synapse-models/src/verify.rs     # sha256_file / digest normalization
crates/synapse-models/src/error.rs      # model errors
```
Automated tests were removed by policy; see [17_test_suite.md](17_test_suite.md).

### crates/synapse-telemetry
Tracing/log init (JSON file + console), log-dir GC, metrics registration, panic hook. Depends on `synapse-core`.

```
crates/synapse-telemetry/src/lib.rs      # init_tracing, TelemetryGuard, log GC worker, panic hook
crates/synapse-telemetry/src/metrics.rs   # M3 metric registration helpers
```
Automated tests were removed by policy; see [17_test_suite.md](17_test_suite.md).

### crates/synapse-overlay
System-tray companion binary (Windows). `#![allow(unsafe_op_in_unsafe_fn)]`. Depends on `synapse-core`, `synapse-telemetry`.

```
crates/synapse-overlay/src/main.rs   # Win32 system-tray icon + popup menu (daemon status/links)
```

### crates/synapse-mcp
The daemon: MCP server binding all subsystems, organized M1 (perception) → M2 (action) → M3 (background/agents) → M4 (orchestration/shell). Depends on the runtime crates except `test-utils`/`overlay`. ~202 source files.

**Crate root / lifecycle / transports:**
```
crates/synapse-mcp/src/main.rs              # binary entry: CLI (8 modes), tokio runtime, telemetry, dispatch
crates/synapse-mcp/src/server.rs            # SynapseService: rmcp ServerHandler, ToolRouter, shared state
crates/synapse-mcp/src/connect.rs           # --mode connect: thin stdio<->HTTP bridge to shared daemon
crates/synapse-mcp/src/doctor.rs            # --mode doctor: enumerate/kill stray daemon processes
crates/synapse-mcp/src/daemon_lifecycle.rs  # lifecycle ledger (run/exit/tool event files)
crates/synapse-mcp/src/single_instance.rs   # single-instance lock per DB path
crates/synapse-mcp/src/stdio_eof.rs         # CancelOnEofRead: cancel on stdin EOF
crates/synapse-mcp/src/safety.rs            # install operator panic-hotkey for the service
crates/synapse-mcp/src/secret_crypto.rs     # secret encryption helpers
crates/synapse-mcp/src/desktop_worker.rs    # --mode desktop-worker: hidden-desktop UIA/PrintWindow child
crates/synapse-mcp/src/approval_protocol.rs # --mode approval-protocol: actionable-toast activation child
crates/synapse-mcp/src/local_agent.rs       # --mode local-agent: run a local model as an MCP agent
crates/synapse-mcp/src/chrome_debugger_bridge.rs # Chrome native-messaging host + WS bridge
```

**`src/bin/` (extra binary):**
```
crates/synapse-mcp/src/bin/synapse-chrome-native-host.rs # standalone Chrome native-messaging host exe
```

**`src/http/` (HTTP transport + SSE):**
```
crates/synapse-mcp/src/http/mod.rs        # axum HTTP serve(), /mcp + /dashboard routes
crates/synapse-mcp/src/http/auth.rs        # bearer-token auth, loopback gate
crates/synapse-mcp/src/http/session.rs     # per-connection MCP session
crates/synapse-mcp/src/http/transport.rs   # streamable-HTTP transport glue
crates/synapse-mcp/src/http/sse.rs         # SSE module root + SseState
crates/synapse-mcp/src/http/sse/stream.rs  # SSE event stream
crates/synapse-mcp/src/http/sse/ring.rs    # SSE replay ring buffer
crates/synapse-mcp/src/http/sse/replay.rs  # last-event-id replay
crates/synapse-mcp/src/http/sse/lossy.rs   # lossy (drop-on-overflow) channel
```

**`src/m1/` — perception tools (observe / find / capture / browser perception):**
```
crates/synapse-mcp/src/m1.rs               # M1State, observe/find/read-text input building, browser+CDP enrich
crates/synapse-mcp/src/m1/detection.rs      # object-detection runtime (ONNX) integration
crates/synapse-mcp/src/m1/ocr.rs            # OCR request resolution + capture-source selection
crates/synapse-mcp/src/m1/search.rs         # find/search over observation elements
crates/synapse-mcp/src/m1/sources.rs        # perception input sources (fs recent, clipboard, etc.)
```

**`src/m2/` — action tools (click/press/type/scroll/stroke/pad/clipboard):**
```
crates/synapse-mcp/src/m2.rs               # M2 service state, emitter wiring, foreground restore policy
crates/synapse-mcp/src/m2/auto_wait.rs      # auto-wait for actionability before acting
crates/synapse-mcp/src/m2/click.rs          # act_click root
crates/synapse-mcp/src/m2/click/element.rs   # element-target click
crates/synapse-mcp/src/m2/click/record.rs    # click action recording
crates/synapse-mcp/src/m2/click/schema.rs    # click param JSON schema
crates/synapse-mcp/src/m2/press.rs          # act_press (key) root
crates/synapse-mcp/src/m2/press/keys.rs      # key resolution
crates/synapse-mcp/src/m2/press/live.rs      # live key press/hold
crates/synapse-mcp/src/m2/press/postmessage.rs # background PostMessage key delivery
crates/synapse-mcp/src/m2/press/record.rs    # press recording
crates/synapse-mcp/src/m2/press/schema.rs    # press param schema
crates/synapse-mcp/src/m2/type_text.rs      # act_type text entry
crates/synapse-mcp/src/m2/set_field_text.rs # set field text via a11y value pattern
crates/synapse-mcp/src/m2/set_value.rs      # act_set_value
crates/synapse-mcp/src/m2/scroll.rs         # act_scroll
crates/synapse-mcp/src/m2/stroke.rs         # act_stroke (humanized mouse stroke)
crates/synapse-mcp/src/m2/pad.rs            # act_pad (gamepad)
crates/synapse-mcp/src/m2/clipboard.rs      # act_clipboard (per-session buffers)
crates/synapse-mcp/src/m2/focus_window.rs   # act_focus_window
crates/synapse-mcp/src/m2/release_all.rs    # release all held inputs
crates/synapse-mcp/src/m2/postcondition.rs  # post-action verification
crates/synapse-mcp/src/m2/config.rs         # M2ServiceConfig (from env)
```

**`src/m3/` — background services, agents, audit, profiles, reflex, timeline:**
```
crates/synapse-mcp/src/m3.rs               # M3 service state: storage, profiles, reflex, audio, bus, recorder
crates/synapse-mcp/src/m3/activity_recorder.rs # records events/observations to timeline
crates/synapse-mcp/src/m3/a11y_events.rs    # UIA event -> event-bus bridge
crates/synapse-mcp/src/m3/audio.rs          # audio tail/transcribe tools
crates/synapse-mcp/src/m3/approvals.rs      # approval request/decide/list + actionable toasts
crates/synapse-mcp/src/m3/permissions.rs    # RequiredPermissions + authorization gating
crates/synapse-mcp/src/m3/armed_routines.rs # armed-routine tick execution
crates/synapse-mcp/src/m3/routines.rs       # routine lifecycle tools
crates/synapse-mcp/src/m3/routine_miner_job.rs # background routine mining job
crates/synapse-mcp/src/m3/intent.rs         # operator-intent current/update
crates/synapse-mcp/src/m3/intent_events.rs  # intent event stream
crates/synapse-mcp/src/m3/episodes.rs       # episode get/list/segment tools
crates/synapse-mcp/src/m3/timeline.rs       # timeline get/search/digest queries
crates/synapse-mcp/src/m3/timeline_control.rs # recorder start/stop control
crates/synapse-mcp/src/m3/interaction_cadence.rs # interaction cadence metrics
crates/synapse-mcp/src/m3/hygiene.rs        # PII/secret hygiene scan (text/storage/flags)
crates/synapse-mcp/src/m3/audit_export.rs   # export audit bundle
crates/synapse-mcp/src/m3/audit_retention.rs # audit retention enforcement
crates/synapse-mcp/src/m3/replay.rs         # session replay root
crates/synapse-mcp/src/m3/replay/events.rs   # replay event stream
crates/synapse-mcp/src/m3/replay/observations.rs # replay observations
crates/synapse-mcp/src/m3/replay/record.rs    # replay recording
crates/synapse-mcp/src/m3/replay/serializer.rs # replay serialization
crates/synapse-mcp/src/m3/demo_recording.rs # demo (recording-backend) start/stop
crates/synapse-mcp/src/m3/plan.rs           # plan model
crates/synapse-mcp/src/m3/plan_execution.rs # plan execution engine
crates/synapse-mcp/src/m3/profile.rs        # profile list/activate tools
crates/synapse-mcp/src/m3/profile_authoring.rs # profile authoring (generate/inspect/export/decide)
crates/synapse-mcp/src/m3/profile_quality.rs # profile quality scoring root
crates/synapse-mcp/src/m3/profile_quality/aggregate.rs # quality aggregation
crates/synapse-mcp/src/m3/profile_quality/model.rs     # quality model
crates/synapse-mcp/src/m3/profile_registry.rs # curated profile registry
crates/synapse-mcp/src/m3/reflex.rs         # reflex tools root
crates/synapse-mcp/src/m3/reflex/register.rs  # reflex register
crates/synapse-mcp/src/m3/reflex/cancel.rs    # reflex cancel
crates/synapse-mcp/src/m3/reflex/list.rs      # reflex list
crates/synapse-mcp/src/m3/reflex/history.rs   # reflex history
crates/synapse-mcp/src/m3/reflex/file_jsonl_tail.rs # tail reflex jsonl audit
crates/synapse-mcp/src/m3/reflex/common.rs    # shared reflex-tool helpers
crates/synapse-mcp/src/m3/local_models.rs   # local-model register/list/probe/update/remove
crates/synapse-mcp/src/m3/subscribe.rs      # event subscription tool
crates/synapse-mcp/src/m3/suggestions.rs    # suggestion list/accept/tick
crates/synapse-mcp/src/m3/storage.rs        # storage inspect tool
crates/synapse-mcp/src/m3/config.rs (in m3.rs) # M3ServiceConfig (from CLI/env)
```

**`src/m4.rs` + `src/server/` — orchestration, multi-agent, and browser tools:**
```
crates/synapse-mcp/src/m4.rs               # M4 service: act_run_shell / act_launch allow-list config
crates/synapse-mcp/src/server.rs           # (see lifecycle) ServerHandler + tool_router aggregation
crates/synapse-mcp/src/server/handler.rs    # ServerHandler impl / tool dispatch
crates/synapse-mcp/src/server/context.rs    # shared server context/state
crates/synapse-mcp/src/server/m1_tools.rs   # M1 perception tool registrations
crates/synapse-mcp/src/server/m2_tools.rs   # M2 action tool registrations
crates/synapse-mcp/src/server/m3_tools.rs   # M3 background tool registrations
crates/synapse-mcp/src/server/m4_tools.rs   # M4 shell/launch/agent tool registrations
crates/synapse-mcp/src/server/health.rs     # health tool
crates/synapse-mcp/src/server/drain.rs      # graceful drain on shutdown
crates/synapse-mcp/src/server/background_router.rs # background task routing
crates/synapse-mcp/src/server/schema_sanitize.rs   # JSON-schema sanitization for tool params
crates/synapse-mcp/src/server/session_lifecycle.rs # MCP session lifecycle
crates/synapse-mcp/src/server/session_registry.rs  # per-session registry
crates/synapse-mcp/src/server/session_continuity.rs # session continuity across reconnect
crates/synapse-mcp/src/server/session_tools.rs      # session list/status/end tools
crates/synapse-mcp/src/server/target_claims.rs      # target claim/adopt/release coordination
crates/synapse-mcp/src/server/target_policy.rs      # target-claim policy
crates/synapse-mcp/src/server/lease_tools.rs        # control-lease acquire/release/handoff tools
crates/synapse-mcp/src/server/permission_gate.rs    # permission gating
crates/synapse-mcp/src/server/permission_policy.rs  # permission policy resolution
crates/synapse-mcp/src/server/action_audit.rs       # action audit logging
crates/synapse-mcp/src/server/action_preflight.rs   # pre-action gating/preflight
crates/synapse-mcp/src/server/audit_context.rs      # audit context capture
crates/synapse-mcp/src/server/command_audit.rs      # shell-command audit
crates/synapse-mcp/src/server/data_cleaning.rs      # data-cleaning tool
crates/synapse-mcp/src/server/reality.rs            # reality audit/baseline tools
crates/synapse-mcp/src/server/timeline_query.rs     # timeline query tool impls
crates/synapse-mcp/src/server/timeline_digest.rs    # timeline digest tool
crates/synapse-mcp/src/server/hygiene_report.rs     # hygiene report tool
crates/synapse-mcp/src/server/intent_tools.rs       # intent tool impls
crates/synapse-mcp/src/server/plan_tools.rs         # plan tool impls
crates/synapse-mcp/src/server/suggestions.rs        # suggestion tool impls
crates/synapse-mcp/src/server/routine_feedback.rs   # routine feedback
crates/synapse-mcp/src/server/routine_labeling.rs   # routine labeling
crates/synapse-mcp/src/server/notify_tools.rs       # notify-human toast tools
crates/synapse-mcp/src/server/tool_profiles.rs      # tool-profile (enabled-tool set) management
crates/synapse-mcp/src/server/escalation/mod.rs     # escalation list/ack
crates/synapse-mcp/src/server/workspace_blackboard.rs # workspace blackboard (shared KV for agents)
crates/synapse-mcp/src/server/codex_app_server_bridge.rs # Codex app-server bridge
crates/synapse-mcp/src/server/terminal_capture.rs       # terminal capture root
crates/synapse-mcp/src/server/terminal_capture/asciicast.rs   # asciicast recording
crates/synapse-mcp/src/server/terminal_capture/capture.rs     # terminal capture loop
crates/synapse-mcp/src/server/terminal_capture/shadow_screen.rs # shadow-screen model
```

**`src/server/` — multi-agent control & cost/transcripts:**
```
crates/synapse-mcp/src/server/agent_control.rs      # spawn/kill/pause/resume/interrupt/steer agents
crates/synapse-mcp/src/server/agent_state.rs         # agent runtime state
crates/synapse-mcp/src/server/agent_query.rs         # agent query tool
crates/synapse-mcp/src/server/agent_stats.rs         # agent stats
crates/synapse-mcp/src/server/agent_cost.rs          # agent cost/receipts accounting
crates/synapse-mcp/src/server/agent_tasks.rs         # task create/claim/dispatch/reconcile queue
crates/synapse-mcp/src/server/agent_templates.rs     # agent template CRUD
crates/synapse-mcp/src/server/agent_mailbox.rs       # agent send/broadcast/inbox messaging
crates/synapse-mcp/src/server/agent_events.rs        # agent event records
crates/synapse-mcp/src/server/agent_event_ingress.rs # ingress for agent-emitted events
crates/synapse-mcp/src/server/agent_transcripts.rs   # agent transcript records
crates/synapse-mcp/src/server/ambient_agents.rs      # ambient (always-on) agent management
```

**`src/server/browser_*` — Playwright-style browser tools (over CDP, see [synapse-a11y]):**
```
crates/synapse-mcp/src/server/browser_assert.rs       # browser_assert
crates/synapse-mcp/src/server/browser_clock_events.rs # clock / page events
crates/synapse-mcp/src/server/browser_dialog.rs       # dialog handling
crates/synapse-mcp/src/server/browser_dnd.rs          # drag-and-drop
crates/synapse-mcp/src/server/browser_emulate.rs      # unified emulation tool surface (viewport/device/geolocation/locale/media/network)
crates/synapse-mcp/src/server/browser_field.rs        # form-field fill/set
crates/synapse-mcp/src/server/browser_frames.rs       # frame enumeration
crates/synapse-mcp/src/server/browser_network.rs      # network requests/HAR/websockets/overrides
crates/synapse-mcp/src/server/browser_storage.rs      # cookies/storage tool
```
M4 cdp tools (`cdp_open_tab`, etc.) live in m1/server modules above.

Examples: `examples/dump_action_log.rs`, `dump_agent_events.rs`, `dump_agent_transcripts.rs`. Automated integration tests were removed by policy; manual FSV is the behavioral gate.

---

## 3. Inter-Crate Dependency Graph

Edges from each crate's `Cargo.toml [dependencies]` (internal `synapse-*` only). `synapse-core` is the root (no internal deps); `synapse-mcp` is the sink (depends on nearly all).

```
synapse-mcp     -> synapse-action, synapse-a11y, synapse-audio, synapse-core,
                   synapse-capture, synapse-models,
                   synapse-perception, synapse-profiles, synapse-reflex,
                   synapse-storage, synapse-telemetry
synapse-perception -> synapse-a11y, synapse-capture, synapse-core
synapse-reflex     -> synapse-action, synapse-core, synapse-storage
synapse-audio      -> synapse-core, synapse-models
synapse-capture    -> synapse-core, synapse-telemetry
synapse-storage    -> synapse-core, synapse-telemetry
synapse-overlay    -> synapse-core, synapse-telemetry
synapse-action     -> synapse-core
synapse-a11y       -> synapse-core
synapse-models     -> synapse-core
synapse-profiles   -> synapse-core
synapse-telemetry  -> synapse-core
synapse-core       -> (none — root)
```

| Crate | Internal deps | Role |
|---|---|---|
| synapse-core | — | shared types/IDs/error codes (root) |
| synapse-telemetry | core | tracing/log init, metrics |
| synapse-storage | core, telemetry | RocksDB persistence |
| synapse-a11y | core | UIA + CDP accessibility |
| synapse-capture | core, telemetry | screen/window capture |
| synapse-action | core | input emission, leases, safety |
| synapse-models | core | ONNX model registry/loading |
| synapse-profiles | core | per-app profile parsing/matching |
| synapse-perception | a11y, capture, core | observation assembly + OCR |
| synapse-audio | core, models | loopback + STT |
| synapse-reflex | action, core, storage | reactive automation engine |
| synapse-mcp | all above | the daemon / MCP server (sink) |
| synapse-overlay | core, telemetry | tray companion binary |

---

## 4. Entry Points

### `synapse-mcp` daemon — `crates/synapse-mcp/src/main.rs`
`main()` builds a multi-thread Tokio runtime → `run()`:
1. If invoked as a Chrome native-messaging host (argv origin), routes to `chrome_debugger_bridge::run_native_host`.
2. Parses `Cli` (`clap`), configures telemetry (`synapse-telemetry::init_tracing`), then branches on `--mode`:
   - `connect` → `connect::run_connect` (stdio↔HTTP proxy)
   - `doctor` → `doctor::run_doctor`
   - `chrome-native-host` → `chrome_debugger_bridge::run_native_host`
   - `approval-protocol` → `approval_protocol::run_protocol_activation`
   - `desktop-worker` → `desktop_worker::run_worker_from_cli`
   - `local-agent` → `local_agent::run_from_cli`
   - `stdio` / `http` → full daemon.
3. Full-daemon path: `synapse_capture::init_process_dpi_awareness`, `synapse_action::configure_crash_recovery_file` + `recover_stale_inputs_from_configured_path`, builds `m2/m3/m4` configs.
4. `run_stdio` (or `http::serve`): acquires `single_instance::SingleInstanceGuard` per DB path, configures `daemon_lifecycle`, builds `server::SynapseService` (M1–M4 state: storage/profiles/reflex/audio/emitter), installs `synapse_action` panic hook + operator hotkey, serves rmcp over stdio/HTTP with graceful Ctrl-C/Ctrl-Break shutdown.

### `synapse-overlay` tray — `crates/synapse-overlay/src/main.rs`
Windows-only; `tray::run()` registers a Win32 window class + Shell notify-icon and popup menu (daemon status/links). No-op stub off Windows.

### Chrome native host — `crates/synapse-mcp/src/bin/synapse-chrome-native-host.rs`
Standalone exe (`windows_subsystem = "windows"`). `main()` → parses native-host invocation from argv → reads `SYNAPSE_BIND` → telemetry (console off, file on) → `chrome_debugger_bridge::run_native_host(bind, invocation)`. Reuses `chrome_debugger_bridge.rs` via `#[path]`. Startup errors written to `%APPDATA%/synapse/chrome-debugger/native-host-startup-error.log`.

---

## 5. Non-Rust Components

### `dashboard/` — Command Center web UI
React 19 + Vite + TypeScript SPA (`@synapse/command-center-dashboard`, private). Local-only browser dashboard for the daemon; built static assets in `dashboard/dist/` are embedded in the Rust daemon and served on loopback under `/dashboard`. Stack: Radix UI, TanStack Query/Table, Zustand, Recharts, xterm.js, Tailwind 4, Storybook 9, Playwright (visual + a11y). `bun run build` → `tsc -b && vite build`. `src/` (app.tsx, components/ui, primitives, store, stories, styles); `.storybook/`, `tests/`, `design/`. Bun/Vite/Node are build-time only — not part of runtime.

### `extensions/synapse-chrome-debugger/`
Chrome MV3 extension "Synapse Chrome Bridge" (v0.1.1, min Chrome 125). Controls normal-profile Chrome tabs via a direct `127.0.0.1:7700` WebSocket bridge to the daemon. Permissions: `debugger`, `tabs`, `scripting`, `cookies`, `webRequest`, `webNavigation`, `alarms`, `management`; host perms `<all_urls>` + loopback. Files: `manifest.json`, `service_worker.js` (module background), `README.md`.

### `firmware/`
`README.md` only at top level. Documents the retired physical Pico HID path and states that current Synapse action uses the software backend. No `firmware/pico-hid/` workspace, release firmware script, or UF2 artifact path is present on current `main`.

### `scripts/` — operational PowerShell / shell / Python
```
synapse-setup.ps1                    # Windows setup: build/install daemon, deploy profiles, gen token, register auto-start HTTP daemon, wire MCP clients (idempotent)
synapse-install.sh                   # WSL-side installer entry (controlling body is the Windows synapse-mcp.exe HTTP daemon)
install-synapse-chrome-debugger.ps1  # install/register Chrome native host + debugger extension; self-heal removal of blockers
add-defender-exclusions.ps1          # add Defender real-time-scan exclusions for the Rust build tree (major build-speed win)
repo-maintenance.ps1                 # prune merged/stale git worktrees + cargo-sweep stale build artifacts (disk-buildup control)
install-maintenance-task.ps1         # register/remove a Windows Scheduled Task running repo-maintenance.ps1
run-daemon-with-secrets.ps1          # start shared daemon with cloud model API keys injected from Infisical (no secret on disk)
run-issue-daemon.ps1                 # start an issue-diagnostic daemon from a copied dev synapse-mcp.exe so target\debug stays relinkable
start-local-model-endpoint.ps1       # serve a local model as an OpenAI-compatible endpoint (vLLM)
check_doc_paths.ps1                  # validate documentation file paths
local-model-openai-chat.py           # serve a cached HF chat model as OpenAI chat-completions (operational helper)
manual_mcp_stdio_probe.py            # manual MCP-over-stdio probe (launches synapse-mcp as an MCP client would)
swarm.py                             # run a local-model agent swarm against a live daemon (operational probe)
```

### `tests/` (workspace-level)
Only `tests/fixtures/` (e.g. `fixtures/audio/`) — shared test fixture assets consumed by crate integration tests. No workspace-level `.rs` integration suites; integration tests live per-crate under `crates/*/tests/`.

---

See also: [01_system_overview.md](01_system_overview.md) for subsystem responsibilities and data flow.
