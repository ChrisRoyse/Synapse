use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{CaptureRuntimeReadback, ObservationCaptureConfig, PerceptionMode, ProfileId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub ok: bool,
    pub version: String,
    pub build: String,
    /// OS process ID of the daemon serving this payload. Lets bridges and
    /// `doctor` confirm which process answered and that all clients share one
    /// daemon.
    pub pid: u32,
    pub uptime_s: u64,
    /// Number of currently advertised MCP tools after schema sanitization.
    pub tool_count: usize,
    /// Stable SHA-256 fingerprint of the currently advertised sanitized tools/list
    /// surface, sorted by tool name.
    pub tool_surface_sha256: String,
    /// Current sanitized tool names, sorted for deterministic stale-client
    /// readback.
    pub tool_names: Vec<String>,
    pub subsystems: BTreeMap<String, SubsystemHealth>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubsystemHealth {
    pub status: String,
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile_id: Option<ProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_maintenance_supported: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_maintenance_unsupported_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_sizes: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_cf_sizes_skipped_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_task_running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_tick_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_task_running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_probe_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_probe_observed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_completed_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage_gc_last_unsupported_policy_skips: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_completed_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_open: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_identity_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_machine_salt_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_lock_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_pid_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_latest_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_last_recovered_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_torn_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_last_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_last_calyx_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_remediation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_bit_floor_bits: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_correlation_ceiling: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_guard_far_identity: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_guard_far_content: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_guard_far_stylistic: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_guard_cold_start_tau: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_kernel_fraction: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_kernel_recall_gate: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_fusion_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_temporal_boost_min: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_temporal_boost_max: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vram_budget_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_backend_requested: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_cuda_compiled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_device_vram_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_device_avx512: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_cpu_avx512_available: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_cpu_simd_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_fallback_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_fallback_source_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_fallback_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_tolerance: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_dot: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_cosine: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_l2_squared: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_topk: Option<Vec<CalyxMathProbeTopKEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_clock_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_fixed_clock_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_rng_seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tick_jitter_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_tick_jitter_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_tick_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_tick_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursion_clamps_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reload_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ring_buffer_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_model_loaded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_sessions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse_subscribers: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_resolution: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_shell_inline_await_limit_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_shell_inline_client_call_budget_ms: Option<u64>,
    /// Outer `None` omits the field for unrelated subsystems; inner `None`
    /// serializes as JSON null to make an unbounded durable shell policy visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_shell_durable_default_timeout_ms: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_shell_durable_max_timeout_ms: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perception_mode: Option<PerceptionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_config: Option<ObservationCaptureConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_runtime: Option<CaptureRuntimeReadback>,
    /// Structured `chrome_bridge` verdict. `None` for every subsystem except
    /// `chrome_bridge`; the MCP health builder populates it so the bridge
    /// readiness is machine-readable instead of a single concatenated
    /// `detail` string. In compact health responses only the verdict-critical
    /// fields are retained; full responses populate every parsed field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_bridge: Option<ChromeBridgeDetail>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalyxMathProbeTopKEntry {
    pub index: usize,
    pub score: f32,
}

/// Structured replacement for the `chrome_bridge` subsystem's concatenated
/// `detail` blob.
///
/// Each field names one piece the blob previously encoded as
/// `key=value` text. Every field is optional so partially-observed hosts and
/// the no-host/unavailable branch omit what they cannot report, and so compact
/// health responses can drop the verbose identity fields while keeping the
/// readiness verdict.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChromeBridgeDetail {
    /// Whether tab-control debugger commands can currently be issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_control_available: Option<bool>,
    /// Whether the connected extension identity is stale versus expectations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_stale: Option<bool>,
    /// Pipe-joined stale reasons, or `none` when the identity is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_stale_reasons: Option<String>,
    /// Reason code emitted when no active bridge host is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Number of registered bridge hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_count: Option<u64>,
    /// Number of commands queued for the active host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_count: Option<u64>,
    /// Number of commands pending acknowledgement from the active host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_count: Option<u64>,
    /// Extension id reported by the active host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_id: Option<String>,
    /// Extension id the bridge expects (identity anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_extension_id: Option<String>,
    /// Extension version reported by the active host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_version: Option<String>,
    /// Transport carrying bridge traffic (e.g. `native_messaging`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Chrome extension health endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}
