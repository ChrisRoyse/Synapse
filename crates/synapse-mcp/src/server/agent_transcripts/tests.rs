//! Engine tests for the `CF_AGENT_TRANSCRIPTS` ingester (#900).
//!
//! No mock data: the Claude and Codex streams are byte-for-byte captures of
//! real `act_spawn_agent` runs from 2026-06-12 (Claude Code 2.1.x
//! `--output-format stream-json`, Codex `exec --json`), checked in as
//! `tests/fixtures/*.jsonl`. Every test runs against a real temp `RocksDB`
//! and verifies the physical rows, not return values.

use std::path::PathBuf;

use synapse_core::SCHEMA_VERSION;
use synapse_storage::agent_transcripts::decode_agent_transcript_key;

use super::*;

/// Real captured Claude Code stream-json run (44 lines, ends with a
/// `result/success` line).
const CLAUDE_REAL_STREAM: &str = include_str!("../../../tests/fixtures/claude_stream_real.jsonl");
/// Real captured Codex `exec --json` run (18 lines, ends with
/// `turn.completed`).
const CODEX_REAL_STREAM: &str = include_str!("../../../tests/fixtures/codex_exec_real.jsonl");
/// Byte-for-byte Codex app-server stdout lines captured from
/// `agent-spawn-019ec7e6-95bb-74c2-b357-579f75b053ce` during #1425.
const CODEX_APP_SERVER_REAL_STREAM: &str = r#"{"id":1,"result":{"userAgent":"synapse-act-spawn-agent/0.139.0 (Windows 10.0.26200; x86_64) unknown (synapse-act-spawn-agent; 0.1.0)","codexHome":"C:\\Users\\hotra\\.codex","platformFamily":"windows","platformOs":"windows"}}
{"method":"thread/started","params":{"thread":{"id":"019ec7e6-99d1-7132-b3dc-a5005af88f47","sessionId":"019ec7e6-99d1-7132-b3dc-a5005af88f47","forkedFromId":null,"parentThreadId":null,"preview":"","ephemeral":true,"modelProvider":"openai","createdAt":1781470239,"updatedAt":1781470239,"status":{"type":"idle"},"path":null,"cwd":"C:\\code\\Synapse","cliVersion":"0.139.0","source":"vscode","threadSource":"subagent","agentNickname":null,"agentRole":null,"gitInfo":null,"name":null,"turns":[]}}}
{"method":"turn/started","params":{"threadId":"019ec7e6-99d1-7132-b3dc-a5005af88f47","turn":{"id":"019ec7e6-9a11-7be1-9993-48e7a0125c9e","items":[],"itemsView":"notLoaded","status":"inProgress","error":null,"startedAt":1781470239,"completedAt":null,"durationMs":null}}}
{"method":"item/agentMessage/delta","params":{"threadId":"019ec7e6-99d1-7132-b3dc-a5005af88f47","turnId":"019ec7e6-9a11-7be1-9993-48e7a0125c9e","itemId":"msg_0a7de64c6c34ee41016a2f142a95cc8197b76a8c4b87d6478f","delta":"I"}}
{"method":"item/started","params":{"item":{"type":"mcpToolCall","id":"call_BNQFKVunuXkRF6B2aRP3AfWx","server":"synapse","tool":"health","status":"inProgress","arguments":{},"pluginId":null,"result":null,"error":null,"durationMs":null},"threadId":"019ec7e6-99d1-7132-b3dc-a5005af88f47","turnId":"019ec7e6-9a11-7be1-9993-48e7a0125c9e","startedAtMs":1781470256712}}
{"method":"thread/tokenUsage/updated","params":{"threadId":"019ec7e6-99d1-7132-b3dc-a5005af88f47","turnId":"019ec7e6-9a11-7be1-9993-48e7a0125c9e","tokenUsage":{"total":{"totalTokens":14404,"inputTokens":13987,"cachedInputTokens":4480,"outputTokens":417,"reasoningOutputTokens":333},"last":{"totalTokens":14404,"inputTokens":13987,"cachedInputTokens":4480,"outputTokens":417,"reasoningOutputTokens":333},"modelContextWindow":258400}}}
{"method":"turn/completed","params":{"threadId":"019ec7e6-99d1-7132-b3dc-a5005af88f47","turn":{"id":"019ec7e6-9a11-7be1-9993-48e7a0125c9e","items":[],"itemsView":"notLoaded","status":"completed","error":null,"startedAt":1781470239,"completedAt":1781470467,"durationMs":228069}}}
"#;
const LOCAL_MODEL_STREAM: &str = r#"{"type":"local.thread.started","conversation_id":"local-model-test-thread","model":"gemma4:e4b","registry_name":"ollama-gemma4-e4b","tool_count":107}
{"type":"local.turn.started","conversation_id":"local-model-test-thread","model":"gemma4:e4b","turn_index":1}
{"type":"local.assistant.message","conversation_id":"local-model-test-thread","model":"gemma4:e4b","turn_index":1,"content":"","finish_reason":"tool_calls","raw_response_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
{"type":"local.tool_call.started","conversation_id":"local-model-test-thread","model":"gemma4:e4b","turn_index":1,"tool_name":"workspace_put","tool_call_id":"call_1","arguments":"{\"run_id\":\"issue931-test\",\"key\":\"result\",\"value\":{\"actual\":4}}"}
{"type":"local.tool_call.gate_bypassed","conversation_id":"local-model-test-thread","model":"gemma4:e4b","turn_index":1,"tool_name":"workspace_put","tool_call_id":"call_1","reason_code":"trusted_unattended_exact_contract","approval_gate_used":false,"exact_contract_authorized":true}
{"type":"local.tool_call.finished","conversation_id":"local-model-test-thread","model":"gemma4:e4b","turn_index":1,"tool_name":"workspace_put","tool_call_id":"call_1","status":"ok","result":{"ok":true}}
{"type":"local.turn.finished","conversation_id":"local-model-test-thread","model":"gemma4:e4b","turn_index":1,"finish_reason":"tool_calls","usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}
{"type":"local.context.truncated","conversation_id":"local-model-test-thread","model":"gemma4:e4b","turn_index":2,"before_chars":2048,"after_chars":1024,"limit_chars":1024}
{"type":"local.error","conversation_id":"local-model-test-thread","model":"gemma4:e4b","turn_index":2,"error_code":"MODEL_ENDPOINT_UNREACHABLE","error_detail":"MODEL_ENDPOINT_UNREACHABLE: synthetic"}
"#;

/// Byte-for-byte rows captured from live local-agent `stdout.jsonl` files on
/// this host after #900. These are current writer events that are not in the
/// older synthetic `LOCAL_MODEL_STREAM` above.
const LOCAL_MODEL_CURRENT_LIFECYCLE_STREAM: &str = r#"{"attributed_arguments":{"context":"Expected operator response is token-73. This verifies local-agent spawn attribution.","notify":false,"question":"REGRESSION-1028-ATTRIBUTED-HAPPY: What synthetic token should be recorded?","spawn_id":"agent-spawn-019efe12-18d6-7c90-b25d-31733fdb8793","suppress_popup":true,"timeout_ms":120000},"conversation_id":"local-model-019efe121f90712289b5524225272ad3","model":"deepseek-v4-flash","reason_code":"local_agent_spawn_id_attribution","routed_tool_name":null,"spawn_id":"agent-spawn-019efe12-18d6-7c90-b25d-31733fdb8793","tool_call_id":"call_00_Ct1SfrCZn4ajLQhBRKk28151","tool_exposure":"routed","tool_name":"agent_ask_operator","turn_index":1,"type":"local.tool_call.arguments_normalized"}
{"completed_task_tool_sources":{"workspace_get":"workspace_put_post_write_readback","workspace_put":"model_tool_call"},"completed_task_tools":["workspace_get","workspace_put"],"conversation_id":"local-model-019ee30871017472b4dbc33c51abc9a7","final_message":"{\"case\":\"happy\",\"expected\":\"workspace row exists\",\"ok\":true}","model":"qwen8v2-tool","reason_code":"task_tool_contract_verified","type":"local.agent.completed"}
{"conversation_id":"local-model-019ee575212b7931950ac7002a8dea93","hold_open_ms":60000,"model":"qwen8v2-tool","session_id":"b93d7034-9d5c-4711-aabb-c7b72d23a15e","source":"local_agent_mcp_session","started_at_unix_ms":1781966127405,"type":"local.hold_open.started"}
{"conversation_id":"local-model-019ee575212b7931950ac7002a8dea93","finished_at_unix_ms":1781966187409,"hold_open_ms":60000,"model":"qwen8v2-tool","session_id":"b93d7034-9d5c-4711-aabb-c7b72d23a15e","source":"local_agent_mcp_session","started_at_unix_ms":1781966127405,"type":"local.hold_open.finished"}
{"conversation_id":"local-model-019ed1a56e6f7721815802b3caf80f8b","kind":"interrupt","message_id":"agentmsg-00000001781633775598-00000000000000000002","model":"qwen8v2-tool","payload_summary":"stop the current turn at the next safe point","session_id":"2567dfa0-77ea-4aaa-b646-b92af130d539","turn_index":2,"type":"local.steering.received"}
"#;

fn open_temp_db() -> (tempfile::TempDir, Db) {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = Db::open(&temp.path().join("db"), SCHEMA_VERSION).expect("temp DB must open");
    (temp, db)
}

#[test]
fn custom_db_without_explicit_transcript_root_disables_host_spawn_scan() {
    let custom = tempfile::tempdir().expect("custom db root");
    let host_root = tempfile::tempdir().expect("host spawn root");

    let decision = transcript_spawn_root_for_db_with(custom.path(), None, || {
        Ok(host_root.path().to_path_buf())
    })
    .expect("root decision");

    println!(
        "edge=custom_db_no_explicit_root db={} host_root={} decision={decision:?}",
        custom.path().display(),
        host_root.path().display()
    );
    assert!(
        decision.is_none(),
        "custom DBs must not scan the host-global spawn root without an explicit root"
    );
}

#[test]
fn custom_db_with_explicit_transcript_root_uses_only_that_root() {
    let custom = tempfile::tempdir().expect("custom db root");
    let explicit_root = tempfile::tempdir().expect("explicit spawn root");
    let host_root = tempfile::tempdir().expect("host spawn root");

    let decision = transcript_spawn_root_for_db_with(
        custom.path(),
        Some(explicit_root.path().to_path_buf()),
        || Ok(host_root.path().to_path_buf()),
    )
    .expect("root decision")
    .expect("explicit root should enable ingest");

    println!(
        "edge=custom_db_explicit_root db={} explicit_root={} host_root={} selected={} scope={}",
        custom.path().display(),
        explicit_root.path().display(),
        host_root.path().display(),
        decision.root.display(),
        decision.scope.as_str()
    );
    assert_eq!(decision.root, explicit_root.path());
    assert_eq!(decision.scope, TranscriptRootScope::ExplicitEnv);
}

#[test]
fn configured_daemon_db_without_explicit_root_keeps_default_host_spawn_scan() {
    let host_root = tempfile::tempdir().expect("host spawn root");

    let decision = transcript_spawn_root_for_db_with(&default_daemon_db_path(), None, || {
        Ok(host_root.path().to_path_buf())
    })
    .expect("root decision")
    .expect("configured daemon DB should keep host spawn ingest");

    println!(
        "edge=configured_daemon_db db={} selected={} scope={}",
        default_daemon_db_path().display(),
        decision.root.display(),
        decision.scope.as_str()
    );
    assert_eq!(decision.root, host_root.path());
    assert_eq!(decision.scope, TranscriptRootScope::ConfiguredDaemonDb);
}

/// Builds a real-shaped spawn dir: the CLI marker file decides the parser,
/// exactly like a live `act_spawn_agent` dir does.
fn plant_spawn_dir(
    root: &Path,
    spawn_id: &str,
    source: TranscriptSource,
    stdout_content: &str,
) -> PathBuf {
    let log_dir = root.join(spawn_id);
    std::fs::create_dir_all(&log_dir).expect("create spawn dir");
    let marker = match source {
        TranscriptSource::ClaudeStreamJson => "claude-mcp-config.json",
        TranscriptSource::CodexExecJson => "codex-notify.ps1",
        TranscriptSource::CodexAppServerJsonRpc => "codex-app-server-runner.ps1",
        TranscriptSource::LocalModelJson => "local-model-runner.json",
        // Session-file transcripts are not spawn-dir sourced; this helper never
        // builds one. Panic loudly if a test ever asks for it here.
        TranscriptSource::ClaudeSessionJsonl => {
            unreachable!("spawn-dir ingester does not handle ClaudeSessionJsonl")
        }
    };
    std::fs::write(log_dir.join(marker), b"{}").expect("write marker");
    std::fs::write(log_dir.join("stdout.jsonl"), stdout_content).expect("write stdout");
    log_dir
}

fn mark_completed(log_dir: &Path) {
    std::fs::write(
        log_dir.join("completion-status.json"),
        br#"{"schema_version":1,"status":"ok"}"#,
    )
    .expect("write completion status");
}

fn scan_spawn_rows(db: &Db, spawn_id: &str) -> Vec<(u64, AgentTranscriptRecord)> {
    let rows = db
        .scan_cf_prefix(
            cf::CF_AGENT_TRANSCRIPTS,
            &agent_transcript_spawn_prefix(spawn_id),
        )
        .expect("scan must work");
    rows.iter()
        .map(|(key, value)| {
            let (decoded_id, line_no) = decode_agent_transcript_key(key).expect("key must decode");
            assert_eq!(
                decoded_id, spawn_id,
                "prefix scan must stay within the spawn"
            );
            let record: AgentTranscriptRecord = decode_json(value).expect("row must decode");
            assert_eq!(record.line_no, line_no, "key and record line_no must agree");
            (line_no, record)
        })
        .collect()
}

#[test]
fn claude_real_stream_reconciles_line_for_line() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-claude-real";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::ClaudeStreamJson,
        CLAUDE_REAL_STREAM,
    );
    mark_completed(&log_dir);

    let outcome =
        ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("ingest must succeed");
    let source_lines = CLAUDE_REAL_STREAM.lines().count() as u64;
    println!(
        "edge=claude_real before_rows=0 after: parsed={} invalid={} total={} complete={}",
        outcome.new_parsed_rows,
        outcome.new_invalid_rows,
        outcome.lines_ingested_total,
        outcome.source_complete
    );
    assert_eq!(outcome.lines_ingested_total, source_lines);
    assert_eq!(
        outcome.new_invalid_rows, 0,
        "a real capture must parse fully"
    );
    assert!(outcome.source_complete, "terminal completion must finalize");

    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(
        rows.len() as u64,
        source_lines,
        "physical rows must reconcile line-for-line with the raw JSONL"
    );
    for (index, (line_no, _record)) in rows.iter().enumerate() {
        assert_eq!(*line_no, index as u64 + 1, "line numbers must be dense");
    }

    // Line 1 is system/init: it must capture the conversation id.
    let (_one, init) = &rows[0];
    assert_eq!(init.role, Some(TranscriptRole::System));
    assert_eq!(init.event_kind.as_deref(), Some("system/init"));
    assert_eq!(init.status, TranscriptParseStatus::Parsed);

    // Reconcile the normalized tool-call sequence against the raw file.
    let mut raw_tool_names: Vec<String> = Vec::new();
    for line in CLAUDE_REAL_STREAM.lines() {
        let value: Value = serde_json::from_str(line).expect("fixture line is JSON");
        if value["type"] == "assistant" {
            for block in value["message"]["content"].as_array().expect("content") {
                if block["type"] == "tool_use" {
                    raw_tool_names.push(block["name"].as_str().expect("name").to_owned());
                }
            }
        }
    }
    let ingested_tool_names: Vec<String> = rows
        .iter()
        .filter(|(_line, record)| record.role == Some(TranscriptRole::Assistant))
        .flat_map(|(_line, record)| record.tool_calls.iter().map(|call| call.tool_name.clone()))
        .collect();
    assert_eq!(
        ingested_tool_names, raw_tool_names,
        "normalized tool-call sequence must match the raw stream exactly"
    );
    assert!(
        !raw_tool_names.is_empty(),
        "the real capture must contain tool calls"
    );

    // The final line is result/success with usage and cost.
    let (_last, result) = rows.last().expect("rows");
    assert_eq!(result.role, Some(TranscriptRole::Result));
    assert_eq!(result.event_kind.as_deref(), Some("result/success"));
    let usage = result.usage.as_ref().expect("result line carries usage");
    assert!(
        usage.input_tokens.is_some(),
        "usage must carry input tokens"
    );
    assert!(
        usage.total_cost_micro_usd.is_some(),
        "result must carry the reported cost"
    );

    // Assistant rows carry the model and increasing turn indexes.
    let assistant_turns: Vec<u64> = rows
        .iter()
        .filter(|(_line, record)| record.role == Some(TranscriptRole::Assistant))
        .map(|(_line, record)| record.turn_index.expect("assistant rows have turns"))
        .collect();
    assert!(!assistant_turns.is_empty());
    assert!(
        assistant_turns.windows(2).all(|pair| pair[0] <= pair[1]),
        "turn indexes must be monotonic: {assistant_turns:?}"
    );
    let models: std::collections::BTreeSet<Option<String>> = rows
        .iter()
        .filter(|(_line, record)| record.role == Some(TranscriptRole::Assistant))
        .map(|(_line, record)| record.model.clone())
        .collect();
    assert!(
        models.iter().all(Option::is_some),
        "assistant rows must carry the model: {models:?}"
    );
}

#[test]
fn codex_real_stream_reconciles_line_for_line() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-codex-real";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::CodexExecJson,
        CODEX_REAL_STREAM,
    );
    mark_completed(&log_dir);

    let outcome =
        ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("ingest must succeed");
    let source_lines = CODEX_REAL_STREAM.lines().count() as u64;
    println!(
        "edge=codex_real after: parsed={} invalid={} total={}",
        outcome.new_parsed_rows, outcome.new_invalid_rows, outcome.lines_ingested_total
    );
    assert_eq!(outcome.lines_ingested_total, source_lines);
    assert_eq!(
        outcome.new_invalid_rows, 0,
        "a real capture must parse fully"
    );

    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len() as u64, source_lines);

    // thread.started captures the conversation id; later rows carry it.
    let (_one, thread_started) = &rows[0];
    assert_eq!(thread_started.event_kind.as_deref(), Some("thread.started"));
    let raw_thread_id: Value =
        serde_json::from_str(CODEX_REAL_STREAM.lines().next().expect("line")).expect("json");
    let thread_id = raw_thread_id["thread_id"].as_str().expect("thread id");
    assert_eq!(
        thread_started.conversation_id.as_deref(),
        Some(thread_id),
        "thread id must become the conversation id"
    );
    let (_last_no, last) = rows.last().expect("rows");
    assert_eq!(last.event_kind.as_deref(), Some("turn.completed"));
    assert_eq!(last.conversation_id.as_deref(), Some(thread_id));
    let usage = last.usage.as_ref().expect("turn.completed carries usage");
    assert!(
        usage.cache_read_input_tokens.is_some(),
        "codex cached_input_tokens must map to cache_read_input_tokens"
    );
    assert!(usage.reasoning_output_tokens.is_some());

    // MCP tool calls must read `server.tool` and command executions must
    // carry exit codes, reconciled against the raw stream.
    let raw_mcp_count = CODEX_REAL_STREAM
        .lines()
        .filter(|line| line.contains("\"mcp_tool_call\""))
        .count();
    let ingested_mcp: Vec<&TranscriptToolCall> = rows
        .iter()
        .flat_map(|(_line, record)| record.tool_calls.iter())
        .filter(|call| call.tool_name.starts_with("synapse."))
        .collect();
    assert_eq!(
        ingested_mcp.len(),
        raw_mcp_count,
        "every raw mcp_tool_call event must yield a synapse.* tool call"
    );
    let command_rows: Vec<&TranscriptToolCall> = rows
        .iter()
        .flat_map(|(_line, record)| record.tool_calls.iter())
        .filter(|call| call.tool_name == "command_execution")
        .collect();
    assert!(
        command_rows
            .iter()
            .any(|call| call.exit_code == Some(0) && call.status.as_deref() == Some("completed")),
        "the real capture contains a completed command with exit 0"
    );
}

#[test]
fn codex_app_server_real_stream_reconciles_without_invalid_rows() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-codex-app-server-real";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::CodexAppServerJsonRpc,
        CODEX_APP_SERVER_REAL_STREAM,
    );
    // Current app-server spawns also carry the Codex notify marker; detection
    // must still choose the app-server parser first.
    std::fs::write(log_dir.join("codex-notify.ps1"), "$null").expect("write codex marker");
    mark_completed(&log_dir);

    let outcome =
        ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("ingest must succeed");
    let source_lines = CODEX_APP_SERVER_REAL_STREAM.lines().count() as u64;
    println!(
        "edge=codex_app_server parsed={} invalid={} total={}",
        outcome.new_parsed_rows, outcome.new_invalid_rows, outcome.lines_ingested_total
    );
    assert_eq!(outcome.lines_ingested_total, source_lines);
    assert_eq!(
        outcome.new_invalid_rows, 0,
        "real app-server JSON-RPC lines must not flood invalid rows"
    );
    assert!(outcome.source_complete);

    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len() as u64, source_lines);
    assert!(
        rows.iter()
            .all(|(_line, record)| record.source == TranscriptSource::CodexAppServerJsonRpc),
        "every row must carry the app-server source"
    );

    let response = &rows[0].1;
    assert_eq!(
        response.event_kind.as_deref(),
        Some("codex_app_server/response/result")
    );
    assert_eq!(response.status, TranscriptParseStatus::Parsed);

    let thread_started = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| record.event_kind.as_deref() == Some("codex_app_server/thread/started"))
        .expect("thread started row");
    assert_eq!(thread_started.role, Some(TranscriptRole::System));
    assert_eq!(
        thread_started.conversation_id.as_deref(),
        Some("019ec7e6-99d1-7132-b3dc-a5005af88f47")
    );

    let delta = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| {
            record.event_kind.as_deref() == Some("codex_app_server/item/agentMessage/delta")
        })
        .expect("delta row");
    assert_eq!(delta.role, Some(TranscriptRole::Assistant));
    assert_eq!(delta.content_summary.as_deref(), Some("I"));

    let tool = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| {
            record.event_kind.as_deref() == Some("codex_app_server/item/started/mcpToolCall")
        })
        .expect("tool row");
    assert_eq!(tool.role, Some(TranscriptRole::Tool));
    assert_eq!(tool.tool_calls[0].tool_name, "synapse.health");

    let usage = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| {
            record.event_kind.as_deref() == Some("codex_app_server/thread/tokenUsage/updated")
        })
        .and_then(|record| record.usage.as_ref())
        .expect("usage row");
    assert_eq!(usage.input_tokens, Some(13_987));
    assert_eq!(usage.output_tokens, Some(417));
    assert_eq!(usage.cache_read_input_tokens, Some(4_480));
    assert_eq!(usage.reasoning_output_tokens, Some(333));
}

#[test]
fn malformed_codex_app_server_envelope_is_invalid() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-codex-app-server-malformed";
    let content = "{\"params\":{\"threadId\":\"thread-without-method-or-id\"}}\n";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::CodexAppServerJsonRpc,
        content,
    );

    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, true).expect("ingest");
    assert_eq!(outcome.new_invalid_rows, 1);
    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.status, TranscriptParseStatus::Invalid);
    let parse_error = rows[0].1.parse_error.as_deref().expect("detail");
    assert!(
        parse_error.contains("CODEX_APP_SERVER_MESSAGE_MISSING_METHOD_OR_ID"),
        "malformed app-server envelopes must stay fail-closed: {parse_error}"
    );
}

#[test]
fn local_model_stream_reconciles_usage_tool_calls_and_errors() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-local-realshape";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::LocalModelJson,
        LOCAL_MODEL_STREAM,
    );
    mark_completed(&log_dir);

    let outcome =
        ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("ingest must succeed");
    assert_eq!(outcome.new_invalid_rows, 0);
    assert_eq!(
        outcome.lines_ingested_total,
        LOCAL_MODEL_STREAM.lines().count() as u64
    );

    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len(), LOCAL_MODEL_STREAM.lines().count());
    assert!(
        rows.iter()
            .all(|(_line, record)| record.conversation_id.as_deref()
                == Some("local-model-test-thread")),
        "conversation id must stamp every parsed local row"
    );
    let tool_started = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| record.event_kind.as_deref() == Some("local.tool_call.started"))
        .expect("tool row");
    assert_eq!(tool_started.role, Some(TranscriptRole::Tool));
    assert_eq!(tool_started.tool_calls[0].tool_name, "workspace_put");
    assert_eq!(
        tool_started.tool_calls[0].tool_call_id.as_deref(),
        Some("call_1")
    );
    assert!(
        tool_started.tool_calls[0]
            .arguments
            .as_deref()
            .expect("arguments")
            .contains("issue931-test")
    );
    // #1327: gate_bypassed is an expected local lifecycle event, not schema drift.
    // It must parse to a typed Tool row (the new_invalid_rows==0 assertion above
    // already proves it is not counted invalid).
    let gate_bypassed = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| record.event_kind.as_deref() == Some("local.tool_call.gate_bypassed"))
        .expect("gate_bypassed row must parse");
    assert_eq!(gate_bypassed.role, Some(TranscriptRole::Tool));
    assert_eq!(gate_bypassed.tool_calls[0].tool_name, "workspace_put");
    assert_eq!(
        gate_bypassed.tool_calls[0].status.as_deref(),
        Some("gate_bypassed:trusted_unattended_exact_contract")
    );
    let usage_row = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| record.event_kind.as_deref() == Some("local.turn.finished"))
        .expect("usage row");
    let usage = usage_row.usage.as_ref().expect("usage");
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.output_tokens, Some(20));
    let error_row = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| record.event_kind.as_deref() == Some("local.error"))
        .expect("error row");
    assert!(
        error_row
            .source_error
            .as_deref()
            .is_some_and(|detail| detail.contains("MODEL_ENDPOINT_UNREACHABLE"))
    );
}

#[test]
fn local_model_current_lifecycle_events_parse_from_real_rows() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-local-current-lifecycle";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::LocalModelJson,
        LOCAL_MODEL_CURRENT_LIFECYCLE_STREAM,
    );
    mark_completed(&log_dir);

    let outcome =
        ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("ingest must succeed");
    let source_lines = LOCAL_MODEL_CURRENT_LIFECYCLE_STREAM.lines().count() as u64;
    println!(
        "edge=local_current_lifecycle parsed={} invalid={} total={}",
        outcome.new_parsed_rows, outcome.new_invalid_rows, outcome.lines_ingested_total
    );
    assert_eq!(outcome.new_invalid_rows, 0);
    assert_eq!(outcome.lines_ingested_total, source_lines);

    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len() as u64, source_lines);

    let normalized = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| record.event_kind.as_deref() == Some("local.tool_call.arguments_normalized"))
        .expect("arguments_normalized row must parse");
    assert_eq!(normalized.role, Some(TranscriptRole::Tool));
    assert_eq!(normalized.tool_calls[0].tool_name, "agent_ask_operator");
    assert_eq!(
        normalized.tool_calls[0].status.as_deref(),
        Some("arguments_normalized:local_agent_spawn_id_attribution")
    );
    assert!(
        normalized.tool_calls[0]
            .arguments
            .as_deref()
            .expect("normalized arguments")
            .contains("REGRESSION-1028-ATTRIBUTED-HAPPY")
    );

    let completed = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| record.event_kind.as_deref() == Some("local.agent.completed"))
        .expect("agent.completed row must parse");
    assert_eq!(completed.role, Some(TranscriptRole::Result));
    assert!(
        completed
            .content_summary
            .as_deref()
            .expect("final message")
            .contains("workspace row exists")
    );

    for event_kind in ["local.hold_open.started", "local.hold_open.finished"] {
        let hold_open = rows
            .iter()
            .map(|(_line, record)| record)
            .find(|record| record.event_kind.as_deref() == Some(event_kind))
            .expect("hold_open row must parse");
        assert_eq!(hold_open.role, Some(TranscriptRole::System));
        assert!(
            hold_open
                .content_summary
                .as_deref()
                .expect("hold_open summary")
                .contains("\"hold_open_ms\":60000")
        );
    }

    let steering = rows
        .iter()
        .map(|(_line, record)| record)
        .find(|record| record.event_kind.as_deref() == Some("local.steering.received"))
        .expect("steering row must parse");
    assert_eq!(steering.role, Some(TranscriptRole::System));
    assert_eq!(
        steering.content_summary.as_deref(),
        Some("stop the current turn at the next safe point")
    );
}

#[test]
fn malformed_current_local_lifecycle_event_is_still_invalid() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-local-current-malformed";
    let content = "{\"type\":\"local.agent.completed\",\"conversation_id\":\"local-model-bad\"}\n";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::LocalModelJson,
        content,
    );

    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, true).expect("ingest");
    assert_eq!(outcome.new_invalid_rows, 1);
    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.status, TranscriptParseStatus::Invalid);
    let parse_error = rows[0].1.parse_error.as_deref().expect("detail");
    assert!(
        parse_error.contains("required string field \"final_message\""),
        "malformed current event must stay fail-closed: {parse_error}"
    );
}

#[test]
fn planted_garbage_line_is_counted_and_ingestion_continues() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-garbage";
    // Real stream with one corrupt line planted in the middle (issue #900
    // acceptance edge case).
    let mut lines: Vec<&str> = CLAUDE_REAL_STREAM.lines().collect();
    let garbage = "{this is not json at all";
    lines.insert(5, garbage);
    let content = lines.join("\n") + "\n";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::ClaudeStreamJson,
        &content,
    );
    mark_completed(&log_dir);

    let outcome =
        ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("ingest must continue");
    println!(
        "edge=garbage_line after: parsed={} invalid={} total={}",
        outcome.new_parsed_rows, outcome.new_invalid_rows, outcome.lines_ingested_total
    );
    assert_eq!(
        outcome.new_invalid_rows, 1,
        "exactly the planted line fails"
    );
    assert_eq!(outcome.lines_ingested_total, lines.len() as u64);

    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len(), lines.len(), "line-for-line contract must hold");
    let (line_no, invalid_row) = &rows[5];
    assert_eq!(*line_no, 6, "the planted line is line 6 (1-based)");
    assert_eq!(invalid_row.status, TranscriptParseStatus::Invalid);
    let parse_error = invalid_row.parse_error.as_deref().expect("error detail");
    assert!(parse_error.starts_with("LINE_NOT_JSON"), "{parse_error}");
    assert_eq!(invalid_row.raw_line_bytes, garbage.len() as u64);
    assert_eq!(invalid_row.raw_line_sha256, sha256_hex(garbage.as_bytes()));
    // Lines after the garbage still parsed.
    assert_eq!(rows[6].1.status, TranscriptParseStatus::Parsed);
}

#[test]
fn incremental_tail_consumes_only_complete_lines() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-tail";
    let all_lines: Vec<&str> = CODEX_REAL_STREAM.lines().collect();
    let first_chunk = all_lines[..3].join("\n") + "\n";
    let partial = &all_lines[3][..all_lines[3].len() / 2];
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::CodexExecJson,
        &(first_chunk.clone() + partial),
    );

    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("first pass");
    println!(
        "edge=partial_line before: file_has=3.5_lines after: ingested={}",
        outcome.lines_ingested_total
    );
    assert_eq!(
        outcome.lines_ingested_total, 3,
        "the unterminated line must wait for its newline"
    );
    let cursor = load_cursor(&db, spawn_id)
        .expect("cursor read")
        .expect("cursor exists");
    assert_eq!(cursor.offset_bytes, first_chunk.len() as u64);

    // Complete the file and finish the run.
    let full = all_lines.join("\n") + "\n";
    std::fs::write(log_dir.join("stdout.jsonl"), &full).expect("complete the file");
    mark_completed(&log_dir);
    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("second pass");
    println!(
        "edge=partial_line second_pass: ingested={} complete={}",
        outcome.lines_ingested_total, outcome.source_complete
    );
    assert_eq!(outcome.lines_ingested_total, all_lines.len() as u64);
    assert!(outcome.source_complete);
    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len(), all_lines.len());
}

#[test]
fn truncated_source_is_a_sticky_error() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-truncated";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::CodexExecJson,
        CODEX_REAL_STREAM,
    );
    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("first pass");
    assert_eq!(
        outcome.lines_ingested_total,
        CODEX_REAL_STREAM.lines().count() as u64
    );

    // Shrink the file below the consumed offset.
    std::fs::write(log_dir.join("stdout.jsonl"), "{}\n").expect("truncate");
    let error =
        ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect_err("truncation must error");
    println!("edge=truncation error={error}");
    assert!(error.contains("TRANSCRIPT_SOURCE_TRUNCATED"), "{error}");

    // The error is sticky: the next pass skips instead of guessing.
    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("parked pass");
    assert!(outcome.skipped, "sticky error must park the spawn");
    let cursor = load_cursor(&db, spawn_id)
        .expect("cursor read")
        .expect("cursor exists");
    assert!(
        cursor
            .error
            .as_deref()
            .is_some_and(|detail| detail.contains("TRANSCRIPT_SOURCE_TRUNCATED")),
        "{cursor:?}"
    );
}

#[test]
fn empty_stream_finalizes_with_zero_rows() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-empty";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::ClaudeStreamJson,
        "",
    );

    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, true).expect("finalize empty");
    println!(
        "edge=empty_input after: total={} complete={}",
        outcome.lines_ingested_total, outcome.source_complete
    );
    assert_eq!(outcome.lines_ingested_total, 0);
    assert!(outcome.source_complete);
    assert!(scan_spawn_rows(&db, spawn_id).is_empty());
    let cursor = load_cursor(&db, spawn_id)
        .expect("cursor read")
        .expect("cursor exists");
    assert!(cursor.source_complete);
    assert_eq!(
        cursor.completed_reason.as_deref(),
        Some("finalized_at_teardown")
    );
}

#[test]
fn unattributable_spawn_dir_is_a_sticky_error() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-no-markers";
    let log_dir = root.path().join(spawn_id);
    std::fs::create_dir_all(&log_dir).expect("create dir");
    std::fs::write(log_dir.join("stdout.jsonl"), "{}\n").expect("write stdout");

    let error =
        ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect_err("must refuse to guess");
    println!("edge=unknown_format error={error}");
    assert!(
        error.contains("TRANSCRIPT_SOURCE_FORMAT_UNKNOWN"),
        "{error}"
    );
    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("parked");
    assert!(outcome.skipped);
}

#[test]
fn reingest_after_cursor_loss_is_idempotent() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-idempotent";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::CodexExecJson,
        CODEX_REAL_STREAM,
    );
    ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("first pass");
    let rows_before = scan_spawn_rows(&db, spawn_id);

    // Simulate cursor loss (e.g. CF_KV row cleared): re-ingestion must
    // overwrite the same keys, never duplicate.
    db.delete_batch(cf::CF_KV, [cursor_kv_key(spawn_id)])
        .expect("clear cursor");
    db.flush().expect("flush");
    ingest_spawn_dir_once(&db, spawn_id, &log_dir, false).expect("re-ingest");
    let rows_after = scan_spawn_rows(&db, spawn_id);
    println!(
        "edge=idempotent_reingest before_rows={} after_rows={}",
        rows_before.len(),
        rows_after.len()
    );
    assert_eq!(rows_before.len(), rows_after.len(), "no duplicates");
    for ((line_before, record_before), (line_after, record_after)) in
        rows_before.iter().zip(rows_after.iter())
    {
        assert_eq!(line_before, line_after);
        // Everything except the ingest timestamp must be identical.
        let mut normalized_before = record_before.clone();
        let mut normalized_after = record_after.clone();
        normalized_before.ts_ns = 0;
        normalized_after.ts_ns = 0;
        assert_eq!(normalized_before, normalized_after);
    }
}

#[test]
fn crlf_line_endings_hash_identically_to_lf() {
    // The spawn wrapper writes CRLF on Windows (verified during manual FSV #900):
    // the CR is part of the line terminator, so hashes and byte counts must
    // match the logical line exactly as an LF-terminated stream would.
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let crlf_content = CODEX_REAL_STREAM.lines().collect::<Vec<_>>().join("\r\n") + "\r\n";
    let lf_dir = plant_spawn_dir(
        root.path(),
        "agent-spawn-lf",
        TranscriptSource::CodexExecJson,
        CODEX_REAL_STREAM,
    );
    let crlf_dir = plant_spawn_dir(
        root.path(),
        "agent-spawn-crlf",
        TranscriptSource::CodexExecJson,
        &crlf_content,
    );
    ingest_spawn_dir_once(&db, "agent-spawn-lf", &lf_dir, true).expect("lf ingest");
    ingest_spawn_dir_once(&db, "agent-spawn-crlf", &crlf_dir, true).expect("crlf ingest");
    let lf_rows = scan_spawn_rows(&db, "agent-spawn-lf");
    let crlf_rows = scan_spawn_rows(&db, "agent-spawn-crlf");
    assert_eq!(lf_rows.len(), crlf_rows.len());
    for ((_n1, lf), (_n2, crlf)) in lf_rows.iter().zip(crlf_rows.iter()) {
        assert_eq!(
            lf.raw_line_sha256, crlf.raw_line_sha256,
            "line terminator must not change the recorded hash"
        );
        assert_eq!(lf.raw_line_bytes, crlf.raw_line_bytes);
        assert_eq!(lf.status, TranscriptParseStatus::Parsed);
    }
    // The recorded hash must equal a straight sha256 of the logical line.
    let first_line = CODEX_REAL_STREAM.lines().next().expect("line");
    assert_eq!(
        lf_rows[0].1.raw_line_sha256,
        sha256_hex(first_line.as_bytes())
    );
}

#[test]
fn claude_model_fallback_block_parses() {
    // Byte-for-byte real line captured 2026-06-12 from
    // agent-spawn-019ebe1f-f63c-75e3-b895-ccefdcb97c56: the CLI emitted a
    // `fallback` content block when claude-fable-5 fell back to
    // claude-opus-4-8. Found live by the manual FSV invalid-row counter (#900).
    let real_fallback_line = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg_01WkRP6miebFSLdz1rLkAbXa","type":"message","role":"assistant","content":[{"type":"fallback","from":{"model":"claude-fable-5"},"to":{"model":"claude-opus-4-8"}}],"stop_reason":null,"stop_sequence":null,"stop_details":null,"usage":{"input_tokens":3684,"cache_creation_input_tokens":4558,"cache_read_input_tokens":20866,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":4558},"output_tokens":3,"service_tier":"standard","inference_geo":"not_available"},"diagnostics":null,"context_management":null},"parent_tool_use_id":null,"session_id":"a2432ee1-03e2-42c2-be5a-d2e462581d1e","uuid":"05c2a484-d658-40fb-b682-b5f6360b8224","request_id":"req_011CbzDe9nyHAK7DsQjgKNc9"}"#;
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-fallback";
    let content = real_fallback_line.to_owned() + "\n";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::ClaudeStreamJson,
        &content,
    );
    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, true).expect("ingest");
    println!(
        "edge=model_fallback parsed={} invalid={}",
        outcome.new_parsed_rows, outcome.new_invalid_rows
    );
    assert_eq!(outcome.new_invalid_rows, 0, "fallback blocks must parse");
    let rows = scan_spawn_rows(&db, spawn_id);
    let record = &rows[0].1;
    assert_eq!(record.status, TranscriptParseStatus::Parsed);
    assert_eq!(record.role, Some(TranscriptRole::Assistant));
    assert_eq!(record.model.as_deref(), Some("claude-opus-4-8"));
    let summary = record.content_summary.as_deref().expect("fallback content");
    assert!(summary.contains("\"fallback\""), "{summary}");
    assert!(summary.contains("claude-fable-5"), "{summary}");
}

#[test]
fn unknown_event_type_is_an_invalid_row_not_a_skip() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-future-event";
    let content = "{\"type\":\"event_from_a_future_cli_version\",\"data\":1}\n";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::ClaudeStreamJson,
        content,
    );
    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, true).expect("ingest");
    assert_eq!(outcome.new_invalid_rows, 1);
    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len(), 1);
    let parse_error = rows[0].1.parse_error.as_deref().expect("detail");
    assert!(
        parse_error.contains("UNKNOWN_EVENT_TYPE: event_from_a_future_cli_version"),
        "format drift must surface with the offending type: {parse_error}"
    );
}

#[test]
fn ingest_all_walks_real_spawn_root_shape() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let claude_dir = plant_spawn_dir(
        root.path(),
        "agent-spawn-fleet-claude",
        TranscriptSource::ClaudeStreamJson,
        CLAUDE_REAL_STREAM,
    );
    let codex_dir = plant_spawn_dir(
        root.path(),
        "agent-spawn-fleet-codex",
        TranscriptSource::CodexExecJson,
        CODEX_REAL_STREAM,
    );
    mark_completed(&claude_dir);
    mark_completed(&codex_dir);
    // Noise the walker must ignore.
    std::fs::create_dir_all(root.path().join("not-a-spawn")).expect("noise dir");
    std::fs::write(root.path().join("stray-file.txt"), b"x").expect("noise file");

    let summary = ingest_all_spawn_dirs_once(&db, root.path());
    println!("edge=fleet_cycle summary={summary}");
    assert_eq!(summary["dirs_seen"], 2);
    assert_eq!(summary["errors"], 0);
    assert_eq!(summary["sources_completed"], 2);
    let expected_rows =
        CLAUDE_REAL_STREAM.lines().count() as u64 + CODEX_REAL_STREAM.lines().count() as u64;
    assert_eq!(summary["new_rows"], expected_rows);

    // Second cycle: both sources complete, nothing re-ingested.
    let summary = ingest_all_spawn_dirs_once(&db, root.path());
    assert_eq!(summary["new_rows"], 0);
    assert_eq!(summary["sources_completed"], 0);
}

#[test]
fn finalize_readback_counts_match_physical_rows() {
    let (_temp, db) = open_temp_db();
    let root = tempfile::tempdir().expect("spawn root");
    let spawn_id = "agent-spawn-readback";
    let log_dir = plant_spawn_dir(
        root.path(),
        spawn_id,
        TranscriptSource::ClaudeStreamJson,
        CLAUDE_REAL_STREAM,
    );
    let outcome = ingest_spawn_dir_once(&db, spawn_id, &log_dir, true).expect("finalize");
    assert!(outcome.source_complete);
    // The engine already performed the physical read-back; verify the same
    // truth independently here.
    let rows = scan_spawn_rows(&db, spawn_id);
    assert_eq!(rows.len() as u64, outcome.lines_ingested_total);
    let cursor = load_cursor(&db, spawn_id)
        .expect("cursor read")
        .expect("cursor exists");
    assert_eq!(cursor.parsed_rows + cursor.invalid_rows, rows.len() as u64);
    assert_eq!(
        cursor.completed_reason.as_deref(),
        Some("finalized_at_teardown"),
        "completion reason must be recorded: {cursor:?}"
    );
}
