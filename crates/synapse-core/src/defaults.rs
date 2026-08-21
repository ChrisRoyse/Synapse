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

/// Hard committed/private byte ceiling for the daemon Job.
///
/// The ceiling covers each process and aggregate descendant committed/private
/// bytes. It is enforced by the daemon's immediate Windows Job.
/// No working-set limit is installed: the configured launch token cannot set
/// that privilege-dependent Job branch, and the branch would not provide an
/// aggregate resident-RAM ceiling anyway. Resident working set remains
/// measured telemetry.
///
/// Runtime ownership is nested beneath a separate supervisor Job.
///
/// The long-lived supervisor tree is nested inside the page-aligned aggregate
/// parent Job below. This lower 789,999,616-byte child boundary leaves
/// committed/private capacity for the native bootstrap and PowerShell
/// supervisor while keeping every daemon shell descendant inside the same
/// committed-memory contract. This value is shared by every Rust writer/decoder
/// that seals pre-trigger resource state, and setup queries the same value from
/// the kernel before resume.
pub const SYNAPSE_PROCESS_HARD_LIMIT_BYTES: u64 = 789_999_616;

/// Aggregate committed-memory ceiling for the bound Synapse supervisor tree.
///
/// Windows rounds committed-memory Job limits down to a page boundary. The
/// compiled 949,997,568-byte value is already page aligned, so exact kernel
/// readback is stable. The remaining 50,002,432 bytes below the operator's
/// decimal 1 GB ceiling are a measured, fail-closed reserve for the tiny native
/// bootstrap's pre-association peak, which Windows explicitly does not charge
/// retroactively when it joins the Job. This is not an aggregate
/// resident-working-set bound; Windows Job Objects do not expose an equivalent
/// hard sum-of-resident-pages limit.
pub const SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES: u64 = 949_997_568;

/// Windows Job hard CPU-rate units are hundredths of one percent. `2500`
/// therefore caps the complete owned tree at 25% of host CPU capacity.
pub const SYNAPSE_OWNED_TREE_CPU_RATE: u32 = 2_500;

/// The daemon's nested Job does not add a second throttle: nested Job CPU
/// rates multiply, so 100% of the 25%-capped parent preserves the single
/// intended process-tree ceiling.
pub const SYNAPSE_DAEMON_JOB_CPU_RATE: u32 = 10_000;
