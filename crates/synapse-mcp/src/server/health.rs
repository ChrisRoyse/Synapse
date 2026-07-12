use super::{BTreeMap, ErrorData, Health, SubsystemHealth, SynapseService};
use rmcp::model::Tool;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use synapse_action::BackendResolutionPolicy;
use synapse_core::{Backend, ChromeBridgeDetail};

/// Verbosity control for the `health` tool response.
///
/// `health` is called frequently and its verbose per-subsystem `detail` prose
/// dominates the payload token cost (issue #1554). `Compact` (the default)
/// keeps every structured verdict field but drops the long human-readable
/// `detail` blobs, so callers still learn the health conclusion at a fraction
/// of the wire size. `Full` preserves the complete legacy output for
/// debugging.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum HealthDetail {
    /// Drop verbose per-subsystem `detail` prose; keep structured verdicts.
    #[default]
    Compact,
    /// Preserve every `detail` string (the legacy behavior).
    Full,
}

/// Request parameters for the `health` tool.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct HealthParams {
    /// `compact` (default) trims verbose per-subsystem detail prose while
    /// keeping every structured status field; `full` returns the complete
    /// diagnostic detail for every subsystem.
    pub detail: HealthDetail,
}

fn state_lock_health() -> SubsystemHealth {
    SubsystemHealth {
        status: "error".to_owned(),
        detail: Some("M3 service state lock poisoned".to_owned()),
        ..SubsystemHealth::default()
    }
}

fn storage_pressure_status(level: synapse_storage::DiskPressureLevel) -> String {
    match level {
        synapse_storage::DiskPressureLevel::Normal => "ok",
        synapse_storage::DiskPressureLevel::Level1 => "disk_pressure_l1",
        synapse_storage::DiskPressureLevel::Level2 => "disk_pressure_l2",
        synapse_storage::DiskPressureLevel::Level3 => "disk_pressure_l3",
        synapse_storage::DiskPressureLevel::Level4 => "disk_pressure_l4",
    }
    .to_owned()
}

impl SynapseService {
    #[cfg(test)]
    pub(crate) fn health_payload(&self) -> Health {
        self.health_payload_with_http_sessions(None)
    }

    pub(crate) fn health_payload_for_session(
        &self,
        session_id: Option<&str>,
        detail: HealthDetail,
    ) -> Health {
        self.health_payload_with_http_sessions_and_session_detail(None, session_id, detail)
    }

    pub(crate) fn health_payload_with_http_sessions(
        &self,
        active_sessions: Option<usize>,
    ) -> Health {
        self.health_payload_with_http_sessions_and_session(active_sessions, None)
    }

    /// Non-MCP callers (HTTP `/health`, dashboard, tests) keep the full detail
    /// so their existing readback is unchanged; only the frequently-called MCP
    /// `health` tool defaults to compact.
    pub(crate) fn health_payload_with_http_sessions_and_session(
        &self,
        active_sessions: Option<usize>,
        session_id: Option<&str>,
    ) -> Health {
        self.health_payload_with_http_sessions_and_session_detail(
            active_sessions,
            session_id,
            HealthDetail::Full,
        )
    }

    pub(crate) fn health_payload_with_http_sessions_and_session_detail(
        &self,
        active_sessions: Option<usize>,
        session_id: Option<&str>,
        detail: HealthDetail,
    ) -> Health {
        let mut subsystems = BTreeMap::new();
        subsystems.insert("storage".to_owned(), self.storage_health());
        subsystems.insert("reflex".to_owned(), self.reflex_health());
        subsystems.insert("profiles".to_owned(), self.profile_health());
        subsystems.insert("perception".to_owned(), self.perception_health());
        subsystems.insert("action".to_owned(), self.action_health());
        subsystems.insert("audio".to_owned(), self.audio_health());
        subsystems.insert(
            "chrome_bridge".to_owned(),
            crate::chrome_debugger_bridge::health_subsystem(),
        );
        subsystems.insert("http".to_owned(), self.http_health(active_sessions));
        subsystems.insert("daemon_drain".to_owned(), self.daemon_drain_health());
        subsystems.insert(
            "daemon_lifecycle".to_owned(),
            crate::daemon_lifecycle::health_subsystem(),
        );
        subsystems.insert(
            "public_tool_registry".to_owned(),
            self.public_tool_registry_health(),
        );
        subsystems.insert("facade_contract".to_owned(), self.facade_contract_health());
        let tool_surface = self.tool_surface_fingerprint(session_id);
        if let Some(error) = &tool_surface.error {
            subsystems.insert(
                "tool_surface".to_owned(),
                SubsystemHealth {
                    status: "error".to_owned(),
                    detail: Some(error.clone()),
                    ..SubsystemHealth::default()
                },
            );
        }
        let ok = subsystems.values().all(|health| health.status != "error");
        apply_health_detail(&mut subsystems, detail);
        Health {
            ok,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: option_env!("VERGEN_GIT_SHA").unwrap_or("dev").to_owned(),
            pid: std::process::id(),
            uptime_s: self.started_at.elapsed().as_secs(),
            tool_count: tool_surface.names.len(),
            tool_surface_sha256: tool_surface.sha256,
            tool_names: tool_surface.names,
            subsystems,
        }
    }

    fn public_tool_registry_health(&self) -> SubsystemHealth {
        match self.public_tool_registry_snapshot() {
            Ok(snapshot) => {
                let missing_count = snapshot.registered_tools_missing.len();
                let status = if missing_count == 0 {
                    "ok"
                } else {
                    "pending_facades"
                };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(format!(
                        "source_of_truth={} public_tool_count={} max_public_tool_count={} implementation_tool_count={} registered_tools_present={} registered_tools_missing={}",
                        snapshot.source_of_truth,
                        snapshot.public_tool_count,
                        snapshot.max_public_tool_count,
                        snapshot.implementation_tool_count,
                        snapshot.registered_tools_present.len(),
                        missing_count
                    )),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!("{error:?}")),
                ..SubsystemHealth::default()
            },
        }
    }

    fn facade_contract_health(&self) -> SubsystemHealth {
        match Self::facade_contract_snapshot() {
            Ok(snapshot) => {
                let invalid_count = snapshot.missing_contract_tool_names.len()
                    + snapshot.unknown_contract_tool_names.len()
                    + snapshot.duplicate_contract_tool_names.len()
                    + snapshot.duplicate_operation_names.len()
                    + snapshot.invalid_contract_reasons.len();
                let status = if invalid_count == 0 { "ok" } else { "error" };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(format!(
                        "source_of_truth={} public_tool_count={} contract_tool_count={} operation_count={} mutating_operation_count={} invalid_count={} contract_sha256={}",
                        snapshot.source_of_truth,
                        snapshot.public_tool_count,
                        snapshot.contract_tool_count,
                        snapshot.operation_count,
                        snapshot.mutating_operation_count,
                        invalid_count,
                        snapshot.facade_contract_sha256
                    )),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!("{error:?}")),
                ..SubsystemHealth::default()
            },
        }
    }

    fn daemon_drain_health(&self) -> SubsystemHealth {
        let snapshot = self.drain_state_handle().snapshot();
        let status = if snapshot.state_error.is_some() {
            "error"
        } else if snapshot.draining {
            "draining"
        } else {
            "ok"
        };
        let detail = if let Some(error) = snapshot.state_error {
            error
        } else if snapshot.draining {
            format!(
                "reason_code={} source={} started_at_unix_ms={}",
                snapshot.reason_code.unwrap_or("unknown"),
                snapshot.source.unwrap_or("unknown"),
                snapshot.started_at_unix_ms.unwrap_or_default()
            )
        } else {
            "daemon accepting work".to_owned()
        };
        SubsystemHealth {
            status: status.to_owned(),
            detail: Some(detail),
            ..SubsystemHealth::default()
        }
    }

    fn tool_surface_fingerprint(&self, session_id: Option<&str>) -> ToolSurfaceFingerprint {
        let mut tools = match self.health_tool_surface(session_id) {
            Ok(tools) => tools,
            Err(error) => {
                tracing::error!(
                    code = "MCP_TOOL_SURFACE_HEALTH_READ_FAILED",
                    session_id,
                    error = ?error,
                    "failed to resolve MCP health tool surface"
                );
                return ToolSurfaceFingerprint {
                    names: Vec::new(),
                    sha256: "TOOL_SURFACE_HEALTH_READ_FAILED".to_owned(),
                    error: Some(format!(
                        "failed to resolve MCP health tool surface: {error}"
                    )),
                };
            }
        };
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        let names = tools
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        let canonical = serde_json::json!({
            "mcp_surface": "tools/list",
            "tools": tools,
        });
        let bytes = match canonical_json_bytes(canonical) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(
                    code = "MCP_TOOL_SURFACE_FINGERPRINT_SERIALIZE_FAILED",
                    %error,
                    "sanitized MCP tool surface failed to serialize for health fingerprinting"
                );
                return ToolSurfaceFingerprint {
                    names,
                    sha256: "TOOL_SURFACE_FINGERPRINT_ERROR".to_owned(),
                    error: Some(format!(
                        "sanitized MCP tool surface failed to serialize for health fingerprinting: {error}"
                    )),
                };
            }
        };
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        ToolSurfaceFingerprint {
            names,
            sha256: hex_lower(&hasher.finalize()),
            error: None,
        }
    }

    fn health_tool_surface(&self, session_id: Option<&str>) -> Result<Vec<Tool>, ErrorData> {
        if session_id.is_some() {
            return self.tools_for_session_profile(session_id);
        }
        let public_snapshot = self.public_tool_registry_snapshot()?;
        let public_names = public_snapshot
            .public_tool_names
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut tools = self.full_sanitized_tools();
        tools.retain(|tool| public_names.contains(tool.name.as_ref()));
        Ok(tools)
    }

    fn storage_health(&self) -> SubsystemHealth {
        match self.m3_state.lock() {
            Ok(state) => {
                let db_path = state
                    .db_path
                    .as_ref()
                    .map(|path| path.display().to_string());
                if let Some(error) = &state.storage_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        db_path,
                        ..SubsystemHealth::default()
                    };
                }
                let Some(runtime) = &state.reflex_runtime else {
                    if state.db.is_some() {
                        return SubsystemHealth {
                            status: "ok".to_owned(),
                            detail: Some(
                                "storage opened at daemon startup (reflex runtime idle)".to_owned(),
                            ),
                            db_path,
                            schema_version: Some(synapse_core::SCHEMA_VERSION),
                            ..SubsystemHealth::default()
                        };
                    }
                    return SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some("storage opens on first reflex tool call".to_owned()),
                        db_path,
                        ..SubsystemHealth::default()
                    };
                };
                match runtime.lock() {
                    Ok(runtime) => match runtime.storage_cf_live_data_size_estimates() {
                        Ok(cf_sizes) => SubsystemHealth {
                            status: storage_pressure_status(runtime.storage_pressure_level()),
                            detail: Some(
                                "storage runtime initialized; cf_sizes use RocksDB live-data estimates"
                                    .to_owned(),
                            ),
                            db_path: Some(runtime.storage_path().display().to_string()),
                            schema_version: Some(runtime.schema_version()),
                            cf_sizes: Some(cf_sizes.0),
                            ..SubsystemHealth::default()
                        },
                        Err(error) => SubsystemHealth {
                            status: "error".to_owned(),
                            detail: Some(error.to_string()),
                            db_path: Some(runtime.storage_path().display().to_string()),
                            schema_version: Some(runtime.schema_version()),
                            ..SubsystemHealth::default()
                        },
                    },
                    Err(_err) => SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(
                            "reflex runtime lock poisoned while reading storage".to_owned(),
                        ),
                        db_path,
                        ..SubsystemHealth::default()
                    },
                }
            }
            Err(_err) => state_lock_health(),
        }
    }

    fn reflex_health(&self) -> SubsystemHealth {
        match self.m3_state.lock() {
            Ok(state) => {
                if let Some(error) = &state.reflex_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        ..SubsystemHealth::default()
                    };
                }
                if state.reflex_disabled {
                    return SubsystemHealth {
                        status: "disabled".to_owned(),
                        detail: Some("reflex runtime disabled by operator".to_owned()),
                        active_count: Some(0),
                        ..SubsystemHealth::default()
                    };
                }
                let Some(runtime) = &state.reflex_runtime else {
                    return SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some("reflex runtime starts on first reflex tool call".to_owned()),
                        active_count: Some(0),
                        recursion_clamps_total: Some(0),
                        ..SubsystemHealth::default()
                    };
                };
                match runtime.lock() {
                    Ok(runtime) => match runtime.recursion_clamps_total() {
                        Ok(recursion_clamps_total) => SubsystemHealth {
                            status: if runtime.degraded_latency() {
                                "degraded_latency".to_owned()
                            } else {
                                "ok".to_owned()
                            },
                            detail: Some("reflex runtime initialized".to_owned()),
                            active_count: Some(runtime.active_count()),
                            sample_count: Some(runtime.sample_count()),
                            sample_limit: Some(runtime.sample_limit()),
                            last_tick_jitter_us: runtime.last_tick_jitter_us(),
                            p99_tick_jitter_us: runtime.p99_tick_jitter_us(),
                            late_tick_count: Some(runtime.late_tick_count()),
                            degraded_tick_count: Some(runtime.degraded_tick_count()),
                            recursion_clamps_total: Some(recursion_clamps_total),
                            ..SubsystemHealth::default()
                        },
                        Err(error) => SubsystemHealth {
                            status: "error".to_owned(),
                            detail: Some(error.to_string()),
                            active_count: Some(runtime.active_count()),
                            sample_count: Some(runtime.sample_count()),
                            sample_limit: Some(runtime.sample_limit()),
                            last_tick_jitter_us: runtime.last_tick_jitter_us(),
                            p99_tick_jitter_us: runtime.p99_tick_jitter_us(),
                            late_tick_count: Some(runtime.late_tick_count()),
                            degraded_tick_count: Some(runtime.degraded_tick_count()),
                            ..SubsystemHealth::default()
                        },
                    },
                    Err(_err) => SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some("reflex runtime lock poisoned".to_owned()),
                        ..SubsystemHealth::default()
                    },
                }
            }
            Err(_err) => state_lock_health(),
        }
    }

    fn profile_health(&self) -> SubsystemHealth {
        match self.m3_state.lock() {
            Ok(state) => {
                if let Some(error) = &state.profile_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        ..SubsystemHealth::default()
                    };
                }
                state.profile_runtime.as_ref().map_or_else(
                    || SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some(
                            "profile runtime initializes on first profile tool call".to_owned(),
                        ),
                        ..SubsystemHealth::default()
                    },
                    |runtime| {
                        let active_profile_id = runtime.active_profile_id();
                        let profiles = runtime.list(true);
                        let last_reload_at = runtime.last_reload_at();
                        match (active_profile_id, profiles, last_reload_at) {
                            (Ok(active_profile_id), Ok(profiles), Ok(last_reload_at)) => {
                                SubsystemHealth {
                                    status: "ok".to_owned(),
                                    detail: Some(format!(
                                        "profile_dir={}",
                                        runtime.profile_dir().display()
                                    )),
                                    active_profile_id,
                                    profile_count: Some(profiles.len()),
                                    last_reload_at,
                                    ..SubsystemHealth::default()
                                }
                            }
                            (active_profile_id, profiles, last_reload_at) => {
                                let detail = active_profile_id
                                    .err()
                                    .map(|error| error.to_string())
                                    .or_else(|| profiles.err().map(|error| error.to_string()))
                                    .or_else(|| last_reload_at.err().map(|error| error.to_string()))
                                    .unwrap_or_else(|| "profile runtime error".to_owned());
                                SubsystemHealth {
                                    status: "error".to_owned(),
                                    detail: Some(detail),
                                    ..SubsystemHealth::default()
                                }
                            }
                        }
                    },
                )
            }
            Err(_err) => state_lock_health(),
        }
    }

    fn perception_health(&self) -> SubsystemHealth {
        match self.m1_state.lock() {
            Ok(state) => SubsystemHealth {
                status: "ok".to_owned(),
                detail: Some("perception runtime initialized".to_owned()),
                perception_mode: Some(state.perception_mode),
                capture_config: Some(state.active_capture_config.clone()),
                capture_runtime: Some(state.capture_runtime_readback()),
                ..SubsystemHealth::default()
            },
            Err(_err) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some("M1 service state lock poisoned".to_owned()),
                ..SubsystemHealth::default()
            },
        }
    }

    fn action_health(&self) -> SubsystemHealth {
        match self.m2_state.lock() {
            Ok(state) => match state.backend_resolution_readback() {
                Ok((source, policy)) => {
                    let emitter_available = state.emitter_available();
                    let operator_hotkey = synapse_action::operator_hotkey_status().label();
                    let allow_shell = if self.m4_config.allow_shell_any() {
                        "any".to_owned()
                    } else {
                        self.m4_config.allow_shell_count().to_string()
                    };
                    let allow_launch = if self.m4_config.allow_launch_any() {
                        "any".to_owned()
                    } else {
                        self.m4_config.allow_launch_count().to_string()
                    };
                    let lease = synapse_action::lease::status();
                    let lease_detail = lease.owner_session_id.as_deref().map_or_else(
                        || "input_lease_held=false".to_owned(),
                        |owner| {
                            format!(
                                "input_lease_held=true input_lease_owner={owner} input_lease_expires_in_ms={}",
                                lease.expires_in_ms.unwrap_or(0)
                            )
                        },
                    );
                    let search_tools = crate::m4::shell_search_tool_readback();
                    SubsystemHealth {
                        status: if emitter_available { "ok" } else { "error" }.to_owned(),
                        detail: Some(format!(
                            "emitter_available={} recording_enabled={} operator_hotkey={} allow_shell_patterns={} allow_launch_patterns={} {} {}",
                            emitter_available,
                            state.recording_enabled(),
                            operator_hotkey,
                            allow_shell,
                            allow_launch,
                            lease_detail,
                            search_tools
                        )),
                        backend_resolution: Some(backend_resolution_health(source, policy)),
                        run_shell_inline_await_limit_ms: Some(
                            self.m4_config.run_shell_inline_await_limit_ms(),
                        ),
                        run_shell_inline_client_call_budget_ms: Some(
                            self.m4_config.run_shell_inline_client_call_budget_ms(),
                        ),
                        run_shell_durable_default_timeout_ms: Some(
                            self.m4_config.run_shell_durable_default_timeout_ms(),
                        ),
                        run_shell_durable_max_timeout_ms: Some(
                            self.m4_config.run_shell_durable_max_timeout_ms(),
                        ),
                        ..SubsystemHealth::default()
                    }
                }
                Err(error) => SubsystemHealth {
                    status: "error".to_owned(),
                    detail: Some(error),
                    ..SubsystemHealth::default()
                },
            },
            Err(_err) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some("M2 service state lock poisoned".to_owned()),
                ..SubsystemHealth::default()
            },
        }
    }

    fn audio_health(&self) -> SubsystemHealth {
        match self.m3_state.lock() {
            Ok(state) => {
                if let Some(error) = &state.audio_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        ..SubsystemHealth::default()
                    };
                }
                if !state.enable_audio {
                    return SubsystemHealth {
                        status: "disabled".to_owned(),
                        detail: Some("audio is disabled; start with --enable-audio".to_owned()),
                        ring_buffer_seconds: Some(synapse_audio::DEFAULT_RING_SECONDS),
                        stt_model_loaded: Some(false),
                        ..SubsystemHealth::default()
                    };
                }
                let Some(runtime) = &state.audio_runtime else {
                    return SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some(
                            "audio runtime initializes on buffered audio or transcription requests"
                                .to_owned(),
                        ),
                        ring_buffer_seconds: Some(synapse_audio::DEFAULT_RING_SECONDS),
                        stt_model_loaded: Some(false),
                        ..SubsystemHealth::default()
                    };
                };
                let loopback_status = runtime.loopback_status();
                let status = if loopback_status.last_error_code.is_some() {
                    "error"
                } else {
                    "ok"
                };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(loopback_status.last_error_code.map_or_else(
                        || {
                            if loopback_status.running {
                                "audio loopback running".to_owned()
                            } else {
                                "audio runtime initialized; loopback disabled".to_owned()
                            }
                        },
                        |code| format!("audio loopback error: {code}"),
                    )),
                    ring_buffer_seconds: Some(runtime.config().ring_seconds),
                    stt_model_loaded: Some(runtime.stt_model_loaded()),
                    ..SubsystemHealth::default()
                }
            }
            Err(_err) => state_lock_health(),
        }
    }

    fn http_health(&self, active_sessions: Option<usize>) -> SubsystemHealth {
        match self.m3_state.lock() {
            Ok(state) => {
                if state.shutdown_reason == "http" {
                    let diagnostics = crate::http::http_transport_diagnostics_detail();
                    SubsystemHealth {
                        status: "ok".to_owned(),
                        detail: Some(format!("HTTP transport initialized; {diagnostics}")),
                        bind_addr: Some(state.bind.clone()),
                        active_sessions,
                        sse_subscribers: Some(state.sse_state.active_subscription_count()),
                        ..SubsystemHealth::default()
                    }
                } else {
                    SubsystemHealth {
                        status: "disabled".to_owned(),
                        detail: Some("HTTP transport disabled in stdio mode".to_owned()),
                        bind_addr: Some(state.bind.clone()),
                        active_sessions: Some(0),
                        sse_subscribers: Some(state.sse_state.active_subscription_count()),
                        ..SubsystemHealth::default()
                    }
                }
            }
            Err(_err) => state_lock_health(),
        }
    }
}

struct ToolSurfaceFingerprint {
    names: Vec<String>,
    sha256: String,
    error: Option<String>,
}

/// Apply the requested detail verbosity to the assembled subsystem map.
///
/// `Compact` (the default) drops every verbose per-subsystem `detail` string
/// but leaves the structured verdict fields (status, counts, schema versions,
/// timeouts, ...) untouched, so the health conclusion is unchanged — only the
/// human-readable prose is trimmed. `Full` preserves the `detail` strings.
///
/// The `chrome_bridge` subsystem's only structured information historically
/// lived inside its concatenated `detail` blob, so in both modes we parse that
/// blob into the typed `ChromeBridgeDetail`. `Full` keeps the original blob and
/// the fully-parsed struct; `Compact` keeps only the verdict-critical fields
/// and drops the blob.
fn apply_health_detail(subsystems: &mut BTreeMap<String, SubsystemHealth>, detail: HealthDetail) {
    for (name, subsystem) in subsystems.iter_mut() {
        if name == "chrome_bridge" {
            let parsed = subsystem.detail.as_deref().map(parse_chrome_bridge_detail);
            match detail {
                HealthDetail::Full => {
                    subsystem.chrome_bridge = parsed;
                }
                HealthDetail::Compact => {
                    subsystem.chrome_bridge = parsed.map(compact_chrome_bridge_detail);
                    subsystem.detail = None;
                }
            }
        } else if detail == HealthDetail::Compact {
            subsystem.detail = None;
        }
    }
}

/// Parse the `chrome_bridge` `detail` blob (`key=value` tokens joined by
/// spaces, with trailing free-text guidance) into the typed
/// `ChromeBridgeDetail`.
///
/// Only whitespace-free `key=value` tokens are consumed, so the trailing
/// human-readable guidance strings (which contain spaces) are ignored rather
/// than corrupting fields. Full-mode responses retain the original `detail`
/// string, so no information is lost even for fields this parser does not
/// surface.
fn parse_chrome_bridge_detail(detail: &str) -> ChromeBridgeDetail {
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for token in detail.split_whitespace() {
        if let Some((key, value)) = token.split_once('=') {
            if !key.is_empty()
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            {
                // First occurrence wins; blob keys are unique.
                fields.entry(key).or_insert(value);
            }
        }
    }
    let bool_field = |key: &str| {
        fields.get(key).and_then(|value| match *value {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    };
    let u64_field = |key: &str| fields.get(key).and_then(|value| (*value).parse::<u64>().ok());
    let string_field = |key: &str| fields.get(key).map(|value| (*value).to_owned());
    ChromeBridgeDetail {
        tab_control_available: bool_field("tab_control_available"),
        extension_stale: bool_field("extension_stale"),
        extension_stale_reasons: string_field("extension_stale_reasons"),
        reason: string_field("reason"),
        host_count: u64_field("host_count"),
        queued_count: u64_field("queued_count"),
        pending_count: u64_field("pending_count"),
        extension_id: string_field("extension_id"),
        expected_extension_id: string_field("expected_extension_id"),
        extension_version: string_field("extension_version"),
        transport: string_field("transport"),
        endpoint: string_field("endpoint"),
    }
}

/// Reduce a fully-parsed `ChromeBridgeDetail` to the verdict-critical fields
/// retained in compact health responses. The verbose identity/version/endpoint
/// fields are dropped (they remain available via `detail=full`).
fn compact_chrome_bridge_detail(detail: ChromeBridgeDetail) -> ChromeBridgeDetail {
    ChromeBridgeDetail {
        tab_control_available: detail.tab_control_available,
        extension_stale: detail.extension_stale,
        extension_stale_reasons: detail.extension_stale_reasons,
        reason: detail.reason,
        host_count: detail.host_count,
        queued_count: detail.queued_count,
        pending_count: detail.pending_count,
        extension_id: None,
        expected_extension_id: None,
        extension_version: None,
        transport: None,
        endpoint: None,
    }
}

fn canonical_json_bytes(value: Value) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&canonical_json_value(value))
}

fn canonical_json_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_json_value).collect()),
        Value::Object(map) => {
            let mut entries = map.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut ordered = Map::new();
            for (key, child) in entries {
                ordered.insert(key, canonical_json_value(child));
            }
            Value::Object(ordered)
        }
        scalar => scalar,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn backend_resolution_health(
    source: String,
    policy: BackendResolutionPolicy,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("source".to_owned(), source),
        (
            "default_backend".to_owned(),
            backend_config_name(policy.default_backend).to_owned(),
        ),
        (
            "keyboard_default".to_owned(),
            backend_config_name(policy.keyboard_default).to_owned(),
        ),
        (
            "mouse_default".to_owned(),
            backend_config_name(policy.mouse_default).to_owned(),
        ),
        (
            "pad_default".to_owned(),
            backend_config_name(policy.pad_default).to_owned(),
        ),
        (
            "keyboard_auto".to_owned(),
            policy.keyboard_auto_backend().as_str().to_owned(),
        ),
        (
            "mouse_auto".to_owned(),
            policy.mouse_auto_backend().as_str().to_owned(),
        ),
        (
            "pad_auto".to_owned(),
            policy.pad_auto_backend().as_str().to_owned(),
        ),
        (
            "release_all_auto".to_owned(),
            policy.release_all_auto_backend().as_str().to_owned(),
        ),
    ])
}

const fn backend_config_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Software => "software",
        Backend::Vigem => "vigem",
        Backend::Hardware => "hardware",
        Backend::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HealthDetail, HealthParams, SynapseService, canonical_json_bytes,
        parse_chrome_bridge_detail,
    };

    #[test]
    fn canonical_json_bytes_sorts_object_keys_recursively() {
        let left = serde_json::json!({
            "tools": [
                {"name": "b", "inputSchema": {"z": 2, "a": 1}},
                {"inputSchema": {"b": true, "a": false}, "name": "a"}
            ],
            "mcp_surface": "tools/list"
        });
        let right = serde_json::json!({
            "mcp_surface": "tools/list",
            "tools": [
                {"inputSchema": {"a": 1, "z": 2}, "name": "b"},
                {"name": "a", "inputSchema": {"a": false, "b": true}}
            ]
        });

        let left = canonical_json_bytes(left).expect("canonical left");
        let right = canonical_json_bytes(right).expect("canonical right");

        assert_eq!(left, right);
        assert_eq!(
            String::from_utf8(left).expect("utf8"),
            r#"{"mcp_surface":"tools/list","tools":[{"inputSchema":{"a":1,"z":2},"name":"b"},{"inputSchema":{"a":false,"b":true},"name":"a"}]}"#
        );
    }

    #[test]
    fn compact_health_is_materially_smaller_than_full() {
        // #1554: the verbose per-subsystem detail prose is the payload's bulk.
        // Compact drops it while keeping every structured verdict field, so the
        // serialized JSON must be materially smaller than full.
        let service = SynapseService::new();
        let compact = service.health_payload_with_http_sessions_and_session_detail(
            None,
            None,
            HealthDetail::Compact,
        );
        let full = service.health_payload_with_http_sessions_and_session_detail(
            None,
            None,
            HealthDetail::Full,
        );
        let compact_json = serde_json::to_string(&compact).expect("serialize compact health");
        let full_json = serde_json::to_string(&full).expect("serialize full health");
        let saved = full_json.len().saturating_sub(compact_json.len());
        println!(
            "evidence=health_detail_size compact_bytes={} full_bytes={} saved_bytes={}",
            compact_json.len(),
            full_json.len(),
            saved
        );
        assert!(
            compact_json.len() < full_json.len(),
            "compact ({}) must be smaller than full ({})",
            compact_json.len(),
            full_json.len()
        );
        assert!(
            saved >= 300,
            "compact should drop at least a few hundred bytes of detail prose; saved={saved}"
        );
    }

    #[test]
    fn compact_and_full_report_identical_subsystem_verdicts() {
        // Compact must change only the prose, never the health conclusion.
        let service = SynapseService::new();
        let compact = service.health_payload_with_http_sessions_and_session_detail(
            None,
            None,
            HealthDetail::Compact,
        );
        let full = service.health_payload_with_http_sessions_and_session_detail(
            None,
            None,
            HealthDetail::Full,
        );
        assert_eq!(compact.ok, full.ok, "overall verdict must match");
        assert_eq!(
            compact.subsystems.keys().collect::<Vec<_>>(),
            full.subsystems.keys().collect::<Vec<_>>(),
            "same subsystems must be present in both modes"
        );
        for (name, full_sub) in &full.subsystems {
            let compact_sub = compact
                .subsystems
                .get(name)
                .expect("subsystem present in compact");
            assert_eq!(
                compact_sub.status, full_sub.status,
                "subsystem {name} status must match between compact and full"
            );
            println!("evidence=verdict subsystem={name} status={}", full_sub.status);
        }
        // Full keeps the detail prose; compact drops it.
        let full_bridge = &full.subsystems["chrome_bridge"];
        let compact_bridge = &compact.subsystems["chrome_bridge"];
        assert!(
            full_bridge.detail.is_some(),
            "full chrome_bridge keeps its detail string"
        );
        assert!(
            compact_bridge.detail.is_none(),
            "compact chrome_bridge drops its detail string"
        );
    }

    #[test]
    fn full_chrome_bridge_detail_is_structured() {
        // The chrome_bridge blob is surfaced as a typed struct whose fields
        // agree with the raw detail string they were parsed from.
        let service = SynapseService::new();
        let full = service.health_payload_with_http_sessions_and_session_detail(
            None,
            None,
            HealthDetail::Full,
        );
        let bridge = &full.subsystems["chrome_bridge"];
        let raw_detail = bridge
            .detail
            .as_deref()
            .expect("full chrome_bridge detail string");
        let structured = bridge
            .chrome_bridge
            .as_ref()
            .expect("full chrome_bridge structured detail");
        let tab_control_available = structured
            .tab_control_available
            .expect("tab_control_available populated");
        let host_count = structured.host_count.expect("host_count populated");
        let expected_extension_id = structured
            .expected_extension_id
            .as_deref()
            .expect("expected_extension_id populated");
        assert!(
            raw_detail.contains(&format!("tab_control_available={tab_control_available}")),
            "structured tab_control_available must match the raw blob"
        );
        assert!(
            raw_detail.contains(&format!("expected_extension_id={expected_extension_id}")),
            "structured expected_extension_id must match the raw blob"
        );
        // An independent parse of the raw blob reproduces the surfaced struct.
        let reparsed = parse_chrome_bridge_detail(raw_detail);
        assert_eq!(&reparsed, structured, "parser is deterministic and lossless");
        println!(
            "evidence=chrome_bridge_struct tab_control_available={tab_control_available} host_count={host_count} expected_extension_id={expected_extension_id}"
        );
    }

    #[test]
    fn default_detail_is_compact() {
        // No `detail` param (the common `health {}` call) must resolve to
        // compact — the new default that fixes the verbose-by-default bug.
        assert_eq!(HealthDetail::default(), HealthDetail::Compact);
        assert_eq!(HealthParams::default().detail, HealthDetail::Compact);
        let empty: HealthParams =
            serde_json::from_str("{}").expect("deserialize empty health params");
        assert_eq!(empty.detail, HealthDetail::Compact);
        let full: HealthParams =
            serde_json::from_str(r#"{"detail":"full"}"#).expect("deserialize full detail");
        assert_eq!(full.detail, HealthDetail::Full);
        let compact: HealthParams =
            serde_json::from_str(r#"{"detail":"compact"}"#).expect("deserialize compact detail");
        assert_eq!(compact.detail, HealthDetail::Compact);
        println!(
            "evidence=default_detail value={:?}",
            HealthParams::default().detail
        );
    }
}
