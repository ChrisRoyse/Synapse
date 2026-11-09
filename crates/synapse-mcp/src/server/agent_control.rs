//! `agent_interrupt` / `agent_kill` (#904) — first-class stop verbs for
//! Synapse-spawned sub-agents.
//!
//! Until this module, the only way to stop a spawned agent was the
//! coarse-grained `session_end` teardown of the *caller's* whole session. These
//! two verbs target **one** agent by its own MCP session id (or `agent-spawn-*`
//! id) and stop it with source-of-truth readback in the response: the
//! actual OS process table is read back before and after, the registry/journal
//! state transition is recorded, and every channel reports its real outcome.
//!
//! # Channel ranking (research-grounded, #904)
//!
//! The issue asks for a *channel-ranked* graceful interrupt. Each channel
//! reports its true status; **no channel ever silently "succeeds"**:
//!
//! 1. `codex_app_server_turn_interrupt` — Codex `turn/interrupt` JSON-RPC
//!    (`{threadId,turnId}` → `{}`, turn ends `interrupted`). Wired for Codex
//!    agents spawned through the app-server runner, using the per-spawn
//!    `codex-control.json` artifact as the physical SoT for endpoint/thread/turn
//!    ids. Older plain-CLI Codex rows report `channel_not_wired`.
//! 2. `claude_stream_json_control` — there is **no supported stdin cancel frame**
//!    for `claude -p` today (anthropics/claude-code#51078 is an open feature
//!    request); the Agent SDK's `interruptTurn` only works when the SDK owns the
//!    persistent stream-json stdin pipe, which the daemon does not. **Not wired.**
//! 3. `mailbox_interrupt` — **wired.** A durable `interrupt` mailbox row (#908)
//!    delivered to the agent's steering inbox. Cooperative agents drain it
//!    between tool calls and bail (`steering_requests_shutdown` honors
//!    `kill|stop|cancel|interrupt|shutdown`). Delivery is proven by the
//!    persisted `CF_KV` row readback.
//! 4. `pty_esc` — the documented last-resort interrupt key requires owned-PTY
//!    capture (#902), which does not exist yet. **Not wired.**
//!
//! `agent_kill` reuses the authoritative per-session teardown machinery
//! (`session_lifecycle::teardown_session`): every spawned agent's *own* session
//! id owns both its process resource (the Windows job handle) and its
//! leases/claims/desktops, so a single teardown of that session does job-close →
//! force-kill of the process tree and releases all of the agent's resources.

use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::{AgentEndState, AgentEventKind, AgentEventRecord, error_codes};

use rmcp::{RoleServer, service::RequestContext};

use super::agent_events::{record_agent_event_durable, unix_time_ns_now};
use super::command_audit::CommandAuditInput;
use super::session_lifecycle::{SessionTeardownOptions, SessionTeardownReport};
use super::session_registry::{SpawnedAgentControlRead, unix_time_ms_now};
use super::{ErrorData, Json, Parameters, SynapseService, mcp_error, tool, tool_router};
use crate::safety::OperatorHotkeyImmediateReport;
use futures_util::future::join_all;

// ----------------------------------------------------------------------------
// Tunables
// ----------------------------------------------------------------------------

/// Default graceful window between the interrupt attempt and the force-kill.
const DEFAULT_KILL_GRACE_MS: u64 = 3_000;
/// Hard ceiling on the graceful window so a kill cannot block unbounded.
const MAX_KILL_GRACE_MS: u64 = 120_000;
/// Poll cadence while waiting for the process tree to exit during the grace
/// window.
const GRACE_POLL_INTERVAL_MS: u64 = 100;
/// TTL for the cooperative interrupt mailbox row — short, because a stale
/// interrupt request is noise once the agent is gone.
const INTERRUPT_MESSAGE_TTL_MS: u64 = 60_000;
/// Mailbox kind the cooperative shutdown contract recognizes.
const INTERRUPT_MAILBOX_KIND: &str = "interrupt";

const TOOL_AGENT_INTERRUPT: &str = "agent_interrupt";
const TOOL_AGENT_KILL: &str = "agent_kill";

#[cfg(windows)]
fn apply_hidden_helper_window_flags(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn apply_hidden_helper_window_flags(_command: &mut Command) {}
const TOOL_FLEET_STOP: &str = "fleet_stop";
const TOOL_AGENT_STEER: &str = "agent_steer";
const TOOL_AGENT_PAUSE: &str = "agent_pause";
const TOOL_AGENT_RESUME: &str = "agent_resume";
const TOOL_AGENT_RESPAWN: &str = "agent_respawn";

/// TTL for a cooperative steering instruction. Long enough that an agent busy
/// in a multi-minute turn still sees it when it next drains its inbox, short
/// enough that a stale instruction does not haunt a later turn.
const STEER_MESSAGE_TTL_MS: u64 = 600_000;
/// Hard ceiling on a single steering instruction. The mailbox row caps the
/// whole payload at 64 KiB; this bounds the human-authored text well under that
/// so the envelope (control fields, from, reason) always fits.
const MAX_STEER_INSTRUCTION_CHARS: usize = 16_000;
const CODEX_APP_SERVER_INTERRUPT_SCRIPT: &str = include_str!("codex_app_server_interrupt.ps1");
const CODEX_INTERRUPT_HELPER_TIMEOUT_MS: u64 = 8_000;
const CODEX_APP_SERVER_STEER_SCRIPT: &str = include_str!("codex_app_server_steer.ps1");
const CODEX_STEER_HELPER_TIMEOUT_MS: u64 = 8_000;
const OPERATOR_PANIC_SPAWN_QUIESCENCE_TIMEOUT: Duration = Duration::from_millis(750);
const OPERATOR_PANIC_SPAWN_QUIESCENCE_POLL: Duration = Duration::from_millis(25);
const OPERATOR_PANIC_FLEET_STOP_MAX_STABLE_ROUNDS: u32 = 3;

/// Destructive-action confirmation token for `fleet_stop`, matching the
/// action-diagnostic confirm pattern. A typo or empty value is refused.
const FLEET_STOP_CONFIRM: &str = "STOP-FLEET";

// ----------------------------------------------------------------------------
// Params
// ----------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentInterruptParams {
    /// The agent to interrupt: its own MCP session id, or its `agent-spawn-*`
    /// id. Resolves through the live session registry.
    pub session_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentKillParams {
    /// The agent to kill: its own MCP session id, or its `agent-spawn-*` id.
    pub session_id: String,
    /// Graceful window (ms) to wait for the agent to stop on its own after the
    /// interrupt attempt, before force-terminating the process tree.
    #[serde(default = "default_kill_grace_ms")]
    #[schemars(default = "default_kill_grace_ms", range(min = 0, max = 120_000))]
    pub grace_ms: u64,
    /// When true (default) a graceful interrupt is attempted first; when false
    /// the process tree is force-terminated immediately.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub interrupt_first: bool,
}

const fn default_kill_grace_ms() -> u64 {
    DEFAULT_KILL_GRACE_MS
}
const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FleetStopParams {
    /// `kill` force-terminates every live agent's process tree; `interrupt`
    /// delivers a graceful interrupt to each.
    pub mode: String,
    /// Destructive-action confirmation token; must equal `STOP-FLEET`.
    pub confirm: String,
    /// Optional registry `agent_kind` filter (e.g. `["codex"]`). Empty = every
    /// live spawned agent.
    #[serde(default)]
    #[schemars(default)]
    pub agent_kinds: Vec<String>,
    /// Graceful window (ms) per agent for `mode=kill` before force-termination.
    #[serde(default = "default_kill_grace_ms")]
    #[schemars(default = "default_kill_grace_ms", range(min = 0, max = 120_000))]
    pub grace_ms: u64,
}

// ----------------------------------------------------------------------------
// Response types
// ----------------------------------------------------------------------------

/// A live readback of the agent's process tree from the OS process table — the
/// source of truth for "is it actually dead".
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessReadback {
    pub launcher_process_id: u32,
    /// Every pid the owned job/process tree currently maps to.
    pub process_tree_ids: Vec<u32>,
    /// The subset of `process_tree_ids` that are still alive right now.
    pub live_process_ids: Vec<u32>,
}

/// One ranked channel's real outcome. `status` is one of `delivered`,
/// `unavailable`, `failed`, or `skipped` — never a fabricated success.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelAttempt {
    pub channel: String,
    pub rank: u32,
    pub status: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_key: Option<String>,
}

/// Physical readback of a persisted journal row.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JournalReadback {
    pub kind: String,
    pub ts_ns: u64,
    pub seq: u32,
    pub value_len_bytes: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentInterruptResponse {
    pub requested_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    pub agent_kind: String,
    pub lifecycle: String,
    pub resolution_source: String,
    /// True when at least one channel actually delivered the interrupt.
    pub delivered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_via: Option<String>,
    /// Every ranked channel and its true outcome.
    pub channels: Vec<ChannelAttempt>,
    /// The `interrupted` journal row written when a channel delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_event: Option<JournalReadback>,
    /// The OS process table at interrupt time (an interrupt never kills, so it
    /// is read once for evidence).
    pub process: ProcessReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentKillResponse {
    pub requested_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    pub agent_kind: String,
    pub resolution_source: String,
    /// True when the process tree was already gone before this call acted —
    /// makes double-kill idempotent (the second call reports `already_dead`).
    pub already_dead: bool,
    /// The graceful interrupt attempt, when `interrupt_first` was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupt: Option<AgentInterruptResponse>,
    pub grace_ms: u64,
    /// True when the agent exited on its own during the grace window, with no
    /// force-termination needed.
    pub natural_exit: bool,
    pub process_before: ProcessReadback,
    pub process_after: ProcessReadback,
    /// Live pids still standing after teardown. MUST be empty for `killed`.
    pub orphan_process_ids: Vec<u32>,
    /// True iff zero orphan processes remain (the OS process table is the SoT).
    pub killed: bool,
    /// Exact-PID fallback used when no live teardown job handle exists, such as
    /// after daemon restart handoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_teardown_force_termination: Option<crate::m4::OwnedProcessTerminationReadback>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_artifact_cleanup_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_artifact_cleanup_error: Option<String>,
    /// The `killed` journal row, written before teardown when a force-kill was
    /// actually required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_killed_event: Option<JournalReadback>,
    /// Full per-resource teardown report (process job close/force, lease, claim,
    /// desktop, registry transitions). Present even when cleanup sections fail,
    /// so callers can inspect the exact resource/channel readback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teardown: Option<SessionTeardownReport>,
    /// Structured summary of failed teardown sections. The full `teardown`
    /// report remains the source of truth for exact resource readbacks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teardown_failure_summary: Option<AgentTeardownFailureSummary>,
    /// Set when teardown cleanup failed; the kill's success is still judged by
    /// `orphan_process_ids` (the OS process table), never by this alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teardown_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTeardownFailureSummary {
    pub session_id: String,
    pub failure_count: u32,
    pub failed_sections: Vec<AgentTeardownFailedSection>,
    pub next_action: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTeardownFailedSection {
    pub section: String,
    pub detail: String,
}

/// One agent's outcome in a `fleet_stop` sweep.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FleetStopAgentOutcome {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    pub agent_kind: String,
    /// True when the agent was stopped as requested (killed with zero orphans,
    /// or interrupt delivered).
    pub ok: bool,
    /// Outcome detail: how it was stopped, or exactly why it could not be.
    pub reason: String,
    /// Live pids still standing for this agent (non-empty only on a failed kill).
    pub surviving_process_ids: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FleetStopResponse {
    pub mode: String,
    /// Live spawned agents that matched the filter at sweep time.
    pub matched: usize,
    pub succeeded: usize,
    pub failed: usize,
    /// True iff every matched agent was stopped (vacuously true for an empty
    /// fleet).
    pub all_stopped: bool,
    pub agents: Vec<FleetStopAgentOutcome>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorPanicKillAllResponse {
    pub immediate: OperatorHotkeyImmediateReport,
    pub chrome_extension_mutation_owners: OperatorPanicChromeExtensionOwnersReadback,
    pub durable_browser_mutation_owners_drain:
        synapse_a11y::CdpDurableBrowserMutationOwnersDrainReadback,
    pub durable_browser_mutation_owners_after:
        synapse_a11y::CdpDurableBrowserMutationOwnersReadback,
    pub durable_browser_mutation_owners_terminal: bool,
    pub chrome_mutation_command_owners_drain:
        crate::chrome_debugger_bridge::ChromeDebuggerMutationOwnersDrainReadback,
    pub chrome_mutation_command_owners_after:
        crate::chrome_debugger_bridge::ChromeDebuggerMutationOwnersReadback,
    pub chrome_mutation_command_owners_terminal: bool,
    pub prior_lease_owner_session_id: Option<String>,
    pub prior_lease_row_cleanup: Option<super::session_continuity::LeaseContinuityCleanupReadback>,
    pub prior_lease_row_cleanup_error: Option<String>,
    pub matched_sessions_before: Vec<String>,
    pub matched_sessions_before_error: Option<String>,
    pub fleet_stop: Option<FleetStopResponse>,
    pub fleet_stop_error: Option<String>,
    pub operator_lease_cleared: Option<synapse_action::LeaseStatus>,
    pub lease_after: synapse_action::LeaseStatus,
    pub operator_panic_safety_after: synapse_action::OperatorPanicSafetyReadback,
    pub live_sessions_after: Vec<String>,
    pub live_sessions_after_error: Option<String>,
    pub spawn_activity_before: super::m4_tools::AgentSpawnActivityReadback,
    pub spawn_activity_after: super::m4_tools::AgentSpawnActivityReadback,
    pub spawn_activity_stable: bool,
    pub mcp_mutation_activity: OperatorPanicMutationActivityReadback,
    pub final_safety_sweep: Option<OperatorPanicFinalSafetySweepReadback>,
    pub final_safety_sweep_error: Option<String>,
    pub fleet_stop_rounds: u32,
    pub all_stopped: bool,
    pub audit_intent_error: Option<String>,
    pub audit_final_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorPanicChromeExtensionOwnersReadback {
    pub disable: Option<crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback>,
    pub disable_error: Option<String>,
    pub cleanup: Option<crate::chrome_debugger_bridge::ChromeDebuggerExtensionCleanupReadback>,
    pub cleanup_error: Option<String>,
    pub cdp_target_owner_reconciliation:
        Option<super::m1_tools::OperatorPanicCdpTargetOwnerReconciliationReadback>,
    pub cdp_target_owner_reconciliation_error: Option<String>,
    pub before_command_drain:
        Option<crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback>,
    pub before_command_drain_error: Option<String>,
    pub after_command_drain:
        Option<crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback>,
    pub after_command_drain_error: Option<String>,
    pub terminal: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorPanicMutationActivityReadback {
    pub initial: Option<super::operator_panic_boundary::McpMutationActivitySnapshot>,
    pub stable_before: Option<super::operator_panic_boundary::McpMutationActivitySnapshot>,
    pub after: Option<super::operator_panic_boundary::McpMutationActivitySnapshot>,
    pub quiescent: bool,
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorPanicEmitterStateReadback {
    pub held_keys: usize,
    pub held_key_bits: usize,
    pub held_key_timer_keys: usize,
    pub held_key_timer_count: usize,
    pub held_buttons: usize,
    pub held_button_bits: usize,
    pub held_pads: usize,
    pub held_keys_by_backend_entries: usize,
    pub held_keys_by_backend_values: usize,
    pub held_buttons_by_backend_entries: usize,
    pub held_buttons_by_backend_values: usize,
    pub terminal: bool,
}

impl OperatorPanicEmitterStateReadback {
    fn from_snapshot(snapshot: &synapse_action::ActionStateSnapshot) -> Self {
        let mut readback = Self {
            held_keys: snapshot.held_keys.len(),
            held_key_bits: snapshot.held_key_bits.len(),
            held_key_timer_keys: snapshot.held_key_timer_keys.len(),
            held_key_timer_count: snapshot.held_key_timer_count,
            held_buttons: snapshot.held_buttons.len(),
            held_button_bits: snapshot.held_button_bits.len(),
            held_pads: snapshot.pad_state.len(),
            held_keys_by_backend_entries: snapshot.held_keys_by_backend.len(),
            held_keys_by_backend_values: snapshot.held_keys_by_backend.values().map(Vec::len).sum(),
            held_buttons_by_backend_entries: snapshot.held_buttons_by_backend.len(),
            held_buttons_by_backend_values: snapshot
                .held_buttons_by_backend
                .values()
                .map(Vec::len)
                .sum(),
            terminal: false,
        };
        readback.terminal = readback.held_keys == 0
            && readback.held_key_bits == 0
            && readback.held_key_timer_keys == 0
            && readback.held_key_timer_count == 0
            && readback.held_buttons == 0
            && readback.held_button_bits == 0
            && readback.held_pads == 0
            && readback.held_keys_by_backend_entries == 0
            && readback.held_keys_by_backend_values == 0
            && readback.held_buttons_by_backend_entries == 0
            && readback.held_buttons_by_backend_values == 0;
        readback
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorPanicFinalSafetySweepReadback {
    pub release_all: crate::safety::ReleaseAllReport,
    pub reflex_disable: crate::safety::DisableReport,
    pub reflex_active_count_after: Option<usize>,
    pub reflex_readback_error: Option<String>,
    pub emitter_after: Option<OperatorPanicEmitterStateReadback>,
    pub emitter_readback_error: Option<String>,
    pub lease_after: synapse_action::LeaseStatus,
    pub operator_lease_generation_after: Option<u64>,
    pub terminal: bool,
}

fn operator_panic_spawn_activity_stable(
    baseline: &super::m4_tools::AgentSpawnActivityReadback,
    after: &super::m4_tools::AgentSpawnActivityReadback,
    quiescent_readback_stable: bool,
) -> bool {
    quiescent_readback_stable
        && baseline.sequence == after.sequence
        && after.in_flight == 0
        && !after.cleanup_incident
}

async fn wait_for_operator_panic_spawn_quiescence()
-> (super::m4_tools::AgentSpawnActivityReadback, bool) {
    let deadline = Instant::now() + OPERATOR_PANIC_SPAWN_QUIESCENCE_TIMEOUT;
    loop {
        let first = super::m4_tools::agent_spawn_activity_readback();
        if first.in_flight == 0 {
            tokio::time::sleep(OPERATOR_PANIC_SPAWN_QUIESCENCE_POLL).await;
            let second = super::m4_tools::agent_spawn_activity_readback();
            if second.in_flight == 0 && second.sequence == first.sequence {
                return (second, true);
            }
        } else {
            tokio::time::sleep(OPERATOR_PANIC_SPAWN_QUIESCENCE_POLL).await;
        }
        if Instant::now() >= deadline {
            return (super::m4_tools::agent_spawn_activity_readback(), false);
        }
    }
}

fn operator_panic_mcp_mutation_activity_stable(
    before: &super::operator_panic_boundary::McpMutationActivitySnapshot,
    after: &super::operator_panic_boundary::McpMutationActivitySnapshot,
) -> bool {
    before.in_flight == 0 && after.in_flight == 0 && before.sequence == after.sequence
}

async fn wait_for_operator_panic_mcp_mutation_quiescence(
    initial: Result<super::operator_panic_boundary::McpMutationActivitySnapshot, String>,
) -> OperatorPanicMutationActivityReadback {
    let started = Instant::now();
    let deadline = started + crate::safety::OPERATOR_PANIC_K2_STOP_TIMEOUT;
    let initial = match initial {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return OperatorPanicMutationActivityReadback {
                initial: None,
                stable_before: None,
                after: None,
                quiescent: false,
                error: Some(error),
                elapsed_ms: started.elapsed().as_millis(),
            };
        }
    };
    let mut first = initial;
    loop {
        if Instant::now() >= deadline {
            return OperatorPanicMutationActivityReadback {
                initial: Some(initial),
                stable_before: Some(first),
                after: Some(first),
                quiescent: false,
                error: Some(format!(
                    "MCP mutation activity did not reach stable zero within {} ms",
                    crate::safety::OPERATOR_PANIC_K2_STOP_TIMEOUT.as_millis()
                )),
                elapsed_ms: started.elapsed().as_millis(),
            };
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(OPERATOR_PANIC_SPAWN_QUIESCENCE_POLL.min(remaining)).await;
        let second = match super::operator_panic_boundary::mcp_mutation_activity_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return OperatorPanicMutationActivityReadback {
                    initial: Some(initial),
                    stable_before: Some(first),
                    after: None,
                    quiescent: false,
                    error: Some(error),
                    elapsed_ms: started.elapsed().as_millis(),
                };
            }
        };
        if operator_panic_mcp_mutation_activity_stable(&first, &second) {
            return OperatorPanicMutationActivityReadback {
                initial: Some(initial),
                stable_before: Some(first),
                after: Some(second),
                quiescent: true,
                error: None,
                elapsed_ms: started.elapsed().as_millis(),
            };
        }
        first = second;
    }
}

async fn operator_panic_final_safety_sweep(
    service: &SynapseService,
) -> OperatorPanicFinalSafetySweepReadback {
    let context = service.m2_release_all_context();
    // With request-owned mutation reservations at stable zero, no admitted MCP
    // request can register a new combo after this point. Disable first, then
    // drain the emitter so a final reflex tick cannot reassert held input after
    // the final ReleaseAll.
    let reflex_disable = crate::safety::disable_reflexes(&service.m3_state_handle());
    let (release_all, snapshot_handle, context_error) = match context {
        Ok((handle, snapshot_handle, _reflex_runtime)) => (
            crate::safety::fire_release_all_with_handle_timeout(
                &handle,
                crate::safety::OPERATOR_PANIC_K2_STOP_TIMEOUT,
            ),
            Some(snapshot_handle),
            None,
        ),
        Err(error) => {
            let detail = error.message.to_string();
            (
                crate::safety::ReleaseAllReport {
                    result: "error",
                    error_code: Some(error_codes::TOOL_INTERNAL_ERROR),
                    detail: Some(detail.clone()),
                },
                None,
                Some(detail),
            )
        }
    };
    let (reflex_active_count_after, reflex_readback_error) =
        match crate::safety::operator_panic_reflex_active_count_readback(&service.m3_state_handle())
        {
            Ok(count) => (count, None),
            Err(error) => (None, Some(error)),
        };
    let (emitter_after, emitter_readback_error) = match snapshot_handle {
        Some(snapshot_handle) => match tokio::time::timeout(
            crate::safety::OPERATOR_PANIC_K2_STOP_TIMEOUT,
            snapshot_handle.snapshot(),
        )
        .await
        {
            Ok(Ok(snapshot)) => (
                Some(OperatorPanicEmitterStateReadback::from_snapshot(&snapshot)),
                None,
            ),
            Ok(Err(error)) => (None, Some(error.to_string())),
            Err(_elapsed) => (
                None,
                Some(format!(
                    "action emitter readback timed out after {} ms",
                    crate::safety::OPERATOR_PANIC_K2_STOP_TIMEOUT.as_millis()
                )),
            ),
        },
        None => (None, context_error),
    };
    let lease_safety_after = synapse_action::input_lease_safety_snapshot();
    let emitter_terminal = emitter_readback_error.is_none()
        && emitter_after
            .as_ref()
            .is_some_and(|readback| readback.terminal);
    let release_all_terminal =
        release_all.result == "ok" || (release_all.error_code.is_some() && emitter_terminal);
    let terminal = release_all_terminal
        && matches!(reflex_disable.result, "ok" | "not_initialized")
        && reflex_readback_error.is_none()
        && reflex_active_count_after.unwrap_or(0) == 0
        && emitter_terminal;
    OperatorPanicFinalSafetySweepReadback {
        release_all,
        reflex_disable,
        reflex_active_count_after,
        reflex_readback_error,
        emitter_after,
        emitter_readback_error,
        lease_after: lease_safety_after.status,
        operator_lease_generation_after: lease_safety_after.operator_panic_generation,
        terminal,
    }
}

fn chrome_debugger_bridge_error_detail(
    error: &crate::chrome_debugger_bridge::ChromeDebuggerBridgeError,
) -> String {
    format!("{}: {}", error.code(), error.detail())
}

fn operator_panic_chrome_extension_owner_terminal(
    readback: &crate::chrome_debugger_bridge::ChromeDebuggerExtensionOwnerReadback,
    expected_disable_sequence: u64,
) -> bool {
    !readback.enabled
        && readback.disable_sequence == expected_disable_sequence
        && readback.command_in_flight_count == 1
        && readback.command_activity_sequence
            == readback.command_last_completed_sequence.saturating_add(1)
        && readback.mutation_handlers_started_count == readback.mutation_handlers_completed_count
        && !readback.worker_boot_id.is_empty()
        && readback
            .browser_session_id
            .as_deref()
            .is_some_and(|session_id| !session_id.is_empty())
        && !readback.ledger_browser_session_id.is_empty()
        && readback.browser_session_id.as_deref()
            == Some(readback.ledger_browser_session_id.as_str())
        && readback.browser_session_continuity_matched
        && readback.stale_browser_session_owner_count == 0
        && readback.storage_state_loaded
        && readback.storage_state_load_error.is_none()
        && readback.persisted_state_revision > 0
        && readback.persisted_in_flight_mutation.is_none()
        && readback.unresolved_debugger_command_timeouts.is_empty()
        && readback.unresolved_worker_restart_mutation_count == 0
        && readback.owner_continuity_healthy
        && readback.active_after.is_empty()
        && readback.fully_drained
}

fn operator_panic_chrome_extension_terminal(
    readback: &OperatorPanicChromeExtensionOwnersReadback,
) -> bool {
    if readback.disable_error.is_some()
        || readback.cleanup_error.is_some()
        || readback.cdp_target_owner_reconciliation_error.is_some()
        || readback.before_command_drain_error.is_some()
        || readback.after_command_drain_error.is_some()
    {
        return false;
    }
    let Some(disable) = readback.disable.as_ref() else {
        return false;
    };
    let expected_disable_sequence = disable.disable_sequence;
    if disable.enabled
        || expected_disable_sequence == 0
        || disable.worker_boot_id.is_empty()
        || disable
            .browser_session_id
            .as_deref()
            .is_none_or(str::is_empty)
        || disable.ledger_browser_session_id.is_empty()
        || disable.browser_session_id.as_deref() != Some(disable.ledger_browser_session_id.as_str())
        || !disable.browser_session_continuity_matched
        || disable.stale_browser_session_owner_count != 0
        || !disable.storage_state_loaded
        || disable.storage_state_load_error.is_some()
        || disable.persisted_state_revision == 0
        || disable.unresolved_worker_restart_mutation_count != 0
        || !disable.owner_continuity_healthy
    {
        return false;
    }
    let Some(cleanup) = readback.cleanup.as_ref() else {
        return false;
    };
    let Some(owner_reconciliation) = readback.cdp_target_owner_reconciliation.as_ref() else {
        return false;
    };
    if cleanup.expected_disable_sequence != expected_disable_sequence
        || !cleanup.failures.is_empty()
        || !owner_reconciliation.terminal
        || cleanup.opened_tabs_found != cleanup.opened_tabs_closed
        || cleanup.opened_tabs_closed != cleanup.closed_opened_tabs.len()
        || !cleanup.remaining_opened_tabs.is_empty()
        || owner_reconciliation.successful_physical_closes != cleanup.closed_opened_tabs.len()
        || !operator_panic_chrome_extension_owner_terminal(
            &cleanup.owner,
            expected_disable_sequence,
        )
        || cleanup.init_scripts_found != cleanup.init_scripts_removed
        || cleanup.init_script_effects_found != cleanup.init_script_effects_cleared
        || cleanup.bindings_found != cleanup.bindings_removed
        || cleanup.debugger_tabs_found != cleanup.debugger_tabs_detached
        || cleanup.debugger_tabs_detached != cleanup.detached_debugger_tabs.len()
        || cleanup.dialog_policies_found != cleanup.dialog_policies_disabled
        || cleanup.file_chooser_interceptions_found != cleanup.file_chooser_interceptions_disabled
        || cleanup.clocks_found != cleanup.clocks_uninstalled
    {
        return false;
    }
    let (Some(before), Some(after)) = (
        readback.before_command_drain.as_ref(),
        readback.after_command_drain.as_ref(),
    ) else {
        return false;
    };
    operator_panic_chrome_extension_owner_terminal(before, expected_disable_sequence)
        && operator_panic_chrome_extension_owner_terminal(after, expected_disable_sequence)
        && before.mutation_handlers_started_count == after.mutation_handlers_started_count
        && before.mutation_handlers_completed_count == after.mutation_handlers_completed_count
        && before.worker_boot_id == after.worker_boot_id
        && before.browser_session_id == after.browser_session_id
        && before.ledger_browser_session_id == after.ledger_browser_session_id
        && before.persisted_state_revision == after.persisted_state_revision
        && after.command_activity_sequence == before.command_activity_sequence.saturating_add(1)
        && after.command_last_completed_sequence == before.command_activity_sequence
}

fn operator_panic_k2_lease_retained(
    immediate_generation: u64,
    current_epoch: u64,
    tagged_generation: Option<u64>,
    lease: &synapse_action::LeaseStatus,
) -> bool {
    lease.held
        && lease.owner_session_id.as_deref()
            == Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID)
        && tagged_generation.is_some_and(|generation| {
            generation == immediate_generation
                || (generation > immediate_generation && generation <= current_epoch)
        })
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSteerParams {
    /// The agent to steer: its own MCP session id, or its `agent-spawn-*` id.
    pub session_id: String,
    /// The instruction to inject. Codex app-server agents receive it through
    /// `turn/steer` guarded by the active `expectedTurnId`; other agents fall
    /// back to the cooperative steering-inbox contract. Non-empty, bounded to
    /// 16 000 chars.
    pub instruction: String,
    /// When true (default), mailbox fallback requests a read receipt: when the
    /// agent drains and applies the instruction it writes a receipt to the
    /// caller's receipt box (readable via `agent_receipts`), turning mailbox
    /// delivery into provable consumption. Codex app-server steering proves
    /// physical delivery by the `turn/steer` response instead.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub request_receipt: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSteerResponse {
    pub requested_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    pub agent_kind: String,
    pub lifecycle: String,
    pub resolution_source: String,
    /// True when at least one channel actually delivered the instruction.
    pub delivered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_via: Option<String>,
    /// Character count of the instruction that was delivered. The durable
    /// `MessageSent` journal row records what was injected; mailbox fallback
    /// also persists the exact payload in the row referenced by `row_key`.
    pub instruction_chars: usize,
    /// Every ranked channel and its true outcome.
    pub channels: Vec<ChannelAttempt>,
    /// The `MessageSent` journal row written when a channel delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_event: Option<JournalReadback>,
    /// Where a mailbox consumption receipt will appear once the agent applies
    /// the instruction, when mailbox fallback delivered and `request_receipt`
    /// was set. Poll `agent_receipts` for proof of mailbox consumption
    /// (delivery is already proven by `delivered`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_box_session_id: Option<String>,
    /// The OS process table at steer time (steering never kills; read once for
    /// evidence the target is live).
    pub process: ProcessReadback,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPauseParams {
    /// The agent to pause/resume: its own MCP session id, or `agent-spawn-*` id.
    pub session_id: String,
}

/// Shared response for `agent_pause` and `agent_resume`. Both freeze/thaw the
/// agent's whole owned process tree and prove the outcome by reading the OS
/// thread table back, so the response shape is identical.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSuspendResponse {
    pub requested_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    pub agent_kind: String,
    pub lifecycle: String,
    pub resolution_source: String,
    /// `pause` or `resume`.
    pub operation: String,
    /// Whole tree fully suspended BEFORE this call (physical thread-table read).
    pub was_suspended_before: bool,
    /// Whole tree fully suspended AFTER this call (physical thread-table read).
    pub is_suspended_after: bool,
    /// True when the call changed nothing because the tree was already in the
    /// requested state — pause/resume are idempotent and never stack suspends.
    pub no_op: bool,
    /// True when the call reached the requested terminal state (paused for
    /// pause, running for resume), judged by the thread-table readback.
    pub ok: bool,
    /// Physical suspend/resume readback (per-pid thread suspend counts after).
    pub suspend: crate::m4::OwnedProcessSuspendReadback,
    /// The `StateChanged` journal row, written only when the state changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_event: Option<JournalReadback>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRespawnParams {
    /// The agent to respawn: its own MCP session id, or `agent-spawn-*` id. May
    /// be live (it is killed first) or already dead (it is simply re-launched).
    pub session_id: String,
    /// The work prompt for the new instance. Required: the original prompt is
    /// not persisted (only its size is journaled), so respawn never fabricates
    /// it — supply the continued task. The new agent's kind/model/model_ref and
    /// manifest-recorded working_dir are reused from the prior spawn.
    pub prompt: String,
    /// When true (default), a continuity packet naming the prior spawn/session
    /// and, if present, its final message is prepended to the prompt so the new
    /// instance knows it is resuming.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub carry_context: bool,
    /// Graceful window (ms) for killing the prior instance when it is still live.
    #[serde(default = "default_kill_grace_ms")]
    #[schemars(default = "default_kill_grace_ms", range(min = 0, max = 120_000))]
    pub grace_ms: u64,
}

/// What was reused from the prior spawn to build the respawn.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RespawnManifest {
    pub agent_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Source of the reused manifest fields, e.g. `spawn-manifest.json` /
    /// `spawn_requested_journal`.
    pub source: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRespawnResponse {
    pub requested_id: String,
    /// The prior instance's MCP session id.
    pub prior_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_spawn_id: Option<String>,
    /// True when the prior instance was live and this call killed it first.
    pub prior_killed: bool,
    /// True when the prior instance was already dead (no kill needed).
    pub prior_already_dead: bool,
    /// What the respawn reused from the prior spawn.
    pub manifest: RespawnManifest,
    /// True when a continuity packet was prepended to the new prompt.
    pub carried_context: bool,
    /// Character count of the final prompt the new instance was spawned with.
    pub effective_prompt_chars: usize,
    /// The new instance's MCP session id.
    pub new_session_id: String,
    /// The new instance's `agent-spawn-*` id.
    pub new_spawn_id: String,
    /// The `StateChanged` lineage row written on the prior agent (respawned_into).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineage_journal_event: Option<JournalReadback>,
}

// ----------------------------------------------------------------------------
// Resolved-target model
// ----------------------------------------------------------------------------

/// The side-effect-free plan for an `agent_respawn`: everything reconstructed
/// from the prior agent's persisted state before any kill or launch happens.
#[derive(Clone, Debug)]
struct RespawnPlan {
    lookup: String,
    target: ResolvedAgent,
    manifest: RespawnManifest,
    effective_prompt: String,
    carried_context: bool,
    request_value: Value,
}

/// A spawned agent located in the live session registry.
#[derive(Clone, Debug)]
struct ResolvedAgent {
    /// The agent's own MCP session id (owns the process resource and leases).
    session_id: String,
    spawn_id: Option<String>,
    agent_kind: String,
    lifecycle: String,
    resolution_source: String,
    dead: bool,
    launcher_process_id: u32,
    agent_process_id: Option<u32>,
    log_dir: String,
    control: Option<SpawnedAgentControlRead>,
}

// ----------------------------------------------------------------------------
// Tools
// ----------------------------------------------------------------------------

#[tool_router(router = agent_control_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Gracefully interrupt one running spawned agent (#904/#958) by its MCP session id or agent-spawn-* id, via ranked clean channels. Codex app-server spawns use real turn/interrupt from codex-control.json; cooperative mailbox remains available; claude stream-json cancel and PTY ESC are reported unavailable unless their real channel exists. Reports each channel's real outcome plus a process-table readback; errors if no channel can deliver. Use agent_kill to force-terminate."
    )]
    pub async fn agent_interrupt(
        &self,
        params: Parameters<AgentInterruptParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentInterruptResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_interrupt",
            "tool.invocation kind=agent_interrupt"
        );
        let caller = super::context::mcp_session_id_from_request_context(&request_context)?;
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            service.agent_interrupt_impl(params.0, caller.as_deref())
        })
        .await
        .map_err(|error| blocking_worker_join_error(TOOL_AGENT_INTERRUPT, error))?
        .map(Json)
    }

    #[tool(
        description = "Force-stop one spawned agent (#904): attempt a graceful interrupt, wait grace_ms, then terminate the recorded process tree (Windows job-close → force kill) by reusing per-session teardown, releasing the agent's leases/claims/desktops and journaling a durable killed event. Source-of-truth readback is in the response: the OS process table is read back before and after, and killed is true only when zero orphan processes remain. Double-kill is idempotent (reports already_dead); unknown/non-spawned sessions error."
    )]
    pub async fn agent_kill(
        &self,
        params: Parameters<AgentKillParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentKillResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_kill",
            "tool.invocation kind=agent_kill"
        );
        let caller = super::context::mcp_session_id_from_request_context(&request_context)?;
        self.agent_kill_impl(params.0, caller.as_deref())
            .await
            .map(Json)
    }

    #[tool(
        description = "Fleet kill switch (#907): interrupt or kill EVERY live spawned agent (optionally filtered by agent_kind) in one call. Requires confirm=\"STOP-FLEET\" (destructive-action token). mode=kill force-terminates each agent's process tree and releases its leases/claims/desktops; mode=interrupt delivers a graceful interrupt to each. Returns a per-agent outcome table; any agent that could not be stopped is listed loudly with its reason and surviving pids (never summarized away). Empty fleet is an honest no-op. Writes a single fleet_stop command-audit pair."
    )]
    pub async fn fleet_stop(
        &self,
        params: Parameters<FleetStopParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<FleetStopResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "fleet_stop",
            "tool.invocation kind=fleet_stop"
        );
        let caller = super::context::mcp_session_id_from_request_context(&request_context)?;
        self.fleet_stop_impl(params.0, caller.as_deref())
            .await
            .map(Json)
    }

    #[tool(
        description = "Inject an instruction into ONE running spawned agent (#905/#1294) by its MCP session id or agent-spawn-* id, via ranked clean channels. Codex app-server spawns use real turn/steer from codex-control.json with expectedTurnId as the active-turn precondition; cooperative mailbox remains the lower-ranked durable fallback and can request agent_receipts consumption proof. Claude stream-json stdin inject and PTY stdin are reported unavailable unless their real owned channel exists. Reports each channel's real outcome plus a process-table readback; errors if no channel can deliver."
    )]
    pub async fn agent_steer(
        &self,
        params: Parameters<AgentSteerParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentSteerResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_steer",
            "tool.invocation kind=agent_steer"
        );
        let caller = super::context::mcp_session_id_from_request_context(&request_context)?;
        let service = self.clone();
        tokio::task::spawn_blocking(move || service.agent_steer_impl(params.0, caller.as_deref()))
            .await
            .map_err(|error| blocking_worker_join_error(TOOL_AGENT_STEER, error))?
            .map(Json)
    }

    #[tool(
        description = "Pause (freeze) ONE running spawned agent (#906) by its MCP session id or agent-spawn-* id. Suspends every process in the agent's owned tree with ntdll NtSuspendProcess (race-free, the PsSuspend/py-spy approach), then reads the OS thread table back to PROVE every thread is suspended. Idempotent: an already-paused tree is a no-op (never stacks suspend counts). Resume with agent_resume. Errors if the tree cannot be fully suspended."
    )]
    pub async fn agent_pause(
        &self,
        params: Parameters<AgentPauseParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentSuspendResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_pause",
            "tool.invocation kind=agent_pause"
        );
        let caller = super::context::mcp_session_id_from_request_context(&request_context)?;
        self.agent_pause_impl(params.0, caller.as_deref()).map(Json)
    }

    #[tool(
        description = "Resume (thaw) ONE paused spawned agent (#906) by its MCP session id or agent-spawn-* id. Resumes every process in the agent's owned tree with ntdll NtResumeProcess, then reads the OS thread table back to PROVE every thread is running again. Idempotent: an already-running tree is a no-op. Errors if the tree cannot be fully resumed."
    )]
    pub async fn agent_resume(
        &self,
        params: Parameters<AgentPauseParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentSuspendResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_resume",
            "tool.invocation kind=agent_resume"
        );
        let caller = super::context::mcp_session_id_from_request_context(&request_context)?;
        self.agent_resume_impl(params.0, caller.as_deref())
            .map(Json)
    }

    #[tool(
        description = "Respawn ONE spawned agent (#906): kill the prior instance (if live) and launch a fresh one that reuses the prior spawn's kind/model/model_ref and manifest-recorded working_dir, with a continuity packet (prior spawn/session id + final message) prepended so it resumes. prompt is REQUIRED — the original prompt is not persisted, so respawn supplies the continued task rather than fabricate it. Writes a StateChanged lineage row (respawned_into) on the prior agent and returns both ids. Errors loudly if the prior spawn manifest or working_dir cannot be read."
    )]
    pub async fn agent_respawn(
        &self,
        params: Parameters<AgentRespawnParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentRespawnResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_respawn",
            "tool.invocation kind=agent_respawn"
        );
        self.agent_respawn_impl(params.0, &request_context)
            .await
            .map(Json)
    }
}

impl SynapseService {
    pub(crate) async fn dashboard_agent_kill_request(
        &self,
        params: AgentKillParams,
    ) -> Result<AgentKillResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_AGENT_KILL_REQUESTED",
            kind = TOOL_AGENT_KILL,
            requested_id = %params.session_id,
            "dashboard.invocation kind=agent_kill"
        );
        self.agent_kill_impl(params, None).await
    }

    pub(crate) async fn dashboard_fleet_stop_request(
        &self,
        params: FleetStopParams,
    ) -> Result<FleetStopResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_FLEET_STOP_REQUESTED",
            kind = TOOL_FLEET_STOP,
            mode = %params.mode,
            agent_kind_count = params.agent_kinds.len(),
            "dashboard.invocation kind=fleet_stop"
        );
        self.fleet_stop_impl(params, Some("dashboard-fleet")).await
    }

    pub(crate) fn dashboard_agent_interrupt_request(
        &self,
        params: AgentInterruptParams,
    ) -> Result<AgentInterruptResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_AGENT_INTERRUPT_REQUESTED",
            kind = TOOL_AGENT_INTERRUPT,
            requested_id = %params.session_id,
            "dashboard.invocation kind=agent_interrupt"
        );
        self.agent_interrupt_impl(params, Some("dashboard-agent-detail"))
    }

    pub(crate) fn dashboard_agent_pause_request(
        &self,
        params: AgentPauseParams,
    ) -> Result<AgentSuspendResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_AGENT_PAUSE_REQUESTED",
            kind = TOOL_AGENT_PAUSE,
            requested_id = %params.session_id,
            "dashboard.invocation kind=agent_pause"
        );
        self.agent_pause_impl(params, Some("dashboard-agent-detail"))
    }

    pub(crate) fn dashboard_agent_resume_request(
        &self,
        params: AgentPauseParams,
    ) -> Result<AgentSuspendResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_AGENT_RESUME_REQUESTED",
            kind = TOOL_AGENT_RESUME,
            requested_id = %params.session_id,
            "dashboard.invocation kind=agent_resume"
        );
        self.agent_resume_impl(params, Some("dashboard-agent-detail"))
    }

    pub(crate) async fn dashboard_agent_respawn_request(
        &self,
        params: AgentRespawnParams,
        mcp_url: String,
    ) -> Result<AgentRespawnResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_AGENT_RESPAWN_REQUESTED",
            kind = TOOL_AGENT_RESPAWN,
            requested_id = %params.session_id,
            "dashboard.invocation kind=agent_respawn"
        );
        self.agent_respawn_core(params, None, None, Some(mcp_url))
            .await
    }

    // ------------------------------------------------------------------
    // agent_interrupt
    // ------------------------------------------------------------------

    fn agent_interrupt_impl(
        &self,
        params: AgentInterruptParams,
        caller_session: Option<&str>,
    ) -> Result<AgentInterruptResponse, ErrorData> {
        let lookup = validate_lookup_id(&params.session_id, TOOL_AGENT_INTERRUPT)?;
        let target = self.resolve_spawned_agent(&lookup, TOOL_AGENT_INTERRUPT)?;
        if target.dead {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_ALREADY_DEAD: agent {} (session {}) is closed; interrupt targets a live agent — use agent_kill to reclaim a dead agent's resources",
                    lookup, target.session_id
                ),
            ));
        }
        let process = process_readback_for_target(&target);

        let payload = json!({
            "reason": "operator_interrupt",
            "requested_id": lookup,
            "from": caller_session,
        });
        let before = json!({ "process": &process, "lifecycle": target.lifecycle });
        self.command_audit_intent(
            CommandAuditInput::mcp(
                TOOL_AGENT_INTERRUPT,
                "interrupt",
                caller_session.map(ToOwned::to_owned),
                Some(target.session_id.clone()),
                payload.clone(),
                before.clone(),
                Value::Null,
                "pending",
            )
            .with_target(json!({ "spawn_id": target.spawn_id, "agent_kind": target.agent_kind })),
        )?;

        let response = self.interrupt_core(&lookup, &target, caller_session)?;

        let after = json!({
            "delivered": response.delivered,
            "delivered_via": response.delivered_via,
            "channels": response.channels,
        });
        self.command_audit_final(
            CommandAuditInput::mcp(
                TOOL_AGENT_INTERRUPT,
                "interrupt",
                caller_session.map(ToOwned::to_owned),
                Some(target.session_id.clone()),
                payload,
                before,
                after,
                if response.delivered { "ok" } else { "error" },
            )
            .with_target(json!({ "spawn_id": target.spawn_id, "agent_kind": target.agent_kind })),
        )?;

        if !response.delivered {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "AGENT_INTERRUPT_NO_CHANNEL: no clean channel could deliver an interrupt to agent {} (session {}); see the per-channel `channels` report for why each was unavailable. Use agent_kill to force-terminate.",
                    response.requested_id, response.session_id
                ),
            ));
        }
        Ok(response)
    }

    /// Builds an interrupt response (channels attempted + durable `interrupted`
    /// journal row on delivery) without auditing or erroring on non-delivery.
    /// Shared by `agent_interrupt` (which wraps it with audit + a no-channel
    /// error) and `agent_kill` (which calls it best-effort before force-kill).
    fn interrupt_core(
        &self,
        requested_id: &str,
        target: &ResolvedAgent,
        caller_session: Option<&str>,
    ) -> Result<AgentInterruptResponse, ErrorData> {
        let process = process_readback_for_target(target);
        let (channels, delivered_via, _send_row) =
            self.attempt_interrupt_channels(target, caller_session);
        let delivered = delivered_via.is_some();
        // Journal a durable `interrupted` event only when a channel actually
        // delivered — never claim an interruption that did not happen.
        let journal_event = if delivered {
            Some(self.journal_lifecycle_event(
                AgentEventKind::Interrupted,
                target,
                "agent_interrupt",
                None,
                json!({ "delivered_via": delivered_via, "process": &process }),
            )?)
        } else {
            None
        };
        Ok(AgentInterruptResponse {
            requested_id: requested_id.to_owned(),
            session_id: target.session_id.clone(),
            spawn_id: target.spawn_id.clone(),
            agent_kind: target.agent_kind.clone(),
            lifecycle: target.lifecycle.clone(),
            resolution_source: target.resolution_source.clone(),
            delivered,
            delivered_via,
            channels,
            journal_event,
            process,
        })
    }

    /// Attempts each ranked channel and returns `(attempts, delivered_via,
    /// send_row)`. Channels report true outcomes; unsupported legacy rows stay
    /// unavailable instead of being treated as delivered.
    fn attempt_interrupt_channels(
        &self,
        target: &ResolvedAgent,
        caller_session: Option<&str>,
    ) -> (Vec<ChannelAttempt>, Option<String>, Option<String>) {
        let mut attempts = Vec::new();
        let mut delivered_via = None;
        let mut send_row = None;

        let codex = self.deliver_codex_app_server_interrupt(target);
        record_first_delivered_channel(&mut delivered_via, &codex);
        attempts.push(codex);
        attempts.push(ChannelAttempt {
            channel: "claude_stream_json_control".to_owned(),
            rank: 2,
            status: "unavailable".to_owned(),
            reason: "channel_not_wired: no supported claude -p stdin cancel frame today \
                     (anthropics/claude-code#51078); the daemon does not own the stream-json pipe"
                .to_owned(),
            message_id: None,
            row_key: None,
        });

        // Rank 3: cooperative mailbox interrupt — the one wired channel.
        let mailbox = self.deliver_mailbox_interrupt(target, caller_session);
        if mailbox.status == "delivered" {
            record_first_delivered_channel(&mut delivered_via, &mailbox);
            send_row.clone_from(&mailbox.row_key);
        }
        attempts.push(mailbox);

        attempts.push(ChannelAttempt {
            channel: "pty_esc".to_owned(),
            rank: 4,
            status: "unavailable".to_owned(),
            reason: "channel_not_wired: PTY ESC (the documented interrupt key) needs owned-PTY \
                     capture (#902), which is not implemented yet"
                .to_owned(),
            message_id: None,
            row_key: None,
        });

        (attempts, delivered_via, send_row)
    }

    fn deliver_codex_app_server_interrupt(&self, target: &ResolvedAgent) -> ChannelAttempt {
        if target.agent_kind != "codex" {
            return ChannelAttempt {
                channel: "codex_app_server_turn_interrupt".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: format!(
                    "channel_not_applicable: target agent_kind={} is not codex",
                    target.agent_kind
                ),
                message_id: None,
                row_key: None,
            };
        }
        let Some(control) = target.control.as_ref() else {
            return ChannelAttempt {
                channel: "codex_app_server_turn_interrupt".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: "channel_not_wired: this codex session has no codex-control.json metadata; it was likely spawned by the legacy plain-CLI path before #958".to_owned(),
                message_id: None,
                row_key: None,
            };
        };
        if control.protocol != "codex_app_server_ws" {
            return ChannelAttempt {
                channel: "codex_app_server_turn_interrupt".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: format!(
                    "channel_not_wired: unsupported control protocol {}",
                    control.protocol
                ),
                message_id: None,
                row_key: Some(control.control_path.clone()),
            };
        }
        let Some(thread_id) = control
            .thread_id
            .as_deref()
            .filter(|value| !value.is_empty())
        else {
            return ChannelAttempt {
                channel: "codex_app_server_turn_interrupt".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: "channel_not_ready: codex-control.json has no thread_id".to_owned(),
                message_id: None,
                row_key: Some(control.control_path.clone()),
            };
        };
        let Some(turn_id) = control.turn_id.as_deref().filter(|value| !value.is_empty()) else {
            return ChannelAttempt {
                channel: "codex_app_server_turn_interrupt".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: "channel_not_ready: codex-control.json has no turn_id".to_owned(),
                message_id: None,
                row_key: Some(control.control_path.clone()),
            };
        };
        if matches!(
            control.turn_status.as_str(),
            "completed" | "interrupted" | "failed"
        ) {
            return ChannelAttempt {
                channel: "codex_app_server_turn_interrupt".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: format!(
                    "turn_not_interruptible: codex-control.json reports turn_status={}",
                    control.turn_status
                ),
                message_id: Some(turn_id.to_owned()),
                row_key: Some(control.control_path.clone()),
            };
        }
        if crate::m4::owned_live_process_ids(&[control.app_server_process_id]).is_empty() {
            return ChannelAttempt {
                channel: "codex_app_server_turn_interrupt".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: format!(
                    "app_server_not_live: codex app-server pid {} is not live",
                    control.app_server_process_id
                ),
                message_id: Some(turn_id.to_owned()),
                row_key: Some(control.control_path.clone()),
            };
        }

        let script_path = PathBuf::from(&target.log_dir).join("codex-app-server-interrupt.ps1");
        if let Err(error) = fs::write(&script_path, CODEX_APP_SERVER_INTERRUPT_SCRIPT) {
            return ChannelAttempt {
                channel: "codex_app_server_turn_interrupt".to_owned(),
                rank: 1,
                status: "failed".to_owned(),
                reason: format!(
                    "interrupt_helper_write_failed: {} ({error})",
                    script_path.display()
                ),
                message_id: Some(turn_id.to_owned()),
                row_key: Some(control.control_path.clone()),
            };
        }

        match run_codex_interrupt_helper(&script_path, control, thread_id, turn_id) {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                ChannelAttempt {
                    channel: "codex_app_server_turn_interrupt".to_owned(),
                    rank: 1,
                    status: "delivered".to_owned(),
                    reason: format!(
                        "turn_interrupt_delivered: endpoint={} thread_id={} turn_id={} control_path={} stdout={}",
                        control.endpoint,
                        thread_id,
                        turn_id,
                        control.control_path,
                        compact_for_channel_reason(stdout.trim())
                    ),
                    message_id: Some(turn_id.to_owned()),
                    row_key: Some(control.control_path.clone()),
                }
            }
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                ChannelAttempt {
                    channel: "codex_app_server_turn_interrupt".to_owned(),
                    rank: 1,
                    status: "failed".to_owned(),
                    reason: format!(
                        "turn_interrupt_failed: exit={:?} stdout={} stderr={}",
                        output.status.code(),
                        compact_for_channel_reason(stdout.trim()),
                        compact_for_channel_reason(stderr.trim())
                    ),
                    message_id: Some(turn_id.to_owned()),
                    row_key: Some(control.control_path.clone()),
                }
            }
            Err(error) => ChannelAttempt {
                channel: "codex_app_server_turn_interrupt".to_owned(),
                rank: 1,
                status: "failed".to_owned(),
                reason: error,
                message_id: Some(turn_id.to_owned()),
                row_key: Some(control.control_path.clone()),
            },
        }
    }

    /// Delivers a durable `interrupt` mailbox row to the target's steering
    /// inbox, proving delivery by the persisted `CF_KV` row readback.
    fn deliver_mailbox_interrupt(
        &self,
        target: &ResolvedAgent,
        caller_session: Option<&str>,
    ) -> ChannelAttempt {
        let Some(caller) = caller_session else {
            return ChannelAttempt {
                channel: "mailbox_interrupt".to_owned(),
                rank: 3,
                status: "unavailable".to_owned(),
                reason: "needs the caller's MCP session id (run the daemon in HTTP mode so each \
                         agent has its own Mcp-Session-Id)"
                    .to_owned(),
                message_id: None,
                row_key: None,
            };
        };
        let send = self.agent_send_impl(
            super::agent_mailbox::AgentSendParams {
                to_session: target.session_id.clone(),
                kind: INTERRUPT_MAILBOX_KIND.to_owned(),
                payload: json!({
                    "control": "interrupt",
                    "from": caller,
                    "reason": "operator_interrupt",
                    "instructions": "stop the current turn at the next safe point",
                }),
                artifact_handle: None,
                ttl_ms: INTERRUPT_MESSAGE_TTL_MS,
                request_receipt: false,
            },
            caller,
        );
        match send {
            Ok(response) => ChannelAttempt {
                channel: "mailbox_interrupt".to_owned(),
                rank: 3,
                status: "delivered".to_owned(),
                reason: format!(
                    "durable {} row persisted to CF_KV (queue_depth_after={}); cooperative agents \
                     drain it between tool calls and bail",
                    INTERRUPT_MAILBOX_KIND, response.queue_depth_after
                ),
                message_id: Some(response.message_id),
                row_key: Some(response.row_key),
            },
            Err(error) => ChannelAttempt {
                channel: "mailbox_interrupt".to_owned(),
                rank: 3,
                status: "failed".to_owned(),
                reason: format!("mailbox delivery failed: {}", error.message),
                message_id: None,
                row_key: None,
            },
        }
    }

    pub(crate) fn dashboard_agent_steer(
        &self,
        session_id: String,
        instruction: String,
        request_receipt: bool,
    ) -> Result<Value, ErrorData> {
        dashboard_json_readback(self.agent_steer_impl(
            AgentSteerParams {
                session_id,
                instruction,
                request_receipt,
            },
            Some("dashboard-context"),
        )?)
    }

    // ------------------------------------------------------------------
    // agent_steer (#905)
    // ------------------------------------------------------------------

    fn agent_steer_impl(
        &self,
        params: AgentSteerParams,
        caller_session: Option<&str>,
    ) -> Result<AgentSteerResponse, ErrorData> {
        let lookup = validate_lookup_id(&params.session_id, TOOL_AGENT_STEER)?;
        let instruction = params.instruction.trim();
        if instruction.is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "AGENT_STEER_EMPTY: instruction must be a non-empty string of guidance to inject",
            ));
        }
        let instruction_chars = instruction.chars().count();
        if instruction_chars > MAX_STEER_INSTRUCTION_CHARS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_STEER_TOO_LONG: instruction is {instruction_chars} chars; the cooperative steering channel caps a single instruction at {MAX_STEER_INSTRUCTION_CHARS}"
                ),
            ));
        }
        let target = self.resolve_spawned_agent(&lookup, TOOL_AGENT_STEER)?;
        if target.dead {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_ALREADY_DEAD: agent {} (session {}) is closed; steering targets a live agent",
                    lookup, target.session_id
                ),
            ));
        }
        let process = process_readback_for_target(&target);

        let payload = json!({
            "requested_id": lookup,
            "instruction": instruction,
            "instruction_chars": instruction_chars,
            "request_receipt": params.request_receipt,
            "from": caller_session,
        });
        let before = json!({ "process": &process, "lifecycle": target.lifecycle });
        self.command_audit_intent(
            CommandAuditInput::mcp(
                TOOL_AGENT_STEER,
                "steer",
                caller_session.map(ToOwned::to_owned),
                Some(target.session_id.clone()),
                payload.clone(),
                before.clone(),
                Value::Null,
                "pending",
            )
            .with_target(json!({ "spawn_id": target.spawn_id, "agent_kind": target.agent_kind })),
        )?;

        let response = self.steer_core(
            &lookup,
            &target,
            instruction,
            params.request_receipt,
            caller_session,
        )?;

        let after = json!({
            "delivered": response.delivered,
            "delivered_via": response.delivered_via,
            "channels": response.channels,
        });
        self.command_audit_final(
            CommandAuditInput::mcp(
                TOOL_AGENT_STEER,
                "steer",
                caller_session.map(ToOwned::to_owned),
                Some(target.session_id.clone()),
                payload,
                before,
                after,
                if response.delivered { "ok" } else { "error" },
            )
            .with_target(json!({ "spawn_id": target.spawn_id, "agent_kind": target.agent_kind })),
        )?;

        if !response.delivered {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "AGENT_STEER_NO_CHANNEL: no clean channel could inject an instruction into agent {} (session {}); see the per-channel `channels` report for why each was unavailable.",
                    response.requested_id, response.session_id
                ),
            ));
        }
        Ok(response)
    }

    /// Builds a steer response (channels attempted + durable `MessageSent`
    /// journal row on delivery) without auditing or erroring on non-delivery.
    fn steer_core(
        &self,
        requested_id: &str,
        target: &ResolvedAgent,
        instruction: &str,
        request_receipt: bool,
        caller_session: Option<&str>,
    ) -> Result<AgentSteerResponse, ErrorData> {
        let process = process_readback_for_target(target);
        let (channels, delivered_via) =
            self.attempt_steer_channels(target, instruction, request_receipt, caller_session);
        let delivered = delivered_via.is_some();
        let instruction_chars = instruction.chars().count();
        // Journal a durable `MessageSent` event only when a channel actually
        // delivered — never claim an injection that did not happen. `MessageSent`
        // is the #897 minimum-set kind for an orchestrator-to-agent message; the
        // steer reason_code distinguishes it from peer mailbox traffic.
        let journal_event = if delivered {
            Some(self.journal_lifecycle_event(
                AgentEventKind::MessageSent,
                target,
                "agent_steer",
                None,
                json!({
                    "delivered_via": delivered_via,
                    "instruction_chars": instruction_chars,
                    "instruction": instruction,
                    "process": &process,
                }),
            )?)
        } else {
            None
        };
        let receipt_box_session_id =
            if delivered_via.as_deref() == Some("mailbox_steer") && request_receipt {
                caller_session.map(ToOwned::to_owned)
            } else {
                None
            };
        Ok(AgentSteerResponse {
            requested_id: requested_id.to_owned(),
            session_id: target.session_id.clone(),
            spawn_id: target.spawn_id.clone(),
            agent_kind: target.agent_kind.clone(),
            lifecycle: target.lifecycle.clone(),
            resolution_source: target.resolution_source.clone(),
            delivered,
            delivered_via,
            instruction_chars,
            channels,
            journal_event,
            receipt_box_session_id,
            process,
        })
    }

    /// Attempts each ranked steering channel and returns `(attempts,
    /// delivered_via)`. Channels report true outcomes; unwired channels stay
    /// unavailable with a precise reason instead of being faked.
    fn attempt_steer_channels(
        &self,
        target: &ResolvedAgent,
        instruction: &str,
        request_receipt: bool,
        caller_session: Option<&str>,
    ) -> (Vec<ChannelAttempt>, Option<String>) {
        let mut attempts = Vec::new();
        let mut delivered_via = None;

        let codex = self.deliver_codex_app_server_steer(target, instruction);
        record_first_delivered_channel(&mut delivered_via, &codex);
        attempts.push(codex);

        attempts.push(ChannelAttempt {
            channel: "claude_stream_json_input".to_owned(),
            rank: 2,
            status: "unavailable".to_owned(),
            reason: "channel_not_wired: the daemon does not own the claude -p stream-json stdin \
                     pipe; even where owned, mid-turn stdin messages are processed in-memory only \
                     and dropped from session history (anthropics/claude-code#41230)"
                .to_owned(),
            message_id: None,
            row_key: None,
        });

        // Rank 3: cooperative steering mailbox — durable fallback. If the live
        // app-server channel delivered, do not queue a duplicate instruction
        // that the agent could consume later between tool calls.
        if delivered_via.is_some() {
            attempts.push(ChannelAttempt {
                channel: "mailbox_steer".to_owned(),
                rank: 3,
                status: "skipped".to_owned(),
                reason: "higher_ranked_channel_delivered: codex_app_server_inject delivered to the active turn, so no duplicate durable mailbox row was written".to_owned(),
                message_id: None,
                row_key: None,
            });
        } else {
            let mailbox =
                self.deliver_mailbox_steer(target, instruction, request_receipt, caller_session);
            if mailbox.status == "delivered" {
                record_first_delivered_channel(&mut delivered_via, &mailbox);
            }
            attempts.push(mailbox);
        }

        attempts.push(ChannelAttempt {
            channel: "pty_stdin".to_owned(),
            rank: 4,
            status: "unavailable".to_owned(),
            reason:
                "channel_not_wired: writing an instruction to the agent's terminal stdin needs \
                     owned-PTY capture (#902), which is not implemented yet"
                    .to_owned(),
            message_id: None,
            row_key: None,
        });

        (attempts, delivered_via)
    }

    fn deliver_codex_app_server_steer(
        &self,
        target: &ResolvedAgent,
        instruction: &str,
    ) -> ChannelAttempt {
        if target.agent_kind != "codex" {
            return ChannelAttempt {
                channel: "codex_app_server_inject".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: format!(
                    "channel_not_applicable: target agent_kind={} is not codex",
                    target.agent_kind
                ),
                message_id: None,
                row_key: None,
            };
        }
        let Some(control) = target.control.as_ref() else {
            return ChannelAttempt {
                channel: "codex_app_server_inject".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: "channel_not_wired: this codex session has no codex-control.json metadata; it was likely spawned by the legacy plain-CLI path before #958".to_owned(),
                message_id: None,
                row_key: None,
            };
        };
        if control.protocol != "codex_app_server_ws" {
            return ChannelAttempt {
                channel: "codex_app_server_inject".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: format!(
                    "channel_not_wired: unsupported control protocol {}",
                    control.protocol
                ),
                message_id: None,
                row_key: Some(control.control_path.clone()),
            };
        }
        let Some(thread_id) = control
            .thread_id
            .as_deref()
            .filter(|value| !value.is_empty())
        else {
            return ChannelAttempt {
                channel: "codex_app_server_inject".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: "channel_not_ready: codex-control.json has no thread_id".to_owned(),
                message_id: None,
                row_key: Some(control.control_path.clone()),
            };
        };
        let Some(turn_id) = control.turn_id.as_deref().filter(|value| !value.is_empty()) else {
            return ChannelAttempt {
                channel: "codex_app_server_inject".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: "channel_not_ready: codex-control.json has no turn_id".to_owned(),
                message_id: None,
                row_key: Some(control.control_path.clone()),
            };
        };
        if matches!(
            control.turn_status.as_str(),
            "completed" | "interrupted" | "failed" | "runner_error"
        ) {
            return ChannelAttempt {
                channel: "codex_app_server_inject".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: format!(
                    "turn_not_steerable: codex-control.json reports turn_status={}",
                    control.turn_status
                ),
                message_id: Some(turn_id.to_owned()),
                row_key: Some(control.control_path.clone()),
            };
        }
        if crate::m4::owned_live_process_ids(&[control.app_server_process_id]).is_empty() {
            return ChannelAttempt {
                channel: "codex_app_server_inject".to_owned(),
                rank: 1,
                status: "unavailable".to_owned(),
                reason: format!(
                    "app_server_not_live: codex app-server pid {} is not live",
                    control.app_server_process_id
                ),
                message_id: Some(turn_id.to_owned()),
                row_key: Some(control.control_path.clone()),
            };
        }

        let script_path = PathBuf::from(&target.log_dir).join("codex-app-server-steer.ps1");
        if let Err(error) = fs::write(&script_path, CODEX_APP_SERVER_STEER_SCRIPT) {
            return ChannelAttempt {
                channel: "codex_app_server_inject".to_owned(),
                rank: 1,
                status: "failed".to_owned(),
                reason: format!(
                    "steer_helper_write_failed: {} ({error})",
                    script_path.display()
                ),
                message_id: Some(turn_id.to_owned()),
                row_key: Some(control.control_path.clone()),
            };
        }

        match run_codex_steer_helper(&script_path, control, thread_id, turn_id, instruction) {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stdout_trimmed = stdout.trim();
                let delivered_turn_id = serde_json::from_str::<Value>(stdout_trimmed)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("turn_id")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(ToOwned::to_owned)
                    })
                    .unwrap_or_else(|| turn_id.to_owned());
                ChannelAttempt {
                    channel: "codex_app_server_inject".to_owned(),
                    rank: 1,
                    status: "delivered".to_owned(),
                    reason: format!(
                        "turn_steer_delivered: endpoint={} thread_id={} expected_turn_id={} turn_id={} control_path={} stdout={}",
                        control.endpoint,
                        thread_id,
                        turn_id,
                        delivered_turn_id,
                        control.control_path,
                        compact_for_channel_reason(stdout_trimmed)
                    ),
                    message_id: Some(delivered_turn_id),
                    row_key: Some(control.control_path.clone()),
                }
            }
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                ChannelAttempt {
                    channel: "codex_app_server_inject".to_owned(),
                    rank: 1,
                    status: "failed".to_owned(),
                    reason: format!(
                        "turn_steer_failed: exit={:?} stdout={} stderr={}",
                        output.status.code(),
                        compact_for_channel_reason(stdout.trim()),
                        compact_for_channel_reason(stderr.trim())
                    ),
                    message_id: Some(turn_id.to_owned()),
                    row_key: Some(control.control_path.clone()),
                }
            }
            Err(error) => ChannelAttempt {
                channel: "codex_app_server_inject".to_owned(),
                rank: 1,
                status: "failed".to_owned(),
                reason: error,
                message_id: Some(turn_id.to_owned()),
                row_key: Some(control.control_path.clone()),
            },
        }
    }

    /// Delivers a durable `steer` mailbox row to the target's steering inbox,
    /// proving delivery by the persisted `CF_KV` row readback. The cooperative
    /// steering contract (`STEER_KIND`) is the same channel `agent_send` uses;
    /// the receipt (when requested) proves the agent actually applied it.
    fn deliver_mailbox_steer(
        &self,
        target: &ResolvedAgent,
        instruction: &str,
        request_receipt: bool,
        caller_session: Option<&str>,
    ) -> ChannelAttempt {
        let Some(caller) = caller_session else {
            return ChannelAttempt {
                channel: "mailbox_steer".to_owned(),
                rank: 3,
                status: "unavailable".to_owned(),
                reason: "needs the caller's MCP session id (run the daemon in HTTP mode so each \
                         agent has its own Mcp-Session-Id)"
                    .to_owned(),
                message_id: None,
                row_key: None,
            };
        };
        let send = self.agent_send_impl(
            super::agent_mailbox::AgentSendParams {
                to_session: target.session_id.clone(),
                kind: super::agent_mailbox::STEER_KIND.to_owned(),
                payload: json!({
                    "control": "steer",
                    "from": caller,
                    "reason": "operator_steer",
                    "instruction": instruction,
                }),
                artifact_handle: None,
                ttl_ms: STEER_MESSAGE_TTL_MS,
                request_receipt,
            },
            caller,
        );
        match send {
            Ok(response) => ChannelAttempt {
                channel: "mailbox_steer".to_owned(),
                rank: 3,
                status: "delivered".to_owned(),
                reason: format!(
                    "durable {} row persisted to CF_KV (queue_depth_after={}); cooperative agents \
                     drain it between tool calls and splice it into context{}",
                    super::agent_mailbox::STEER_KIND,
                    response.queue_depth_after,
                    if request_receipt {
                        " (read receipt requested — poll agent_receipts for consumption proof)"
                    } else {
                        ""
                    }
                ),
                message_id: Some(response.message_id),
                row_key: Some(response.row_key),
            },
            Err(error) => ChannelAttempt {
                channel: "mailbox_steer".to_owned(),
                rank: 3,
                status: "failed".to_owned(),
                reason: format!("mailbox delivery failed: {}", error.message),
                message_id: None,
                row_key: None,
            },
        }
    }

    // ------------------------------------------------------------------
    // agent_pause / agent_resume (#906)
    // ------------------------------------------------------------------

    fn agent_pause_impl(
        &self,
        params: AgentPauseParams,
        caller_session: Option<&str>,
    ) -> Result<AgentSuspendResponse, ErrorData> {
        self.suspend_resume_core(params, caller_session, true)
    }

    fn agent_resume_impl(
        &self,
        params: AgentPauseParams,
        caller_session: Option<&str>,
    ) -> Result<AgentSuspendResponse, ErrorData> {
        self.suspend_resume_core(params, caller_session, false)
    }

    /// Shared suspend/resume path. `pause=true` freezes the tree, `pause=false`
    /// thaws it. The OS thread table is the source of truth for the before/after
    /// state, the operation is idempotent (never stacks suspend counts), and a
    /// `StateChanged` journal row is written only when the state actually changed.
    fn suspend_resume_core(
        &self,
        params: AgentPauseParams,
        caller_session: Option<&str>,
        pause: bool,
    ) -> Result<AgentSuspendResponse, ErrorData> {
        let tool = if pause {
            TOOL_AGENT_PAUSE
        } else {
            TOOL_AGENT_RESUME
        };
        let operation = if pause { "pause" } else { "resume" };
        let lookup = validate_lookup_id(&params.session_id, tool)?;
        let target = self.resolve_spawned_agent(&lookup, tool)?;
        if target.dead {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_ALREADY_DEAD: agent {} (session {}) is closed; {operation} targets a live agent",
                    lookup, target.session_id
                ),
            ));
        }
        let process = process_readback_for_target(&target);
        if process.live_process_ids.is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_NO_LIVE_PROCESS: agent {} (session {}) has no live process tree to {operation}",
                    lookup, target.session_id
                ),
            ));
        }

        // Physical state BEFORE acting.
        let states_before = crate::m4::process_tree_suspend_state(&process.live_process_ids);
        let was_suspended_before =
            !states_before.is_empty() && states_before.iter().all(|state| state.fully_suspended);
        let any_suspended_before = states_before
            .iter()
            .any(|state| state.suspended_threads > 0);
        // Idempotency: pause is a no-op when already fully suspended (stacking a
        // second suspend would need a second resume); resume is a no-op when no
        // thread is suspended at all.
        let no_op = if pause {
            was_suspended_before
        } else {
            !any_suspended_before
        };

        let payload = json!({
            "requested_id": lookup,
            "operation": operation,
            "from": caller_session,
            "process_tree_ids": process.process_tree_ids,
        });
        let before = json!({
            "process": &process,
            "states_before": &states_before,
            "was_suspended_before": was_suspended_before,
        });
        self.command_audit_intent(
            CommandAuditInput::mcp(
                tool,
                operation,
                caller_session.map(ToOwned::to_owned),
                Some(target.session_id.clone()),
                payload.clone(),
                before.clone(),
                Value::Null,
                "pending",
            )
            .with_target(json!({ "spawn_id": target.spawn_id, "agent_kind": target.agent_kind })),
        )?;

        let suspend = if no_op {
            // Report current physical state without mutating suspend counts.
            crate::m4::OwnedProcessSuspendReadback {
                process_ids: process.process_tree_ids.clone(),
                live_process_ids: process.live_process_ids,
                applied_process_ids: Vec::new(),
                failed: Vec::new(),
                all_suspended: was_suspended_before,
                all_running: !any_suspended_before,
                states_after: states_before,
            }
        } else if pause {
            crate::m4::suspend_owned_process_ids(&process.process_tree_ids)
        } else {
            crate::m4::resume_owned_process_ids(&process.process_tree_ids)
        };

        let is_suspended_after = suspend.all_suspended;
        let ok = if pause {
            suspend.all_suspended
        } else {
            suspend.all_running
        };

        // Journal a StateChanged row only when this call actually changed state.
        let journal_event = if no_op {
            None
        } else {
            Some(self.journal_lifecycle_event(
                AgentEventKind::StateChanged,
                &target,
                tool,
                None,
                json!({
                    "operation": operation,
                    "was_suspended_before": was_suspended_before,
                    "suspend": &suspend,
                }),
            )?)
        };

        let response = AgentSuspendResponse {
            requested_id: lookup,
            session_id: target.session_id.clone(),
            spawn_id: target.spawn_id.clone(),
            agent_kind: target.agent_kind.clone(),
            lifecycle: target.lifecycle.clone(),
            resolution_source: target.resolution_source.clone(),
            operation: operation.to_owned(),
            was_suspended_before,
            is_suspended_after,
            no_op,
            ok,
            suspend,
            journal_event,
        };

        let after = json!({
            "operation": operation,
            "no_op": response.no_op,
            "ok": response.ok,
            "is_suspended_after": response.is_suspended_after,
            "suspend": &response.suspend,
        });
        self.command_audit_final(
            CommandAuditInput::mcp(
                tool,
                operation,
                caller_session.map(ToOwned::to_owned),
                Some(target.session_id.clone()),
                payload,
                before,
                after,
                if response.ok { "ok" } else { "error" },
            )
            .with_target(json!({ "spawn_id": target.spawn_id, "agent_kind": target.agent_kind })),
        )?;

        if !response.ok {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "AGENT_{}_INCOMPLETE: agent {} (session {}) could not be fully {operation}d; per-pid failures: {:?}; thread state after: {:?}",
                    operation.to_ascii_uppercase(),
                    response.requested_id,
                    response.session_id,
                    response.suspend.failed,
                    response.suspend.states_after,
                ),
            ));
        }
        Ok(response)
    }

    // ------------------------------------------------------------------
    // agent_respawn (#906)
    // ------------------------------------------------------------------

    /// Builds the respawn plan from the prior agent's persisted state WITHOUT
    /// any side effect (no kill, no launch). Validates the prompt/grace,
    /// resolves the prior agent, reconstructs its spawn identity from the
    /// manifest, and assembles the continuity prompt + spawn request value.
    /// Split out so the reconstruction is unit-testable without a launch.
    fn agent_respawn_plan(&self, params: &AgentRespawnParams) -> Result<RespawnPlan, ErrorData> {
        let lookup = validate_lookup_id(&params.session_id, TOOL_AGENT_RESPAWN)?;
        let prompt = params.prompt.trim();
        if prompt.is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "AGENT_RESPAWN_PROMPT_REQUIRED: the original prompt is not persisted; supply the continued task prompt for the new instance",
            ));
        }
        if params.grace_ms > MAX_KILL_GRACE_MS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_RESPAWN_GRACE_INVALID: grace_ms must be 0..={MAX_KILL_GRACE_MS}, got {}",
                    params.grace_ms
                ),
            ));
        }
        let target = self.resolve_spawned_agent(&lookup, TOOL_AGENT_RESPAWN)?;

        // Reuse the prior spawn's identity from its persisted manifest (the SoT
        // for kind/model/model_ref) — refuse loudly if it cannot be read rather
        // than guess.
        let manifest = self.read_respawn_manifest(&target)?;
        let Some(cli_token) = spawn_cli_serde_token(&manifest.agent_kind) else {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_RESPAWN_KIND_UNSUPPORTED: prior spawn manifest reports agent kind {:?}, which is not a respawnable CLI",
                    manifest.agent_kind
                ),
            ));
        };

        // Continuity packet: name the prior lineage and, when present, fold in
        // its final message so the new instance resumes with context.
        let mut effective_prompt = String::new();
        let mut carried_context = false;
        if params.carry_context {
            let final_message = read_prior_final_message(&target.log_dir);
            effective_prompt.push_str(&format!(
                "[Respawn continuity] You are a respawn of agent {} (prior MCP session {}). \
                 The prior instance was {}. Resume its work from where it left off.\n",
                target.spawn_id.as_deref().unwrap_or(&target.session_id),
                target.session_id,
                if target.dead {
                    "no longer running"
                } else {
                    "stopped to respawn it"
                },
            ));
            if let Some(message) = final_message {
                effective_prompt.push_str("\nPrior final message:\n");
                effective_prompt.push_str(&message);
                effective_prompt.push('\n');
            }
            effective_prompt.push_str("\n---\n\n");
            carried_context = true;
        }
        effective_prompt.push_str(prompt);

        // Build a direct spawn request reusing the prior identity. Constructed
        // via serde so every unspecified field takes its documented default.
        let mut request_value = json!({
            "cli": cli_token,
            "prompt": effective_prompt,
        });
        if let Some(model) = manifest.model.as_deref() {
            request_value["model"] = json!(model);
        }
        if let Some(model_ref) = manifest.model_ref.as_deref() {
            request_value["model_ref"] = json!(model_ref);
        }
        if let Some(working_dir) = manifest.working_dir.as_deref() {
            request_value["working_dir"] = json!(working_dir);
        }

        Ok(RespawnPlan {
            lookup,
            target,
            manifest,
            effective_prompt,
            carried_context,
            request_value,
        })
    }

    async fn agent_respawn_impl(
        &self,
        params: AgentRespawnParams,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<AgentRespawnResponse, ErrorData> {
        let caller = super::context::mcp_session_id_from_request_context(request_context)?;
        self.agent_respawn_core(params, caller, Some(request_context), None)
            .await
    }

    async fn agent_respawn_core(
        &self,
        params: AgentRespawnParams,
        caller: Option<String>,
        request_context: Option<&RequestContext<RoleServer>>,
        dashboard_mcp_url: Option<String>,
    ) -> Result<AgentRespawnResponse, ErrorData> {
        // Own one spawn-activity generation from respawn entry, before planning
        // or killing the prior process. The inner spawn has its own guard too,
        // but this original epoch prevents a K2 that fires during a long prior
        // kill from finalizing and then allowing a fresh replacement launch.
        let respawn_activity =
            super::m4_tools::AgentSpawnInFlightGuard::enter("agent_respawn_outer")?;
        let RespawnPlan {
            lookup,
            target,
            manifest,
            effective_prompt,
            carried_context,
            request_value,
        } = self.agent_respawn_plan(&params)?;
        let prior_spawn_id = target.spawn_id.clone();

        let payload = json!({
            "requested_id": lookup,
            "carry_context": params.carry_context,
            "grace_ms": params.grace_ms,
            "manifest": &manifest,
            "from": caller,
        });
        let before = json!({
            "prior_session_id": target.session_id,
            "prior_spawn_id": prior_spawn_id,
            "prior_lifecycle": target.lifecycle,
        });
        self.command_audit_intent(
            CommandAuditInput::mcp(
                TOOL_AGENT_RESPAWN,
                "respawn",
                caller.clone(),
                Some(target.session_id.clone()),
                payload.clone(),
                before.clone(),
                Value::Null,
                "pending",
            )
            .with_target(json!({ "spawn_id": prior_spawn_id, "agent_kind": target.agent_kind })),
        )?;

        let finalize_operator_panic =
            |stage: &'static str, error: &ErrorData| -> Result<(), ErrorData> {
                self.command_audit_final(
                    CommandAuditInput::mcp(
                        TOOL_AGENT_RESPAWN,
                        "respawn",
                        caller.clone(),
                        Some(target.session_id.clone()),
                        payload.clone(),
                        before.clone(),
                        json!({
                            "spawn_attempted": false,
                            "operator_panic_stage": stage,
                            "error": error,
                            "spawn_activity": super::m4_tools::agent_spawn_activity_readback(),
                        }),
                        "error",
                    )
                    .with_target(json!({
                        "spawn_id": prior_spawn_id.clone(),
                        "agent_kind": target.agent_kind.clone()
                    })),
                )?;
                Ok(())
            };
        if let Err(error) = respawn_activity.ensure("respawn_before_prior_kill") {
            finalize_operator_panic("respawn_before_prior_kill", &error)?;
            return Err(error);
        }

        // Kill the prior instance first when it is still live; an already-dead
        // prior is simply re-launched.
        let prior_already_dead = target.dead
            || process_readback_for_target(&target)
                .live_process_ids
                .is_empty();
        let prior_killed = if prior_already_dead {
            false
        } else {
            match self
                .agent_kill_impl(
                    AgentKillParams {
                        session_id: target.session_id.clone(),
                        grace_ms: params.grace_ms,
                        interrupt_first: true,
                    },
                    caller.as_deref(),
                )
                .await
            {
                Ok(kill) if kill.killed => true,
                Ok(kill) => {
                    let error = mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "AGENT_RESPAWN_PRIOR_KILL_FAILED: agent {} (session {}) was still live and agent_kill returned killed=false; no replacement was launched",
                            lookup, target.session_id
                        ),
                    );
                    let after = json!({
                        "spawn_attempted": false,
                        "prior_kill_failed": true,
                        "prior_kill": kill,
                        "error": error.message.to_string(),
                    });
                    self.command_audit_final(
                        CommandAuditInput::mcp(
                            TOOL_AGENT_RESPAWN,
                            "respawn",
                            caller.clone(),
                            Some(target.session_id.clone()),
                            payload.clone(),
                            before.clone(),
                            after,
                            "error",
                        )
                        .with_target(json!({
                            "spawn_id": prior_spawn_id.clone(),
                            "agent_kind": target.agent_kind.clone()
                        })),
                    )?;
                    return Err(error);
                }
                Err(source) => {
                    let source_message = source.message.to_string();
                    let error = mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "AGENT_RESPAWN_PRIOR_KILL_FAILED: prior agent kill failed before replacement spawn; no replacement was launched: {source_message}"
                        ),
                    );
                    let after = json!({
                        "spawn_attempted": false,
                        "prior_kill_failed": true,
                        "source_error": source_message,
                        "error": error.message.to_string(),
                    });
                    self.command_audit_final(
                        CommandAuditInput::mcp(
                            TOOL_AGENT_RESPAWN,
                            "respawn",
                            caller.clone(),
                            Some(target.session_id.clone()),
                            payload.clone(),
                            before.clone(),
                            after,
                            "error",
                        )
                        .with_target(json!({
                            "spawn_id": prior_spawn_id.clone(),
                            "agent_kind": target.agent_kind.clone()
                        })),
                    )?;
                    return Err(error);
                }
            }
        };
        if let Err(error) = respawn_activity.ensure("respawn_after_prior_kill") {
            finalize_operator_panic("respawn_after_prior_kill", &error)?;
            return Err(error);
        }

        let request: crate::m4::ActSpawnAgentRequest = serde_json::from_value(request_value)
            .map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("AGENT_RESPAWN_REQUEST_BUILD_FAILED: {error}"),
                )
            })?;
        let mut request = request;
        if let Err(error) = respawn_activity.ensure("respawn_immediately_before_replacement_spawn")
        {
            finalize_operator_panic("respawn_immediately_before_replacement_spawn", &error)?;
            return Err(error);
        }
        let spawned = if let Some(request_context) = request_context {
            self.spawn_agent_journaled(request, request_context).await?
        } else {
            if let Some(mcp_url) = dashboard_mcp_url {
                request.mcp_url = mcp_url;
            }
            self.dashboard_spawn_agent_request(request).await?
        };
        if let Err(error) = respawn_activity.ensure("respawn_after_replacement_spawn") {
            let cleanup = self
                .cleanup_spawn_response_after_operator_panic(
                    &spawned,
                    "respawn_after_replacement_spawn",
                )
                .await;
            self.command_audit_final(
                CommandAuditInput::mcp(
                    TOOL_AGENT_RESPAWN,
                    "respawn",
                    caller.clone(),
                    Some(target.session_id.clone()),
                    payload.clone(),
                    before.clone(),
                    json!({
                        "spawn_attempted": true,
                        "operator_panic_stage": "respawn_after_replacement_spawn",
                        "replacement_spawn_id": spawned.spawn_id,
                        "replacement_session_id": spawned.session_id,
                        "replacement_cleanup": cleanup,
                        "error": &error,
                        "spawn_activity": super::m4_tools::agent_spawn_activity_readback(),
                    }),
                    "error",
                )
                .with_target(json!({
                    "spawn_id": prior_spawn_id.clone(),
                    "agent_kind": target.agent_kind.clone()
                })),
            )?;
            return Err(error);
        }

        // Lineage: record on the prior agent that it was respawned into the new
        // spawn, so agent_query/the journal can trace the chain.
        let lineage_journal_event = self
            .journal_lifecycle_event(
                AgentEventKind::StateChanged,
                &target,
                TOOL_AGENT_RESPAWN,
                None,
                json!({
                    "operation": "respawned_into",
                    "new_spawn_id": spawned.spawn_id,
                    "new_session_id": spawned.session_id,
                    "prior_killed": prior_killed,
                }),
            )
            .ok();

        let response = AgentRespawnResponse {
            requested_id: lookup,
            prior_session_id: target.session_id.clone(),
            prior_spawn_id,
            prior_killed,
            prior_already_dead,
            manifest,
            carried_context,
            effective_prompt_chars: effective_prompt.chars().count(),
            new_session_id: spawned.session_id.clone(),
            new_spawn_id: spawned.spawn_id,
            lineage_journal_event,
        };

        let after = json!({
            "new_session_id": response.new_session_id,
            "new_spawn_id": response.new_spawn_id,
            "prior_killed": response.prior_killed,
            "prior_already_dead": response.prior_already_dead,
            "carried_context": response.carried_context,
        });
        self.command_audit_final(
            CommandAuditInput::mcp(
                TOOL_AGENT_RESPAWN,
                "respawn",
                caller,
                Some(target.session_id.clone()),
                payload,
                before,
                after,
                "ok",
            )
            .with_target(
                json!({ "spawn_id": response.prior_spawn_id, "agent_kind": target.agent_kind }),
            ),
        )?;

        Ok(response)
    }

    /// Reads the prior spawn's reusable identity from its `spawn-manifest.json`
    /// (kind/model/model_ref/working_dir). Errors if no manifest or working
    /// directory exists — respawn never fabricates a spawn identity or cwd.
    fn read_respawn_manifest(&self, target: &ResolvedAgent) -> Result<RespawnManifest, ErrorData> {
        let manifest_path =
            PathBuf::from(&target.log_dir).join(super::m4_tools::AGENT_SPAWN_MANIFEST_FILENAME);
        let bytes = fs::read(&manifest_path).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_RESPAWN_NO_MANIFEST: cannot read prior spawn manifest {}: {error}",
                    manifest_path.display()
                ),
            )
        })?;
        let manifest: Value = serde_json::from_slice(&bytes).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_RESPAWN_MANIFEST_INVALID: prior spawn manifest {} is not valid JSON: {error}",
                    manifest_path.display()
                ),
            )
        })?;
        let agent_kind = manifest
            .get("kind")
            .or_else(|| manifest.get("cli"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map_or_else(|| target.agent_kind.clone(), ToOwned::to_owned);
        let string_field = |key: &str| {
            manifest
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        };
        let working_dir = string_field("working_dir")
            .or_else(|| string_field("effective_working_dir"))
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "AGENT_RESPAWN_WORKING_DIR_MISSING: prior spawn manifest {} has no working_dir/effective_working_dir; refusing to respawn into the daemon default cwd",
                        manifest_path.display()
                    ),
                )
            })?;
        let working_dir_path = PathBuf::from(&working_dir);
        let canonical_working_dir = fs::canonicalize(&working_dir_path).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_RESPAWN_WORKING_DIR_INVALID: prior spawn working_dir {working_dir_path:?} from {} could not be resolved: {error}",
                    manifest_path.display()
                ),
            )
        })?;
        if !canonical_working_dir.is_dir() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_RESPAWN_WORKING_DIR_INVALID: prior spawn working_dir {canonical_working_dir:?} from {} is not a directory",
                    manifest_path.display()
                ),
            ));
        }
        Ok(RespawnManifest {
            agent_kind,
            model: string_field("model"),
            model_ref: string_field("model_ref"),
            working_dir: Some(canonical_working_dir.display().to_string()),
            source: super::m4_tools::AGENT_SPAWN_MANIFEST_FILENAME.to_owned(),
        })
    }

    // ------------------------------------------------------------------
    // operator panic hotkey
    // ------------------------------------------------------------------

    pub(crate) async fn operator_panic_kill_all(
        &self,
        immediate: OperatorHotkeyImmediateReport,
    ) -> Result<OperatorPanicKillAllResponse, ErrorData> {
        let mcp_mutation_activity_initial =
            super::operator_panic_boundary::mcp_mutation_activity_snapshot();
        // The extension gives operatorPanicDisable a priority admission lane:
        // it closes and persists mutation admission without waiting behind the
        // long normal-command tail. Then wait for every pre-K1 MCP mutation
        // reservation to finish. K1 prevents new reservations; stable zero is
        // therefore the terminal cut before all verdict-bearing owner drains
        // and fleet sweeps below.
        let (chrome_extension_disable, chrome_extension_disable_error) =
            match crate::chrome_debugger_bridge::operator_panic_disable().await {
                Ok(readback) => (Some(readback), None),
                Err(error) => (None, Some(chrome_debugger_bridge_error_detail(&error))),
            };
        let expected_chrome_extension_disable_sequence = chrome_extension_disable
            .as_ref()
            .map(|readback| readback.disable_sequence);
        let mcp_mutation_activity =
            wait_for_operator_panic_mcp_mutation_quiescence(mcp_mutation_activity_initial.clone())
                .await;

        // Remove extension-owned durable effects only after the request-wide
        // mutation registry reached its stable terminal cut. The independent
        // readback also authoritatively reconciles a response whose transport
        // disappeared after the extension had already completed its mutation.
        let (chrome_extension_cleanup, chrome_extension_cleanup_error) =
            match expected_chrome_extension_disable_sequence {
                Some(disable_sequence) => {
                    match crate::chrome_debugger_bridge::operator_panic_cleanup(disable_sequence)
                        .await
                    {
                        Ok(readback) => (Some(readback), None),
                        Err(error) => {
                            (None, Some(chrome_debugger_bridge_error_detail(&error)))
                        }
                    }
                }
                None => (
                    None,
                    Some(
                        "operatorPanicCleanup was not attempted because operatorPanicDisable did not return an exact generation"
                            .to_owned(),
                    ),
                ),
            };
        let (
            chrome_extension_cdp_target_owner_reconciliation,
            chrome_extension_cdp_target_owner_reconciliation_error,
        ) = match chrome_extension_cleanup.as_ref() {
            Some(cleanup) => {
                let closed_targets = cleanup
                    .closed_opened_tabs
                    .iter()
                    .map(|target| (target.tab_id, target.target_id.clone()))
                    .collect::<Vec<_>>();
                (
                    Some(
                        self.reconcile_operator_panic_extension_target_owners(
                            cleanup.expected_disable_sequence,
                            &closed_targets,
                        )
                        .await,
                    ),
                    None,
                )
            }
            None => (
                None,
                Some(
                    "M1 CDP target-owner reconciliation was not attempted because extension cleanup returned no physical-close readback"
                        .to_owned(),
                ),
            ),
        };
        let (chrome_extension_before_command_drain, chrome_extension_before_command_drain_error) =
            match crate::chrome_debugger_bridge::operator_panic_readback().await {
                Ok(readback) => (Some(readback), None),
                Err(error) => (None, Some(chrome_debugger_bridge_error_detail(&error))),
            };

        // K1 synchronously closed the daemon's durable browser-owner admission
        // gate. Drain those exact async owners after the extension itself is
        // closed and independently read back.
        let durable_browser_mutation_owners_drain =
            synapse_a11y::durable_browser_mutation_owners_disable_and_drain().await;
        let durable_browser_mutation_owners_after =
            synapse_a11y::durable_browser_mutation_owners_readback();
        let durable_browser_mutation_owners_terminal = durable_browser_mutation_owners_drain
            .fully_drained
            && durable_browser_mutation_owners_drain.failures.is_empty()
            && durable_browser_mutation_owners_drain.persisted_cdp_input_owners_drained
                == durable_browser_mutation_owners_drain.persisted_cdp_input_owners_found
            && durable_browser_mutation_owners_drain.persisted_cdp_evaluate_owners_drained
                == durable_browser_mutation_owners_drain.persisted_cdp_evaluate_owners_found
            && durable_browser_mutation_owners_drain
                .persisted_cdp_init_script_effect_owners_drained
                == durable_browser_mutation_owners_drain
                    .persisted_cdp_init_script_effect_owners_found
            && durable_browser_mutation_owners_drain.persisted_cdp_input_owners_remaining == 0
            && durable_browser_mutation_owners_drain.persisted_cdp_evaluate_owners_remaining == 0
            && durable_browser_mutation_owners_drain
                .persisted_cdp_init_script_effect_owners_remaining
                == 0
            && durable_browser_mutation_owners_drain.persisted_cdp_mutation_owners_remaining == 0
            && durable_browser_mutation_owners_drain
                .persisted_cdp_input_owners_found
                .checked_add(
                    durable_browser_mutation_owners_drain.persisted_cdp_evaluate_owners_found,
                )
                .and_then(|subtotal| {
                    subtotal.checked_add(
                        durable_browser_mutation_owners_drain
                            .persisted_cdp_init_script_effect_owners_found,
                    )
                })
                .is_some_and(|total| {
                    durable_browser_mutation_owners_drain.persisted_cdp_mutation_owners_found
                        == total
                })
            && durable_browser_mutation_owners_drain.readback
                == durable_browser_mutation_owners_after
            && !durable_browser_mutation_owners_after.enabled
            && durable_browser_mutation_owners_after.registry_readback_healthy
            && durable_browser_mutation_owners_after
                .registry_readback_failures
                .is_empty()
            && durable_browser_mutation_owners_after.fetch_interception_active_count == 0
            && durable_browser_mutation_owners_after.network_override_active_count == 0
            && durable_browser_mutation_owners_after.dialog_auto_policy_active_count == 0
            && durable_browser_mutation_owners_after.clock_active_count == 0
            && durable_browser_mutation_owners_after.init_script_active_count == 0
            && durable_browser_mutation_owners_after.unresolved_raw_cdp_evaluate_timeout_count == 0
            && durable_browser_mutation_owners_after.unresolved_raw_cdp_input_owner_count == 0
            && durable_browser_mutation_owners_after.persisted_cdp_mutation_owner_count == 0
            && durable_browser_mutation_owners_after.persisted_cdp_input_owner_count == 0
            && durable_browser_mutation_owners_after.persisted_cdp_evaluate_owner_count == 0
            && durable_browser_mutation_owners_after.persisted_cdp_init_script_effect_owner_count
                == 0;
        if !durable_browser_mutation_owners_terminal {
            synapse_action::record_operator_panic_safety_incident();
        }
        let chrome_mutation_command_owners_drain =
            crate::chrome_debugger_bridge::drain_mutation_command_owners(
                crate::safety::OPERATOR_PANIC_K2_STOP_TIMEOUT,
            )
            .await;
        let chrome_mutation_command_owners_after =
            crate::chrome_debugger_bridge::mutation_command_owners_readback();
        let chrome_mutation_command_owners_terminal = chrome_mutation_command_owners_drain
            .fully_drained
            && chrome_mutation_command_owners_drain.failures.is_empty()
            && chrome_mutation_command_owners_drain.readback
                == chrome_mutation_command_owners_after
            && chrome_mutation_command_owners_after.registry_readback_healthy
            && chrome_mutation_command_owners_after
                .registry_readback_failure
                .is_none()
            && chrome_mutation_command_owners_after.queued_mutation_count == 0
            && chrome_mutation_command_owners_after.delivered_mutation_count == 0
            && chrome_mutation_command_owners_after.delivered_caller_timed_out_count == 0
            && chrome_mutation_command_owners_after
                .delivered_command_ids
                .is_empty();
        let (chrome_extension_after_command_drain, chrome_extension_after_command_drain_error) =
            match crate::chrome_debugger_bridge::operator_panic_readback().await {
                Ok(readback) => (Some(readback), None),
                Err(error) => (None, Some(chrome_debugger_bridge_error_detail(&error))),
            };
        let mut chrome_extension_mutation_owners = OperatorPanicChromeExtensionOwnersReadback {
            disable: chrome_extension_disable,
            disable_error: chrome_extension_disable_error,
            cleanup: chrome_extension_cleanup,
            cleanup_error: chrome_extension_cleanup_error,
            cdp_target_owner_reconciliation: chrome_extension_cdp_target_owner_reconciliation,
            cdp_target_owner_reconciliation_error:
                chrome_extension_cdp_target_owner_reconciliation_error,
            before_command_drain: chrome_extension_before_command_drain,
            before_command_drain_error: chrome_extension_before_command_drain_error,
            after_command_drain: chrome_extension_after_command_drain,
            after_command_drain_error: chrome_extension_after_command_drain_error,
            terminal: false,
        };
        chrome_extension_mutation_owners.terminal =
            operator_panic_chrome_extension_terminal(&chrome_extension_mutation_owners);
        if !chrome_extension_mutation_owners.terminal || !chrome_mutation_command_owners_terminal {
            synapse_action::record_operator_panic_safety_incident();
        }
        let spawn_activity_before = super::m4_tools::agent_spawn_activity_readback();
        let prior_lease_owner_session_id = immediate
            .preempted_lease
            .as_ref()
            .and_then(|status| status.owner_session_id.clone())
            .filter(|session_id| {
                session_id.as_str() != synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID
            });
        let (matched_sessions_before, matched_sessions_before_error) =
            match self.live_spawned_agent_sessions(&[]) {
                Ok(sessions) => (sessions, None),
                Err(error) => (Vec::new(), Some(error.message.to_string())),
            };
        let payload = json!({
            "reason": "operator_hotkey",
            "mode": "kill",
            "grace_ms": 0,
            "prior_lease_owner_session_id": prior_lease_owner_session_id,
        });
        let before = json!({
            "source_of_truth": "synapse_action::lease + server::operator_panic_boundary MCP mutation registry + synapse_a11y durable browser-owner registry + Chrome debugger pending-command registry + extension chrome.storage.session durable-owner ledger/live readback + M1 CdpTargetOwner registry/CF_SESSIONS owner rows + CF_SESSIONS session lease row + session registry/agent_state + OS process table + reflex scheduler/emitter state + CF_ACTION_LOG",
            "immediate": &immediate,
            "matched_sessions_before": &matched_sessions_before,
            "matched_sessions_before_error": &matched_sessions_before_error,
            "spawn_activity_before": &spawn_activity_before,
            "mcp_mutation_activity_initial": &mcp_mutation_activity_initial,
            "chrome_extension_mutation_owners": &chrome_extension_mutation_owners,
            "durable_browser_mutation_owners_drain": &durable_browser_mutation_owners_drain,
            "durable_browser_mutation_owners_after": &durable_browser_mutation_owners_after,
            "durable_browser_mutation_owners_terminal": durable_browser_mutation_owners_terminal,
            "chrome_mutation_command_owners_drain": &chrome_mutation_command_owners_drain,
            "chrome_mutation_command_owners_after": &chrome_mutation_command_owners_after,
            "chrome_mutation_command_owners_terminal": chrome_mutation_command_owners_terminal,
        });
        let audit_intent_error = self
            .command_audit_intent(
                CommandAuditInput::mcp(
                    "operator_hotkey",
                    "panic_kill_all",
                    Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID.to_owned()),
                    None,
                    payload.clone(),
                    before.clone(),
                    Value::Null,
                    "pending",
                )
                .with_channel("operator_hotkey"),
            )
            .err()
            .map(|error| error.message.to_string());

        let (prior_lease_row_cleanup, prior_lease_row_cleanup_error) =
            match prior_lease_owner_session_id.as_deref() {
                Some(owner) => match super::session_continuity::delete_persisted_session_lease_row(
                    &self.m3_state_handle(),
                    owner,
                ) {
                    Ok(readback) => (Some(readback), None),
                    Err(error) => (None, Some(error)),
                },
                None => (None, None),
            };

        let mut spawn_activity_baseline = spawn_activity_before.clone();
        let mut spawn_activity_after = spawn_activity_before.clone();
        let mut spawn_activity_stable = false;
        let mut fleet_stop_rounds = 0_u32;
        let mut fleet_stop = None;
        let mut fleet_stop_errors = Vec::new();
        while fleet_stop_rounds < OPERATOR_PANIC_FLEET_STOP_MAX_STABLE_ROUNDS {
            fleet_stop_rounds = fleet_stop_rounds.saturating_add(1);
            let mut round_failed = false;
            match self
                .fleet_stop_impl(
                    FleetStopParams {
                        mode: "kill".to_owned(),
                        confirm: FLEET_STOP_CONFIRM.to_owned(),
                        agent_kinds: Vec::new(),
                        grace_ms: 0,
                    },
                    Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID),
                )
                .await
            {
                Ok(response) => fleet_stop = Some(response),
                Err(error) => {
                    round_failed = true;
                    fleet_stop_errors.push(format!("round {fleet_stop_rounds}: {}", error.message));
                }
            }
            let (observed, quiescent_readback_stable) =
                wait_for_operator_panic_spawn_quiescence().await;
            spawn_activity_stable = operator_panic_spawn_activity_stable(
                &spawn_activity_baseline,
                &observed,
                quiescent_readback_stable,
            );
            spawn_activity_after = observed;
            if spawn_activity_stable
                || round_failed
                || !quiescent_readback_stable
                || spawn_activity_after.in_flight != 0
                || spawn_activity_after.cleanup_incident
            {
                break;
            }
            // A racing spawn entered during this fleet sweep, but admission
            // forced it to abort/clean up. Sweep once more from that now-stable
            // sequence so no process can live between the fleet snapshot and
            // K2 completion.
            spawn_activity_baseline = spawn_activity_after.clone();
        }
        let fleet_stop_error =
            (!fleet_stop_errors.is_empty()).then(|| fleet_stop_errors.join(" | "));

        let (final_safety_sweep, final_safety_sweep_error) = if mcp_mutation_activity.quiescent
            && mcp_mutation_activity.error.is_none()
        {
            (Some(operator_panic_final_safety_sweep(self).await), None)
        } else {
            (
                    None,
                    Some(
                        "final K2 ReleaseAll/reflex/emitter sweep was not terminally ordered because MCP mutation activity did not reach stable zero"
                            .to_owned(),
                    ),
                )
        };

        // K2 intentionally retains the generation-tagged operator lease. The
        // unique last-completer token in safety.rs clears only its exact
        // generation after this transaction and its audit are proven terminal.
        let operator_lease_cleared = None;
        let lease_after = final_safety_sweep.as_ref().map_or_else(
            || synapse_action::input_lease_safety_snapshot().status,
            |sweep| sweep.lease_after.clone(),
        );
        let operator_lease_generation_after = final_safety_sweep
            .as_ref()
            .map_or_else(synapse_action::operator_panic_lease_generation, |sweep| {
                sweep.operator_lease_generation_after
            });
        let operator_panic_safety_after = synapse_action::operator_panic_safety_readback();
        let (live_sessions_after, live_sessions_after_error) =
            match self.live_spawned_agent_sessions(&[]) {
                Ok(sessions) => (sessions, None),
                Err(error) => (Vec::new(), Some(error.message.to_string())),
            };
        let all_stopped = fleet_stop
            .as_ref()
            .is_some_and(|response| response.all_stopped)
            && live_sessions_after.is_empty()
            && immediate.k1_safety_terminal
            && matched_sessions_before_error.is_none()
            && prior_lease_row_cleanup_error.is_none()
            && fleet_stop_error.is_none()
            && live_sessions_after_error.is_none()
            && audit_intent_error.is_none()
            && spawn_activity_stable
            && mcp_mutation_activity.quiescent
            && mcp_mutation_activity.error.is_none()
            && final_safety_sweep_error.is_none()
            && final_safety_sweep
                .as_ref()
                .is_some_and(|sweep| sweep.terminal)
            && chrome_extension_mutation_owners.terminal
            && durable_browser_mutation_owners_terminal
            && chrome_mutation_command_owners_terminal
            && operator_panic_safety_after.pending
            && !operator_panic_safety_after.accounting_incident
            && operator_panic_k2_lease_retained(
                immediate.operator_panic_generation,
                operator_panic_safety_after.epoch,
                operator_lease_generation_after,
                &lease_after,
            );

        let mut response = OperatorPanicKillAllResponse {
            immediate,
            chrome_extension_mutation_owners,
            durable_browser_mutation_owners_drain,
            durable_browser_mutation_owners_after,
            durable_browser_mutation_owners_terminal,
            chrome_mutation_command_owners_drain,
            chrome_mutation_command_owners_after,
            chrome_mutation_command_owners_terminal,
            prior_lease_owner_session_id,
            prior_lease_row_cleanup,
            prior_lease_row_cleanup_error,
            matched_sessions_before,
            matched_sessions_before_error,
            fleet_stop,
            fleet_stop_error,
            operator_lease_cleared,
            lease_after,
            operator_panic_safety_after,
            live_sessions_after,
            live_sessions_after_error,
            spawn_activity_before,
            spawn_activity_after,
            spawn_activity_stable,
            mcp_mutation_activity,
            final_safety_sweep,
            final_safety_sweep_error,
            fleet_stop_rounds,
            all_stopped,
            audit_intent_error,
            audit_final_error: None,
        };

        let after = json!({
            "source_of_truth": "synapse_action::lease + server::operator_panic_boundary MCP mutation registry + synapse_a11y durable browser-owner registry + Chrome debugger pending-command registry + extension chrome.storage.session durable-owner ledger/live readback + M1 CdpTargetOwner registry/CF_SESSIONS owner rows + CF_SESSIONS session lease row + session registry/agent_state + OS process table + reflex scheduler/emitter state + CF_ACTION_LOG",
            "response": &response,
        });
        response.audit_final_error = self
            .command_audit_final(
                CommandAuditInput::mcp(
                    "operator_hotkey",
                    "panic_kill_all",
                    Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID.to_owned()),
                    None,
                    payload,
                    before,
                    after,
                    if response.all_stopped { "ok" } else { "error" },
                )
                .with_channel("operator_hotkey"),
            )
            .err()
            .map(|error| error.message.to_string());
        if response.audit_final_error.is_some() {
            response.all_stopped = false;
        }

        if response.audit_intent_error.is_some() || response.audit_final_error.is_some() {
            tracing::error!(
                code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                audit_intent_error = ?response.audit_intent_error,
                audit_final_error = ?response.audit_final_error,
                "operator hotkey panic audit write had errors"
            );
        }
        tracing::warn!(
            code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
            matched_before = response.matched_sessions_before.len(),
            matched_after = response.live_sessions_after.len(),
            fleet_all_stopped = response
                .fleet_stop
                .as_ref()
                .is_some_and(|fleet| fleet.all_stopped),
            lease_after_held = response.lease_after.held,
            mcp_mutation_activity_quiescent = response.mcp_mutation_activity.quiescent,
            final_safety_sweep_terminal = response
                .final_safety_sweep
                .as_ref()
                .is_some_and(|sweep| sweep.terminal),
            chrome_extension_mutation_owners_terminal =
                response.chrome_extension_mutation_owners.terminal,
            durable_browser_mutation_owners_terminal =
                response.durable_browser_mutation_owners_terminal,
            chrome_mutation_command_owners_terminal =
                response.chrome_mutation_command_owners_terminal,
            all_stopped = response.all_stopped,
            "operator hotkey K2 fleet kill completed"
        );
        Ok(response)
    }

    // ------------------------------------------------------------------
    // agent_kill
    // ------------------------------------------------------------------

    async fn agent_kill_impl(
        &self,
        params: AgentKillParams,
        caller_session: Option<&str>,
    ) -> Result<AgentKillResponse, ErrorData> {
        let lookup = validate_lookup_id(&params.session_id, TOOL_AGENT_KILL)?;
        if params.grace_ms > MAX_KILL_GRACE_MS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_KILL_GRACE_INVALID: grace_ms must be 0..={MAX_KILL_GRACE_MS}, got {}",
                    params.grace_ms
                ),
            ));
        }
        let target = self.resolve_spawned_agent(&lookup, TOOL_AGENT_KILL)?;
        let process_before = process_readback_for_target(&target);
        let already_dead = process_before.live_process_ids.is_empty();

        let payload = json!({
            "requested_id": lookup,
            "grace_ms": params.grace_ms,
            "interrupt_first": params.interrupt_first,
            "from": caller_session,
        });
        let before = json!({ "process": &process_before, "lifecycle": target.lifecycle });
        self.command_audit_intent(
            CommandAuditInput::mcp(
                TOOL_AGENT_KILL,
                "kill",
                caller_session.map(ToOwned::to_owned),
                Some(target.session_id.clone()),
                payload.clone(),
                before.clone(),
                Value::Null,
                "pending",
            )
            .with_target(json!({ "spawn_id": target.spawn_id, "agent_kind": target.agent_kind })),
        )?;

        // Graceful first: attempt the interrupt (best-effort — its failure must
        // never block the force-kill), then wait the grace window for the tree
        // to exit on its own. Skipped entirely when already dead.
        let interrupt = if params.interrupt_first && !already_dead {
            self.interrupt_core(&target.session_id.clone(), &target, caller_session)
                .ok()
        } else {
            None
        };
        let natural_exit = if !already_dead && params.grace_ms > 0 {
            let (remaining, _waited) =
                wait_for_tree_exit_async(&process_before.process_tree_ids, params.grace_ms).await;
            remaining.is_empty()
        } else {
            false
        };

        // Was a force-kill actually required? (the tree is still alive)
        let live_after_grace = crate::m4::owned_live_process_ids(&process_before.process_tree_ids);
        let force_needed = !live_after_grace.is_empty();

        // Journal the durable `killed` event BEFORE teardown when a force-kill
        // is required, so the terminal transition is recorded as killed (not a
        // generic exit). The reducer treats a dead agent as dead, so teardown's
        // later `exited` row is absorbed without a spurious transition.
        let journal_killed_event = if force_needed {
            Some(self.journal_lifecycle_event(
                AgentEventKind::Killed,
                &target,
                "agent_kill",
                Some(AgentEndState::Error),
                json!({
                    "process_before": &process_before,
                    "live_before_force": live_after_grace,
                    "grace_ms": params.grace_ms,
                    "interrupt_first": params.interrupt_first,
                }),
            )?)
        } else {
            None
        };

        // Reuse the authoritative per-session teardown: job-close → force kill
        // of the process tree, plus lease/claim/desktop release and registry
        // close. Keyed by the agent's OWN session id, which owns all of it.
        let lifecycle = self.session_lifecycle_state()?;
        let teardown_report = lifecycle
            .teardown_session_with_options_report(
                &target.session_id,
                "agent_kill",
                SessionTeardownOptions::explicit_kill(),
            )
            .await?;
        let teardown_failure_summary = summarize_teardown_failures(&teardown_report);
        let teardown_error = teardown_failure_summary
            .as_ref()
            .map(|summary| format_teardown_failure_error(&teardown_report, summary));
        let teardown = Some(teardown_report);

        // Source of truth for "is it dead": re-read the OS process table.
        let mut process_after = process_readback_for_target(&target);
        let mut orphan_process_ids =
            merged_live_process_ids(&process_before.process_tree_ids, &process_after);
        let post_teardown_force_termination = if orphan_process_ids.is_empty() {
            None
        } else {
            Some(crate::m4::terminate_owned_process_ids(&orphan_process_ids))
        };
        if post_teardown_force_termination.is_some() {
            process_after = process_readback_for_target(&target);
            orphan_process_ids =
                merged_live_process_ids(&process_before.process_tree_ids, &process_after);
        }
        let killed = orphan_process_ids.is_empty();
        let (completion_artifact_cleanup_status, completion_artifact_cleanup_error) =
            if killed && post_teardown_force_termination.is_some() {
                match write_agent_kill_restart_completion_artifact(
                    &target,
                    &process_before,
                    &orphan_process_ids,
                    post_teardown_force_termination.as_ref(),
                ) {
                    Ok(status) => (Some(status), None),
                    Err(error) => (None, Some(error)),
                }
            } else {
                (None, None)
            };

        let response = AgentKillResponse {
            requested_id: lookup,
            session_id: target.session_id.clone(),
            spawn_id: target.spawn_id.clone(),
            agent_kind: target.agent_kind.clone(),
            resolution_source: target.resolution_source.clone(),
            already_dead,
            interrupt,
            grace_ms: params.grace_ms,
            natural_exit,
            process_before,
            process_after,
            orphan_process_ids,
            killed,
            post_teardown_force_termination,
            completion_artifact_cleanup_status,
            completion_artifact_cleanup_error,
            journal_killed_event,
            teardown,
            teardown_failure_summary,
            teardown_error,
        };

        let after = json!({
            "killed": response.killed,
            "already_dead": response.already_dead,
            "natural_exit": response.natural_exit,
            "orphan_process_ids": response.orphan_process_ids,
            "process_after": response.process_after,
            "resolution_source": response.resolution_source,
            "teardown_failure_summary": response.teardown_failure_summary,
            "teardown_error": response.teardown_error,
            "post_teardown_force_termination": response.post_teardown_force_termination,
            "completion_artifact_cleanup_status": response.completion_artifact_cleanup_status,
            "completion_artifact_cleanup_error": response.completion_artifact_cleanup_error,
        });
        self.command_audit_final(
            CommandAuditInput::mcp(
                TOOL_AGENT_KILL,
                "kill",
                caller_session.map(ToOwned::to_owned),
                Some(target.session_id.clone()),
                payload,
                before,
                after,
                if response.killed { "ok" } else { "error" },
            )
            .with_target(json!({ "spawn_id": target.spawn_id, "agent_kind": target.agent_kind })),
        )?;

        if !response.killed {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "AGENT_KILL_ORPHANS: agent {} (session {}) still has live processes after teardown: {:?}{}. The kill is reported as failed; these pids survived.",
                    response.requested_id,
                    response.session_id,
                    response.orphan_process_ids,
                    response
                        .teardown_error
                        .as_ref()
                        .map(|error| format!(" (teardown error: {error})"))
                        .unwrap_or_default(),
                ),
            ));
        }
        Ok(response)
    }

    // ------------------------------------------------------------------
    // fleet_stop
    // ------------------------------------------------------------------

    async fn fleet_stop_impl(
        &self,
        params: FleetStopParams,
        caller_session: Option<&str>,
    ) -> Result<FleetStopResponse, ErrorData> {
        let mode = params.mode.trim().to_ascii_lowercase();
        if mode != "kill" && mode != "interrupt" {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "FLEET_STOP_MODE_INVALID: mode must be \"kill\" or \"interrupt\", got {:?}",
                    params.mode
                ),
            ));
        }
        if params.confirm != FLEET_STOP_CONFIRM {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "FLEET_STOP_CONFIRM_REQUIRED: fleet_stop is destructive and requires confirm=\"{FLEET_STOP_CONFIRM}\""
                ),
            ));
        }
        if params.grace_ms > MAX_KILL_GRACE_MS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "FLEET_STOP_GRACE_INVALID: grace_ms must be 0..={MAX_KILL_GRACE_MS}, got {}",
                    params.grace_ms
                ),
            ));
        }

        // Snapshot the matched live agents, then drop the registry lock BEFORE
        // stopping any (the stop path re-locks the registry to resolve).
        let matched_sessions = self.live_spawned_agent_sessions(&params.agent_kinds)?;

        let payload = json!({
            "mode": mode,
            "agent_kinds": params.agent_kinds,
            "grace_ms": params.grace_ms,
            "from": caller_session,
        });
        let before =
            json!({ "matched_sessions": matched_sessions, "matched": matched_sessions.len() });
        let verb = if mode == "kill" {
            "fleet_kill"
        } else {
            "fleet_interrupt"
        };
        self.command_audit_intent(CommandAuditInput::mcp(
            TOOL_FLEET_STOP,
            verb,
            caller_session.map(ToOwned::to_owned),
            None,
            payload.clone(),
            before.clone(),
            Value::Null,
            "pending",
        ))?;

        let agents: Vec<FleetStopAgentOutcome> =
            join_all(matched_sessions.iter().map(|session_id| {
                self.fleet_stop_one(&mode, session_id, params.grace_ms, caller_session)
            }))
            .await;

        let succeeded = agents.iter().filter(|outcome| outcome.ok).count();
        let failed = agents.len() - succeeded;
        let response = FleetStopResponse {
            mode: mode.clone(),
            matched: agents.len(),
            succeeded,
            failed,
            all_stopped: failed == 0,
            agents,
        };

        let after = json!({
            "matched": response.matched,
            "succeeded": response.succeeded,
            "failed": response.failed,
            "all_stopped": response.all_stopped,
            "agents": response.agents,
        });
        self.command_audit_final(CommandAuditInput::mcp(
            TOOL_FLEET_STOP,
            verb,
            caller_session.map(ToOwned::to_owned),
            None,
            payload,
            before,
            after,
            if response.all_stopped { "ok" } else { "error" },
        ))?;

        Ok(response)
    }

    /// Stops one agent for a fleet sweep, mapping any error to a loud per-agent
    /// outcome rather than aborting the whole sweep.
    async fn fleet_stop_one(
        &self,
        mode: &str,
        session_id: &str,
        grace_ms: u64,
        caller_session: Option<&str>,
    ) -> FleetStopAgentOutcome {
        if mode == "kill" {
            match self
                .agent_kill_impl(
                    AgentKillParams {
                        session_id: session_id.to_owned(),
                        grace_ms,
                        interrupt_first: true,
                    },
                    caller_session,
                )
                .await
            {
                Ok(kill) => FleetStopAgentOutcome {
                    session_id: kill.session_id,
                    spawn_id: kill.spawn_id,
                    agent_kind: kill.agent_kind,
                    ok: kill.killed,
                    reason: if kill.already_dead {
                        "already_dead".to_owned()
                    } else if kill.natural_exit {
                        "exited_during_grace".to_owned()
                    } else {
                        "force_killed".to_owned()
                    },
                    surviving_process_ids: kill.orphan_process_ids,
                },
                Err(error) => FleetStopAgentOutcome {
                    session_id: session_id.to_owned(),
                    spawn_id: None,
                    agent_kind: "unknown".to_owned(),
                    ok: false,
                    reason: error.message.to_string(),
                    surviving_process_ids: Vec::new(),
                },
            }
        } else {
            let service = self.clone();
            let worker_session_id = session_id.to_owned();
            let outcome_session_id = worker_session_id.clone();
            let caller_session = caller_session.map(ToOwned::to_owned);
            match tokio::task::spawn_blocking(move || {
                service.agent_interrupt_impl(
                    AgentInterruptParams {
                        session_id: worker_session_id,
                    },
                    caller_session.as_deref(),
                )
            })
            .await
            {
                Ok(Ok(interrupt)) => FleetStopAgentOutcome {
                    session_id: interrupt.session_id,
                    spawn_id: interrupt.spawn_id,
                    agent_kind: interrupt.agent_kind,
                    ok: interrupt.delivered,
                    reason: interrupt
                        .delivered_via
                        .unwrap_or_else(|| "no_channel_delivered".to_owned()),
                    surviving_process_ids: Vec::new(),
                },
                Ok(Err(error)) => FleetStopAgentOutcome {
                    session_id: outcome_session_id,
                    spawn_id: None,
                    agent_kind: "unknown".to_owned(),
                    ok: false,
                    reason: error.message.to_string(),
                    surviving_process_ids: Vec::new(),
                },
                Err(error) => FleetStopAgentOutcome {
                    session_id: outcome_session_id,
                    spawn_id: None,
                    agent_kind: "unknown".to_owned(),
                    ok: false,
                    reason: blocking_worker_join_error(TOOL_FLEET_STOP, error)
                        .message
                        .to_string(),
                    surviving_process_ids: Vec::new(),
                },
            }
        }
    }

    /// Snapshots the session ids of every live spawned agent (optionally
    /// filtered by registry `agent_kind`). The registry lock is released before
    /// the caller stops anyone.
    fn live_spawned_agent_sessions(
        &self,
        agent_kinds: &[String],
    ) -> Result<Vec<String>, ErrorData> {
        let now = unix_time_ms_now();
        let registry = self.session_registry.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while enumerating the live fleet",
            )
        })?;
        let mut sessions = BTreeSet::new();
        for read in registry.reads(now) {
            if read.spawned_agent.is_none() || read.lifecycle == "closed" {
                continue;
            }
            if !agent_kinds.is_empty() && !agent_kinds.iter().any(|kind| kind == &read.agent_kind) {
                continue;
            }
            sessions.insert(read.session_id.clone());
        }
        drop(registry);
        for read in super::agent_state::reads(now) {
            if read.spawn_id.is_none()
                || read.state == super::agent_state::AgentLifecycleState::Dead
            {
                continue;
            }
            let Some(session_id) = read.session_id else {
                continue;
            };
            let agent_kind = read.agent_kind.unwrap_or_else(|| "unknown".to_owned());
            if !agent_kinds.is_empty() && !agent_kinds.iter().any(|kind| kind == &agent_kind) {
                continue;
            }
            sessions.insert(session_id);
        }
        Ok(sessions.into_iter().collect())
    }

    // ------------------------------------------------------------------
    // Shared helpers
    // ------------------------------------------------------------------

    /// Locates a spawned agent in the live session registry by its own session
    /// id or its `agent-spawn-*` id. Errors structurally for unknown ids and
    /// for known sessions that are not Synapse-spawned (no owned process tree).
    fn resolve_spawned_agent(&self, lookup: &str, tool: &str) -> Result<ResolvedAgent, ErrorData> {
        let now = unix_time_ms_now();
        let registry = self.session_registry.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while resolving agent target",
            )
        })?;
        let mut session_match = None;
        let mut non_spawned_session_hit = false;
        for read in registry.reads(now) {
            let Some(spawned) = read.spawned_agent.as_ref() else {
                if read.session_id == lookup {
                    non_spawned_session_hit = true;
                }
                continue;
            };
            if read.session_id == lookup || spawned.spawn_id == lookup {
                session_match = Some(ResolvedAgent {
                    session_id: read.session_id.clone(),
                    spawn_id: Some(spawned.spawn_id.clone()),
                    agent_kind: read.agent_kind.clone(),
                    lifecycle: read.lifecycle.clone(),
                    resolution_source: "session_registry".to_owned(),
                    dead: read.lifecycle == "closed",
                    launcher_process_id: spawned.launcher_process_id,
                    agent_process_id: spawned.agent_process_id,
                    log_dir: spawned.log_dir.clone(),
                    control: spawned.control.clone(),
                });
                break;
            }
        }
        drop(registry);

        if let Some(resolved) = session_match {
            return Ok(resolved);
        }
        if let Some(state_read) = super::agent_state::read_for_session(lookup, now) {
            if let Some(resolved) =
                resolved_agent_from_durable_state_read(lookup, tool, state_read)?
            {
                return Ok(resolved);
            }
        }
        if non_spawned_session_hit {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_NOT_SPAWNED: session {lookup} exists but is not a Synapse-spawned agent; {tool} owns no process tree for it"
                ),
            ));
        }
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "AGENT_NOT_FOUND: no live spawned agent resolves to '{lookup}' (try its MCP session id or agent-spawn-* id from act_spawn_agent / agent_query)"
            ),
        ))
    }

    /// Writes a durable lifecycle journal row (`Interrupted` / `Killed`) for an
    /// agent and returns its physical readback. Mirrors the attribution that
    /// `session_lifecycle` uses for its `Exited` rows.
    fn journal_lifecycle_event(
        &self,
        kind: AgentEventKind,
        target: &ResolvedAgent,
        reason_code: &str,
        end_state: Option<AgentEndState>,
        payload: Value,
    ) -> Result<JournalReadback, ErrorData> {
        let db = self.agent_control_db()?;
        let mut record = AgentEventRecord::new(unix_time_ns_now(), kind);
        record.session_id = Some(target.session_id.clone());
        record.spawn_id.clone_from(&target.spawn_id);
        record.reason_code = Some(reason_code.to_owned());
        record.end_state = end_state;
        record.attributes.conversation_id = Some(target.session_id.clone());
        if target.agent_kind != "unknown" {
            record.attributes.agent_name = Some(target.agent_kind.clone());
        }
        record.payload = payload;
        let readback = record_agent_event_durable(&db, &record)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        Ok(JournalReadback {
            kind: format!("{kind:?}"),
            ts_ns: readback.ts_ns,
            seq: readback.seq,
            value_len_bytes: readback.value_len_bytes as u64,
        })
    }

    /// Opens the shared M3 storage handle (same path `agent_query` uses).
    fn agent_control_db(&self) -> Result<std::sync::Arc<synapse_storage::Db>, ErrorData> {
        let state = self.m3_state_handle();
        let mut guard = state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening agent control storage",
            )
        })?;
        guard
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }
}

fn process_readback_for_target(target: &ResolvedAgent) -> ProcessReadback {
    if target.launcher_process_id == 0 {
        return ProcessReadback {
            launcher_process_id: 0,
            process_tree_ids: Vec::new(),
            live_process_ids: Vec::new(),
        };
    }
    let mut process_tree_ids = crate::m4::owned_process_tree_ids(target.launcher_process_id);
    if let Some(agent_pid) = target.agent_process_id {
        process_tree_ids.extend(crate::m4::owned_process_tree_ids(agent_pid));
    }
    process_tree_ids.sort_unstable();
    process_tree_ids.dedup();
    let live_process_ids = crate::m4::owned_live_process_ids(&process_tree_ids);
    ProcessReadback {
        launcher_process_id: target.launcher_process_id,
        process_tree_ids,
        live_process_ids,
    }
}

fn merged_live_process_ids(before_ids: &[u32], after: &ProcessReadback) -> Vec<u32> {
    let mut ids = before_ids.to_vec();
    ids.extend(after.process_tree_ids.iter().copied());
    ids.sort_unstable();
    ids.dedup();
    crate::m4::owned_live_process_ids(&ids)
}

fn resolved_agent_from_durable_state_read(
    lookup: &str,
    tool: &str,
    read: super::agent_state::AgentStateRead,
) -> Result<Option<ResolvedAgent>, ErrorData> {
    let Some(spawn_id) = read.spawn_id.clone() else {
        return Ok(None);
    };
    let session_matches = read.session_id.as_deref() == Some(lookup);
    let spawn_matches = spawn_id == lookup || read.anchor == lookup;
    if !session_matches && !spawn_matches {
        return Ok(None);
    }
    let terminal_dead = read.state == super::agent_state::AgentLifecycleState::Dead;
    let session_id = match read.session_id.clone() {
        Some(session_id) => session_id,
        None if terminal_dead => spawn_id.clone(),
        None => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_NOT_READY: {tool} resolved spawn {spawn_id} from durable agent state but no MCP session id is linked yet"
                ),
            ));
        }
    };
    let launcher_process_id = match read.launcher_process_id {
        Some(launcher_process_id) => launcher_process_id,
        None if terminal_dead => 0,
        None => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_PROCESS_UNAVAILABLE: {tool} resolved spawn {spawn_id} from durable agent state but no launcher process id is recorded"
                ),
            ));
        }
    };
    if session_id == spawn_id && terminal_dead {
        tracing::info!(
            code = "AGENT_KILL_ALREADY_DEAD_UNLINKED",
            spawn_id,
            tool,
            "durable terminal spawn has no session registration; resolving as already dead"
        );
    }
    let log_dir = read.log_dir.clone().unwrap_or_else(|| {
        super::m4_tools::agent_spawn_root_dir()
            .ok()
            .map(|root| root.join(&spawn_id).display().to_string())
            .unwrap_or_default()
    });
    Ok(Some(ResolvedAgent {
        session_id,
        spawn_id: Some(spawn_id),
        agent_kind: read.agent_kind.unwrap_or_else(|| "unknown".to_owned()),
        lifecycle: format!("agent_state:{}", read.state.as_str()),
        resolution_source: "durable_agent_state".to_owned(),
        dead: read.state == super::agent_state::AgentLifecycleState::Dead,
        launcher_process_id,
        agent_process_id: read.agent_process_id,
        log_dir,
        control: None,
    }))
}

fn write_agent_kill_restart_completion_artifact(
    target: &ResolvedAgent,
    process_before: &ProcessReadback,
    remaining_process_ids_after: &[u32],
    termination: Option<&crate::m4::OwnedProcessTerminationReadback>,
) -> Result<String, String> {
    let Some(spawn_id) = target.spawn_id.as_deref() else {
        return Ok("not_spawned_agent".to_owned());
    };
    let log_dir = if target.log_dir.trim().is_empty() {
        super::m4_tools::agent_spawn_root_dir()
            .map_err(|error| {
                format!(
                    "failed to resolve agent spawn root for restart artifact: {}",
                    error.message
                )
            })?
            .join(spawn_id)
    } else {
        PathBuf::from(&target.log_dir)
    };
    fs::create_dir_all(&log_dir).map_err(|error| {
        format!(
            "failed to create restart artifact log dir {}: {error}",
            log_dir.display()
        )
    })?;
    let stdout_path = log_dir.join("stdout.jsonl");
    let stderr_path = log_dir.join("stderr.log");
    let final_message_path = log_dir.join("final-message.txt");
    let completion_status_path = log_dir.join("completion-status.json");
    let completion_status_before_cleanup = spawn_completion_status_value(&completion_status_path);
    let completion_status_before_cleanup_status = completion_status_before_cleanup
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let overwrite_wrapper_fallback = completion_status_before_cleanup
        .as_ref()
        .is_some_and(|value| restart_kill_can_overwrite_wrapper_fallback_status(value));
    if completion_status_before_cleanup_status
        .as_deref()
        .is_some_and(|status| status != "running")
        && !overwrite_wrapper_fallback
    {
        return Ok("already_terminal".to_owned());
    }
    let final_message_len_before = spawn_file_len(&final_message_path);
    let stdout_len = spawn_file_len(&stdout_path);
    let stderr_len = spawn_file_len(&stderr_path);
    let (stdout_line_count, last_stdout_event_type) = spawn_stdout_summary(&stdout_path);
    let status = "agent_kill_forced_after_daemon_restart";
    let error_message =
        "spawned agent was force-killed after daemon restart without a live process job handle";
    let details = json!({
        "reason": "agent_kill_restart_fallback_without_process_job",
        "session_id": target.session_id,
        "spawn_id": spawn_id,
        "agent_kind": target.agent_kind,
        "resolution_source": target.resolution_source,
        "launcher_process_id": target.launcher_process_id,
        "agent_process_id": target.agent_process_id,
        "process_before": process_before,
        "remaining_process_ids_after": remaining_process_ids_after,
        "post_teardown_force_termination": termination,
        "completion_status_before_cleanup": completion_status_before_cleanup,
    });
    let write_final_message = final_message_len_before == 0 || overwrite_wrapper_fallback;
    if write_final_message {
        let final_message = json!({
            "schema_version": 1,
            "spawn_id": spawn_id,
            "cli": target.agent_kind,
            "status": status,
            "exit_code": null,
            "error_message": error_message,
            "message": "Synapse agent_kill wrote this terminal artifact after reclaiming a daemon-restart-handoff spawn without a live process job handle.",
            "stdout_path": stdout_path.display().to_string(),
            "stderr_path": stderr_path.display().to_string(),
            "completion_status_path": completion_status_path.display().to_string(),
            "details": details,
        });
        let bytes = serde_json::to_vec_pretty(&final_message)
            .map_err(|error| format!("failed to encode restart final-message artifact: {error}"))?;
        fs::write(&final_message_path, bytes).map_err(|error| {
            format!(
                "failed to write restart final-message artifact {}: {error}",
                final_message_path.display()
            )
        })?;
    }
    let final_message_len_after = spawn_file_len(&final_message_path);
    let completion_status = json!({
        "schema_version": 1,
        "spawn_id": spawn_id,
        "cli": target.agent_kind,
        "status": status,
        "exit_code": null,
        "error_message": error_message,
        "wrapper_started_at_unix_ms": null,
        "completed_at_unix_ms": unix_time_ms_now(),
        "elapsed_ms": null,
        "requested_hold_open_ms": null,
        "hold_open_elapsed_ms_met": false,
        "final_message_path": final_message_path.display().to_string(),
        "final_message_bytes": final_message_len_after,
        "final_message_present": final_message_len_after > 0,
        "final_message_source": if write_final_message {
            "agent_kill_restart_artifact_json"
        } else {
            "preexisting_final_message_without_terminal_status"
        },
        "recovered_final_message_written": false,
        "fallback_final_message_written": write_final_message,
        "stdout_path": stdout_path.display().to_string(),
        "stdout_line_count": stdout_line_count,
        "last_stdout_event_type": last_stdout_event_type,
        "stdout_bytes": stdout_len,
        "stderr_path": stderr_path.display().to_string(),
        "stderr_bytes": stderr_len,
        "daemon_terminal_artifact": true,
        "agent_kill_restart_artifact": true,
        "completion_status_source": "agent_kill_restart_artifact_json",
        "completion_status_before_cleanup": completion_status_before_cleanup,
        "log_dir": log_dir.display().to_string(),
        "details": details,
    });
    let bytes = serde_json::to_vec_pretty(&completion_status)
        .map_err(|error| format!("failed to encode restart completion-status artifact: {error}"))?;
    fs::write(&completion_status_path, bytes).map_err(|error| {
        format!(
            "failed to write restart completion-status artifact {}: {error}",
            completion_status_path.display()
        )
    })?;
    Ok(status.to_owned())
}

fn spawn_completion_status_value(path: &PathBuf) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

fn restart_kill_can_overwrite_wrapper_fallback_status(value: &Value) -> bool {
    let Some(status) = value.get("status").and_then(Value::as_str) else {
        return false;
    };
    if !matches!(
        status,
        "failed" | "missing_final_response" | "wrapper_error"
    ) {
        return false;
    }
    let final_message_source = value
        .get("final_message_source")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let fallback_final_message_written = value
        .get("fallback_final_message_written")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    final_message_source == "wrapper_fallback_json" || fallback_final_message_written
}

fn spawn_file_len(path: &PathBuf) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn spawn_stdout_summary(path: &PathBuf) -> (u64, Option<String>) {
    let Ok(stdout) = fs::read(path) else {
        return (0, None);
    };
    let stdout = String::from_utf8_lossy(&stdout);
    let mut line_count = 0;
    let mut last_event_type = None;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        line_count += 1;
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if let Some(event_type) = value.get("type").and_then(Value::as_str) {
                last_event_type = Some(event_type.to_owned());
            } else if let Some(event_type) = value
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
            {
                last_event_type = Some(event_type.to_owned());
            }
        }
    }
    (line_count, last_event_type)
}

fn summarize_teardown_failures(
    report: &SessionTeardownReport,
) -> Option<AgentTeardownFailureSummary> {
    if report.failure_count == 0 {
        return None;
    }

    let mut failed_sections = Vec::new();
    if report.termination_marker_failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "termination_marker".to_owned(),
            detail: report
                .termination_marker_error_message
                .clone()
                .unwrap_or_else(|| "termination marker failed without an error message".to_owned()),
        });
    }
    if report.input.failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "input".to_owned(),
            detail: format!(
                "released_keys={} released_buttons={} neutralized_pads={} lease_owner_before={:?} error_code={:?} error_message={:?}",
                report.input.released_keys,
                report.input.released_buttons,
                report.input.neutralized_pads,
                report.input.lease_owner_before,
                report.input.error_code,
                report.input.error_message
            ),
        });
    }
    if report.target.failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "target".to_owned(),
            detail: format!(
                "target_sessions_before={} target_sessions_after={} target_cleared={} error_message={:?}",
                report.target.target_sessions_before,
                report.target.target_sessions_after,
                report.target.target_cleared,
                report.target.error_message
            ),
        });
    }
    if report.continuity.failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "continuity".to_owned(),
            detail: format!(
                "target_row_deleted={} target_row_exists_after={} lease_row_deleted={} lease_row_exists_after={} error_message={:?}",
                report.continuity.target_row_deleted,
                report.continuity.target_row_exists_after,
                report.continuity.lease_row_deleted,
                report.continuity.lease_row_exists_after,
                report.continuity.error_message
            ),
        });
    }
    if report.audit_session.failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "audit_session".to_owned(),
            detail: format!(
                "cache_sessions_before={} cache_sessions_after={} removed={} error_message={:?}",
                report.audit_session.cache_sessions_before,
                report.audit_session.cache_sessions_after,
                report.audit_session.removed,
                report.audit_session.error_message
            ),
        });
    }
    if report.clipboard.failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "clipboard".to_owned(),
            detail: format!(
                "buffer_existed_before={} buffer_exists_after={} buffer_count_before={} buffer_count_after={} error_message={:?}",
                report.clipboard.buffer_existed_before,
                report.clipboard.buffer_exists_after,
                report.clipboard.buffer_count_before,
                report.clipboard.buffer_count_after,
                report.clipboard.error_message
            ),
        });
    }
    if report.cdp.failed > 0 {
        failed_sections.push(AgentTeardownFailedSection {
            section: "cdp".to_owned(),
            detail: format!(
                "owned_before={} closed={} failed={} target_ids={:?}",
                report.cdp.owned_before,
                report.cdp.closed,
                report.cdp.failed,
                report.cdp.target_ids
            ),
        });
    }
    if report.target_claims.failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "target_claims".to_owned(),
            detail: format!(
                "owned_before={} released={} target_keys={:?} error_message={:?}",
                report.target_claims.owned_before,
                report.target_claims.released,
                report.target_claims.target_keys,
                report.target_claims.error_message
            ),
        });
    }
    if report.shell.failed > 0 {
        failed_sections.push(AgentTeardownFailedSection {
            section: "shell".to_owned(),
            detail: format!(
                "job_root={:?} live_jobs_before={} retained_live_jobs={} termination_attempted={} termination_succeeded={} failed={} job_ids={:?} remaining_process_ids={:?} error_code={:?} error_message={:?}",
                report.shell.job_root,
                report.shell.live_jobs_before,
                report.shell.retained_live_jobs,
                report.shell.termination_attempted,
                report.shell.termination_succeeded,
                report.shell.failed,
                report.shell.job_ids,
                report.shell.remaining_process_ids,
                report.shell.error_code,
                report.shell.error_message
            ),
        });
    }
    if report.processes.failed > 0 {
        let items = report
            .processes
            .items
            .iter()
            .map(|item| {
                format!(
                    "{} pid={} resource={:?} launch_target={} remaining={:?} completion_artifact_cleanup_status={:?} completion_artifact_cleanup_error={:?}",
                    item.tool,
                    item.pid,
                    item.resource_id,
                    item.launch_target,
                    item.remaining_process_ids_after,
                    item.completion_artifact_cleanup_status,
                    item.completion_artifact_cleanup_error
                )
            })
            .collect::<Vec<_>>();
        failed_sections.push(AgentTeardownFailedSection {
            section: "processes".to_owned(),
            detail: format!(
                "owned_before={} job_close_attempted={} force_termination_attempted={} terminated={} failed={} items=[{}]",
                report.processes.owned_before,
                report.processes.job_close_attempted,
                report.processes.force_termination_attempted,
                report.processes.terminated,
                report.processes.failed,
                items.join("; ")
            ),
        });
    }
    if report.subscriptions.failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "subscriptions".to_owned(),
            detail: format!(
                "owned_before={} cancelled={} subscription_ids={:?} error_code={:?} error_message={:?}",
                report.subscriptions.owned_before,
                report.subscriptions.cancelled,
                report.subscriptions.subscription_ids,
                report.subscriptions.error_code,
                report.subscriptions.error_message
            ),
        });
    }
    if report.session_store.failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "session_store".to_owned(),
            detail: format!(
                "key={} existed_before={} deleted={} exists_after={} error_message={:?}",
                report.session_store.key,
                report.session_store.existed_before,
                report.session_store.deleted,
                report.session_store.exists_after,
                report.session_store.error_message
            ),
        });
    }
    if report.registry.failed {
        failed_sections.push(AgentTeardownFailedSection {
            section: "registry".to_owned(),
            detail: format!(
                "closed_recorded={} reason_code={} journal_event_written={} error_message={:?}",
                report.registry.closed_recorded,
                report.registry.reason_code,
                report.registry.journal_event_written,
                report.registry.error_message
            ),
        });
    }
    if failed_sections.is_empty() {
        failed_sections.push(AgentTeardownFailedSection {
            section: "unknown".to_owned(),
            detail: format!(
                "failure_count={} but no failed sub-report flag was set; inspect full teardown report",
                report.failure_count
            ),
        });
    }

    Some(AgentTeardownFailureSummary {
        session_id: report.session_id.clone(),
        failure_count: report.failure_count,
        failed_sections,
        next_action: "Inspect the named failed sections in the full teardown report; reclaim the exact target/process/shell/session resource if it remains live, or classify it as already cleaned before rerunning the worker ramp.".to_owned(),
    })
}

fn format_teardown_failure_error(
    report: &SessionTeardownReport,
    summary: &AgentTeardownFailureSummary,
) -> String {
    let sections = summary
        .failed_sections
        .iter()
        .map(|section| format!("{}: {}", section.section, section.detail))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "session teardown for {:?} failed with {} cleanup failure(s); failed_sections=[{}]; see `teardown_failure_summary` and `teardown` for exact resource readback",
        report.session_id, report.failure_count, sections
    )
}

fn run_codex_interrupt_helper(
    script_path: &PathBuf,
    control: &SpawnedAgentControlRead,
    thread_id: &str,
    turn_id: &str,
) -> Result<Output, String> {
    let mut command = Command::new("powershell.exe");
    apply_hidden_helper_window_flags(&mut command);
    let mut child = command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script_path)
        .args([
            "-Endpoint",
            control.endpoint.as_str(),
            "-ThreadId",
            thread_id,
            "-TurnId",
            turn_id,
            "-ControlPath",
            control.control_path.as_str(),
            "-EventsPath",
            control.events_path.as_str(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("interrupt_helper_spawn_failed: {error}"))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| format!("interrupt_helper_output_failed: {error}"));
            }
            Ok(None)
                if started.elapsed() < Duration::from_millis(CODEX_INTERRUPT_HELPER_TIMEOUT_MS) =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let pid = child.id();
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "interrupt_helper_timeout: helper pid {pid} exceeded {CODEX_INTERRUPT_HELPER_TIMEOUT_MS}ms"
                ));
            }
            Err(error) => {
                let pid = child.id();
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "interrupt_helper_wait_failed: helper pid {pid}: {error}"
                ));
            }
        }
    }
}

fn run_codex_steer_helper(
    script_path: &PathBuf,
    control: &SpawnedAgentControlRead,
    thread_id: &str,
    turn_id: &str,
    instruction: &str,
) -> Result<Output, String> {
    let mut command = Command::new("powershell.exe");
    apply_hidden_helper_window_flags(&mut command);
    let mut child = command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script_path)
        .args([
            "-Endpoint",
            control.endpoint.as_str(),
            "-ThreadId",
            thread_id,
            "-TurnId",
            turn_id,
            "-Instruction",
            instruction,
            "-ControlPath",
            control.control_path.as_str(),
            "-EventsPath",
            control.events_path.as_str(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("steer_helper_spawn_failed: {error}"))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| format!("steer_helper_output_failed: {error}"));
            }
            Ok(None)
                if started.elapsed() < Duration::from_millis(CODEX_STEER_HELPER_TIMEOUT_MS) =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let pid = child.id();
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "steer_helper_timeout: helper pid {pid} exceeded {CODEX_STEER_HELPER_TIMEOUT_MS}ms"
                ));
            }
            Err(error) => {
                let pid = child.id();
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "steer_helper_wait_failed: helper pid {pid}: {error}"
                ));
            }
        }
    }
}

fn compact_for_channel_reason(value: &str) -> String {
    const LIMIT: usize = 512;
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= LIMIT {
        compact
    } else {
        format!("{}...", &compact[..LIMIT])
    }
}

/// Maps a stored agent-kind token (manifest/registry, which may use the
/// `local-model` hyphen form from `ActSpawnAgentCli::as_str`) to the canonical
/// serde token the spawn request deserializes (`snake_case`). `None` for kinds
/// that are not a respawnable CLI.
fn spawn_cli_serde_token(kind: &str) -> Option<&'static str> {
    match kind.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        "local_model" => Some("local_model"),
        _ => None,
    }
}

/// Reads a prior spawn's final message for respawn continuity, bounded so a
/// huge transcript cannot blow up the new prompt. `None` when the agent left no
/// final message (e.g. it was killed mid-run).
fn read_prior_final_message(log_dir: &str) -> Option<String> {
    const LIMIT: usize = 4_000;
    if log_dir.trim().is_empty() {
        return None;
    }
    let path = PathBuf::from(log_dir).join("final-message.txt");
    let text = fs::read_to_string(&path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > LIMIT {
        Some(trimmed.chars().take(LIMIT).collect::<String>())
    } else {
        Some(trimmed.to_owned())
    }
}

fn record_first_delivered_channel(delivered_via: &mut Option<String>, attempt: &ChannelAttempt) {
    if delivered_via.is_none() && attempt.status == "delivered" {
        *delivered_via = Some(attempt.channel.clone());
    }
}

/// Polls the owned process tree for exit up to `grace_ms`, yielding to the async
/// runtime between polls so the daemon stays responsive during the grace window.
async fn wait_for_tree_exit_async(process_ids: &[u32], grace_ms: u64) -> (Vec<u32>, u64) {
    let deadline = Duration::from_millis(grace_ms);
    let started = tokio::time::Instant::now();
    loop {
        let remaining = crate::m4::owned_live_process_ids(process_ids);
        if remaining.is_empty() {
            return (
                remaining,
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
        }
        if started.elapsed() >= deadline {
            return (
                remaining,
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
        }
        tokio::time::sleep(Duration::from_millis(GRACE_POLL_INTERVAL_MS)).await;
    }
}

fn dashboard_json_readback(value: impl Serialize) -> Result<Value, ErrorData> {
    serde_json::to_value(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("serialize dashboard agent-control readback: {error}"),
        )
    })
}

fn blocking_worker_join_error(tool: &str, error: tokio::task::JoinError) -> ErrorData {
    mcp_error(
        error_codes::TOOL_INTERNAL_ERROR,
        format!("{tool}: blocking worker join failed: {error}"),
    )
}

fn validate_lookup_id(session_id: &str, tool: &str) -> Result<String, ErrorData> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool}: session_id must be a non-empty MCP session id or agent-spawn-* id"),
        ));
    }
    Ok(trimmed.to_owned())
}
