/// Storage schema version. Pre-v1 migrations may bump this freely.
pub const SCHEMA_VERSION: u32 = 1;

/// Reference-machine warm hybrid observe p99 budget in milliseconds.
pub const REFERENCE_OBSERVE_WARM_HYBRID_P99_MS: f32 = 30.0;

/// Reference-machine idle reflex tick jitter p99 budget in microseconds.
pub const REFERENCE_REFLEX_TICK_JITTER_IDLE_P99_US: u32 = 200;

/// Reference-machine event-to-subscriber p99 budget in milliseconds.
pub const REFERENCE_EVENT_TO_SUBSCRIBER_P99_MS: f32 = 50.0;

/// Default EMA smoothing alpha for `aim_track` reflex target deltas.
pub const DEFAULT_AIM_TRACK_EMA_ALPHA: f32 = 0.7;
