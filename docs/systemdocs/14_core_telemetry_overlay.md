# 14. Core, Telemetry, and Overlay

**Source files covered:**

- `crates/synapse-core/src/lib.rs`
- `crates/synapse-core/src/defaults.rs`
- `crates/synapse-core/src/error_codes.rs`
- `crates/synapse-core/src/filter.rs`
- `crates/synapse-core/src/intent.rs`
- `crates/synapse-core/src/retention.rs`
- `crates/synapse-core/src/routines.rs`
- `crates/synapse-core/src/episodes.rs`
- `crates/synapse-core/src/types.rs` and submodules under `crates/synapse-core/src/types/` (action, agent_cost, agent_event, agent_transcript, episode, event, geometry, health, observation, profile, reality, reflex, routine, stored, timeline, web_perception)
- `crates/synapse-core/Cargo.toml`
- `crates/synapse-telemetry/src/lib.rs`
- `crates/synapse-telemetry/src/metrics.rs`
- `crates/synapse-telemetry/Cargo.toml`
- `crates/synapse-overlay/src/main.rs`
- `crates/synapse-overlay/Cargo.toml`

These three crates are foundational: `synapse-core` is the shared vocabulary (types, constants, error codes) plus pure derived-data engines; `synapse-telemetry` owns process logging and the metric registry; `synapse-overlay` is the Windows tray companion binary.

---

## 1. synapse-core — shared vocabulary and derived-data engines

`synapse-core` is a dependency-light crate (`chrono`, `regex`, `schemars`, `serde`, `serde_json`, `sha2`, `thiserror`, `tracing`, `uuid`). It defines: shared types and enums (`types.rs`), tuning/retention defaults (`defaults.rs`, `retention.rs`), the full daemon error-code catalog (`error_codes.rs`), the event-filter matcher (`filter.rs`), and three pure deterministic engines — episode segmentation (`episodes.rs`), routine mining (`routines.rs`), and live intent matching (`intent.rs`).

`lib.rs` re-exports `SCHEMA_VERSION`, `DEFAULT_AIM_TRACK_EMA_ALPHA`, and the entire `types` surface (every struct/enum/constant/factory listed in sections 1.2 and 1.3).

### 1.1 Constants — `defaults.rs`

| Constant | Type | Value | Meaning |
| --- | --- | --- | --- |
| `SCHEMA_VERSION` | `u32` | `1` | Storage schema version. Pre-v1 migrations may bump freely. See [04_storage_and_persistence.md](04_storage_and_persistence.md). |
| `REFERENCE_OBSERVE_WARM_HYBRID_P99_MS` | `f32` | `30.0` | Reference-machine warm hybrid `observe` p99 budget (ms). |
| `REFERENCE_REFLEX_TICK_JITTER_IDLE_P99_US` | `u32` | `200` | Reference-machine idle reflex tick jitter p99 budget (µs). |
| `REFERENCE_EVENT_TO_SUBSCRIBER_P99_MS` | `f32` | `50.0` | Reference-machine event-to-subscriber p99 budget (ms). |
| `DEFAULT_AIM_TRACK_EMA_ALPHA` | `f32` | `0.7` | Default EMA smoothing alpha for `aim_track` reflex target deltas. |

#### Other module-level constants (defined in `types/` submodules)

| Constant | Type | Value | Source file | Meaning |
| --- | --- | --- | --- | --- |
| `TIMELINE_RECORD_VERSION` | `u32` | `1` | `types/timeline.rs` | Envelope version for `TimelineRecord`. |
| `EPISODE_RECORD_VERSION` | `u32` | `1` | `types/episode.rs` | Envelope version for `EpisodeRecord`. |
| `ROUTINE_RECORD_VERSION` | `u32` | `1` | `types/routine.rs` | Envelope version for `RoutineRecord`. |
| `ROUTINE_STATE_RECORD_VERSION` | `u32` | `2` | `types/routine.rs` | Envelope version for `RoutineStateRecord` (v2 added #856 feedback fields). |
| `ROUTINE_STATE_MAX_FEEDBACK_EVENTS` | `usize` | `200` | `types/routine.rs` | Cap on `feedback_events`. |
| `ROUTINE_STATE_MAX_TRANSITIONS` | `usize` | `64` | `types/routine.rs` | Cap on lifecycle `transitions`. |
| `ROUTINE_STATE_MAX_CONFIDENCE_POINTS` | `usize` | `180` | `types/routine.rs` | Cap on `confidence_history`. |
| `EVENT_FILTER_MAX_DEPTH` | `u32` | `8` | `types/event.rs` | Max nesting depth for an `EventFilter` tree. |
| `AGENT_EVENT_RECORD_VERSION` | `u32` | `1` | `types/agent_event.rs` | Envelope version for `AgentEventRecord`. |
| `AGENT_EVENT_MAX_ID_CHARS` | `usize` | `512` | `types/agent_event.rs` | Max id-field length on agent events. |
| `AGENT_EVENT_MAX_REASON_CHARS` | `usize` | `128` | `types/agent_event.rs` | Max reason-field length on agent events. |
| `AGENT_TRANSCRIPT_RECORD_VERSION` | `u32` | `1` | `types/agent_transcript.rs` | Envelope version for `AgentTranscriptRecord`. |
| `AGENT_TRANSCRIPT_MAX_SUMMARY_CHARS` | `usize` | `2048` | `types/agent_transcript.rs` | Max transcript summary length. |
| `AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS` | `usize` | `8192` | `types/agent_transcript.rs` | Max tool-args length. |
| `AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS` | `usize` | `8192` | `types/agent_transcript.rs` | Max tool-result length. |
| `MODEL_PRICE_VERSION` | `u32` | `1` | `types/agent_cost.rs` | Envelope version for `ModelPrice`. |
| `MODEL_PRICE_MAX_ID_CHARS` | `usize` | `256` | `types/agent_cost.rs` | Max model-id length in pricing. |
| `PROFILE_SCHEMA_VERSION` | `u32` | `2` | `types/profile.rs` | Profile document schema version. |
| `DEFAULT_HUD_CONFIDENCE_THRESHOLD` | `f32` | `0.85` | `types/profile.rs` | Default HUD-reading confidence threshold (factory `default_hud_confidence_threshold`). |
| `PERCEIVED_TEXT_UNTRUSTED_NOTICE` | `&str` | (notice string) | `types/observation.rs` | Untrusted-text safety notice attached to perceived text. |
| `MINUTES_PER_DAY` | `u32` | `1_440` | `routines.rs` | Minutes in the time-of-day circle (also used by `intent.rs`). |
| `MAX_EVIDENCE_OCCURRENCES` | `usize` | `8` | `routines.rs` | Evidence occurrences persisted per routine. |

### 1.2 Shared types — `types.rs` and submodules

All types below are re-exported flat from `synapse_core::types` (and most from `synapse_core` root). Identifier aliases: `SessionId`, `EntityId`, `ReflexId`, `SubscriptionId`, `ProfileId` are all `String`. Factory functions: `new_session_id()`, `new_reflex_id()`, `new_subscription_id()` (UUID v7), `element_id(hwnd, runtime_id_hex)`, `entity_id(track)`.

#### Event bus (`types/event.rs`, `filter.rs`)

- **`Event`** — `seq: u64`, `at: DateTime<Utc>`, `source: EventSource`, `kind: String`, `data: serde_json::Value`, `correlations: Vec<EventRef>`. `summary()` produces an `EventSummary`.
- **`EventSource`** (enum, snake_case): `A11yUia`, `A11yWinEvent`, `A11yCdp`, `Perception`, `PerceptionDetection`, `PerceptionHud`, `PerceptionAudio`, `Filesystem`, `Process`, `Clipboard`, `ActionEmitter`, `Reflex`, `System`.
- **`EventRef`** — `seq: u64`, `relation: String`.
- **`EventSummary`** — `seq`, `at`, `source`, `kind`, `data_excerpt`.
- **`EventFilter`** (tagged by `op`): `All`, `None`, `Kind{kind}`, `Source{source}`, `And{args}`, `Or{args}`, `Not{arg}`, `Data{path, predicate}`. Methods: `matches`, `is_trivially_always_true`, `depth`, `validate` (rejects empty `And`/`Or`, depth > `EVENT_FILTER_MAX_DEPTH`, bad JSON-pointer paths, invalid regex).
- **`DataPredicate`** (tagged by `op`, snake_case): `Eq`, `Ne`, `Lt`, `Le`, `Gt`, `Ge`, `Regex{pattern}`, `InSet{values}`, `Exists`.
- **`EventFilterValidationError`** — `EmptyAnd`, `EmptyOr`, `DepthExceeded`, `InvalidDataPath`, `InvalidRegex`.
- `filter.rs` provides the pure matcher functions `matches_event_filter` and `matches_data_predicate`. Numeric/string comparison only; cross-type comparisons return no ordering.

#### Geometry and identifiers (`types/geometry.rs`)

- **`Point`** `{x:i32, y:i32}` (`distance_to`); **`PathPoint`** `{x:f64, y:f64}` (`new`, `distance_to`, `is_finite`, `lerp`); **`Rect`** `{x,y,w,h:i32}` (`contains`, right/bottom edges exclusive); **`Size`** `{w,h:u32}`.
- **`PathSpec`** (tagged `kind`): `Line`, `Arc`, `Circle`, `CubicBezier`, `Polyline`, `CatmullRom{waypoints, alpha=0.5, tension, closed}`.
- **`Backend`** (lowercase): `Software`, `Vigem`, `Hardware`, `Auto`.
- **`PerceptionMode`** (snake_case): `A11yOnly`, `PixelOnly`, `Hybrid`, `Auto`.
- **`ElementId`** — validated wrapper over `<hwnd_hex>:<runtime_id_hex>` (regex `^-?0x[0-9a-fA-F]+:[0-9a-fA-F]+$`). `ElementIdParts` `{hwnd:i64, runtime_id_hex:String}`. `ElementIdParseError` — `MissingSeparator`, `InvalidHwnd`, `InvalidRuntimeId`.

#### Timeline / episodes / routines (data models — see sections 1.4–1.6 for engines)

- **`TimelineRecord`** (`types/timeline.rs`) — `record_version`, `ts_ns` (required for TTL compaction), `kind: TimelineKind`, `actor: TimelineActor`, `app: Option<String>`, `payload: Value`. Built with `TimelineRecord::new`.
- **`TimelineKind`** — `FocusChange`, `TitleChange`, `IdleStart`, `IdleEnd`, `SessionStart`, `SessionEnd`, `InteractionSummary`, `Clipboard`, `FileActivity`, `BrowserNav`, `DemoMarker`, `Purge`. (Raw keystroke content is deliberately unrepresentable — interaction rows carry counts/cadence only.)
- **`TimelineActor`** (tagged `actor`): `Human`, `Agent{session_id}`.
- **`EpisodeRecord`** / **`EpisodeBoundary`** (`types/episode.rs`) — documented in 1.4.
- **`RoutineRecord`**, **`RoutineStep`**, **`RoutineEvidence`**, **`RoutineGranularity`** (`App`/`AppDocument`), **`RoutineDowClass`** (`Daily`/`Weekdays`/`Weekend`/`Days{days}`), **`RoutineLifecycle`** (`Candidate`/`Confirmed`/`Disabled`/`Archived`), **`RoutineStateAction`** (`Discovered`/`Confirm`/`Disable`/`Enable`/`Archive`/`Rename`), **`RoutineTransition`**, **`RoutineConfidencePoint`**, **`RoutineFeedbackOutcome`** (`Accepted`/`Declined`/`IgnoredTimeout`/`Abandoned`), **`RoutineFeedbackEvent`**, **`RoutineStateRecord`** (`types/routine.rs`).

#### Actions (`types/action.rs`)

`Action` enum is the action vocabulary; supporting humanization/input types: `AimCurve`, `VelocityProfile`, `StrokeTiming`, `StrokeMotionModel`, `HumanizeParams`, `AimNaturalParams`, `AimStyle`, `KeystrokeDynamics`, `KeystrokeNaturalParams`, `Key`, `KeyCode`, `MouseButton`, `ButtonAction`, `MouseTarget`, `AimTarget`, `GamepadController`, `PadButton`, `PadId`, `Stick`, `Trigger`, `GamepadReport`, `ComboStep`, `ComboInput`.

#### Reflexes (`types/reflex.rs`)

`ReflexRegistration`, `ReflexKind`, `ReflexAimAxis`, `ReflexButtonTarget`, `ReflexThen`, `ReflexLifetime`, `ReflexState`, `ReflexStatus`.

#### Observation / perception (`types/observation.rs`)

`Observation`, `ForegroundContext`, `FocusedElement`, `UiaPattern`, `AccessibleNode`, `AccessibleSubtree`, `AccessibleQuery`, `AccessibleQueryScope`, `DetectedEntity`, `Detection`, `DetectionBatch`, `HudReadings`, `HudFieldError`, `HudReading`, `HudValue`, `AudioContext`, `AudioEvent`, `AudioCue`, `DirectionEstimate`, `ClipboardSummary`, `FsEvent`, `FsEventKind`, `ObservationDiagnostics`, `SuspectedInjectionAnnotation`, `SuspectedInjectionSpan`, `InputBackendDiagnostics`, `InputBackendCapability`, `ObservationElementsPage`, `ObservationCaptureConfig`, `ObservationCaptureTarget`, `CaptureRuntimeReadback`, `SensorStatus`, `OcrBackend`, `OcrResult`, `OcrWord`.

#### Profiles (`types/profile.rs`)

`Profile`, `ProfileMatch`, `ProfileUseScope`, `ProfileCapture`, `ProfileCaptureTarget`, `ProfileDetection`, `ProfileOcr`, `HudFieldSpec`, `HudRegion`, `WindowEdge`, `HudExtractor`, `HudParser`, `ProfileBackends`, `EventExtension`.

#### Web perception (`types/web_perception.rs`)

`WebPerceptionPath`, `CdpStatus`, `CdpCapability`, `CdpDiagnostics`.

#### Reality / drift audit (`types/reality.rs`)

`SourceRef`, `RealitySourceSurface`, `RedactionSummary`, `RedactionPolicy`, `ForbiddenRawDataKind`, `RealityBaseline`, `RealityDelta`, `RealityTargetRef`, `RealityTargetKind`, `RealityDeltaConflict`, `RealityAudit`, `RealityBaselineStatus`, `RealityDriftItem`, `RealityDriftStatus`, `RealityDeltaValidationError`.

#### Agent fleet (`types/agent_event.rs`, `agent_transcript.rs`, `agent_cost.rs`)

- Events: `AgentEventRecord`, `AgentEventKind`, `AgentEndState`, `GenAiOperationName`, `GenAiAttributes`.
- Transcripts: `AgentTranscriptRecord`, `TranscriptSource`, `TranscriptParseStatus`, `TranscriptRole`, `TranscriptToolCall`, `TranscriptUsage`, `TranscriptModelUsage`.
- Cost: `ModelPrice`, `BillableUsage`, `CostBreakdown`, `CostOutcome`.

#### Stored / persistence envelopes (`types/stored.rs`)

`StoredRedaction`, `StoredBackendPolicy`, `StoredAppContext`, `StoredAuditContext`, `StoredEvent`, `StoredObservation`, `StoredReflexStep`, `StoredReflexAudit`, `StoredProfileHistoryEntry`, `StoredSession`.

#### Health (`types/health.rs`)

- **`Health`** — `ok`, `version`, `build`, `pid:u32`, `uptime_s:u64`, `tool_count`, `tool_surface_sha256`, `tool_names: Vec<String>`, `subsystems: BTreeMap<String, SubsystemHealth>`.
- **`SubsystemHealth`** — `status`, `detail`, plus a large set of optional per-subsystem fields (profile/capture/reflex/storage/HTTP/audio/shell-policy diagnostics).

### 1.3 Error-code catalog — `error_codes.rs`

All entries are `pub const … : &str` whose string value equals the constant name. Grouped by the PRD §8 subsection from which they derive.

#### Perception (§8.1)

| Code | Meaning |
| --- | --- |
| `OBSERVE_NO_PERCEPTION_AVAILABLE` | No perception backend available for an observe. |
| `OBSERVE_INTERNAL` | Internal observe failure. |
| `CAPTURE_GRAPHICS_API_UNSUPPORTED` | Capture graphics API unsupported. |
| `CAPTURE_PRINTWINDOW_DISABLED` | PrintWindow capture path disabled. |
| `CAPTURE_PRINTWINDOW_BLACK` | PrintWindow returned a black frame. |
| `CAPTURE_TARGET_LOST` | Capture target window lost. |
| `CAPTURE_NO_DIRTY_REGIONS` | No dirty regions to capture. |
| `A11Y_NOT_AVAILABLE` | Accessibility backend unavailable. |
| `A11Y_ELEMENT_STALE` | Cached a11y element is stale. |
| `A11Y_NO_FOREGROUND` | No foreground window for a11y. |
| `A11Y_CDP_UNREACHABLE` | CDP endpoint unreachable. |
| `A11Y_CDP_ATTACH_FAILED` | CDP attach failed. |
| `A11Y_CDP_AXTREE_FAILED` | CDP accessibility-tree fetch failed. |
| `A11Y_CDP_EXTENSION_UNAVAILABLE` | Chrome debugger extension unavailable. |
| `A11Y_CDP_EXTENSION_DETACHED` | Chrome debugger extension detached. |
| `A11Y_CDP_EXTENSION_TIMEOUT` | Chrome debugger extension timed out. |
| `A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED` | CDP debugger warning banner not suppressed. |
| `CHROME_BRIDGE_EXTENSION_STALE` | Chrome bridge extension stale. |
| `CHROME_SCRIPTING_EXECUTE_FAILED` | Chrome scripting execution failed. |
| `CHROME_DOM_SELECTOR_INVALID` | Invalid DOM selector. |
| `CHROME_DOM_ELEMENT_NOT_FOUND` | DOM element not found. |
| `CHROME_DOM_ELEMENT_AMBIGUOUS` | DOM selector matched multiple elements. |
| `CHROME_DOM_ELEMENT_NOT_ACTIONABLE` | DOM element not actionable. |
| `CHROME_DOM_ACTION_UNSUPPORTED` | DOM action unsupported. |
| `CHROME_DOM_ACTION_POSTCONDITION_FAILED` | DOM action postcondition failed. |
| `BROWSER_WAIT_TIMEOUT` | Browser wait condition timed out. |
| `A11Y_UIA_WORKER_TIMEOUT` | UIA worker timed out. |
| `A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE` | Minimized target window: UIA unavailable. |
| `DETECTION_MODEL_NOT_LOADED` | Detection model not loaded. |
| `DETECTION_MODEL_INFER_FAILED` | Detection inference failed. |
| `DETECTION_NO_FRAME` | No frame available for detection. |
| `OCR_NO_TEXT` | OCR found no text. |
| `OCR_BACKEND_UNAVAILABLE` | OCR backend unavailable. |
| `TARGET_WINDOW_NOT_FOUND` | Per-agent target window not found. |
| `TARGET_NOT_SET` | No active per-agent target set. |
| `TARGET_CDP_UNRESOLVED` | Target CDP endpoint unresolved. |
| `TARGET_CO_OWNED` | Target is co-owned by another session. |
| `TARGET_CLAIM_NOT_FOUND` | Target claim not found. |
| `TARGET_CLAIM_ADOPT_REFUSED` | Target claim adoption refused. |
| `TARGET_CLAIM_OWNER_ACTIVE` | Target claim owner still active. |
| `HUD_NO_ACTIVE_PROFILE` | No active profile for HUD extraction. |
| `HUD_FIELD_NOT_DEFINED` | HUD field not defined in profile. |
| `HUD_EXTRACTION_FAILED` | HUD extraction failed. |
| `AUDIO_DEVICE_LOST` | Audio device lost. |
| `AUDIO_LOOPBACK_INIT_FAILED` | Audio loopback init failed. |
| `AUDIO_STT_MODEL_NOT_LOADED` | STT model not loaded. |

#### Action (§8.2)

| Code | Meaning |
| --- | --- |
| `ACTION_QUEUE_FULL` | Action queue full. |
| `ACTION_RATE_LIMITED` | Action rate-limited. |
| `ACTION_BACKEND_UNAVAILABLE` | Action backend unavailable. |
| `ACTION_TARGET_INVALID` | Action target invalid. |
| `ACTION_HOLD_EXCEEDED_MAX` | Hold exceeded maximum. |
| `ACTION_VIGEM_NOT_INSTALLED` | ViGEm not installed. |
| `ACTION_VIGEM_PLUGIN_FAILED` | ViGEm plugin failed. |
| `ACTION_ELEMENT_NOT_RESOLVED` | Action element not resolved. |
| `ACTION_ELEMENT_PATTERN_UNSUPPORTED` | Element UIA pattern unsupported. |
| `TRANSIENT_ELEMENT_EXPIRED` | Transient element handle expired. |
| `ACTION_FOREGROUND_LOST` | Foreground lost mid-action. |
| `ACTION_NO_OBSERVED_DELTA` | No observed delta after action. |
| `ACTION_VERIFY_SURFACE_UNAVAILABLE` | Verification surface unavailable. |
| `ACTION_POSTCONDITION_FAILED` | Action postcondition failed. |
| `ACTION_LAUNCH_WINDOW_NOT_FOUND` | Launched app window not found. |
| `ACTION_LAUNCH_FOREGROUND_FAILED` | Launched app failed to foreground. |
| `ACTION_LAUNCH_URL_NOT_REACHED` | Launch target URL not reached. |
| `ACTION_AGENT_SPAWN_FAILED` | Agent spawn failed. |
| `ACTION_AGENT_SPAWN_SESSION_TIMEOUT` | Agent spawn session timed out. |
| `ACTION_AGENT_SPAWN_TASK_NOT_STARTED` | Agent spawn task did not start. |
| `ACTION_BUDGET_EXPIRED` | Action budget expired. |
| `ACTION_WINDOW_NOT_FOUND` | Action window not found. |
| `ACTION_WINDOW_AMBIGUOUS` | Action window match ambiguous. |
| `ACTION_FOCUS_WINDOW_FAILED` | Window focus failed. |
| `ACTION_UNSUPPORTED_KEY` | Unsupported key. |
| `ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT` | Drag distance exceeds limit. |
| `STUCK_KEY_AUTO_RELEASED` | Stuck key auto-released. |
| `SAFETY_RELEASE_ALL_FIRED` | Safety release-all fired. |
| `SAFETY_OPERATOR_HOTKEY_FIRED` | Operator safety hotkey fired. |
| `ACTION_FOREGROUND_LEASE_BUSY` | Foreground input lease held by another session. |
| `ACTION_FOREGROUND_LEASE_NOT_HELD` | Foreground lease not held by caller. |
| `FOREGROUND_ACTIVATION_REFUSED` | Foreground activation refused. |
| `ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED` | Foreground context capture failed. |
| `ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED` | Foreground context restore failed. |
| `ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED` | Foreground context restore skipped. |
| `FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED` | Restore skipped because human moved focus. |
| `ACTION_ELEMENT_VALUE_READ_ONLY` | Element value is read-only. |
| `ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED` | Remote process cleanup unverified. |

#### Reflex (§8.3)

| Code | Meaning |
| --- | --- |
| `REFLEX_CAP_REACHED` | Reflex registration cap reached. |
| `REFLEX_KIND_INVALID` | Invalid reflex kind. |
| `REFLEX_PARAMS_INVALID` | Invalid reflex params. |
| `REFLEX_TARGET_INVALID` | Invalid reflex target. |
| `REFLEX_FILTER_INVALID` | Invalid reflex event filter. |
| `REFLEX_PRIORITY_INVALID` | Invalid reflex priority. |
| `REFLEX_TICK_LATE` | Reflex tick late. |
| `REFLEX_TRACK_LOST` | Reflex tracked target lost. |
| `REFLEX_STARVED` | Reflex starved by conflict resolution. |
| `REFLEX_DISABLED_BY_OPERATOR` | Reflex disabled by operator. |
| `REFLEX_LIFETIME_EXPIRED` | Reflex lifetime expired. |
| `REFLEX_RECURSION_LIMIT` | Reflex recursion limit hit. |
| `REFLEX_ACTION_PERMISSION_DENIED` | Reflex action permission denied. |
| `REFLEX_DEBOUNCED` | Reflex debounced. |

#### Profile and config (§8.4)

| Code | Meaning |
| --- | --- |
| `PROFILE_NOT_FOUND` | Profile not found. |
| `PROFILE_PARSE_ERROR` | Profile parse error. |
| `PROFILE_VERSION_INCOMPATIBLE` | Profile schema version incompatible. |
| `PROFILE_KEYMAP_INVALID` | Profile keymap invalid. |
| `PROFILE_HUD_REGION_INVALID` | Profile HUD region invalid. |
| `PROFILE_TRUST_VERIFICATION_FAILED` | Profile trust verification failed. |
| `PROFILE_ROLLBACK_UNAVAILABLE` | Profile rollback unavailable. |
| `AUDIT_EXPORT_CONSENT_REQUIRED` | Audit export consent required. |
| `AUDIT_EXPORT_REDACTION_REQUIRED` | Audit export redaction required. |
| `AUDIT_EXPORT_PAYLOAD_TOO_LARGE` | Audit export payload too large. |
| `PROFILE_AUTHORING_INSUFFICIENT_EVIDENCE` | Profile authoring: insufficient evidence. |
| `PROFILE_AUTHORING_CONFLICTING_EVIDENCE` | Profile authoring: conflicting evidence. |
| `PROFILE_AUTHORING_UNSAFE_ESCALATION` | Profile authoring: unsafe escalation. |
| `PROFILE_AUTHORING_CANDIDATE_NOT_FOUND` | Profile authoring candidate not found. |
| `PROFILE_AUTHORING_INVALID_STATE` | Profile authoring invalid state. |
| `CAPTURE_TARGET_INVALID` | Capture target invalid. |
| `PERCEPTION_MODE_INVALID` | Perception mode invalid. |

#### MCP and session (§8.5)

| Code | Meaning |
| --- | --- |
| `SESSION_NOT_FOUND` | Session not found. |
| `SESSION_EXPIRED` | Session expired. |
| `RECIPIENT_UNKNOWN` | Message recipient unknown. |
| `SUBSCRIPTION_NOT_FOUND` | Subscription not found. |
| `SUBSCRIPTION_CAP_REACHED` | Subscription cap reached. |
| `TOOL_NOT_FOUND` | Tool not found. |
| `TOOL_PROFILE_POLICY_DENIED` | Tool denied by profile policy. |
| `TOOL_PARAMS_INVALID` | Tool params invalid. |
| `TOOL_INTERNAL_ERROR` | Tool internal error. |
| `HTTP_BIND_NON_LOOPBACK_REFUSED` | Non-loopback HTTP bind refused. |
| `HTTP_TOKEN_INVALID` | HTTP bearer token invalid. |
| `HTTP_ORIGIN_REFUSED` | HTTP origin refused. |
| `HTTP_SESSION_INVALID` | HTTP session invalid. |
| `DAEMON_RESTARTING` | Daemon restarting. |
| `REPLAY_TARGET_INVALID` | Replay target invalid. |
| `REPLAY_FORMAT_INVALID` | Replay format invalid. |

#### Storage (§8.6) — see [04_storage_and_persistence.md](04_storage_and_persistence.md)

| Code | Meaning |
| --- | --- |
| `STORAGE_OPEN_FAILED` | Storage open failed. |
| `STORAGE_WRITE_FAILED` | Storage write failed. |
| `STORAGE_READ_FAILED` | Storage read failed. |
| `STORAGE_CORRUPTED` | Storage corrupted. |
| `STORAGE_SCHEMA_MISMATCH` | Storage schema mismatch. |
| `STORAGE_DISK_PRESSURE_LEVEL_1` … `_4` | Disk pressure escalation levels 1–4. |
| `STORAGE_CF_HARD_CAP_REACHED` | Column-family hard cap reached. |

#### Episodes / templates / tasks

| Code | Meaning |
| --- | --- |
| `EPISODE_NOT_FOUND` | Episode not found (#846/#847). |
| `AGENT_TEMPLATE_NOT_FOUND` | Agent spawn template not found (#909). |
| `AGENT_TASK_NOT_FOUND` | Agent task not found (#910). |
| `AGENT_TASK_INVALID_TRANSITION` | Invalid agent-task state transition. |

#### Models (§8.7)

| Code | Meaning |
| --- | --- |
| `MODEL_DOWNLOAD_FAILED` | Model download failed. |
| `MODEL_HASH_MISMATCH` | Model hash mismatch. |
| `MODEL_LOAD_FAILED` | Model load failed. |
| `MODEL_BACKEND_UNAVAILABLE` | Model backend unavailable. |
| `MODEL_TOOLS_UNSUPPORTED` | Model does not support tools. |
| `MODEL_EMPTY_COMPLETION` | Degenerate completion: no tool call and no content. |
| `MODEL_ENDPOINT_UNREACHABLE` | Model endpoint unreachable. |
| `MODEL_REGISTRY_NOT_FOUND` | Model registry entry not found. |
| `MODEL_REGISTRY_CONFLICT` | Model registry conflict. |
| `MODEL_REGISTRY_DISABLED` | Model registry entry disabled. |
| `MODEL_REGISTRY_UNPROBED` | Model registry entry unprobed. |
| `MODEL_REGISTRY_PROBE_STALE` | Model registry probe stale. |
| `MODEL_API_KEY_MISSING` | Model API key missing. |
| `MODEL_API_KEY_DECRYPT_FAILED` | Stored API-key secret could not be decrypted (DPAPI). |
| `MODEL_API_KEY_STORE_FAILED` | Encrypting/persisting API-key secret failed. |

#### Human notifications (#866)

| Code | Meaning |
| --- | --- |
| `NOTIFY_UNSUPPORTED_PLATFORM` | Notification platform unsupported. |
| `NOTIFY_AUMID_REGISTRATION_FAILED` | AUMID registration failed. |
| `NOTIFY_DISABLED_FOR_APPLICATION` | Notifications disabled for the application. |
| `NOTIFY_DISABLED_FOR_USER` | Notifications disabled for the user. |
| `NOTIFY_DISABLED_BY_GROUP_POLICY` | Notifications disabled by group policy. |
| `NOTIFY_DISABLED_BY_MANIFEST` | Notifications disabled by manifest. |
| `NOTIFY_XML_PAYLOAD_INVALID` | Notification XML payload invalid. |
| `NOTIFY_SHOW_FAILED` | Notification show failed. |
| `NOTIFY_DELIVERY_UNVERIFIED` | Notification delivery unverified. |
| `NOTIFY_WORKER_FAILED` | Notification worker failed. |

#### Safety (§8.9)

| Code | Meaning |
| --- | --- |
| `SAFETY_KILLSWITCH_ACTIVE` | Kill switch active. |
| `SAFETY_PROCESS_DENYLISTED` | Process is denylisted. |
| `SAFETY_SHELL_DENIED_BY_POLICY` | Shell command denied by policy. |
| `SAFETY_SHELL_GLOBAL_INPUT_DENIED` | Shell command requested global OS input (SendKeys/SendInput/etc.) — bypasses the input lease. |
| `SAFETY_LAUNCH_DENIED_BY_POLICY` | App launch denied by policy. |
| `SAFETY_SECRET_REDACTED` | Secret redacted. |
| `SAFETY_PERMISSION_DENIED` | Permission denied. |
| `SAFETY_PROFILE_ACTION_DENIED` | Profile action denied. |

### 1.4 Episode segmentation — `episodes.rs`

Pure, deterministic chunking of `CF_TIMELINE` rows into `EpisodeRecord` spans of focused work. Entry point: `segment_range(rows, range_start_ns, range_end_ns, end_is_day_boundary, &SegmentationConfig) -> Result<Segmentation, SegmentationError>`. Stable id factory `episode_id(start_ts_ns, actor, app, document)` → `ep1-` + first 16 hex of SHA-256 over the identity tuple (re-segmentation reproduces ids).

- **`SegmentationConfig`** defaults: `min_focus_ns = 5_000_000_000` (5 s), `silent_gap_ns = 600_000_000_000` (10 min), `include_agent_activity = false`, `browser_apps = [chrome, msedge, firefox, brave, opera, vivaldi, arc]`.
- Boundary heuristics: app switch / document switch split; `IdleStart`/`IdleEnd` close/reopen; `SessionStart`/`SessionEnd` always split; silent gap closes at last evidence; range/day edge closes the tail. Rapid alt-tab flicker (foreign focus shorter than `min_focus_ns` between two same-identity spans) is absorbed as an interruption.
- **`Segmentation`** output: `episodes`, plus loud counters `considered_rows`, `ignored_agent_rows`, `payload_anomalies`.
- **`SegmentationError`**: `InvalidRange`, `RowOutOfRange`, `RowsNotChronological`, `InvalidConfig`.

### 1.5 Routine mining — `routines.rs`

Pure, deterministic mining of recurring routines from the episode stream (periodic frequent-pattern mining with circular-statistics time clustering). Entry point: `mine_routines(&[MiningDay], mined_at_ts_ns, &RoutineMiningConfig) -> Result<RoutineMining, RoutineMiningError>`. Stable id factory `routine_id(granularity, steps, dow_class, cluster_ordinal)` → `rt1-` + 16 hex of SHA-256 (excludes the mining timestamp, so re-mining reproduces ids). Confidence is the Wilson 95% lower bound (`wilson_lower_bound`, `WILSON_Z_95 = 1.959963984540054`).

- **`RoutineMiningConfig`** defaults: `min_episode_duration_ns = 60 s`, `collapse_gap_ns = 15 min`, `max_step_gap_ns = 30 min`, `max_pattern_len = 6`, `min_support_days = 3`, `cluster_split_gap_minutes = 120`, `max_cluster_spread_minutes = 180`, `min_tolerance_minutes = 5`, `max_candidates = 50_000`, `max_routines = 256`, `max_occurrences_per_day = 32`, `min_confidence = 0.15`, `include_agent_activity = false`.
- **`MiningDay`** — caller-grouped local day: `day_start_ns`, `day_end_ns`, `weekday (0=Mon..6=Sun)`, `episodes`.
- **`RoutineMining`** output: `routines` plus a full set of loud accounting counters (`considered_episodes`, `eligible_episodes`, `filtered_agent_episodes`, `filtered_short_episodes`, `filtered_no_app_episodes`, `candidates_evaluated`, `candidates_truncated`, `occurrences_skipped_over_cap`, `clusters_rejected_low_support`, `clusters_rejected_dispersed`, `clusters_rejected_low_confidence`, `candidates_rejected_as_subpattern`, `routines_dropped_over_cap`, `active_days`).
- **`RoutineMiningError`**: `InvalidConfig`, `InvalidDay`, `InvalidWeekday`, `DaysNotChronological`, `EpisodeOutsideDay`, `EpisodesNotChronological`.

### 1.6 Live intent matching — `intent.rs`

Pure, deterministic prefix-matcher: given recent episodes and the mined routine library, decide which routines the operator appears to be executing right now. Entry point: `match_intents(&[EpisodeRecord], &[RoutineForMatch], NowContext, &IntentMatchConfig) -> Result<Vec<IntentCandidate>, IntentMatchError>`. Mirrors the miner's eligibility/collapse rules so freshly-performed routines match the templates they produced.

- **`IntentMatchConfig`** defaults: `min_episode_duration_ns = 60 s`, `collapse_gap_ns = 15 min`, `freshness_ns = 30 min`, `schedule_decay_minutes = 180`, `off_dow_factor = 0.3`, `min_combined_confidence = 0.0`, `max_candidates = 10`, `include_agent_activity = false`.
- **`NowContext`** — `ts_ns`, `weekday`, `minute_of_day` (caller owns calendar math; engine is clock/locale-free).
- **`RoutineForMatch`** — `record`, `lifecycle`, `label`. Only `Candidate`/`Confirmed` routines match; `Disabled`/`Archived` never match.
- **`IntentCandidate`** — `routine_id`, `label`, `schedule_label`, `lifecycle`, `granularity`, combined `confidence` (= `routine_confidence × prefix_factor × schedule_factor`), `routine_confidence`, `prefix_factor`, `schedule_factor`, `matched_prefix_len`, `total_steps`, `matched_steps: Vec<MatchedStep>`, `remaining_steps: Vec<RoutineStep>`, `last_matched_end_ts_ns`, `schedule: ScheduleContext`.
- **`MatchedStep`**, **`ScheduleContext`** carry the per-step and schedule-alignment evidence.
- **`IntentMatchError`**: `InvalidConfig`, `InvalidWeekday`, `InvalidMinute`.

### 1.7 Storage retention defaults — `retention.rs`

`DEFAULTS: [RetentionDefault; 17]` (PRD §4/§6). Each entry is `{cf, ttl: RetentionTtl, soft_cap_mb, hard_cap_mb}`. `RetentionTtl` = `None | Hours(u64) | Days(u64) | LruOnly`. See [04_storage_and_persistence.md](04_storage_and_persistence.md).

| CF | TTL | Soft cap (MB) | Hard cap (MB) |
| --- | --- | --- | --- |
| `CF_EVENTS` | Hours(24) | 2048 | 4096 |
| `CF_OBSERVATIONS` | Hours(6) | 500 | 1000 |
| `CF_PROFILES` | None | 20 | 50 |
| `CF_MODEL_CACHE` | LruOnly | 1024 | 2048 |
| `CF_SESSIONS` | Days(30) | 50 | 100 |
| `CF_REFLEX_AUDIT` | Days(7) | 200 | 500 |
| `CF_OCR_CACHE` | Hours(1) | 50 | 100 |
| `CF_TELEMETRY` | Hours(6) | 100 | 200 |
| `CF_ACTION_LOG` | Hours(24) | 200 | 500 |
| `CF_PROCESS_HISTORY` | Hours(6) | 20 | 50 |
| `CF_KV` | None | 10 | 50 |
| `CF_TIMELINE` | Days(90) | 4096 | 8192 |
| `CF_EPISODES` | Days(90) | 256 | 512 |
| `CF_ROUTINES` | None | 16 | 64 |
| `CF_ROUTINE_STATE` | None | 16 | 64 |
| `CF_AGENT_EVENTS` | Days(30) | 512 | 1024 |
| `CF_AGENT_TRANSCRIPTS` | Days(30) | 512 | 1024 |

---

## 2. synapse-telemetry — logging and metrics

Dependencies: `metrics`, `metrics-exporter-prometheus`, `opentelemetry`, `opentelemetry-otlp`, `tracing`, `tracing-appender`, `tracing-subscriber`, `synapse-core`.

### 2.1 Logging (`lib.rs`)

`init()` / `init_tracing(TelemetryConfig)` install the process-wide `tracing` subscriber: a JSON file layer (daily-rolling `synapse.log` via `tracing-appender`, non-blocking) plus a stderr console layer (ANSI off). Returns a `TelemetryGuard` (holds the writer guard and the background GC worker).

- **`TelemetryConfig`** — `log_dir: Option<PathBuf>`, `file_level`/`console_level: LevelFilter` (default `INFO`), `max_dir_bytes` (default `500 MiB`), `keep_days` (default `7`), `gc_interval: Option<Duration>` (default `6 h`, overridable via env `SYNAPSE_LOG_GC_INTERVAL_S`; `Some(ZERO)` disables).
- **`TelemetryError`** — `LogDirNotWritable` (`TELEMETRY_LOG_DIR_NOT_WRITABLE`), `SubscriberInit` (`TELEMETRY_SUBSCRIBER_INIT_FAILED`), `Gc` (`TELEMETRY_GC_FAILED`).
- A `payload_safe_filter` clamps the noisy `rmcp` / `rmcp::service` / `rmcp::transport` targets so request/response payloads are not logged above `info`.
- `install_panic_hook()` forwards panic payload + location to `tracing` (code `TELEMETRY_PANIC_HOOK_FIRED`) before delegating to the prior hook (idempotent).
- `default_log_dir()` → `%LOCALAPPDATA%\synapse\logs` on Windows, else `$XDG_STATE_HOME`/`$HOME/.local/state`/`./.synapse-state` + `synapse/logs`.
- A background `GcWorker` thread re-runs log-dir GC on the configured interval; GC deletes files older than `keep_days`, then trims oldest-first until under `max_dir_bytes`.

### 2.2 Metrics export and registry (`metrics.rs`)

Metrics use the `metrics` crate facade (re-exports `counter`, `gauge`, `histogram`, `describe_*`, `Unit`). `init_tracing` calls `register_m3_metrics()`, which describes each metric once (`Once`-guarded) and logs registration. **Export is via Prometheus** — the `metrics-exporter-prometheus` dependency is declared on this crate (the recorder/exporter is installed elsewhere in the daemon); `metrics-exporter-prometheus` and `opentelemetry-otlp` are both present as dependencies, but only the Prometheus exporter backs the registered metrics here.

Cardinality budget: `CARDINALITY_LIMIT = 1_000`; each `MetricSpec` carries a `max_label_combinations` that must stay below that limit (tested). `MetricKind` = `Counter | Gauge | Histogram`. The registry `M3_METRICS` holds exactly 19 specs (12 counters, 5 gauges, 2 histograms).

| Metric name | Kind | Unit | Labels | Max combos | Measures |
| --- | --- | --- | --- | --- | --- |
| `events_dropped_for_subscriber` | Counter | Count | `subscription_id` | 64 | Events dropped by a bounded per-subscriber queue. |
| `events_published_total` | Counter | Count | `source`, `kind` | 832 | Events published onto the M3 event bus. |
| `reflex_fires_total` | Counter | Count | `kind`, `reflex_id` | 64 | Reflex fire outcomes accepted by the scheduler. |
| `reflex_tick_jitter_us` | Histogram | Microseconds | — | 1 | Reflex scheduler tick jitter (µs). |
| `reflex_recursion_clamps_total` | Counter | Count | — | 1 | Times the on-event recursion guard clamped firing. |
| `reflex_starved_total` | Counter | Count | `reflex_id` | 32 | Reflexes marked starved by conflict resolution. |
| `cache_evictions_total` | Counter | Count | `cf`, `reason` | 64 | Rows evicted from caches / CF retention. |
| `storage_disk_pressure_level` | Gauge | Count | — | 1 | Current storage disk pressure level (0..4). |
| `storage_cf_bytes` | Gauge | Bytes | `cf` | 16 | Estimated live bytes per storage column family. |
| `storage_write_batch_flushes_total` | Counter | Count | `trigger` | 8 | Storage write-batch flushes by trigger. |
| `profiles_active` | Gauge | Count | `profile_id` | 128 | Active-profile marker (1 active / 0 inactive). |
| `profile_reloads_total` | Counter | Count | `profile_id`, `outcome` | 256 | Profile reload attempts by profile and outcome. |
| `audio_loopback_underruns_total` | Counter | Count | — | 1 | Audio loopback underruns while reading the ring. |
| `audio_stt_inferences_total` | Counter | Count | `outcome` | 8 | STT inference attempts by outcome. |
| `audio_stt_latency_ms` | Histogram | Milliseconds | — | 1 | STT inference latency (ms). |
| `http_requests_total` | Counter | Count | `path`, `status` | 64 | HTTP transport requests by normalized path/status. |
| `http_active_sessions` | Gauge | Count | — | 1 | Active streamable HTTP MCP sessions. |
| `sse_active_subscribers` | Gauge | Count | — | 1 | Active SSE event subscribers. |
| `sse_buffer_overflows_total` | Counter | Count | — | 1 | SSE ring-buffer overflows. |

`m3_metric_specs()` exposes the slice; each spec also carries a `label_policy` string documenting how the labels are bounded (e.g. bounded subscriber slot rather than raw UUID; closed enum/CF sets).

---

## 3. synapse-overlay — Windows tray companion

`crates/synapse-overlay/Cargo.toml` declares one binary `synapse-overlay` (`src/main.rs`). Dependencies: `anyhow`, `reqwest`, `serde`, `serde_json`, `tokio`, `tracing`, `synapse-core`, `synapse-telemetry`, and (Windows-only) the `windows` crate. Features: `default = []`, `overlay`.

### 3.1 What it is

A **Windows system-tray (notification-area) companion**, not a graphical screen overlay. Rendering is via the Win32 Shell notification-icon API (`Shell_NotifyIconW` with `NOTIFYICONDATAW`) and Win32 popup menus (`CreatePopupMenu`/`TrackPopupMenu`) — there is no GPU/canvas rendering. On non-Windows targets `main()` only prints "implemented for Windows only".

### 3.2 What it does

- Registers a window class (`SynapseTrayCompanionWindow`), creates a hidden message-only window, and adds a tray icon (`IDI_APPLICATION`).
- A background thread polls the daemon every `POLL_MS = 2000` ms and posts `WM_STATUS` to refresh the icon tooltip.
- Polling: HTTP GET `…/health`, then GET `/dashboard/tray-state.json` (falling back to `/dashboard/state.json`), with bearer auth. Base URL from `SYNAPSE_TRAY_BASE_URL` (default `http://127.0.0.1:7700`); token required from `SYNAPSE_BEARER_TOKEN`. HTTP timeout `30 s`; runs each request on a current-thread tokio runtime.
- Parses the dashboard JSON into a **`TraySnapshot`** — `recorder_paused`, `demo_armed`, `pending_approvals`, `active_sessions`, `lease_holder`, `daemon_pid`. The tooltip reads e.g. `Synapse recording; demo off; approvals N; sessions M`.
- **`DaemonState`** is `Connected(TraySnapshot)` or `Disconnected(String)`.
- Right/left click opens a popup menu showing (disabled, read-only) recorder state, demo mode, pending approvals, active sessions, and lease holder, plus actions:
  - **Open dashboard** (`ShellExecuteW` to `…/dashboard`).
  - **Pause/Resume recording** (POST `/dashboard/timeline/pause` or `/resume`).
  - **Refresh** (re-poll).
  - **Quit** (`DestroyWindow`).
- CLI flags: `--status-once` prints the current `TraySnapshot` as pretty JSON and exits; `--help`/`-h` prints usage.

### 3.3 What it displays

The tray icon tooltip and the right-click menu only — recorder paused/recording, demo armed/off, pending-approval count, active-session count, lease holder, and daemon disconnect errors. It displays no perception/observation content; it is a status surface plus a small set of daemon controls over local HTTP.
