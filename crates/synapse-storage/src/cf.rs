/// Replay event log.
pub const CF_EVENTS: &str = "CF_EVENTS";
/// Observation snapshots retained for replay and debugging.
pub const CF_OBSERVATIONS: &str = "CF_OBSERVATIONS";
/// Cached profile loads; on-disk TOML remains the source of truth.
pub const CF_PROFILES: &str = "CF_PROFILES";
/// Downloaded ONNX model cache.
pub const CF_MODEL_CACHE: &str = "CF_MODEL_CACHE";
/// MCP session continuity records.
pub const CF_SESSIONS: &str = "CF_SESSIONS";
/// Per-reflex audit trail.
pub const CF_REFLEX_AUDIT: &str = "CF_REFLEX_AUDIT";
/// OCR memoization cache for stable regions.
pub const CF_OCR_CACHE: &str = "CF_OCR_CACHE";
/// Local metric ring buffer.
pub const CF_TELEMETRY: &str = "CF_TELEMETRY";
/// Emitted action log.
pub const CF_ACTION_LOG: &str = "CF_ACTION_LOG";
/// Process start/exit history.
pub const CF_PROCESS_HISTORY: &str = "CF_PROCESS_HISTORY";
/// Generic bounded key-value extension.
pub const CF_KV: &str = "CF_KV";
/// Operator activity timeline (ADR 2026-06-11-timeline-data-model).
pub const CF_TIMELINE: &str = "CF_TIMELINE";
/// Derived episodes segmented from `CF_TIMELINE` (#846). Fully rebuildable:
/// re-segmentation replaces day-aligned key ranges idempotently.
pub const CF_EPISODES: &str = "CF_EPISODES";
/// Derived routines mined from `CF_EPISODES` (#848). Fully rebuildable:
/// re-mining replaces the entire CF atomically.
pub const CF_ROUTINES: &str = "CF_ROUTINES";
/// Operator-owned routine lifecycle state (#849).
///
/// Stores confirmations, disables, labels, transition audit trail, and
/// confidence history. NOT derived state — it must survive every
/// `CF_ROUTINES` replace-all, keyed by the same stable routine id.
pub const CF_ROUTINE_STATE: &str = "CF_ROUTINE_STATE";
/// Durable agent lifecycle/telemetry event journal (#897).
///
/// Keys use `(ts_ns, seq)` like `CF_TIMELINE`. One row is stored per agent
/// event (spawn, state change, tool call, turn, message, lease, exit) with
/// `OTel` GenAI-aligned attributes. Append-only; retention is TTL + GC, never
/// rewritten.
pub const CF_AGENT_EVENTS: &str = "CF_AGENT_EVENTS";
/// Normalized spawned-agent transcripts (#900), keyed
/// `spawn_id || 0x00 || line_no BE`.
///
/// Exactly one row per source JSONL line (parsed or invalid) so rows
/// reconcile line-for-line against the raw file. Idempotently
/// re-ingestable: the same line always maps to the same key.
pub const CF_AGENT_TRANSCRIPTS: &str = "CF_AGENT_TRANSCRIPTS";

/// PRD §4 column family names, excluding `RocksDB`'s implicit `default` CF.
pub const ALL_COLUMN_FAMILIES: [&str; 17] = [
    CF_EVENTS,
    CF_OBSERVATIONS,
    CF_PROFILES,
    CF_MODEL_CACHE,
    CF_SESSIONS,
    CF_REFLEX_AUDIT,
    CF_OCR_CACHE,
    CF_TELEMETRY,
    CF_ACTION_LOG,
    CF_PROCESS_HISTORY,
    CF_KV,
    CF_TIMELINE,
    CF_EPISODES,
    CF_ROUTINES,
    CF_ROUTINE_STATE,
    CF_AGENT_EVENTS,
    CF_AGENT_TRANSCRIPTS,
];
