use super::*;

fn test_spawn_params() -> ActSpawnAgentParams {
    ActSpawnAgentParams {
        cli: Some(ActSpawnAgentCli::Codex),
        kind: None,
        model: None,
        model_ref: None,
        prompt: Some("write a report".to_owned()),
        target: None,
        working_dir: None,
        mcp_url: "http://127.0.0.1:7700/mcp".to_owned(),
        wait_timeout_ms: 30_000,
        hold_open_ms: 1234,
        require_approval_gate: true,
        template_id: None,
        template_version: None,
        template_config_hash: None,
    }
}

fn test_spawn_session_summary(
    lifecycle: &str,
    last_action: Option<&str>,
) -> crate::server::session_tools::SessionSummary {
    crate::server::session_tools::SessionSummary {
        registry: crate::server::session_registry::SessionRegistryRead {
            session_id: "spawned-session".to_owned(),
            transport: "http".to_owned(),
            client_name: Some("codex-mcp-client".to_owned()),
            client_version: Some("test".to_owned()),
            protocol_version: Some("test".to_owned()),
            agent_kind: "codex".to_owned(),
            lifecycle: lifecycle.to_owned(),
            started_at_unix_ms: 1_000,
            last_seen_unix_ms: 1_500,
            last_seen_ms_ago: 50,
            stale_after_ms: 300_000,
            closed_at_unix_ms: (lifecycle != "live").then_some(1_550),
            last_action: last_action.map(ToOwned::to_owned),
            last_reason_code: (lifecycle != "live")
                .then_some("http_session_store_deleted".to_owned()),
            spawned_agent: None,
        },
        active_target: None,
        agent_logical_foreground: crate::server::session_tools::AgentLogicalForegroundReadback {
            source_of_truth: "test".to_owned(),
            session_id: "spawned-session".to_owned(),
            status: "missing".to_owned(),
            target: None,
            persisted_row_key: None,
            no_human_os_foreground_fallback: true,
            missing_reason: Some("test".to_owned()),
        },
        foreground_lane: crate::server::session_tools::ForegroundLaneReadback {
            source_of_truth: "test".to_owned(),
            session_id: "spawned-session".to_owned(),
            status: "missing".to_owned(),
            capacity_model: "test".to_owned(),
            capacity_exhausted: false,
            lane_kind: None,
            target_key: None,
            target: None,
            target_claim: None,
            owner_session_id: None,
            explicit_real_foreground_lease: false,
            no_human_os_foreground_fallback: true,
            missing_reason: Some("test".to_owned()),
        },
        target_claims: Vec::new(),
        persisted_cdp_target_owners: Vec::new(),
        lease: crate::server::session_tools::SessionLeaseReadback {
            held: false,
            owner_session_id: None,
            is_owner: false,
            acquired_at_ms_ago: None,
            renewed_at_ms_ago: None,
            ttl_ms: None,
            expires_in_ms: None,
        },
        agent_state: None,
        attention_class: crate::server::agent_state::AgentAttentionClass::None,
    }
}

fn test_local_model_row(api_key_env_var: Option<&str>) -> LocalModelRegistryRow {
    LocalModelRegistryRow {
        schema_version: 1,
        row_key: "local_model_registry/v1/model/deadbeef".to_owned(),
        name: "deepseek".to_owned(),
        base_url: "https://api.deepseek.com".to_owned(),
        model_id: "deepseek-v4-flash".to_owned(),
        api_shape: LocalModelApiShape::OpenAiChatCompletions,
        runtime_preset:
            crate::m3::local_models::LocalModelRuntimePreset::DeepSeekV4FlashNonThinking,
        context_length: Some(1_000_000),
        max_tools: Some(128),
        notes: None,
        enabled: true,
        allow_non_loopback: true,
        api_key_env_var: api_key_env_var.map(ToOwned::to_owned),
        created_at_unix_ms: 1,
        updated_at_unix_ms: 1,
        created_by_session: "session-test".to_owned(),
        updated_by_session: "session-test".to_owned(),
        last_probe: None,
        has_api_key_secret: false,
    }
}

fn resolver_test_db() -> (tempfile::TempDir, std::sync::Arc<synapse_storage::Db>) {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let db = std::sync::Arc::new(
        synapse_storage::Db::open(dir.path(), synapse_core::SCHEMA_VERSION).expect("open temp db"),
    );
    (dir, db)
}

fn shell_params(operation: ShellOperation) -> ShellParams {
    ShellParams {
        operation,
        command: None,
        args: None,
        working_dir: None,
        env: None,
        timeout_ms: None,
        execution_mode: None,
        durable_timeout_ms: None,
        idempotency_key: None,
        job_id: None,
        tail_bytes: None,
    }
}

fn process_params(operation: ProcessOperation) -> ProcessParams {
    ProcessParams {
        operation,
        target: None,
        args: None,
        working_dir: None,
        env: None,
        wait_for_window_title_regex: None,
        timeout_ms: None,
        idempotency_key: None,
        cdp_debug: None,
        force_renderer_accessibility: None,
        windows_console_window_state: None,
        desktop: None,
        pid: None,
        process_name_contains: None,
        command_line_contains: None,
        limit: None,
        include_command_line: None,
    }
}

fn tool_param_error_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}

fn sanitized_tool_input_schema(tool_name: &str) -> Value {
    let tools = crate::server::schema_sanitize::sanitize_tools(
        crate::server::SynapseService::tool_router().list_all(),
    );
    let tool = tools
        .iter()
        .find(|tool| tool.name.as_ref() == tool_name)
        .unwrap_or_else(|| panic!("{tool_name} tool missing"));
    Value::Object((*tool.input_schema).clone())
}

fn shell_schema_variant<'a>(schema: &'a Value, operation: &str) -> &'a Value {
    schema["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("shell schema oneOf missing"))
        .iter()
        .find(|variant| variant["properties"]["operation"]["const"] == operation)
        .unwrap_or_else(|| panic!("shell schema operation={operation} variant missing"))
}

fn schema_property_names(schema: &Value) -> BTreeSet<String> {
    schema["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema properties missing"))
        .keys()
        .cloned()
        .collect()
}

#[test]
fn shell_facade_rejects_unknown_operation_enum() {
    let error = serde_json::from_value::<ShellParams>(json!({"operation": "not_real"}))
        .expect_err("unknown shell operation must fail closed");
    assert!(error.to_string().contains("unknown variant"));
}

#[test]
fn shell_facade_public_schema_is_operation_specific() {
    let schema = sanitized_tool_input_schema("shell");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], Value::Bool(false));
    assert_eq!(
        schema["properties"]["durable_timeout_ms"]["description"],
        "run only. Applies if run creates a durable/background job; start uses timeout_ms for its durable lifetime cap."
    );

    let variants = schema["oneOf"]
        .as_array()
        .expect("shell schema oneOf present");
    assert_eq!(variants.len(), 4);
    for variant in variants {
        assert_eq!(variant["type"], "object");
        assert_eq!(variant["additionalProperties"], Value::Bool(false));
    }

    let run_fields = schema_property_names(shell_schema_variant(&schema, "run"));
    assert!(run_fields.contains("durable_timeout_ms"));
    assert!(run_fields.contains("idempotency_key"));
    assert!(run_fields.contains("execution_mode"));
    assert!(!run_fields.contains("job_id"));
    assert!(!run_fields.contains("tail_bytes"));

    let start_fields = schema_property_names(shell_schema_variant(&schema, "start"));
    assert!(start_fields.contains("timeout_ms"));
    assert!(start_fields.contains("job_id"));
    assert!(!start_fields.contains("durable_timeout_ms"));
    assert!(!start_fields.contains("idempotency_key"));
    assert!(!start_fields.contains("execution_mode"));
    assert!(!start_fields.contains("tail_bytes"));

    let status_fields = schema_property_names(shell_schema_variant(&schema, "status"));
    assert_eq!(
        status_fields,
        BTreeSet::from([
            "job_id".to_owned(),
            "operation".to_owned(),
            "tail_bytes".to_owned()
        ])
    );

    let cancel_fields = schema_property_names(shell_schema_variant(&schema, "cancel"));
    assert_eq!(
        cancel_fields,
        BTreeSet::from(["job_id".to_owned(), "operation".to_owned()])
    );
}

#[test]
fn shell_facade_validates_operation_specific_fields() {
    let empty_run = shell_run_params(ShellParams {
        command: Some(" ".to_owned()),
        ..shell_params(ShellOperation::Run)
    })
    .expect_err("run requires non-empty command");
    assert_eq!(
        tool_param_error_code(&empty_run),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );

    let wrong_run = shell_run_params(ShellParams {
        command: Some("powershell.exe".to_owned()),
        job_id: Some("job-from-status".to_owned()),
        ..shell_params(ShellOperation::Run)
    })
    .expect_err("run must reject status/cancel-only job_id");
    assert_eq!(
        wrong_run
            .data
            .as_ref()
            .and_then(|data| data.get("operation"))
            .and_then(Value::as_str),
        Some("run")
    );

    let missing_status = shell_status_params(shell_params(ShellOperation::Status))
        .expect_err("status requires job_id");
    assert_eq!(
        tool_param_error_code(&missing_status),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );

    let wrong_status = shell_status_params(ShellParams {
        command: Some("powershell.exe".to_owned()),
        job_id: Some("job-1".to_owned()),
        ..shell_params(ShellOperation::Status)
    })
    .expect_err("status rejects run-only command");
    assert_eq!(
        wrong_status
            .data
            .as_ref()
            .and_then(|data| data.get("operation"))
            .and_then(Value::as_str),
        Some("status")
    );
}

#[test]
fn process_facade_rejects_unknown_operation_enum() {
    let error = serde_json::from_value::<ProcessParams>(json!({"operation": "not_real"}))
        .expect_err("unknown process operation must fail closed");
    assert!(error.to_string().contains("unknown variant"));
}

#[test]
fn process_facade_validates_operation_specific_fields() {
    let missing_launch = process_launch_params(process_params(ProcessOperation::Launch))
        .expect_err("launch requires target");
    assert_eq!(
        tool_param_error_code(&missing_launch),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );

    let wrong_launch = process_launch_params(ProcessParams {
        target: Some("notepad.exe".to_owned()),
        pid: Some(1234),
        ..process_params(ProcessOperation::Launch)
    })
    .expect_err("launch rejects list/history-only pid");
    assert_eq!(
        wrong_launch
            .data
            .as_ref()
            .and_then(|data| data.get("operation"))
            .and_then(Value::as_str),
        Some("launch")
    );

    let wrong_history = validate_process_query_params(
        ProcessOperation::History,
        &ProcessParams {
            target: Some("notepad.exe".to_owned()),
            ..process_params(ProcessOperation::History)
        },
    )
    .expect_err("history rejects launch target field");
    assert_eq!(
        wrong_history
            .data
            .as_ref()
            .and_then(|data| data.get("operation"))
            .and_then(Value::as_str),
        Some("history")
    );

    let unbounded_history = validate_process_query_params(
        ProcessOperation::History,
        &ProcessParams {
            limit: Some(PROCESS_HISTORY_MAX_LIMIT + 1),
            ..process_params(ProcessOperation::History)
        },
    )
    .expect_err("history rejects limits above the explicit cap");
    assert_eq!(
        tool_param_error_code(&unbounded_history),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn process_facade_delegate_error_preserves_code_and_adds_context() {
    let low_level = ErrorData::new(
        ErrorCode(-32099),
        "target missing",
        Some(json!({
            "code": error_codes::ACTION_TARGET_INVALID,
            "target": "C:\\missing.exe",
        })),
    );
    let error = process_facade_delegate_error(
        ProcessOperation::Launch,
        "C:\\missing.exe",
        low_level,
        "fix target",
    );
    let data = error.data.as_ref().expect("facade data");
    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_TARGET_INVALID)
    );
    assert_eq!(
        data.get("operation").and_then(Value::as_str),
        Some("launch")
    );
    assert_eq!(
        data.get("source_id").and_then(Value::as_str),
        Some("C:\\missing.exe")
    );
    assert_eq!(
        data.get("remediation").and_then(Value::as_str),
        Some("fix target")
    );
    assert!(data.get("cause").is_some());
}

#[test]
fn resolve_spawn_api_key_none_when_row_absent_or_keyless() {
    let (_dir, db) = resolver_test_db();
    // Non-local-model spawn: nothing to resolve.
    assert!(
        resolve_spawn_local_model_api_key(&db, None)
            .expect("no row resolves cleanly")
            .is_none()
    );
    // Loopback model with no declared key (e.g. Ollama): nothing to forward.
    let row = test_local_model_row(None);
    assert!(
        resolve_spawn_local_model_api_key(&db, Some(&row))
            .expect("keyless row resolves cleanly")
            .is_none()
    );
}

#[test]
fn resolve_spawn_api_key_forwards_value_when_present() {
    let (_dir, db) = resolver_test_db();
    // Unique env var name so parallel tests never collide on process env.
    let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_PRESENT";
    // SAFETY: single-threaded within this test; unique key avoids races.
    unsafe { std::env::set_var(env_var, "sk-secret-value") };
    let row = test_local_model_row(Some(env_var));
    let resolved = resolve_spawn_local_model_api_key(&db, Some(&row))
        .expect("present key resolves")
        .expect("present key yields a value");
    assert_eq!(resolved.0, env_var);
    assert_eq!(resolved.1, "sk-secret-value");
    unsafe { std::env::remove_var(env_var) };
}

#[cfg(windows)]
#[test]
fn resolve_spawn_api_key_prefers_encrypted_secret_store_over_env() {
    // FSV: a DPAPI-encrypted stored key takes priority over the process env
    // and round-trips through CryptProtectData/CryptUnprotectData.
    let (_dir, db) = resolver_test_db();
    let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_PRECEDENCE";
    unsafe { std::env::set_var(env_var, "env-fallback-value") };
    let row = test_local_model_row(Some(env_var));
    crate::m3::local_models::put_model_secret(&db, &row.name, "stored-secret-value", "test")
        .expect("store secret");
    let resolved = resolve_spawn_local_model_api_key(&db, Some(&row))
        .expect("secret resolves")
        .expect("secret yields a value");
    assert_eq!(resolved.0, env_var);
    assert_eq!(
        resolved.1, "stored-secret-value",
        "stored secret must win over the env var"
    );
    unsafe { std::env::remove_var(env_var) };
}

#[test]
fn resolve_spawn_api_key_refuses_loudly_when_missing() {
    let (_dir, db) = resolver_test_db();
    let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_MISSING";
    unsafe { std::env::remove_var(env_var) };
    let row = test_local_model_row(Some(env_var));
    let err = resolve_spawn_local_model_api_key(&db, Some(&row))
        .expect_err("missing key must refuse the spawn loudly");
    let data = err.data.expect("refusal carries structured detail");
    assert_eq!(data["code"], error_codes::MODEL_API_KEY_MISSING);
    assert_eq!(data["reason"], "local_model_api_key_missing");
    assert_eq!(data["detail"]["api_key_env_var"], env_var);
    assert!(
        data["detail"]["resolver_data"]["api_key_env_var"]
            .as_str()
            .unwrap_or_default()
            .contains(env_var),
        "resolver detail names the missing env var"
    );
}

#[test]
fn resolve_spawn_api_key_refuses_when_value_blank() {
    let (_dir, db) = resolver_test_db();
    let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_BLANK";
    unsafe { std::env::set_var(env_var, "   ") };
    let row = test_local_model_row(Some(env_var));
    let err = resolve_spawn_local_model_api_key(&db, Some(&row))
        .expect_err("blank key is treated as missing");
    let data = err.data.expect("refusal carries structured detail");
    assert_eq!(data["code"], error_codes::MODEL_API_KEY_MISSING);
    unsafe { std::env::remove_var(env_var) };
}

#[test]
fn spawn_prompt_names_powershell_contract() {
    let dir = Path::new(r"C:\code\Synapse");
    let task_started_path = dir.join("task-started.json");
    let task_started_script_path = dir.join("write-task-started.ps1");
    let prompt = build_agent_spawn_prompt(
        "agent-spawn-test",
        &test_spawn_params(),
        dir,
        &task_started_path,
        &task_started_script_path,
    )
    .expect("build spawn prompt");

    assert!(prompt.contains("PowerShell on Windows, not Bash"));
    assert!(prompt.contains("Do not use Bash heredocs"));
    assert!(prompt.contains("@'"));
    assert!(prompt.contains("Start-Sleep -Milliseconds 1234"));
    assert!(prompt.contains("task-start readiness artifact"));
    assert!(prompt.contains("task-started.json"));
    assert!(prompt.contains("\"operation\":\"task_started\""));
    assert!(prompt.contains("do not use a helper script or direct file write"));
    assert!(!prompt.contains("agent_spawn_task_started"));
    assert!(!prompt.contains("write-task-started.ps1"));

    let prompt_json = serde_json::to_string(&prompt).expect("prompt serializes");
    assert!(!prompt_json.contains("write-task-started.ps1"));
    assert!(!prompt_json.contains("session_list"));
    assert!(!prompt_json.contains("get_target"));
    assert!(!prompt_json.contains("set_target"));
    assert!(prompt.contains("session facade"));
    assert!(prompt.contains("target facade"));
}

#[test]
fn agent_spawn_files_do_not_materialize_task_started_helper() {
    struct EnvRestore {
        key: &'static str,
        value: Option<std::ffi::OsString>,
    }
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                match &self.value {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    let local_appdata = tempfile::TempDir::new().expect("create local appdata");
    let _restore = EnvRestore {
        key: "LOCALAPPDATA",
        value: std::env::var_os("LOCALAPPDATA"),
    };
    unsafe { std::env::set_var("LOCALAPPDATA", local_appdata.path()) };

    let working_dir = tempfile::TempDir::new().expect("create working dir");
    let codex_files = prepare_agent_spawn_files(
        "agent-spawn-codex",
        &test_spawn_params(),
        working_dir.path(),
    )
    .expect("prepare codex spawn files");

    assert!(
        !codex_files.task_started_script_path.exists(),
        "Codex readiness must go through agent operation=task_started, not a helper script"
    );
    assert!(
        codex_files
            .codex_app_server_runner_path
            .as_ref()
            .is_some_and(|path| path.exists()),
        "Codex still writes the app-server runner"
    );

    let mut claude_params = test_spawn_params();
    claude_params.cli = Some(ActSpawnAgentCli::Claude);
    let claude_files =
        prepare_agent_spawn_files("agent-spawn-claude", &claude_params, working_dir.path())
            .expect("prepare claude spawn files");

    assert!(
        !claude_files.task_started_script_path.exists(),
        "Claude readiness must go through agent operation=task_started, not a helper script"
    );
    let claude_settings_path = claude_files
        .hook_settings_path
        .as_ref()
        .expect("Claude hook settings path");
    let claude_settings =
        fs::read_to_string(claude_settings_path).expect("read Claude hook settings");
    assert!(
        !claude_settings.contains("write-task-started.ps1"),
        "Claude hook settings must not allowlist a task-start helper script"
    );
}

#[test]
fn agent_spawn_wait_deadline_rejects_impossible_timeout() {
    let error = agent_spawn_wait_deadline(MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS + 1)
        .expect_err("over-limit deadline must fail closed");

    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("code")),
        Some(&json!(error_codes::TOOL_PARAMS_INVALID))
    );
    assert!(error.message.contains("must be <="));
}

#[test]
fn agent_spawn_wait_deadline_uses_supplied_phase_start() {
    let start = Instant::now();
    let deadline = agent_spawn_wait_deadline_from(start, 1_234).expect("valid phase deadline");

    assert_eq!(deadline.duration_since(start), Duration::from_millis(1_234));
}

#[test]
fn spawn_session_readiness_rejects_closed_session_even_after_tool_call() {
    let summary = test_spawn_session_summary("closed", Some("tools/call:session_list"));
    let before_session_ids = BTreeSet::new();
    let readiness = spawn_session_candidate_readiness(
        &summary,
        ActSpawnAgentCli::Codex,
        None,
        &before_session_ids,
        1_000,
    );

    assert_eq!(readiness.get("ready").and_then(Value::as_bool), Some(false));
    assert_eq!(
        readiness.get("reason").and_then(Value::as_str),
        Some("session_not_live")
    );
    assert_eq!(
        readiness.get("last_reason_code").and_then(Value::as_str),
        Some("http_session_store_deleted")
    );
    assert!(
        !spawn_session_identity_matches(
            &summary,
            ActSpawnAgentCli::Codex,
            &before_session_ids,
            1_000,
        ),
        "closed sessions must not bind through the task-start or observed-progress paths"
    );
}

#[test]
fn spawn_session_readiness_accepts_live_tool_call() {
    let summary = test_spawn_session_summary("live", Some("tools/call:session_list"));
    let before_session_ids = BTreeSet::new();
    let readiness = spawn_session_candidate_readiness(
        &summary,
        ActSpawnAgentCli::Codex,
        None,
        &before_session_ids,
        1_000,
    );

    assert_eq!(readiness.get("ready").and_then(Value::as_bool), Some(true));
    assert_eq!(
        readiness.get("reason").and_then(Value::as_str),
        Some("tool_call_observed")
    );
    assert!(spawn_session_identity_matches(
        &summary,
        ActSpawnAgentCli::Codex,
        &before_session_ids,
        1_000,
    ));
}

#[test]
fn task_start_rebinds_from_closed_codex_bootstrap_session_to_live_reconnect() {
    let service = SynapseService::new();
    let dir = tempfile::TempDir::new().expect("temp");
    let files = observed_progress_test_files(dir.path());
    let params = test_spawn_params();
    let launch_ms = unix_time_ms_now();
    let spawn_id = "agent-spawn-rebind-test";
    let old_session_id = "codex-bootstrap-closed";
    let new_session_id = "codex-live-reconnect";
    let mut before_session_ids = BTreeSet::new();
    before_session_ids.insert("operator-session".to_owned());

    let spawned_metadata = || SpawnedAgentRead {
        spawn_id: spawn_id.to_owned(),
        cli: "codex".to_owned(),
        launcher_process_id: 4242,
        agent_process_id: Some(5252),
        started_by_session_id: Some("operator-session".to_owned()),
        launched_at_unix_ms: launch_ms,
        launch_target: "none".to_owned(),
        log_dir: dir.path().display().to_string(),
        template_id: None,
        template_version: None,
        control: None,
    };

    {
        let mut registry = service
            .session_registry_ref()
            .lock()
            .expect("session registry");
        registry.record_spawned_agent(old_session_id, spawned_metadata(), launch_ms + 10);
        registry.record_seen(
            old_session_id,
            Some("tools/list".to_owned()),
            launch_ms + 20,
        );
        registry.record_closed_with_reason(
            old_session_id,
            launch_ms + 30,
            Some("http_session_store_deleted"),
        );
        registry.record_spawned_agent(new_session_id, spawned_metadata(), launch_ms + 40);
        registry.record_seen(
            new_session_id,
            Some("tools/list".to_owned()),
            launch_ms + 50,
        );
    }

    let liveness_error = json!({
        "reason": "spawned_session_not_live",
        "session_id": old_session_id,
    });
    let no_progress = service
        .rebind_spawned_agent_session_for_task_start(
            &params,
            ActSpawnAgentCli::Codex,
            spawn_id,
            &before_session_ids,
            launch_ms,
            4242,
            &files,
            &liveness_error,
        )
        .expect("rebind read succeeds");
    assert!(
        no_progress.is_none(),
        "a live replacement without daemon-observed task progress must not be rebound"
    );

    fs::write(
        files.codex_app_server_control_path.as_ref().unwrap(),
        serde_json::to_vec(&json!({
            "thread_id": "019ef79d-abb2-71e0-9e91-3d1f5a34dd9b",
            "turn_status": "inProgress"
        }))
        .expect("encode codex control"),
    )
    .expect("write codex control");
    let rebound = service
        .rebind_spawned_agent_session_for_task_start(
            &params,
            ActSpawnAgentCli::Codex,
            spawn_id,
            &before_session_ids,
            launch_ms,
            4242,
            &files,
            &liveness_error,
        )
        .expect("rebind read succeeds")
        .expect("progress-backed replacement session binds");

    assert_eq!(rebound.session_id, new_session_id);
}

#[test]
fn task_start_artifact_validation_rejects_wrong_session() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: None,
        codex_app_server_runner_path: None,
        codex_app_server_control_path: None,
        codex_app_server_events_path: None,
        codex_app_server_stdout_path: None,
        codex_app_server_stderr_path: None,
        local_model_runner_path: None,
    };
    fs::write(
        &files.task_started_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "spawn_id": "agent-spawn-test",
            "cli": "codex",
            "session_id": "wrong-session",
            "status": "started",
            "health_ok": true,
            "target_ok": true,
            "assigned_prompt_present": true,
            "task_started_path": files.task_started_path.display().to_string(),
            "readiness_source": "agent_spawn_task_started_tool",
            "started_at_unix_ms": 1234
        }))
        .expect("encode task start"),
    )
    .expect("write task start");
    let matched = MatchedSpawnSession {
        session_id: "expected-session".to_owned(),
        registered_at_unix_ms: 1000,
        agent_process_id: Some(42),
    };
    let error = read_agent_spawn_task_start_artifact(
        &files,
        &test_spawn_params(),
        ActSpawnAgentCli::Codex,
        "agent-spawn-test",
        &matched,
    )
    .expect_err("wrong session must fail");

    assert_eq!(
        error.get("reason").and_then(Value::as_str),
        Some("task_start_artifact_invalid")
    );
    assert!(
        error
            .get("validation_errors")
            .and_then(Value::as_array)
            .expect("validation errors")
            .iter()
            .any(|entry| entry.as_str() == Some("session_id mismatch"))
    );
}

#[test]
fn task_start_artifact_validation_rejects_legacy_direct_artifact_for_codex() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: None,
        codex_app_server_runner_path: None,
        codex_app_server_control_path: None,
        codex_app_server_events_path: None,
        codex_app_server_stdout_path: None,
        codex_app_server_stderr_path: None,
        local_model_runner_path: None,
    };
    fs::write(
        &files.task_started_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "spawn_id": "agent-spawn-test",
            "cli": "codex",
            "session_id": "expected-session",
            "status": "started",
            "health_ok": true,
            "target_ok": true,
            "assigned_prompt_present": true,
            "task_started_path": files.task_started_path.display().to_string(),
            "started_at_unix_ms": 1234
        }))
        .expect("encode task start"),
    )
    .expect("write task start");
    let matched = MatchedSpawnSession {
        session_id: "expected-session".to_owned(),
        registered_at_unix_ms: 1000,
        agent_process_id: Some(42),
    };
    let error = read_agent_spawn_task_start_artifact(
        &files,
        &test_spawn_params(),
        ActSpawnAgentCli::Codex,
        "agent-spawn-test",
        &matched,
    )
    .expect_err("Codex must not accept direct task-start artifacts");

    assert_eq!(
        error.get("reason").and_then(Value::as_str),
        Some("task_start_artifact_invalid")
    );
    assert!(
        error
            .get("validation_errors")
            .and_then(Value::as_array)
            .expect("validation errors")
            .iter()
            .any(|entry| entry.as_str()
                == Some(
                    "readiness_source must be agent_spawn_task_started_tool for claude/codex spawns"
                ))
    );
}

#[test]
fn task_start_artifact_validation_accepts_legacy_local_model_artifact() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: None,
        codex_app_server_runner_path: None,
        codex_app_server_control_path: None,
        codex_app_server_events_path: None,
        codex_app_server_stdout_path: None,
        codex_app_server_stderr_path: None,
        local_model_runner_path: None,
    };
    fs::write(
        &files.task_started_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "spawn_id": "agent-spawn-test",
            "cli": "local-model",
            "session_id": "expected-session",
            "status": "started",
            "health_ok": true,
            "target_ok": true,
            "assigned_prompt_present": true,
            "task_started_path": files.task_started_path.display().to_string(),
            "started_at_unix_ms": 1234
        }))
        .expect("encode task start"),
    )
    .expect("write task start");
    let matched = MatchedSpawnSession {
        session_id: "expected-session".to_owned(),
        registered_at_unix_ms: 1000,
        agent_process_id: Some(42),
    };
    let mut params = test_spawn_params();
    params.cli = None;
    params.kind = Some(ActSpawnAgentCli::LocalModel);
    params.model_ref = Some("ollama-gemma4-e4b".to_owned());
    let read = read_agent_spawn_task_start_artifact(
        &files,
        &params,
        ActSpawnAgentCli::LocalModel,
        "agent-spawn-test",
        &matched,
    )
    .expect("read task start")
    .expect("task start present");

    assert_eq!(read.started_at_unix_ms, 1234);
    assert_eq!(read.readiness_source, "task_start_artifact");
}

#[test]
fn task_start_artifact_validation_accepts_daemon_readiness_tool_source() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: None,
        codex_app_server_runner_path: None,
        codex_app_server_control_path: None,
        codex_app_server_events_path: None,
        codex_app_server_stdout_path: None,
        codex_app_server_stderr_path: None,
        local_model_runner_path: None,
    };
    let artifact = build_agent_spawn_task_started_artifact(
        "agent-spawn-test",
        ActSpawnAgentCli::Claude,
        "expected-session",
        true,
        &files.task_started_path,
    );
    fs::write(
        &files.task_started_path,
        serde_json::to_vec_pretty(&artifact).expect("encode task start"),
    )
    .expect("write task start");
    let matched = MatchedSpawnSession {
        session_id: "expected-session".to_owned(),
        registered_at_unix_ms: 1000,
        agent_process_id: Some(42),
    };
    let mut params = test_spawn_params();
    params.cli = Some(ActSpawnAgentCli::Claude);
    let read = read_agent_spawn_task_start_artifact(
        &files,
        &params,
        ActSpawnAgentCli::Claude,
        "agent-spawn-test",
        &matched,
    )
    .expect("read task start")
    .expect("task start present");

    assert!(read.started_at_unix_ms > 0);
    assert_eq!(read.readiness_source, "agent_spawn_task_started_tool");
}

fn observed_progress_test_files(dir: &Path) -> AgentSpawnFiles {
    AgentSpawnFiles {
        log_dir: dir.to_path_buf(),
        prompt_path: dir.join("prompt.txt"),
        stdout_path: dir.join("stdout.jsonl"),
        stderr_path: dir.join("stderr.log"),
        final_message_path: dir.join("final-message.txt"),
        completion_status_path: dir.join("completion-status.json"),
        task_started_path: dir.join("task-started.json"),
        task_started_script_path: dir.join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: None,
        codex_app_server_runner_path: None,
        codex_app_server_control_path: Some(dir.join("codex-control.json")),
        codex_app_server_events_path: None,
        codex_app_server_stdout_path: None,
        codex_app_server_stderr_path: None,
        local_model_runner_path: None,
    }
}

#[test]
fn observed_task_progress_detects_real_liveness_without_artifact() {
    // No artifact + no activity => no false-positive (deadline still governs).
    let empty = tempfile::TempDir::new().expect("temp");
    let files = observed_progress_test_files(empty.path());
    assert_eq!(
        agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::Codex),
        None,
        "an idle spawn dir must not be reported as making progress"
    );

    // A produced final message proves the agent ran the task to completion,
    // even though it never wrote the cooperative task-start artifact.
    let finished = tempfile::TempDir::new().expect("temp");
    let files = observed_progress_test_files(finished.path());
    fs::write(&files.final_message_path, b"PONG").expect("write final message");
    assert_eq!(
        agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::Claude),
        Some("final_message_present")
    );

    // Codex: a control artifact with an established thread + underway turn is
    // daemon-trusted proof the agent connected and is executing.
    let codex = tempfile::TempDir::new().expect("temp");
    let files = observed_progress_test_files(codex.path());
    fs::write(
        files.codex_app_server_control_path.as_ref().unwrap(),
        serde_json::to_vec(&json!({
            "thread_id": "019ec782-052f-7083-a7f9-79d97702b344",
            "turn_status": "completed"
        }))
        .unwrap(),
    )
    .expect("write codex control");
    assert_eq!(
        agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::Codex),
        Some("codex_control_thread_established")
    );

    // A control artifact that only reached `starting` is NOT yet proof.
    let codex_early = tempfile::TempDir::new().expect("temp");
    let files = observed_progress_test_files(codex_early.path());
    fs::write(
        files.codex_app_server_control_path.as_ref().unwrap(),
        serde_json::to_vec(&json!({ "thread_id": "", "turn_status": "starting" })).unwrap(),
    )
    .expect("write codex control");
    assert_eq!(
        agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::Codex),
        None
    );

    // Local-model stdout turn activity proves the task is underway.
    let local = tempfile::TempDir::new().expect("temp");
    let files = observed_progress_test_files(local.path());
    fs::write(
        &files.stdout_path,
        b"{\"type\":\"local.turn.started\",\"turn_index\":1}\n",
    )
    .expect("write stdout");
    assert_eq!(
        agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::LocalModel),
        Some("stdout_turn_activity")
    );
}

#[test]
fn task_start_session_id_only_binds_matching_spawn() {
    let dir = tempfile::TempDir::new().expect("temp");
    let files = observed_progress_test_files(dir.path());
    fs::write(
        &files.task_started_path,
        serde_json::to_vec(&json!({
            "spawn_id": "agent-spawn-current",
            "session_id": "session-current"
        }))
        .expect("encode task-start marker"),
    )
    .expect("write task-start marker");

    assert_eq!(
        task_start_session_id_for_spawn(&files, "agent-spawn-current").as_deref(),
        Some("session-current")
    );
    assert_eq!(
        task_start_session_id_for_spawn(&files, "agent-spawn-other"),
        None
    );
}

#[test]
fn task_start_artifact_validation_accepts_bom_prefixed_artifact() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: None,
        codex_app_server_runner_path: None,
        codex_app_server_control_path: None,
        codex_app_server_events_path: None,
        codex_app_server_stdout_path: None,
        codex_app_server_stderr_path: None,
        local_model_runner_path: None,
    };
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend(
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "spawn_id": "agent-spawn-test",
            "cli": "codex",
            "session_id": "expected-session",
            "status": "started",
            "health_ok": true,
            "target_ok": true,
            "assigned_prompt_present": true,
            "task_started_path": files.task_started_path.display().to_string(),
            "readiness_source": "agent_spawn_task_started_tool",
            "started_at_unix_ms": 1234
        }))
        .expect("encode task start"),
    );
    fs::write(&files.task_started_path, bytes).expect("write task start");
    let matched = MatchedSpawnSession {
        session_id: "expected-session".to_owned(),
        registered_at_unix_ms: 1000,
        agent_process_id: Some(42),
    };
    let read = read_agent_spawn_task_start_artifact(
        &files,
        &test_spawn_params(),
        ActSpawnAgentCli::Codex,
        "agent-spawn-test",
        &matched,
    )
    .expect("read task start")
    .expect("task start present");

    assert_eq!(read.started_at_unix_ms, 1234);
}

#[test]
fn codex_control_artifact_validation_accepts_matching_artifact() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let control_path = dir.path().join("codex-control.json");
    let files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: Some(dir.path().join("codex-notify.ps1")),
        codex_app_server_runner_path: Some(dir.path().join("codex-app-server-runner.ps1")),
        codex_app_server_control_path: Some(control_path.clone()),
        codex_app_server_events_path: Some(dir.path().join("codex-app-server-events.jsonl")),
        codex_app_server_stdout_path: Some(dir.path().join("codex-app-server.stdout.log")),
        codex_app_server_stderr_path: Some(dir.path().join("codex-app-server.stderr.log")),
        local_model_runner_path: None,
    };
    fs::write(
        &control_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "protocol": "codex_app_server_ws",
            "endpoint": "ws://127.0.0.1:38658",
            "control_path": control_path.display().to_string(),
            "events_path": files
                .codex_app_server_events_path
                .as_ref()
                .expect("events path")
                .display()
                .to_string(),
            "app_server_process_id": 1234,
            "thread_id": "thread-1",
            "turn_id": "turn-1",
            "turn_status": "inProgress",
            "last_error": null,
            "approval_policy": "on-request",
            "sandbox_mode": "workspace-write",
            "app_server_request_bridge_url": "http://127.0.0.1:17700/codex-app-server/request",
            "last_app_server_request_status": "responded",
            "last_app_server_request_method": "mcpServer/elicitation/request",
            "last_app_server_request_id": "3",
            "last_app_server_request_approval_id": "apr1-test",
            "last_app_server_request_final_status": "accepted",
            "last_app_server_request_error": null,
            "last_app_server_request_at_unix_ms": 123790,
            "last_steer_status": "delivered",
            "last_steer_error": null,
            "last_steer_at_unix_ms": 123789,
            "last_steer_turn_id": "turn-1",
            "last_steer_instruction_chars": 42,
            "updated_at_unix_ms": 123456
        }))
        .expect("encode control"),
    )
    .expect("write control");

    let control = read_spawned_agent_control_artifact(&files, ActSpawnAgentCli::Codex)
        .expect("control read")
        .expect("codex control present");
    assert_eq!(control.thread_id.as_deref(), Some("thread-1"));
    assert_eq!(control.turn_id.as_deref(), Some("turn-1"));
    assert_eq!(control.last_steer_status.as_deref(), Some("delivered"));
    assert_eq!(control.last_steer_turn_id.as_deref(), Some("turn-1"));
    assert_eq!(control.last_steer_instruction_chars, Some(42));
    assert_eq!(control.approval_policy.as_deref(), Some("on-request"));
    assert_eq!(control.sandbox_mode.as_deref(), Some("workspace-write"));
    assert_eq!(
        control.last_app_server_request_status.as_deref(),
        Some("responded")
    );
    assert_eq!(
        control.last_app_server_request_approval_id.as_deref(),
        Some("apr1-test")
    );

    let mut bom_prefixed = vec![0xEF, 0xBB, 0xBF];
    bom_prefixed.extend(
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "protocol": "codex_app_server_ws",
            "endpoint": "ws://127.0.0.1:38658",
            "control_path": control_path.display().to_string(),
            "events_path": files
                .codex_app_server_events_path
                .as_ref()
                .expect("events path")
                .display()
                .to_string(),
            "app_server_process_id": 1234,
            "thread_id": "thread-bom",
            "turn_id": "turn-bom",
            "turn_status": "inProgress",
            "last_error": null,
            "updated_at_unix_ms": 123456
        }))
        .expect("encode bom control"),
    );
    fs::write(&control_path, bom_prefixed).expect("write bom control");
    let control = read_spawned_agent_control_artifact(&files, ActSpawnAgentCli::Codex)
        .expect("bom control read")
        .expect("codex control present");
    assert_eq!(control.thread_id.as_deref(), Some("thread-bom"));
    assert_eq!(control.turn_id.as_deref(), Some("turn-bom"));

    fs::write(
        &control_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "protocol": "codex_app_server_ws",
            "endpoint": "ws://127.0.0.1:38658",
            "control_path": control_path.display().to_string(),
            "events_path": "events.jsonl",
            "app_server_process_id": 1234,
            "thread_id": null,
            "turn_id": "turn-1",
            "turn_status": "inProgress",
            "last_error": null,
            "updated_at_unix_ms": 123456
        }))
        .expect("encode invalid control"),
    )
    .expect("write invalid control");
    let error = read_spawned_agent_control_artifact(&files, ActSpawnAgentCli::Codex)
        .expect_err("missing thread_id must fail");
    assert_eq!(
        error.get("reason").and_then(Value::as_str),
        Some("codex_control_artifact_invalid")
    );
}

#[test]
fn codex_app_server_runner_prefers_powershell_shim_and_tree_cleanup() {
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Get-Command codex.ps1"),
        "Windows npm installs expose codex.ps1; launching it through powershell preserves -c array arguments"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Get-Command codex.cmd"),
        "codex.cmd remains a fallback when the PowerShell shim is absent"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Stop-OwnedProcessTree"),
        "app-server cleanup must target the exact spawned root PID and descendants"
    );
    assert!(
        !CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Start-Process -FilePath 'codex'"),
        "bare Start-Process codex resolves to a non-executable ps1 shim on this host"
    );
    assert!(
        !CODEX_APP_SERVER_RUNNER_SCRIPT.contains("notify=["),
        "app-server startup must not depend on the legacy Codex notify TOML array"
    );
    let existing_read = CODEX_APP_SERVER_RUNNER_SCRIPT
        .find("foreach ($property in $existing.PSObject.Properties)")
        .expect("runner reads existing control artifact");
    let live_thread_write = CODEX_APP_SERVER_RUNNER_SCRIPT
        .find("$current['thread_id'] = $script:ThreadId")
        .expect("runner writes live thread_id into control artifact");
    let live_turn_write = CODEX_APP_SERVER_RUNNER_SCRIPT
        .find("$current['turn_id'] = $script:TurnId")
        .expect("runner writes live turn_id into control artifact");
    assert!(
        existing_read < live_thread_write && existing_read < live_turn_write,
        "live runner control state must overwrite stale values from the previous artifact"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("phase = 'send_start'")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("phase = 'send_ok'"),
        "runner must journal outbound JSON-RPC send boundaries for app-server stalls"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("SYNAPSE_CODEX_APP_SERVER_RPC_FAILED")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("expected_method")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("response_error_shape")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Receive-Response $socket 2 'thread/start'")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Receive-Response $socket 3 'turn/start'"),
        "runner must fail closed with structured JSON-RPC diagnostics for Bad Request-style app-server failures"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("/codex-app-server/request")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Handle-AppServerRequest")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("app_server_response")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("last_app_server_request_status"),
        "runner must bridge Codex app-server approval/input requests into Synapse queue rows"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'workspace-write'")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'on-request'")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("New-CodexSandboxPolicy"),
        "gated Codex spawns must use Codex's own on-request approval policy and workspace-write sandbox"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("mcp_servers.synapse.tools.")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'health'")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'session_list'")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'get_target'")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'agent'")
            && !CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'agent_spawn_task_started'")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("approval_mode=")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'approve'"),
        "startup-safe Synapse MCP tools must be pre-approved so Codex readiness cannot deadlock on its own health/task-start calls"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("$script:LastFinalAgentMessageText")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("$phase -eq 'final_answer'")
            && CODEX_APP_SERVER_RUNNER_SCRIPT
                .contains("$finalText = $script:LastFinalAgentMessageText"),
        "runner must preserve final_answer item/completed notifications when turn/completed omits turn items"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT
            .contains("$prompt = [string](Get-Content -Raw -LiteralPath $PromptPath"),
        "Windows PowerShell 5.1 must cast Get-Content -Raw prompt bytes before ConvertTo-Json"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT.contains("ConvertTo-Json -Compress -Depth 20"),
        "JSON-RPC request encoding should use bounded schema depth, not an unbounded diagnostic depth"
    );
    assert!(
        CODEX_APP_SERVER_RUNNER_SCRIPT
            .contains("$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Write-TextNoBom -Path $tmp")
            && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Append-LineNoBom"),
        "Codex app-server control/events files must be written without a UTF-8 BOM"
    );
    let interrupt_script = include_str!("../codex_app_server_interrupt.ps1");
    assert!(
        interrupt_script.contains("$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)")
            && interrupt_script.contains("Write-TextNoBom -Path $tmp")
            && interrupt_script.contains("Append-LineNoBom")
            && interrupt_script.contains("Invoke-WithFileRetry")
            && interrupt_script.contains("Move-ReplaceWithRetry"),
        "Codex app-server interrupt control/events files must be no-BOM and retry transient file contention"
    );
    assert!(
        interrupt_script.contains("SYNAPSE_CODEX_APP_SERVER_RPC_FAILED")
            && interrupt_script.contains("Receive-Response $socket 2 'turn/interrupt'")
            && interrupt_script.contains("response_error_shape"),
        "Codex app-server interrupt must preserve structured JSON-RPC failure diagnostics"
    );
    let steer_script = include_str!("../codex_app_server_steer.ps1");
    assert!(
        steer_script.contains("method = 'turn/steer'")
            && steer_script.contains("expectedTurnId = $TurnId")
            && steer_script.contains("text_elements = @()")
            && steer_script.contains("responsesapiClientMetadata")
            && steer_script.contains("last_steer_status")
            && steer_script.contains("Move-ReplaceWithRetry"),
        "Codex app-server steer must use the generated turn/steer protocol with expectedTurnId and durable control readback"
    );
    assert!(
        steer_script.contains("SYNAPSE_CODEX_APP_SERVER_RPC_FAILED")
            && steer_script.contains("Receive-Response $socket 2 'turn/steer'")
            && steer_script.contains("response_error_shape"),
        "Codex app-server steer must preserve structured JSON-RPC failure diagnostics"
    );
}

#[test]
fn spawn_wrapper_forces_utf8_and_records_wrapper_pid() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: Some(dir.path().join("codex-notify.ps1")),
        codex_app_server_runner_path: Some(dir.path().join("codex-app-server-runner.ps1")),
        codex_app_server_control_path: Some(dir.path().join("codex-control.json")),
        codex_app_server_events_path: Some(dir.path().join("codex-app-server-events.jsonl")),
        codex_app_server_stdout_path: Some(dir.path().join("codex-app-server.stdout.log")),
        codex_app_server_stderr_path: Some(dir.path().join("codex-app-server.stderr.log")),
        local_model_runner_path: None,
    };
    let script = agent_spawn_powershell_script(&test_spawn_params(), &files, dir.path())
        .expect("build wrapper script");

    assert!(script.contains("$env:PYTHONUTF8 = '1'"));
    assert!(script.contains("$env:PYTHONIOENCODING = 'utf-8'"));
    assert!(script.contains("Remove-Item Env:PYTHONLEGACYWINDOWSSTDIO"));
    assert!(script.contains("wrapper_process_id = $spawnWrapperProcessId"));
    assert!(script.contains("$spawnTaskStartedPath"));
    assert!(script.contains("task_started_present"));
    assert!(script.contains("function Invoke-SpawnHoldOpen"));
    assert!(script.contains("Start-Sleep -Milliseconds $sleepMs"));
    assert!(
        script.find("Invoke-SpawnHoldOpen")
            < script.find("Write-SpawnCompletionStatus -Status $finalStatus"),
        "wrapper must enforce hold_open before writing terminal status: {script}"
    );
    assert!(script.contains("Get-Content -Raw -LiteralPath $spawnPromptPath -Encoding UTF8"));
    assert!(
        script.contains("$event.type -eq 'assistant'")
            && script.contains("$event.message.role -eq 'assistant'")
            && script.contains("$event.type -eq 'result'")
            && script.contains("$event.result")
            && script.contains("'stdout_jsonl_result'"),
        "wrapper must recover Claude stream-json assistant/result final text: {script}"
    );
    assert!(
        script.contains("codex-app-server-runner.ps1"),
        "codex spawn must run through the app-server runner: {script}"
    );
    assert!(
        script.contains("& '")
            && script.contains("codex-app-server-runner.ps1' @codexRunnerArgs")
            && !script.contains("& powershell.exe @codexRunnerArgs"),
        "codex spawn must invoke the runner inside the already hidden wrapper instead of spawning a visible nested PowerShell: {script}"
    );
    assert!(
        script.contains("$codexRunnerArgs = @{")
            && script.contains("SpawnId = $spawnId")
            && script.contains("EventsPath = '")
            && !script.contains("$codexRunnerArgs = @('-SpawnId'"),
        "codex spawn must use named parameter splatting so direct script invocation cannot misbind runner args: {script}"
    );
    assert!(
        script.contains("RequireApprovalGate"),
        "default gated Codex spawns must tell the app-server runner to bridge approvals: {script}"
    );
    assert!(script.contains("ControlPath"));
    assert!(script.contains("codex-control.json"));
    assert!(script.contains("NotifyScriptPath"));
    assert!(script.contains("codex-notify.ps1"));
}

#[test]
fn claude_spawn_script_injects_hook_settings() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: Some(dir.path().join("claude-debug.log")),
        mcp_config_path: Some(dir.path().join("claude-mcp-config.json")),
        hook_settings_path: Some(dir.path().join("claude-hook-settings.json")),
        notify_script_path: None,
        codex_app_server_runner_path: None,
        codex_app_server_control_path: None,
        codex_app_server_events_path: None,
        codex_app_server_stdout_path: None,
        codex_app_server_stderr_path: None,
        local_model_runner_path: None,
    };
    let mut params = test_spawn_params();
    params.cli = Some(ActSpawnAgentCli::Claude);
    let script =
        agent_spawn_powershell_script(&params, &files, dir.path()).expect("build wrapper script");
    assert!(
        script.contains("'--settings'"),
        "claude args must inject the hook settings file: {script}"
    );
    assert!(script.contains("claude-hook-settings.json"));
}

#[test]
fn spawn_script_injects_model_arg_for_both_clis() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    // Codex: model is passed to the app-server runner, which starts the
    // actual turn through `thread/start`/`turn/start`.
    let codex_files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: Some(dir.path().join("codex-notify.ps1")),
        codex_app_server_runner_path: Some(dir.path().join("codex-app-server-runner.ps1")),
        codex_app_server_control_path: Some(dir.path().join("codex-control.json")),
        codex_app_server_events_path: Some(dir.path().join("codex-app-server-events.jsonl")),
        codex_app_server_stdout_path: Some(dir.path().join("codex-app-server.stdout.log")),
        codex_app_server_stderr_path: Some(dir.path().join("codex-app-server.stderr.log")),
        local_model_runner_path: None,
    };
    let mut codex_params = test_spawn_params();
    codex_params.model = Some("gpt-5-codex".to_owned());
    let codex_script = agent_spawn_powershell_script(&codex_params, &codex_files, dir.path())
        .expect("codex script");
    assert!(
        codex_script.contains("$codexRunnerArgs['Model'] = 'gpt-5-codex'"),
        "codex runner args must inject the pinned model: {codex_script}"
    );
    assert!(
        codex_script.contains("$codexRunnerArgs['RequireApprovalGate'] = $true"),
        "codex runner args must enable app-server approval bridging when the spawn is gated: {codex_script}"
    );

    // Codex without a model: no runner model override appears.
    let codex_no_model =
        agent_spawn_powershell_script(&test_spawn_params(), &codex_files, dir.path())
            .expect("codex script");
    assert!(
        codex_no_model.contains("codex-app-server-runner.ps1"),
        "codex still runs through app-server without a pinned model: {codex_no_model}"
    );
    assert!(!codex_no_model.contains("['Model']"));
    assert!(codex_no_model.contains("['RequireApprovalGate']"));

    // Claude: `--model <model>` injected right after `-p`.
    let claude_files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: Some(dir.path().join("claude-debug.log")),
        mcp_config_path: Some(dir.path().join("claude-mcp-config.json")),
        hook_settings_path: Some(dir.path().join("claude-hook-settings.json")),
        notify_script_path: None,
        codex_app_server_runner_path: None,
        codex_app_server_control_path: None,
        codex_app_server_events_path: None,
        codex_app_server_stdout_path: None,
        codex_app_server_stderr_path: None,
        local_model_runner_path: None,
    };
    let mut claude_params = test_spawn_params();
    claude_params.cli = Some(ActSpawnAgentCli::Claude);
    claude_params.model = Some("claude-fable-5".to_owned());
    let claude_script = agent_spawn_powershell_script(&claude_params, &claude_files, dir.path())
        .expect("claude script");
    assert!(
        claude_script.contains("@('-p','--model','claude-fable-5','--verbose'"),
        "claude args must inject --model after -p: {claude_script}"
    );
}

#[test]
fn spawn_manifest_records_cli_and_model() {
    // The manifest is the transcript ingester's authoritative model source.
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let mut params = test_spawn_params();
    params.model = Some("gpt-5-codex".to_owned());
    let manifest = build_spawn_manifest("agent-spawn-manifest-regression", &params, dir.path())
        .expect("build spawn manifest");
    assert_eq!(manifest["version"], AGENT_SPAWN_MANIFEST_VERSION);
    assert_eq!(manifest["spawn_id"], "agent-spawn-manifest-regression");
    assert_eq!(manifest["cli"], "codex");
    assert_eq!(manifest["model"], "gpt-5-codex");
    assert_eq!(manifest["working_dir"], dir.path().display().to_string());
    assert_eq!(
        manifest["effective_working_dir"],
        dir.path().display().to_string()
    );
    assert_eq!(manifest["require_approval_gate"], true);
    assert_eq!(manifest["approval_gate_effective"], true);
    assert_eq!(manifest["assigned_prompt_present"], true);
    assert!(manifest["created_unix_ms"].as_u64().is_some());

    // No pinned model -> manifest carries an explicit null, never a guess.
    params.model = None;
    let manifest = build_spawn_manifest("agent-spawn-manifest-regression", &params, dir.path())
        .expect("build spawn manifest");
    assert!(manifest["model"].is_null());
}

#[test]
fn local_model_spawn_script_uses_repo_runner_and_model_ref() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let files = AgentSpawnFiles {
        log_dir: dir.path().to_path_buf(),
        prompt_path: dir.path().join("prompt.txt"),
        stdout_path: dir.path().join("stdout.jsonl"),
        stderr_path: dir.path().join("stderr.log"),
        final_message_path: dir.path().join("final-message.txt"),
        completion_status_path: dir.path().join("completion-status.json"),
        task_started_path: dir.path().join("task-started.json"),
        task_started_script_path: dir.path().join("write-task-started.ps1"),
        debug_path: None,
        mcp_config_path: None,
        hook_settings_path: None,
        notify_script_path: None,
        codex_app_server_runner_path: None,
        codex_app_server_control_path: None,
        codex_app_server_events_path: None,
        codex_app_server_stdout_path: None,
        codex_app_server_stderr_path: None,
        local_model_runner_path: Some(dir.path().join("local-model-runner.json")),
    };
    let mut params = test_spawn_params();
    params.cli = None;
    params.kind = Some(ActSpawnAgentCli::LocalModel);
    params.model_ref = Some("ollama-gemma4-e4b".to_owned());
    params.prompt = Some("call health once".to_owned());

    let prompt = build_agent_spawn_prompt(
        "agent-spawn-test",
        &params,
        dir.path(),
        &files.task_started_path,
        &files.task_started_script_path,
    )
    .expect("local prompt");
    assert_eq!(prompt, "call health once");

    let script = agent_spawn_powershell_script(&params, &files, dir.path()).expect("local script");
    assert!(script.contains("--mode"));
    assert!(script.contains("local-agent"));
    assert!(script.contains("--local-agent-model"));
    assert!(script.contains("ollama-gemma4-e4b"));
    assert!(script.contains("--local-agent-task-file"));
    assert!(script.contains("--local-agent-spawn-id"));
    assert!(script.contains("--local-agent-log-dir"));
    assert!(script.contains("--local-agent-hold-open-ms"));
    assert!(script.contains("'1234'"));
    assert!(script.contains("--local-agent-trusted-unattended-exact-contract"));
    assert!(!script.contains("& codex"));
    assert!(!script.contains("& claude"));

    let manifest =
        build_spawn_manifest("agent-spawn-manifest-local", &params, dir.path()).expect("manifest");
    assert_eq!(manifest["cli"], "local-model");
    assert_eq!(manifest["kind"], "local-model");
    assert_eq!(manifest["model"], "ollama-gemma4-e4b");
    assert_eq!(manifest["model_ref"], "ollama-gemma4-e4b");
    assert_eq!(manifest["working_dir"], dir.path().display().to_string());
    assert_eq!(manifest["require_approval_gate"], true);
    assert_eq!(manifest["approval_gate_effective"], false);
    assert_eq!(manifest["local_model_autonomous_tool_calls"], true);
    assert_eq!(manifest["local_model_approval_gate_used"], false);
    assert_eq!(
        manifest["local_model_trusted_unattended_exact_contract"],
        true
    );

    params.require_approval_gate = false;
    let trusted_script =
        agent_spawn_powershell_script(&params, &files, dir.path()).expect("trusted script");
    assert!(trusted_script.contains("--local-agent-trusted-unattended-exact-contract"));
    let trusted_manifest =
        build_spawn_manifest("agent-spawn-manifest-local", &params, dir.path()).expect("manifest");
    assert_eq!(trusted_manifest["require_approval_gate"], false);
    assert_eq!(trusted_manifest["approval_gate_effective"], false);
    assert_eq!(trusted_manifest["local_model_autonomous_tool_calls"], true);
    assert_eq!(trusted_manifest["local_model_approval_gate_used"], false);
    assert_eq!(
        trusted_manifest["local_model_trusted_unattended_exact_contract"],
        true
    );
}

#[test]
fn local_model_spawn_prompt_builder_rejects_blank_prompt() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let task_started_path = dir.path().join("task-started.json");
    let task_started_script_path = dir.path().join("write-task-started.ps1");
    let mut params = test_spawn_params();
    params.cli = None;
    params.kind = Some(ActSpawnAgentCli::LocalModel);
    params.model_ref = Some("ollama-gemma4-e4b".to_owned());
    params.prompt = Some("  \r\n\t ".to_owned());

    let error = build_agent_spawn_prompt(
        "agent-spawn-test",
        &params,
        dir.path(),
        &task_started_path,
        &task_started_script_path,
    )
    .expect_err("blank local-model prompt must fail closed");

    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("code")),
        Some(&json!(error_codes::TOOL_PARAMS_INVALID))
    );
    assert!(
        error
            .message
            .contains("local_model prompt must not be empty"),
        "{}",
        error.message
    );
}

#[test]
fn direct_spawn_prompt_builder_rejects_blank_prompt() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let task_started_path = dir.path().join("task-started.json");
    let task_started_script_path = dir.path().join("write-task-started.ps1");

    for prompt in [None, Some(""), Some("  \r\n\t ")] {
        let mut params = test_spawn_params();
        params.prompt = prompt.map(str::to_owned);
        let error = build_agent_spawn_prompt(
            "agent-spawn-test",
            &params,
            dir.path(),
            &task_started_path,
            &task_started_script_path,
        )
        .expect_err("blank direct spawn prompt must fail closed");

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::TOOL_PARAMS_INVALID))
        );
        assert!(
            error
                .message
                .contains("direct spawn prompt must not be empty"),
            "{}",
            error.message
        );
    }
}

#[test]
fn template_rendered_prompt_builder_accepts_template_prompt() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let task_started_path = dir.path().join("task-started.json");
    let task_started_script_path = dir.path().join("write-task-started.ps1");
    let mut params = test_spawn_params();
    params.template_id = Some("issue1245-template".to_owned());
    params.template_version = Some(1);
    params.template_config_hash = Some("sha256:test".to_owned());
    params.prompt = Some("template-provided task".to_owned());

    let prompt = build_agent_spawn_prompt(
        "agent-spawn-test",
        &params,
        dir.path(),
        &task_started_path,
        &task_started_script_path,
    )
    .expect("template prompt builds");

    assert!(prompt.contains("template-provided task"));
    assert!(prompt.contains("task-start readiness artifact"));
}

#[test]
fn claude_hook_settings_subscribe_every_ingress_event_with_bearer() {
    let settings =
        build_claude_hook_settings("agent-spawn-test", "http://127.0.0.1:7700/mcp", true)
            .expect("settings build");
    let hooks = settings["hooks"].as_object().expect("hooks object");
    for event in super::super::agent_event_ingress::CLAUDE_HOOK_SUBSCRIBED_EVENTS {
        let entry = &hooks[*event][0]["hooks"][0];
        assert_eq!(entry["type"], "http", "{event} must use a native HTTP hook");
        assert_eq!(
            entry["url"],
            "http://127.0.0.1:7700/agent-events?spawn_id=agent-spawn-test&source=claude_code_hooks"
        );
        assert_eq!(
            entry["headers"]["Authorization"],
            "Bearer $SYNAPSE_BEARER_TOKEN"
        );
        assert_eq!(entry["allowedEnvVars"][0], "SYNAPSE_BEARER_TOKEN");
    }
    assert_eq!(
        settings["allowedHttpHookUrls"][0],
        "http://127.0.0.1:7700/agent-events?spawn_id=agent-spawn-test&source=claude_code_hooks*"
    );
    assert_eq!(
        settings["httpHookAllowedEnvVars"][0],
        "SYNAPSE_BEARER_TOKEN"
    );
    // Gated spawns pre-approve the safe tools so they skip the gate (#927).
    let allow = settings["permissions"]["allow"]
        .as_array()
        .expect("permissions.allow present when gating");
    assert!(allow.iter().any(|rule| rule == "Read"));
    assert!(allow.iter().any(|rule| rule == "Bash(git status:*)"));
    assert!(allow.iter().any(|rule| rule == "mcp__synapse__agent"));
    assert!(allow.iter().any(|rule| rule == "mcp__synapse__approval"));
    assert!(allow.iter().any(|rule| rule == "mcp__synapse__session"));
    assert!(allow.iter().any(|rule| rule == "mcp__synapse__target"));
    for tool in CLAUDE_COORDINATION_FACADE_MCP_TOOLS {
        let expected = format!("mcp__synapse__{tool}");
        assert!(
            allow.iter().any(|rule| rule == &expected),
            "Claude static allow list must include {expected}"
        );
    }
    let encoded = serde_json::to_string(&settings).expect("settings serialize");
    assert!(!encoded.contains("write-task-started.ps1"));
    assert!(!encoded.contains("mcp__synapse__approval_gate"));
    assert!(!encoded.contains("mcp__synapse__session_list"));
    assert!(!encoded.contains("mcp__synapse__tool_profile_status"));
    assert!(!encoded.contains("mcp__synapse__get_target"));
    assert!(!encoded.contains("mcp__synapse__set_target"));
    assert!(
        !allow.iter().any(|rule| rule
            .as_str()
            .is_some_and(|rule| rule.starts_with("PowerShell("))),
        "Claude gated allowlist must not preserve task-start helper PowerShell rules"
    );
}

#[test]
fn claude_hook_settings_omit_permissions_when_gate_disabled() {
    let settings =
        build_claude_hook_settings("agent-spawn-test", "http://127.0.0.1:7700/mcp", false)
            .expect("settings build");
    assert!(
        settings.get("permissions").is_none(),
        "ungated spawn (bypassPermissions) must not inject allow rules"
    );
}

#[test]
fn codex_notify_script_posts_to_ingress_and_logs_failures() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let script =
        build_codex_notify_script("agent-spawn-test", "http://127.0.0.1:7700/mcp", dir.path())
            .expect("notify script build");
    assert!(script.contains(
        "http://127.0.0.1:7700/agent-events?spawn_id=agent-spawn-test&source=codex_notify"
    ));
    assert!(script.contains("$args[-1]"), "{script}");
    assert!(script.contains("SYNAPSE_BEARER_TOKEN"));
    assert!(script.contains("notify-errors.log"));
    assert!(script.contains("TimeoutSec 5"));
}

#[test]
fn ingress_url_refuses_mcp_url_without_mcp_suffix() {
    let error = agent_event_ingress_url("agent-spawn-test", "http://127.0.0.1:7700/", "x")
        .expect_err("non-/mcp URL must fail closed");
    assert!(
        error.message.contains("does not end with"),
        "{}",
        error.message
    );
    let error = agent_event_ingress_url("agent-spawn-test", "/mcp", "x")
        .expect_err("authority-less URL must fail closed");
    assert!(
        error.message.contains("no scheme/authority"),
        "{}",
        error.message
    );
}

#[test]
fn orphan_recovery_writes_terminal_artifacts_for_dead_wrapper() {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let log_dir = dir.path().join("agent-spawn-test");
    fs::create_dir_all(&log_dir).expect("create log dir");
    let completion_status_path = log_dir.join("completion-status.json");
    let stdout_path = log_dir.join("stdout.jsonl");
    fs::write(
        &completion_status_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "spawn_id": "agent-spawn-test",
            "cli": "codex",
            "status": "running",
            "wrapper_process_id": 99_999_999u32,
            "wrapper_started_at_unix_ms": unix_time_ms_now().saturating_sub(120_000),
            "requested_hold_open_ms": 60_000
        }))
        .expect("encode running status"),
    )
    .expect("write running status");
    fs::write(
        &stdout_path,
        b"{\"type\":\"agent_message\",\"text\":\"partial\"}\n",
    )
    .expect("write stdout");

    let decision =
        agent_spawn_orphan_recovery_decision("agent-spawn-test", &log_dir, unix_time_ms_now())
            .expect("orphan decision");
    let AgentSpawnOrphanRecoveryDecision::Recover(recovery) = decision else {
        panic!("dead wrapper should recover");
    };
    write_agent_spawn_orphan_terminal_artifacts("agent-spawn-test", &log_dir, &recovery)
        .expect("write orphan artifacts");

    let status: Value =
        serde_json::from_slice(&fs::read(&completion_status_path).expect("read recovered status"))
            .expect("parse recovered status");
    assert_eq!(
        status.get("status").and_then(Value::as_str),
        Some("orphaned_running_recovered")
    );
    assert_eq!(
        status
            .get("orphan_recovery_artifact")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(log_dir.join("final-message.txt").exists());
}
