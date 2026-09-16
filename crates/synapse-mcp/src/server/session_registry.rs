use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rmcp::transport::streamable_http_server::session::SessionState;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_STALE_AFTER_MS: u64 = 5 * 60 * 1000;

pub(crate) type SharedSessionRegistry = Arc<Mutex<SessionRegistry>>;

#[derive(Debug)]
pub(crate) struct SessionRegistry {
    stale_after_ms: u64,
    entries: BTreeMap<String, SessionRegistryEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionRegistryEntry {
    pub session_id: String,
    pub transport: String,
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    pub protocol_version: Option<String>,
    pub agent_kind: String,
    pub started_at_unix_ms: u64,
    pub last_seen_unix_ms: u64,
    pub closed_at_unix_ms: Option<u64>,
    pub last_action: Option<String>,
    pub last_reason_code: Option<String>,
    pub spawned_agent: Option<SpawnedAgentRead>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionRegistryRead {
    pub session_id: String,
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    pub agent_kind: String,
    pub lifecycle: String,
    pub started_at_unix_ms: u64,
    pub last_seen_unix_ms: u64,
    pub last_seen_ms_ago: u64,
    pub stale_after_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_agent: Option<SpawnedAgentRead>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnedAgentRead {
    pub spawn_id: String,
    pub cli: String,
    pub launcher_process_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by_session_id: Option<String>,
    pub launched_at_unix_ms: u64,
    pub launch_target: String,
    pub log_dir: String,
    /// Spawn-template provenance (#909): set when this agent was rendered from a
    /// durable template, so the session row records exactly which template
    /// version produced the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<SpawnedAgentControlRead>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnedAgentControlRead {
    pub schema_version: u32,
    pub protocol: String,
    pub endpoint: String,
    pub control_path: String,
    pub events_path: String,
    pub app_server_process_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub turn_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_control_parse_error: Option<String>,
    pub updated_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_server_request_bridge_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_app_server_request_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_app_server_request_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_app_server_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_app_server_request_approval_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_app_server_request_final_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_app_server_request_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_app_server_request_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_interrupt_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_interrupt_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_interrupt_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_steer_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_steer_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_steer_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_steer_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_steer_instruction_chars: Option<u64>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            stale_after_ms: DEFAULT_STALE_AFTER_MS,
            entries: BTreeMap::new(),
        }
    }
}

impl SessionRegistry {
    pub(crate) fn set_stale_after(&mut self, ttl: Option<Duration>) {
        self.stale_after_ms = ttl
            .map(duration_millis_u64)
            .unwrap_or(DEFAULT_STALE_AFTER_MS)
            .max(1);
    }

    pub(crate) const fn stale_after_ms(&self) -> u64 {
        self.stale_after_ms
    }

    /// Records (or refreshes) a session as initialized. Returns `true` when
    /// the session became newly visible — the entry was created or was
    /// previously closed — so callers journal exactly one lifecycle event
    /// per live transition (#897).
    pub(crate) fn record_initialized(
        &mut self,
        session_id: &str,
        state: &SessionState,
        transport: &str,
        now_unix_ms: u64,
    ) -> bool {
        let client_name = state.initialize_params.client_info.name.clone();
        let client_version = state.initialize_params.client_info.version.clone();
        let protocol_version = Some(format!("{:?}", state.initialize_params.protocol_version));
        let mut newly_visible = false;
        let entry = self
            .entries
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                newly_visible = true;
                SessionRegistryEntry {
                    session_id: session_id.to_owned(),
                    transport: transport.to_owned(),
                    client_name: None,
                    client_version: None,
                    protocol_version: None,
                    agent_kind: "unknown".to_owned(),
                    started_at_unix_ms: now_unix_ms,
                    last_seen_unix_ms: now_unix_ms,
                    closed_at_unix_ms: None,
                    last_action: None,
                    last_reason_code: None,
                    spawned_agent: None,
                }
            });
        newly_visible |= entry.closed_at_unix_ms.is_some();
        entry.transport = transport.to_owned();
        entry.client_name = Some(client_name.clone());
        entry.client_version = Some(client_version);
        entry.protocol_version = protocol_version;
        entry.agent_kind = infer_agent_kind(&client_name);
        entry.last_seen_unix_ms = entry.last_seen_unix_ms.max(now_unix_ms);
        entry.closed_at_unix_ms = None;
        newly_visible
    }

    pub(crate) fn record_seen(
        &mut self,
        session_id: &str,
        action: Option<String>,
        now_unix_ms: u64,
    ) {
        let entry = self
            .entries
            .entry(session_id.to_owned())
            .or_insert_with(|| SessionRegistryEntry {
                session_id: session_id.to_owned(),
                transport: "http".to_owned(),
                client_name: None,
                client_version: None,
                protocol_version: None,
                agent_kind: "unknown".to_owned(),
                started_at_unix_ms: now_unix_ms,
                last_seen_unix_ms: now_unix_ms,
                closed_at_unix_ms: None,
                last_action: None,
                last_reason_code: None,
                spawned_agent: None,
            });
        entry.last_seen_unix_ms = now_unix_ms;
        if let Some(action) = action {
            entry.last_action = Some(action);
            entry.last_reason_code = None;
        }
    }

    pub(crate) fn record_closed(&mut self, session_id: &str, now_unix_ms: u64) -> bool {
        self.record_closed_with_reason(session_id, now_unix_ms, None)
    }

    /// Records a session as closed. Returns `true` when this call actually
    /// transitioned the entry to closed (it was open or unknown before), so
    /// callers journal exactly one `exited` event per session (#897).
    pub(crate) fn record_closed_with_reason(
        &mut self,
        session_id: &str,
        now_unix_ms: u64,
        reason_code: Option<&str>,
    ) -> bool {
        let entry = self
            .entries
            .entry(session_id.to_owned())
            .or_insert_with(|| SessionRegistryEntry {
                session_id: session_id.to_owned(),
                transport: "http".to_owned(),
                client_name: None,
                client_version: None,
                protocol_version: None,
                agent_kind: "unknown".to_owned(),
                started_at_unix_ms: now_unix_ms,
                last_seen_unix_ms: now_unix_ms,
                closed_at_unix_ms: None,
                last_action: None,
                last_reason_code: None,
                spawned_agent: None,
            });
        let transitioned = entry.closed_at_unix_ms.is_none();
        entry.last_seen_unix_ms = now_unix_ms;
        entry.closed_at_unix_ms = Some(now_unix_ms);
        entry.last_reason_code = reason_code.map(ToOwned::to_owned);
        transitioned
    }

    pub(crate) fn record_spawned_agent(
        &mut self,
        session_id: &str,
        spawned_agent: SpawnedAgentRead,
        now_unix_ms: u64,
    ) {
        let entry = self
            .entries
            .entry(session_id.to_owned())
            .or_insert_with(|| SessionRegistryEntry {
                session_id: session_id.to_owned(),
                transport: "http".to_owned(),
                client_name: None,
                client_version: None,
                protocol_version: None,
                agent_kind: spawned_agent.cli.clone(),
                started_at_unix_ms: now_unix_ms,
                last_seen_unix_ms: now_unix_ms,
                closed_at_unix_ms: None,
                last_action: None,
                last_reason_code: None,
                spawned_agent: None,
            });
        if entry.agent_kind == "unknown" {
            entry.agent_kind = spawned_agent.cli.clone();
        }
        entry.last_seen_unix_ms = entry.last_seen_unix_ms.max(now_unix_ms);
        entry.spawned_agent = Some(spawned_agent);
    }

    /// Refreshes liveness from real agent activity rows instead of MCP
    /// request recency. This only updates existing, non-closed entries:
    /// activity cannot fabricate a session row or resurrect a terminal
    /// lifecycle state.
    pub(crate) fn record_agent_activity(
        &mut self,
        session_id: Option<&str>,
        spawn_id: Option<&str>,
        now_unix_ms: u64,
    ) -> Vec<String> {
        let mut target_session_ids = BTreeSet::new();
        if let Some(session_id) = session_id.filter(|value| !value.is_empty())
            && self.entries.contains_key(session_id)
        {
            target_session_ids.insert(session_id.to_owned());
        }
        if let Some(spawn_id) = spawn_id.filter(|value| !value.is_empty()) {
            for (entry_session_id, entry) in &self.entries {
                if entry
                    .spawned_agent
                    .as_ref()
                    .is_some_and(|spawned| spawned.spawn_id == spawn_id)
                {
                    target_session_ids.insert(entry_session_id.clone());
                }
            }
        }

        let mut refreshed = Vec::new();
        for session_id in target_session_ids {
            let Some(entry) = self.entries.get_mut(&session_id) else {
                continue;
            };
            if entry.closed_at_unix_ms.is_some() {
                continue;
            }
            let previous = entry.last_seen_unix_ms;
            entry.last_seen_unix_ms = entry.last_seen_unix_ms.max(now_unix_ms);
            if entry.last_seen_unix_ms != previous {
                refreshed.push(session_id);
            }
        }
        refreshed
    }

    /// Returns the inferred agent kind for a live session (`"local-model"`,
    /// `"codex"`, `"claude"`, or `"unknown"`), or `None` if the session has no
    /// registry entry yet. Used by the MCP tool-profile resolver to grant the
    /// Synapse-spawned local-model harness the full-capability tool surface
    /// while leaving other agents on the least-privilege default (#1031).
    pub(crate) fn agent_kind_for(&self, session_id: &str) -> Option<String> {
        self.entries
            .get(session_id)
            .map(|entry| entry.agent_kind.clone())
    }

    pub(crate) fn reads(&self, now_unix_ms: u64) -> Vec<SessionRegistryRead> {
        self.entries
            .values()
            .map(|entry| self.entry_read(entry, now_unix_ms))
            .collect()
    }

    pub(crate) fn entry_read(
        &self,
        entry: &SessionRegistryEntry,
        now_unix_ms: u64,
    ) -> SessionRegistryRead {
        let last_seen_ms_ago = now_unix_ms.saturating_sub(entry.last_seen_unix_ms);
        let lifecycle = if entry.closed_at_unix_ms.is_some() {
            "closed"
        } else if last_seen_ms_ago > self.stale_after_ms {
            "stale"
        } else {
            "live"
        };
        SessionRegistryRead {
            session_id: entry.session_id.clone(),
            transport: entry.transport.clone(),
            client_name: entry.client_name.clone(),
            client_version: entry.client_version.clone(),
            protocol_version: entry.protocol_version.clone(),
            agent_kind: entry.agent_kind.clone(),
            lifecycle: lifecycle.to_owned(),
            started_at_unix_ms: entry.started_at_unix_ms,
            last_seen_unix_ms: entry.last_seen_unix_ms,
            last_seen_ms_ago,
            stale_after_ms: self.stale_after_ms,
            closed_at_unix_ms: entry.closed_at_unix_ms,
            last_action: entry.last_action.clone(),
            last_reason_code: entry.last_reason_code.clone(),
            spawned_agent: entry.spawned_agent.clone(),
        }
    }
}

pub(crate) fn unix_time_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_millis_u64)
        .unwrap_or_default()
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn infer_agent_kind(client_name: &str) -> String {
    let lower = client_name.to_ascii_lowercase();
    if lower.contains("synapse-local-model-agent") || lower.contains("local-model") {
        "local-model".to_owned()
    } else if lower.contains("codex") {
        "codex".to_owned()
    } else if lower.contains("claude") {
        "claude".to_owned()
    } else {
        "unknown".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};

    use super::*;

    fn state(name: &str) -> SessionState {
        SessionState::new(InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new(name, "0.0.0-test"),
        ))
    }

    #[test]
    fn registry_marks_live_stale_and_closed_from_heartbeats() {
        let mut registry = SessionRegistry::default();
        registry.set_stale_after(Some(Duration::from_millis(100)));
        registry.record_initialized("s1", &state("codex"), "http", 1_000);

        let live = registry.reads(1_050).remove(0);
        assert_eq!(live.lifecycle, "live");
        assert_eq!(live.agent_kind, "codex");

        let stale = registry.reads(1_200).remove(0);
        assert_eq!(stale.lifecycle, "stale");

        registry.record_closed("s1", 1_250);
        let closed = registry.reads(1_251).remove(0);
        assert_eq!(closed.lifecycle, "closed");
        assert_eq!(closed.closed_at_unix_ms, Some(1_250));
    }

    #[test]
    fn registry_records_spawned_agent_process_attribution() {
        let mut registry = SessionRegistry::default();
        registry.record_initialized("s1", &state("codex"), "http", 1_000);
        registry.record_spawned_agent(
            "s1",
            SpawnedAgentRead {
                spawn_id: "spawn-1".to_owned(),
                cli: "codex".to_owned(),
                launcher_process_id: 123,
                agent_process_id: Some(456),
                started_by_session_id: Some("parent".to_owned()),
                launched_at_unix_ms: 990,
                launch_target: "pwsh.exe".to_owned(),
                log_dir: "C:\\temp\\spawn-1".to_owned(),
                template_id: None,
                template_version: None,
                control: None,
            },
            1_050,
        );

        let read = registry.reads(1_060).remove(0);
        let spawned = read.spawned_agent.unwrap();
        assert_eq!(spawned.spawn_id, "spawn-1");
        assert_eq!(spawned.launcher_process_id, 123);
        assert_eq!(spawned.agent_process_id, Some(456));
    }

    #[test]
    fn registry_initialization_never_moves_heartbeat_backwards() {
        let mut registry = SessionRegistry::default();
        registry.record_seen("s1", Some("tools/list".to_owned()), 2_000);
        registry.record_initialized("s1", &state("codex"), "http", 1_000);

        let read = registry.reads(2_001).remove(0);
        assert_eq!(read.last_seen_unix_ms, 2_000);
        assert_eq!(read.last_action.as_deref(), Some("tools/list"));
    }

    #[test]
    fn registry_agent_activity_refreshes_existing_session_or_spawn() {
        let mut registry = SessionRegistry::default();
        registry.record_seen(
            "session-direct",
            Some("tools/call:get_target".to_owned()),
            1_000,
        );
        registry.record_spawned_agent(
            "session-spawn",
            SpawnedAgentRead {
                spawn_id: "agent-spawn-registry-activity".to_owned(),
                cli: "codex".to_owned(),
                launcher_process_id: 123,
                agent_process_id: Some(456),
                started_by_session_id: Some("parent".to_owned()),
                launched_at_unix_ms: 990,
                launch_target: "pwsh.exe".to_owned(),
                log_dir: "C:\\temp\\agent-spawn-registry-activity".to_owned(),
                template_id: None,
                template_version: None,
                control: None,
            },
            1_000,
        );

        let refreshed = registry.record_agent_activity(
            Some("session-direct"),
            Some("agent-spawn-registry-activity"),
            2_000,
        );
        assert_eq!(refreshed, vec!["session-direct", "session-spawn"]);

        let reads = registry.reads(2_001);
        let direct = reads
            .iter()
            .find(|read| read.session_id == "session-direct")
            .expect("direct session readback");
        assert_eq!(direct.last_seen_unix_ms, 2_000);
        assert_eq!(direct.last_action.as_deref(), Some("tools/call:get_target"));
        let spawned = reads
            .iter()
            .find(|read| read.session_id == "session-spawn")
            .expect("spawn session readback");
        assert_eq!(spawned.last_seen_unix_ms, 2_000);
    }

    #[test]
    fn registry_agent_activity_never_creates_or_reopens_sessions() {
        let mut registry = SessionRegistry::default();
        assert!(
            registry
                .record_agent_activity(Some("missing-session"), Some("agent-spawn-missing"), 1_000)
                .is_empty()
        );
        assert!(registry.reads(1_001).is_empty());

        registry.record_seen("closed-session", None, 2_000);
        registry.record_closed("closed-session", 2_100);
        assert!(
            registry
                .record_agent_activity(Some("closed-session"), None, 2_200)
                .is_empty()
        );

        let read = registry.reads(2_300).remove(0);
        assert_eq!(read.lifecycle, "closed");
        assert_eq!(read.last_seen_unix_ms, 2_100);
        assert_eq!(read.closed_at_unix_ms, Some(2_100));
    }
}
