//! Parser tests for the ambient session-file vocabulary, driven by REAL lines
//! captured from a live `~/.claude/projects/<slug>/<uuid>.jsonl` transcript
//! (`tests/fixtures/claude_session_real.jsonl`) — never synthesized shapes.

use super::*;
use std::{
    ffi::OsString,
    sync::{Mutex, MutexGuard},
};

/// Eight real records: mode, file-history-snapshot, user/meta, user/prompt,
/// assistant+tool_use, user/tool_result, assistant/end_turn, system.
const REAL_FIXTURE: &str = include_str!("../../../tests/fixtures/claude_session_real.jsonl");
const AMBIENT_ENV_VARS: [&str; 5] = [
    ROOT_ENV,
    "CLAUDE_CONFIG_DIR",
    "USERPROFILE",
    "HOME",
    "LOCALAPPDATA",
];

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn new(vars: &[&'static str]) -> Self {
        Self {
            saved: vars
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
        }
    }
}

fn set_test_env(name: &str, value: &str) {
    unsafe {
        std::env::set_var(name, value);
    }
}

fn remove_test_env(name: &str) {
    unsafe {
        std::env::remove_var(name);
    }
}

fn fresh_cursor() -> AmbientCursor {
    seed_cursor(
        &spawn_id_for_session("66b1db3d-6b6e-43c1-af0c-41de9236e7c5"),
        "66b1db3d-6b6e-43c1-af0c-41de9236e7c5",
        std::path::Path::new("C:/Users/test/.claude/projects/p/66b1db3d.jsonl"),
    )
}

fn parse_all() -> (
    Vec<AgentTranscriptRecord>,
    Vec<Option<Lifecycle>>,
    AmbientCursor,
) {
    let mut cursor = fresh_cursor();
    let mut records = Vec::new();
    let mut lifecycles = Vec::new();
    for (index, line) in REAL_FIXTURE.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_no = (index + 1) as u64;
        let (record, lifecycle) = parse_session_line(line.as_bytes(), line_no, &mut cursor);
        records.push(record);
        lifecycles.push(lifecycle);
    }
    (records, lifecycles, cursor)
}

#[test]
fn every_real_line_parses_without_an_invalid_row() {
    let (records, _lifecycles, _cursor) = parse_all();
    assert!(!records.is_empty(), "fixture must contain real lines");
    for record in &records {
        assert_eq!(
            record.status,
            TranscriptParseStatus::Parsed,
            "real line {} refused by the session parser: {:?}",
            record.line_no,
            record.parse_error
        );
        assert_eq!(record.source, TranscriptSource::ClaudeSessionJsonl);
        // The conversation id is stamped from the session id on every row.
        assert_eq!(
            record.conversation_id.as_deref(),
            Some("66b1db3d-6b6e-43c1-af0c-41de9236e7c5")
        );
        record
            .validate()
            .unwrap_or_else(|error| panic!("real parsed row failed validation: {error}"));
    }
}

#[test]
fn assistant_tool_use_row_carries_tools_and_drives_working() {
    let (records, lifecycles, _cursor) = parse_all();
    let (index, record) = records
        .iter()
        .enumerate()
        .find(|(_, record)| {
            record.event_kind.as_deref() == Some("assistant") && !record.tool_calls.is_empty()
        })
        .expect("fixture has an assistant row with a real tool_use block");
    assert_eq!(record.role, Some(TranscriptRole::Assistant));
    assert!(record.model.is_some(), "assistant rows carry a model");
    match &lifecycles[index] {
        Some(Lifecycle::ToolUse {
            tool_name,
            input_sha256,
        }) => {
            assert!(!tool_name.is_empty(), "tool_use lifecycle names the tool");
            assert_eq!(input_sha256.len(), 64, "input hash is a sha256 hex");
        }
        other => panic!("assistant+tool_use must yield ToolUse, got {other:?}"),
    }
}

#[test]
fn split_thinking_partial_with_tool_use_stop_is_working() {
    // Real session messages are split one record per content block: a record
    // carrying only `thinking` but `stop_reason:"tool_use"` is the agent mid-
    // turn — Working, even though the tool name lives on a sibling record.
    let (records, lifecycles, _cursor) = parse_all();
    let working = records.iter().enumerate().any(|(index, record)| {
        record.event_kind.as_deref() == Some("assistant")
            && record.tool_calls.is_empty()
            && lifecycles[index] == Some(Lifecycle::Working)
    });
    assert!(
        working,
        "a thinking-only partial with a tool_use stop_reason must drive Working"
    );
}

#[test]
fn assistant_end_turn_row_carries_usage_and_drives_idle() {
    let (records, lifecycles, _cursor) = parse_all();
    let (_index, record) = records
        .iter()
        .enumerate()
        .find(|(index, _)| lifecycles[*index] == Some(Lifecycle::Idle))
        .expect("fixture has an assistant row that ended its turn (Idle)");
    assert_eq!(record.role, Some(TranscriptRole::Assistant));
    assert!(record.tool_calls.is_empty(), "an idle turn issued no tool");
    let usage = record.usage.as_ref().expect("end_turn row carries usage");
    assert!(
        usage.input_tokens.is_some() || usage.output_tokens.is_some(),
        "real usage has token counts"
    );
}

#[test]
fn real_human_prompt_starts_a_turn_but_meta_does_not() {
    let (records, lifecycles, _cursor) = parse_all();
    let prompt = records
        .iter()
        .enumerate()
        .find(|(_, record)| record.event_kind.as_deref() == Some("user/prompt"));
    let (index, _record) = prompt.expect("fixture has a real human prompt");
    assert_eq!(lifecycles[index], Some(Lifecycle::TurnStarted));

    if let Some((meta_index, _)) = records
        .iter()
        .enumerate()
        .find(|(_, record)| record.event_kind.as_deref() == Some("user/meta"))
    {
        assert_eq!(
            lifecycles[meta_index], None,
            "meta user records are not human turns"
        );
    }
}

#[test]
fn tool_result_row_is_a_tool_role() {
    let (records, _lifecycles, _cursor) = parse_all();
    let record = records
        .iter()
        .find(|record| record.event_kind.as_deref() == Some("user/tool_result"))
        .expect("fixture has a tool_result user row");
    assert_eq!(record.role, Some(TranscriptRole::Tool));
    assert!(
        !record.tool_calls.is_empty(),
        "tool_result carries the result"
    );
}

#[test]
fn metadata_records_are_recognized_system_rows_never_invalid() {
    let (records, lifecycles, _cursor) = parse_all();
    for kind in [
        "mode",
        "file-history-snapshot",
        "system",
        // The four metadata types that were 100% of the live invalid rows
        // before they were recognized — locked here so a regression re-surfaces.
        "permission-mode",
        "pr-link",
        "worktree-state",
        "agent-name",
    ] {
        let (index, record) = records
            .iter()
            .enumerate()
            .find(|(_, record)| {
                record
                    .event_kind
                    .as_deref()
                    .is_some_and(|event| event == kind || event.starts_with(&format!("{kind}/")))
            })
            .unwrap_or_else(|| panic!("fixture must contain a {kind} record"));
        assert_eq!(record.status, TranscriptParseStatus::Parsed);
        assert_eq!(record.role, Some(TranscriptRole::System));
        assert_eq!(lifecycles[index], None, "{kind} is not a lifecycle signal");
    }
}

#[test]
fn turn_index_advances_on_distinct_assistant_messages() {
    let (_records, _lifecycles, cursor) = parse_all();
    assert!(
        cursor.turn_index >= 1,
        "at least one assistant turn was counted, got {}",
        cursor.turn_index
    );
}

#[test]
fn unknown_record_type_yields_a_fail_loud_invalid_row() {
    let mut cursor = fresh_cursor();
    let (record, lifecycle) =
        parse_session_line(br#"{"type":"totally-new-record-kind"}"#, 1, &mut cursor);
    assert_eq!(record.status, TranscriptParseStatus::Invalid);
    assert!(
        record
            .parse_error
            .as_deref()
            .is_some_and(|error| error.contains("UNKNOWN_RECORD_TYPE")),
        "unknown record types must be a counted, structured defect: {:?}",
        record.parse_error
    );
    assert_eq!(lifecycle, None);
}

#[test]
fn malformed_json_is_invalid_not_a_panic() {
    let mut cursor = fresh_cursor();
    let (record, _lifecycle) = parse_session_line(b"{not json", 1, &mut cursor);
    assert_eq!(record.status, TranscriptParseStatus::Invalid);
    assert!(
        record
            .parse_error
            .as_deref()
            .is_some_and(|error| error.contains("LINE_NOT_JSON"))
    );
}

#[test]
fn session_stem_accepts_uuid_rejects_sidecars() {
    assert!(is_session_stem("66b1db3d-6b6e-43c1-af0c-41de9236e7c5"));
    assert!(!is_session_stem("agent-a8d9639e7bc8b4733")); // subagent sidecar
    assert!(!is_session_stem("not-a-uuid"));
    assert!(!is_session_stem("66b1db3d6b6e43c1af0c41de9236e7c5")); // no dashes
}

#[test]
fn projects_root_override_wins() {
    let _env_lock = env_lock();
    let _env_guard = EnvGuard::new(&AMBIENT_ENV_VARS);
    set_test_env(ROOT_ENV, "C:/tmp/ambient-test-projects");
    let root = claude_projects_root().expect("override resolves");
    assert_eq!(
        root,
        std::path::PathBuf::from("C:/tmp/ambient-test-projects")
    );
}

#[test]
fn explicit_projects_root_allows_custom_db() {
    let _env_lock = env_lock();
    let _env_guard = EnvGuard::new(&AMBIENT_ENV_VARS);
    set_test_env(ROOT_ENV, "C:/tmp/ambient-explicit-projects");
    set_test_env("USERPROFILE", "C:/Users/test");
    let decision =
        ambient_projects_root_for_db(std::path::Path::new("C:/scratch/synapse/issue/db"))
            .expect("explicit root resolves")
            .expect("custom DB is allowed only because root is explicit");
    assert_eq!(
        decision.root,
        std::path::PathBuf::from("C:/tmp/ambient-explicit-projects")
    );
    assert_eq!(decision.scope, AmbientRootScope::ExplicitEnv);
}

#[test]
fn configured_daemon_db_allows_host_projects_root() {
    let _env_lock = env_lock();
    let _env_guard = EnvGuard::new(&AMBIENT_ENV_VARS);
    remove_test_env(ROOT_ENV);
    remove_test_env("CLAUDE_CONFIG_DIR");
    set_test_env("LOCALAPPDATA", "C:/Users/test/AppData/Local");
    set_test_env("USERPROFILE", "C:/Users/test");
    set_test_env("HOME", "C:/Users/test-home");

    let decision = ambient_projects_root_for_db(&crate::m3::default_daemon_db_path())
        .expect("host projects root resolves")
        .expect("configured daemon DB may discover host ambient sessions");

    assert_eq!(
        decision.root,
        std::path::PathBuf::from("C:/Users/test")
            .join(".claude")
            .join("projects")
    );
    assert_eq!(decision.scope, AmbientRootScope::ConfiguredDaemonDb);
}

#[test]
fn custom_db_without_explicit_projects_root_disables_host_discovery() {
    let _env_lock = env_lock();
    let _env_guard = EnvGuard::new(&AMBIENT_ENV_VARS);
    remove_test_env(ROOT_ENV);
    remove_test_env("CLAUDE_CONFIG_DIR");
    set_test_env("LOCALAPPDATA", "C:/Users/test/AppData/Local");
    set_test_env("USERPROFILE", "C:/Users/test");
    set_test_env("HOME", "C:/Users/test-home");

    let decision = ambient_projects_root_for_db(std::path::Path::new(
        "C:/Users/test/AppData/Local/synapse/issue-1427/db",
    ))
    .expect("custom DB decision is deterministic");

    assert_eq!(decision, None);
}

// ---------------------------------------------------------------------------
// Supporting regression coverage over real-shaped Claude transcript rows.
// ---------------------------------------------------------------------------

use synapse_core::SCHEMA_VERSION;
use synapse_storage::agent_transcripts::agent_transcript_spawn_prefix;

const REAL_SESSION_ID: &str = "66b1db3d-6b6e-43c1-af0c-41de9236e7c5";

fn open_temp_db() -> (tempfile::TempDir, Db) {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = Db::open(&temp.path().join("db"), SCHEMA_VERSION).expect("temp DB must open");
    (temp, db)
}

/// Builds a real `~/.claude/projects`-shaped tree containing one session file
/// whose bytes are the checked-in REAL transcript lines.
fn plant_projects_dir(
    root: &std::path::Path,
    session_id: &str,
    content: &str,
) -> std::path::PathBuf {
    let project = root.join("projects").join("C--code-leapablememory");
    std::fs::create_dir_all(&project).expect("create project dir");
    let file = project.join(format!("{session_id}.jsonl"));
    std::fs::write(&file, content).expect("write session file");
    root.join("projects")
}

fn scan_events_for_spawn(db: &Db, spawn_id: &str) -> Vec<AgentEventRecord> {
    db.scan_cf_prefix(cf::CF_AGENT_EVENTS, &[])
        .expect("scan events")
        .into_iter()
        .filter_map(|(_key, value)| decode_json::<AgentEventRecord>(&value).ok())
        .filter(|record| record.spawn_id.as_deref() == Some(spawn_id))
        .collect()
}

#[test]
fn backfill_writes_transcript_rows_and_registers_the_agent() {
    let (_temp, db) = open_temp_db();
    let root = _temp.path();
    let projects = plant_projects_dir(root, REAL_SESSION_ID, REAL_FIXTURE);
    let spawn_id = spawn_id_for_session(REAL_SESSION_ID);

    // TRIGGER: one discovery + ingest pass over the real-data projects tree.
    let summary = ingest_all_once(&db, &projects, u64::MAX);
    db.flush().expect("flush");

    // Readback 1: CF_AGENT_TRANSCRIPTS has one physical row per source line.
    let expected_lines = REAL_FIXTURE
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count() as u64;
    let rows = db
        .scan_cf_prefix(
            cf::CF_AGENT_TRANSCRIPTS,
            &agent_transcript_spawn_prefix(&spawn_id),
        )
        .expect("scan transcripts");
    assert_eq!(
        rows.len() as u64,
        expected_lines,
        "physical transcript rows must equal real source lines; summary={summary}"
    );
    for (_key, value) in &rows {
        let record: AgentTranscriptRecord = decode_json(value).expect("decode transcript row");
        assert_eq!(record.spawn_id, spawn_id);
        assert_eq!(record.source, TranscriptSource::ClaudeSessionJsonl);
        assert_eq!(record.status, TranscriptParseStatus::Parsed);
    }

    // Readback 2: CF_AGENT_EVENTS has registration + a lifecycle event.
    let events = scan_events_for_spawn(&db, &spawn_id);
    assert!(
        events
            .iter()
            .any(|e| e.kind == AgentEventKind::SpawnRequested),
        "agent must be registered with a SpawnRequested row: {events:?}"
    );
    assert!(
        events.iter().any(|e| e.kind == AgentEventKind::SpawnReady),
        "agent must have a SpawnReady row (drives Working)"
    );
    let spawn_requested = events
        .iter()
        .find(|e| e.kind == AgentEventKind::SpawnRequested)
        .unwrap();
    assert_eq!(
        spawn_requested.attributes.agent_name.as_deref(),
        Some("claude")
    );
    assert_eq!(
        spawn_requested.attributes.conversation_id.as_deref(),
        Some(REAL_SESSION_ID),
        "the Claude session id is carried as the conversation id"
    );
    assert!(
        spawn_requested.session_id.is_none(),
        "ambient agents must be unbound (session_id None) so they surface via unbound_reads"
    );
    // The cwd/git_branch harvested from the real envelope must be journaled.
    assert_eq!(
        spawn_requested
            .payload
            .get("working_dir")
            .and_then(|v| v.as_str()),
        Some("C:\\code\\leapablememory"),
        "real cwd is captured from the transcript envelope: {}",
        spawn_requested.payload
    );
}

#[test]
fn second_pass_is_idempotent_then_tails_appended_lines() {
    let (_temp, db) = open_temp_db();
    let root = _temp.path();
    let projects = plant_projects_dir(root, REAL_SESSION_ID, REAL_FIXTURE);
    let spawn_id = spawn_id_for_session(REAL_SESSION_ID);
    let prefix = agent_transcript_spawn_prefix(&spawn_id);

    ingest_all_once(&db, &projects, u64::MAX);
    let after_first = db
        .scan_cf_prefix(cf::CF_AGENT_TRANSCRIPTS, &prefix)
        .unwrap()
        .len();

    // Idempotent: a second pass with no new bytes writes nothing new.
    ingest_all_once(&db, &projects, u64::MAX);
    let after_second = db
        .scan_cf_prefix(cf::CF_AGENT_TRANSCRIPTS, &prefix)
        .unwrap()
        .len();
    assert_eq!(
        after_first, after_second,
        "re-ingest must not duplicate rows"
    );

    // Append a real new line (the session keeps writing live) and tail it.
    let file = projects
        .join("C--code-leapablememory")
        .join(format!("{REAL_SESSION_ID}.jsonl"));
    let new_line = REAL_FIXTURE
        .lines()
        .find(|l| l.contains("\"type\":\"assistant\"") && l.contains("end_turn"))
        .expect("fixture has an end_turn line to append");
    let mut existing = std::fs::read_to_string(&file).unwrap();
    existing.push_str(new_line);
    existing.push('\n');
    std::fs::write(&file, existing).unwrap();

    ingest_all_once(&db, &projects, u64::MAX);
    let after_append = db
        .scan_cf_prefix(cf::CF_AGENT_TRANSCRIPTS, &prefix)
        .unwrap()
        .len();
    assert_eq!(
        after_append,
        after_second + 1,
        "the appended live line must produce exactly one new physical row"
    );
}

#[test]
fn truncated_source_is_a_sticky_loud_error_not_silent() {
    let (_temp, db) = open_temp_db();
    let root = _temp.path();
    let projects = plant_projects_dir(root, REAL_SESSION_ID, REAL_FIXTURE);
    let spawn_id = spawn_id_for_session(REAL_SESSION_ID);

    ingest_all_once(&db, &projects, u64::MAX);

    // EDGE CASE: the file shrinks below the cursor offset (corruption/rotation).
    let file = projects
        .join("C--code-leapablememory")
        .join(format!("{REAL_SESSION_ID}.jsonl"));
    std::fs::write(&file, b"{}\n").unwrap();

    ingest_all_once(&db, &projects, u64::MAX);

    // The cursor must carry a sticky structured error, and the session is parked.
    let cursor = load_cursor(&db, &spawn_id)
        .expect("cursor read")
        .expect("cursor exists");
    let error = cursor.error.expect("truncation must stick a loud error");
    assert!(
        error.contains("AMBIENT_SOURCE_TRUNCATED"),
        "truncation surfaces a named error, never a silent re-read: {error}"
    );
}

/// Ignored supporting regression check for a machine with live
/// `~/.claude/projects` transcripts:
///   `cargo test -p synapse-mcp --bin synapse-mcp \
///       server::ambient_agents::tests::real_projects_zero_metadata_invalids \
///       -- --ignored --nocapture`
///
/// Snapshots every recent main-session file into a temp tree (so the live
/// daemon's locks and concurrent appends never interfere), ingests it through
/// the real pipeline into a real RocksDB, then physically reads back every
/// transcript row and asserts ZERO `UNKNOWN_RECORD_TYPE` invalids — the defect
/// that produced 652 invalid rows before the metadata vocabulary was completed.
#[test]
#[ignore = "machine-specific: requires live ~/.claude/projects transcripts"]
fn real_projects_zero_metadata_invalids() {
    let real_root = claude_projects_root().expect("resolve ~/.claude/projects");
    if !real_root.is_dir() {
        eprintln!("SKIP: no projects dir at {}", real_root.display());
        return;
    }
    let (_temp, db) = open_temp_db();
    let temp_projects = _temp.path().join("projects");

    // Snapshot recent main-session files (<uuid>.jsonl directly under a project).
    let mut copied = 0_u64;
    for project in std::fs::read_dir(&real_root)
        .expect("read projects")
        .flatten()
    {
        if !project.path().is_dir() {
            continue;
        }
        let dest_dir = temp_projects.join(project.file_name());
        for file in std::fs::read_dir(project.path())
            .expect("read project")
            .flatten()
        {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !is_session_stem(stem) {
                continue;
            }
            std::fs::create_dir_all(&dest_dir).expect("mk dest");
            if std::fs::copy(&path, dest_dir.join(file.file_name())).is_ok() {
                copied += 1;
            }
        }
    }
    assert!(copied > 0, "expected at least one real session file");

    let summary = ingest_all_once(&db, &temp_projects, u64::MAX);
    db.flush().expect("flush");
    eprintln!("ingest summary over {copied} real files: {summary}");

    // Physical readback: scan EVERY ambient transcript row, bucket invalids.
    let all = db
        .scan_cf_prefix(cf::CF_AGENT_TRANSCRIPTS, &[])
        .expect("scan");
    let mut parsed = 0_u64;
    let mut invalid_by_reason: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    for (_key, value) in &all {
        let record: AgentTranscriptRecord = decode_json(value).expect("decode");
        match record.status {
            TranscriptParseStatus::Parsed => parsed += 1,
            TranscriptParseStatus::Invalid => {
                let reason = record
                    .parse_error
                    .as_deref()
                    .map(|e| e.split(':').next().unwrap_or(e).to_owned())
                    .unwrap_or_else(|| "unknown".to_owned());
                *invalid_by_reason.entry(reason).or_default() += 1;
            }
        }
    }
    eprintln!("parsed rows: {parsed}");
    eprintln!("invalid rows by reason: {invalid_by_reason:?}");
    let total_invalid: u64 = invalid_by_reason.values().sum();
    assert_eq!(
        total_invalid, 0,
        "the pinned vocabulary parses every real line; any invalid is unhandled \
         format drift to add: {invalid_by_reason:?}"
    );
}

#[test]
fn ambient_spawn_id_is_shape_valid_for_downstream_readers() {
    let spawn_id = spawn_id_for_session("66b1db3d-6b6e-43c1-af0c-41de9236e7c5");
    assert!(spawn_id.starts_with("agent-spawn-"));
    assert!(
        spawn_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    );
    // A transcript row carrying this spawn id must pass the storage validator.
    let mut record = AgentTranscriptRecord::new(
        1,
        spawn_id,
        1,
        TranscriptSource::ClaudeSessionJsonl,
        0,
        "0".repeat(64),
    );
    record.role = Some(TranscriptRole::System);
    record.event_kind = Some("mode".to_owned());
    record
        .validate()
        .expect("ambient spawn id is storage-valid");
}
