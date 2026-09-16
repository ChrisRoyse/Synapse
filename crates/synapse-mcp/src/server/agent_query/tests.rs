//! Storage-backed tests for `agent_query` (#911). Every test plants physical
//! `CF_AGENT_EVENTS` journal rows and `CF_AGENT_TRANSCRIPTS` rows through the
//! same daemon storage handle the tool uses — no mock storage, no mock state —
//! then verifies the reconstructed snapshot reconciles with those rows.

use std::{num::NonZeroUsize, path::Path, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};
use synapse_core::{
    AgentEventKind, AgentEventRecord, AgentTranscriptRecord, TranscriptParseStatus, TranscriptRole,
    TranscriptSource, TranscriptUsage,
};
use synapse_storage::{
    Db, agent_events::agent_event_key, agent_transcripts::agent_transcript_key, cf,
};

fn service_with_db(path: &Path) -> SynapseService {
    SynapseService::try_with_m2_shutdown_reason_and_m3_config(
        CancellationToken::new(),
        "test",
        CancellationToken::new(),
        &M2ServiceConfig::default(),
        M3ServiceConfig::from_cli_parts(
            Some(path.join("db")),
            Some(path.to_path_buf()),
            false,
            "127.0.0.1:0".to_owned(),
            NonZeroUsize::new(4).expect("nonzero"),
            false,
            true,
            None,
            false,
            None,
        ),
        M4ServiceConfig::default(),
    )
    .expect("construct service")
}

fn db_of(service: &SynapseService) -> Arc<Db> {
    service.agent_query_db().expect("open storage")
}

/// Register a live session stamped at the real wall clock, so it is not stale
/// by the time the tool's mailbox-recipient check reads it.
fn register_session(service: &SynapseService, session_id: &str) {
    register_session_with_action(service, session_id, "test");
}

fn register_session_with_action(service: &SynapseService, session_id: &str, action: &str) {
    let mut registry = service
        .session_registry_ref()
        .lock()
        .expect("session registry lock");
    registry.record_seen(session_id, Some(action.to_owned()), unix_time_ms_now());
}

/// Plant one journal row physically (pressure-bypass so it is immediately
/// durable, mirroring the #897 writer's terminal path).
fn write_event(
    db: &Db,
    ts_ns: u64,
    seq: u32,
    kind: AgentEventKind,
    spawn_id: Option<&str>,
    session_id: Option<&str>,
    decorate: impl FnOnce(&mut AgentEventRecord),
) {
    let mut record = AgentEventRecord::new(ts_ns, kind);
    record.spawn_id = spawn_id.map(ToOwned::to_owned);
    record.session_id = session_id.map(ToOwned::to_owned);
    decorate(&mut record);
    record
        .validate()
        .expect("synthetic journal row must be valid");
    let value = serde_json::to_vec(&record).expect("serialize journal row");
    db.put_batch_pressure_bypass(cf::CF_AGENT_EVENTS, [(agent_event_key(ts_ns, seq), value)])
        .expect("write journal row");
}

fn write_session_target(db: &Db, session_id: &str, hwnd: u64) {
    let row = json!({
        "schema_version": 1,
        "session_id": session_id,
        "stored_at_unix_ms": unix_time_ms_now(),
        "target": {
            "kind": "window",
            "hwnd": hwnd,
        }
    });
    db.put_batch_pressure_bypass(
        cf::CF_SESSIONS,
        [(
            format!("mcp/session-target/v1/{session_id}").into_bytes(),
            serde_json::to_vec(&row).expect("serialize session target row"),
        )],
    )
    .expect("write session target row");
}

#[allow(clippy::too_many_arguments)]
fn write_transcript(
    db: &Db,
    spawn_id: &str,
    line_no: u64,
    ts_ns: u64,
    role: TranscriptRole,
    event_kind: &str,
    model: Option<&str>,
    turn_index: Option<u64>,
    content_summary: Option<&str>,
    usage: Option<TranscriptUsage>,
) {
    let mut record = AgentTranscriptRecord::new(
        ts_ns,
        spawn_id.to_owned(),
        line_no,
        TranscriptSource::ClaudeStreamJson,
        16,
        "a".repeat(64),
    );
    record.status = TranscriptParseStatus::Parsed;
    record.role = Some(role);
    record.event_kind = Some(event_kind.to_owned());
    record.model = model.map(ToOwned::to_owned);
    record.turn_index = turn_index;
    record.content_summary = content_summary.map(ToOwned::to_owned);
    record.usage = usage;
    record
        .validate()
        .expect("synthetic transcript row must be valid");
    let value = serde_json::to_vec(&record).expect("serialize transcript row");
    db.put_batch_pressure_bypass(
        cf::CF_AGENT_TRANSCRIPTS,
        [(agent_transcript_key(spawn_id, line_no), value)],
    )
    .expect("write transcript row");
}

fn usage(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> TranscriptUsage {
    TranscriptUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cache_read_input_tokens: Some(cache_read),
        cache_creation_input_tokens: Some(cache_creation),
        cache_creation_5m_input_tokens: None,
        cache_creation_1h_input_tokens: None,
        reasoning_output_tokens: None,
        total_cost_micro_usd: None,
        model_usage: Vec::new(),
    }
}

fn params(session_id: &str) -> AgentQueryParams {
    AgentQueryParams {
        session_id: session_id.to_owned(),
        max_events: default_max_events(),
        lookback_ms: default_lookback_ms(),
        deep: false,
        deep_timeout_ms: default_deep_timeout_ms(),
    }
}

const SPAWN: &str = "agent-spawn-worker-1";
const SESSION: &str = "session-worker-1";

/// A journal `ts_ns` base ~60s in the past, so planted rows fall inside the
/// default lookback window relative to the real wall clock the tool reads.
fn recent_base_ns() -> u64 {
    unix_time_ms_now()
        .saturating_mul(1_000_000)
        .saturating_sub(60_000_000_000)
}

/// X+X=Y synthetic input: a working agent that ran one tool to completion and
/// is currently inside a second tool, with known token usage and a known
/// assistant line. The snapshot must reconstruct all of it from the rows.
#[tokio::test]
async fn working_agent_snapshot_reconstructs_state_tool_events_and_tokens() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);

    // Base ns; one full turn with two tools, the second still in flight.
    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::SpawnRequested,
        Some(SPAWN),
        None,
        |_| {},
    );
    write_event(
        &db,
        base + 2,
        0,
        AgentEventKind::SpawnReady,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );
    write_event(
        &db,
        base + 3,
        0,
        AgentEventKind::TurnStarted,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );
    write_event(
        &db,
        base + 4,
        0,
        AgentEventKind::ToolCallStarted,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.attributes.tool_name = Some("Read".to_owned());
            record.attributes.tool_call_id = Some("call-read-1".to_owned());
            record.payload = json!({"tool_input_bytes": 42, "tool_input_sha256": "a".repeat(64)});
        },
    );
    write_event(
        &db,
        base + 5,
        0,
        AgentEventKind::ToolCallFinished,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.attributes.tool_name = Some("Read".to_owned());
            record.attributes.tool_call_id = Some("call-read-1".to_owned());
            record.payload = json!({"duration_ms": 12});
        },
    );
    // Second tool: started, never finished -> in flight.
    write_event(
        &db,
        base + 6,
        0,
        AgentEventKind::ToolCallStarted,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.attributes.tool_name = Some("Bash".to_owned());
            record.attributes.tool_call_id = Some("call-bash-1".to_owned());
            record.payload = json!({"tool_input_bytes": 7, "tool_input_sha256": "b".repeat(64)});
        },
    );

    // Transcripts: one assistant line with usage.
    write_transcript(
        &db,
        SPAWN,
        1,
        base + 4,
        TranscriptRole::Assistant,
        "assistant",
        Some("claude-fable-5"),
        Some(1),
        Some("Reading the config file to find the port.\nThen I'll run the build."),
        Some(usage(1000, 200, 5000, 300)),
    );

    let response = service
        .agent_query_impl(params(SESSION), None)
        .await
        .expect("agent_query succeeds");

    assert!(response.found, "agent must be found");
    assert_eq!(response.spawn_id.as_deref(), Some(SPAWN));
    assert_eq!(response.session_id.as_deref(), Some(SESSION));
    // Last applied transition was a ToolCallStarted -> Working.
    assert_eq!(response.state, Some(AgentLifecycleState::Working));

    // Current tool call = the in-flight Bash; last completed = Read.
    let current = response.current_tool_call.expect("in-flight tool call");
    assert_eq!(current.tool_name, "Bash");
    assert!(current.in_flight);
    assert_eq!(current.tool_call_id.as_deref(), Some("call-bash-1"));
    assert!(current.elapsed_ms.is_some());
    let last = response.last_tool_call.expect("last completed tool call");
    assert_eq!(last.tool_name, "Read");
    assert!(!last.in_flight);
    assert_eq!(last.elapsed_ms, Some(12));
    assert_eq!(last.finished_at_unix_ms, Some((base + 5) / 1_000_000));

    // Recent events compact form covers the planted sequence.
    assert_eq!(response.recent_events.len(), 6);
    assert_eq!(
        response.recent_events[0].kind,
        AgentEventKind::SpawnRequested
    );
    assert_eq!(
        response.recent_events.last().expect("last event").kind,
        AgentEventKind::ToolCallStarted
    );

    // Tokens this turn reconciles exactly with the planted usage row.
    let turn = response.turn.expect("turn snapshot");
    assert_eq!(turn.input_tokens, 1000);
    assert_eq!(turn.output_tokens, 200);
    assert_eq!(turn.cache_read_input_tokens, 5000);
    assert_eq!(turn.cache_creation_input_tokens, 300);
    assert_eq!(turn.total_tokens, 1000 + 200 + 5000 + 300);
    assert_eq!(turn.turn_index, Some(1));
    assert_eq!(turn.source_line_no, 1);
    assert_eq!(
        response.context_window_estimate_tokens,
        Some(1000 + 5000 + 300 + 200)
    );

    // Activity summary collapses the multi-line assistant text to one line.
    assert_eq!(
        response.activity_summary.as_deref(),
        Some("Reading the config file to find the port. Then I'll run the build.")
    );

    // No durable task row was planted, so task stays null rather than guessed.
    assert!(response.task.is_none());
    let task_source = response.sources.get("task").expect("task source");
    assert!(task_source.contains("CF_KV"));
    assert!(!task_source.contains("NOT YET IMPLEMENTED"));
    assert!(response.cooperative.is_none());
    assert_eq!(response.scan.events_matched, 6);
}

#[tokio::test]
async fn task_link_reads_pending_attempt_from_durable_task_queue() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);

    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::SpawnReady,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );
    service
        .task_create_for_test("task-query-join", "template-query")
        .expect("create real task row");
    service
        .task_claim_with_spawn_for_test("task-query-join", SESSION, SPAWN, 7)
        .expect("claim real task row with spawn binding");

    let response = service
        .agent_query_impl(params(SESSION), None)
        .await
        .expect("agent_query succeeds");
    let task = response.task.expect("task link must be populated");
    assert_eq!(task["task_id"], json!("task-query-join"));
    assert_eq!(task["state"], json!("in_progress"));
    assert_eq!(task["title"], json!("task-query-join"));
    assert_eq!(task["template_id"], json!("template-query"));
    assert_eq!(task["attempt"]["attempt_id"], json!(1));
    assert_eq!(task["attempt"]["session_id"], json!(SESSION));
    assert_eq!(task["attempt"]["spawn_id"], json!(SPAWN));
    assert_eq!(task["attempt"]["template_version"], json!(7));
    assert_eq!(task["attempt"]["outcome"], json!("pending"));
    assert_eq!(
        task["source"],
        json!("CF_KV agent-task/v1/task/task-query-join")
    );
    let matched_by = task["matched_by"]
        .as_array()
        .expect("matched_by array")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(matched_by.contains(&"session_id"));
    assert!(matched_by.contains(&"spawn_id"));

    let by_spawn = service
        .agent_query_impl(params(SPAWN), None)
        .await
        .expect("spawn lookup succeeds");
    assert_eq!(
        by_spawn.task.expect("spawn lookup task link")["task_id"],
        json!("task-query-join")
    );
}

#[tokio::test]
async fn cleanup_required_attention_overlays_retained_session_target() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);

    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::SpawnReady,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );
    write_event(
        &db,
        base + 2,
        0,
        AgentEventKind::Exited,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.reason_code = Some("process_gone_without_exit_event".to_owned());
        },
    );

    let terminal = service
        .agent_query_impl(params(SPAWN), None)
        .await
        .expect("agent_query succeeds");
    assert_eq!(
        terminal.attention_class,
        Some(AgentAttentionClass::TerminalRuntimeFailure)
    );

    write_session_target(&db, SESSION, 0x1234);

    let cleanup = service
        .agent_query_impl(params(SPAWN), None)
        .await
        .expect("agent_query succeeds");
    assert_eq!(cleanup.spawn_id.as_deref(), Some(SPAWN));
    assert_eq!(cleanup.session_id.as_deref(), Some(SESSION));
    assert_eq!(
        cleanup.attention_class,
        Some(AgentAttentionClass::CleanupRequired)
    );
    assert!(
        cleanup
            .sources
            .get("attention_class")
            .expect("attention source")
            .contains("CF_SESSIONS session-target rows")
    );
}

#[tokio::test]
async fn completed_local_agent_with_recovered_invalid_tool_call_is_not_attention() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);

    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::SpawnReady,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );
    write_event(
        &db,
        base + 2,
        0,
        AgentEventKind::ToolCallStarted,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.attributes.tool_name = Some("workspace_put".to_owned());
        },
    );
    write_event(
        &db,
        base + 3,
        0,
        AgentEventKind::ToolCallFinished,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.attributes.tool_name = Some("workspace_put".to_owned());
            record.reason_code = Some("MODEL_TOOL_CALL_INVALID".to_owned());
        },
    );
    write_event(
        &db,
        base + 4,
        0,
        AgentEventKind::ToolCallStarted,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.attributes.tool_name = Some("workspace_put".to_owned());
        },
    );
    write_event(
        &db,
        base + 5,
        0,
        AgentEventKind::ToolCallFinished,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.attributes.tool_name = Some("workspace_put".to_owned());
        },
    );
    write_event(
        &db,
        base + 6,
        0,
        AgentEventKind::Exited,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.reason_code = Some("local_agent_completed".to_owned());
        },
    );

    let response = service
        .agent_query_impl(params(SPAWN), None)
        .await
        .expect("agent_query succeeds");

    assert_eq!(response.state, Some(AgentLifecycleState::Dead));
    assert_eq!(
        response.reason_code.as_deref(),
        Some("local_agent_completed")
    );
    assert_eq!(response.attention_class, None);
    assert_eq!(
        response.last_tool_call.as_ref().unwrap().tool_name,
        "workspace_put"
    );
}

#[tokio::test]
async fn live_session_recent_tool_activity_suppresses_stale_terminal_history() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    register_session_with_action(&service, SESSION, "tools/call:session_list");

    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::StateChanged,
        None,
        Some(SESSION),
        |record| {
            record.reason_code = Some("unprobeable_silent_ended".to_owned());
            record.state_from = Some("idle".to_owned());
            record.state_to = Some("dead".to_owned());
            record.payload = json!({
                "origin": crate::server::agent_state::STATE_MACHINE_ORIGIN,
            });
        },
    );

    let response = service
        .agent_query_impl(params(SESSION), None)
        .await
        .expect("agent_query succeeds");

    assert_eq!(response.state, Some(AgentLifecycleState::Dead));
    assert_eq!(
        response.reason_code.as_deref(),
        Some("unprobeable_silent_ended")
    );
    assert_eq!(response.attention_class, None);
}

#[tokio::test]
async fn blocked_agent_surfaces_needs_input_and_waiting_for() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);

    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::SpawnReady,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );
    // A sender-pushed state_changed row (no machine origin) -> reducer path.
    write_event(
        &db,
        base + 2,
        0,
        AgentEventKind::StateChanged,
        Some(SPAWN),
        Some(SESSION),
        |record| {
            record.reason_code = Some("permission_request".to_owned());
            record.state_to = Some("needs_input".to_owned());
        },
    );

    let response = service
        .agent_query_impl(params(SESSION), None)
        .await
        .expect("agent_query succeeds");

    assert!(response.found);
    assert_eq!(response.state, Some(AgentLifecycleState::NeedsInput));
    assert_eq!(response.waiting_for.as_deref(), Some("permission_request"));
    assert!(response.current_tool_call.is_none());
}

#[tokio::test]
async fn unknown_session_is_found_false_and_empty() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());

    let response = service
        .agent_query_impl(params("session-does-not-exist"), None)
        .await
        .expect("agent_query succeeds");

    assert!(!response.found);
    assert!(response.state.is_none());
    assert!(response.recent_events.is_empty());
    assert!(response.turn.is_none());
    assert!(response.activity_summary.is_none());
    assert_eq!(response.scan.events_matched, 0);
}

#[tokio::test]
async fn deep_times_out_to_not_answered_without_a_cooperating_agent() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);
    register_session(&service, "caller");
    register_session(&service, SESSION);

    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::SpawnReady,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );

    let mut p = params(SESSION);
    p.deep = true;
    p.deep_timeout_ms = 150;
    let response = service
        .agent_query_impl(p, Some("caller"))
        .await
        .expect("agent_query succeeds");

    let cooperative = response
        .cooperative
        .expect("deep mode returns a cooperative result");
    assert_eq!(cooperative.status, "not_answered");
    assert!(cooperative.request_message_id.is_some());
    assert!(cooperative.answer.is_none());
    // The request really was delivered: a status_request sits in the target's box.
    let inbox = service
        .agent_inbox_impl(
            crate::server::agent_mailbox::AgentInboxParams {
                drain: false,
                max_messages: 10,
                kinds: Vec::new(),
            },
            SESSION,
        )
        .expect("read target inbox");
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].kind, DEEP_REQUEST_KIND);
}

#[tokio::test]
async fn deep_without_caller_session_is_channel_unavailable() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);

    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::SpawnReady,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );

    let mut p = params(SESSION);
    p.deep = true;
    let response = service
        .agent_query_impl(p, None)
        .await
        .expect("agent_query succeeds");

    let cooperative = response
        .cooperative
        .expect("deep mode returns a cooperative result");
    assert_eq!(cooperative.status, "channel_unavailable");
    assert!(cooperative.detail.expect("detail").contains("HTTP mode"));
}

/// Full cooperative round-trip: a background "responder" answers the
/// status_request, and the snapshot includes the agent's own answer.
#[tokio::test]
async fn deep_answered_when_agent_replies_cooperatively() {
    let temp = TempDir::new().expect("tempdir");
    let service = Arc::new(service_with_db(temp.path()));
    let db = db_of(&service);
    register_session(&service, "caller");
    register_session(&service, SESSION);

    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::SpawnReady,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );

    // Responder: poll the target inbox; when the status_request lands, reply to
    // the caller echoing the request message_id, exactly as a cooperating agent
    // would.
    let responder_service = Arc::clone(&service);
    let responder = tokio::spawn(async move {
        for _ in 0..200 {
            let inbox = responder_service
                .agent_inbox_impl(
                    crate::server::agent_mailbox::AgentInboxParams {
                        drain: true,
                        max_messages: 10,
                        kinds: Vec::new(),
                    },
                    SESSION,
                )
                .expect("responder reads inbox");
            if let Some(request) = inbox
                .messages
                .into_iter()
                .find(|message| message.kind == DEEP_REQUEST_KIND)
            {
                responder_service
                    .agent_send_impl(
                        crate::server::agent_mailbox::AgentSendParams {
                            to_session: "caller".to_owned(),
                            kind: DEEP_RESPONSE_KIND.to_owned(),
                            payload: json!({
                                "in_reply_to": request.message_id,
                                "summary": "compiling the kernel module",
                            }),
                            artifact_handle: None,
                            ttl_ms: 60_000,
                            request_receipt: false,
                        },
                        SESSION,
                    )
                    .expect("responder sends reply");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("responder never saw a status_request");
    });

    let mut p = params(SESSION);
    p.deep = true;
    p.deep_timeout_ms = 5_000;
    let response = service
        .agent_query_impl(p, Some("caller"))
        .await
        .expect("agent_query succeeds");
    responder.await.expect("responder task");

    let cooperative = response.cooperative.expect("deep cooperative result");
    assert_eq!(
        cooperative.status, "answered",
        "detail: {:?}",
        cooperative.detail
    );
    let answer = cooperative.answer.expect("answer payload");
    assert_eq!(
        answer.get("summary").and_then(Value::as_str),
        Some("compiling the kernel module")
    );
    // The consumed reply was deleted from the caller's box (peek shows empty).
    let inbox = service
        .agent_inbox_impl(
            crate::server::agent_mailbox::AgentInboxParams {
                drain: false,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "caller",
        )
        .expect("read caller inbox");
    assert!(
        inbox.messages.is_empty(),
        "correlated reply must be consumed"
    );
}

#[tokio::test]
async fn lookup_by_spawn_id_resolves_the_same_agent() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());
    let db = db_of(&service);

    let base = recent_base_ns();
    write_event(
        &db,
        base + 1,
        0,
        AgentEventKind::SpawnReady,
        Some(SPAWN),
        Some(SESSION),
        |_| {},
    );

    let response = service
        .agent_query_impl(params(SPAWN), None)
        .await
        .expect("agent_query succeeds");
    assert!(response.found);
    assert_eq!(response.spawn_id.as_deref(), Some(SPAWN));
    assert_eq!(response.session_id.as_deref(), Some(SESSION));
}

#[tokio::test]
async fn invalid_params_are_rejected() {
    let temp = TempDir::new().expect("tempdir");
    let service = service_with_db(temp.path());

    let mut empty = params("   ");
    empty.max_events = 10;
    assert!(service.agent_query_impl(empty, None).await.is_err());

    let mut too_many = params(SESSION);
    too_many.max_events = MAX_MAX_EVENTS + 1;
    assert!(service.agent_query_impl(too_many, None).await.is_err());

    let mut bad_lookback = params(SESSION);
    bad_lookback.lookback_ms = MAX_LOOKBACK_MS + 1;
    assert!(service.agent_query_impl(bad_lookback, None).await.is_err());
}
