use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use reqwest::Url;
use rmcp::{
    ErrorData, RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientInfo, Content, Implementation, JsonObject,
        Tool,
    },
    service::{RunningService, ServiceError},
    transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SYNAPSE_TOOL_CATALOG: &str = "synapse_tool_catalog";
const SYNAPSE_TOOL_CALL: &str = "synapse_tool";
const INTERNALIZED_TOOL_CALL_ENVELOPE: &str = "tool_call";
const ACT_CALL_TOOL_CALL_ENVELOPE: &str = "act_call";
const LOCAL_MODEL_SPAWN_MAX_PROBE_AGE_MS: u64 = 15 * 60 * 1000;

#[derive(Clone, Debug)]
pub(crate) struct LocalAgentCli {
    pub model_name: Option<String>,
    pub task: Option<String>,
    pub task_file: Option<PathBuf>,
    pub mcp_url: String,
    pub spawn_id: Option<String>,
    pub log_dir: Option<PathBuf>,
    pub target_json: Option<String>,
    pub max_turns: u32,
    pub timeout_ms: u64,
    pub hold_open_ms: u64,
    pub context_char_limit: usize,
    pub tool_parse_retry_limit: u32,
    pub no_stream: bool,
    pub allow_non_loopback: bool,
    pub trusted_unattended_exact_contract: bool,
}

type LocalMcpClient = RunningService<RoleClient, ClientInfo>;

#[derive(Debug, Deserialize)]
struct LocalModelListResult {
    rows: Vec<LocalModelRegistryRow>,
}

#[derive(Clone, Debug, Deserialize)]
struct LocalModelRegistryRow {
    name: String,
    base_url: String,
    model_id: String,
    enabled: bool,
    allow_non_loopback: bool,
    api_key_env_var: Option<String>,
    api_shape: String,
    runtime_preset: Option<String>,
    context_length: Option<u64>,
    max_tools: Option<usize>,
    last_probe: Option<LocalModelProbe>,
}

#[derive(Clone, Debug, Deserialize)]
struct LocalModelProbe {
    observed_at_unix_ms: u64,
    healthy: bool,
    error_code: Option<String>,
    error_detail: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

impl Usage {
    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            prompt_tokens: value.get("prompt_tokens").and_then(Value::as_u64)?,
            completion_tokens: value.get("completion_tokens").and_then(Value::as_u64)?,
            total_tokens: value.get("total_tokens").and_then(Value::as_u64)?,
        })
    }

    fn add(&mut self, other: &Self) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

#[derive(Clone, Debug)]
struct OpenAiToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Clone, Debug, Default)]
struct ChatCompletion {
    content: String,
    tool_calls: Vec<OpenAiToolCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    raw_sha256: String,
}

#[derive(Default)]
struct StreamingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

struct Runner {
    cli: LocalAgentCli,
    spawn_id: String,
    log_dir: PathBuf,
    stdout: File,
    token: String,
    mcp: LocalMcpClient,
    mcp_session_id: String,
    event_url: Url,
    endpoint_url: Url,
    endpoint_api_key: Option<String>,
    registry: LocalModelRegistryRow,
    tool_exposure: ToolExposure,
    tools: Vec<Tool>,
    openai_tools: Vec<Value>,
    messages: Vec<Value>,
    conversation_id: String,
    total_usage: Usage,
    turn_count: u32,
    tool_call_count: u64,
    successful_tool_call_count: u64,
    parse_error_count: u32,
    invalid_tool_call_count: u32,
    tool_call_error_count: u32,
    truncated_context_count: u32,
    completed_after_tool: bool,
    successful_workspace_puts: Vec<JsonObject>,
    task_tool_contract: Option<TaskToolContract>,
    completed_task_tools: BTreeSet<String>,
    completed_task_tool_counts: BTreeMap<String, usize>,
    completed_task_tool_sources: BTreeMap<String, String>,
    approval_wait_elapsed: Duration,
    http: reqwest::Client,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolExposure {
    Direct,
    Routed,
    /// Internalized model: the full tool surface lives in the model's weights,
    /// so NO tool catalog is injected into the request. The model emits tool
    /// calls from a near-empty prompt; the response is routed exactly like
    /// Direct (every real tool is callable by name).
    Internalized,
}

impl ToolExposure {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Routed => "routed",
            Self::Internalized => "internalized",
        }
    }
}

#[derive(Clone, Debug)]
struct TaskToolContract {
    allowed_tools: BTreeSet<String>,
    ordered_tools: Vec<String>,
    argument_templates: BTreeMap<String, JsonObject>,
    step_argument_templates: Vec<Option<JsonObject>>,
    source: &'static str,
}

impl TaskToolContract {
    fn allowed_tools_json(&self) -> Value {
        Value::Array(
            self.allowed_tools
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        )
    }

    fn allowed_tools_display(&self) -> String {
        self.allowed_tools
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn ordered_tools_json(&self) -> Value {
        Value::Array(
            self.ordered_tools
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        )
    }

    fn ordered_tools_display(&self) -> String {
        self.ordered_tools.join(" -> ")
    }

    fn argument_templates_json(&self) -> Value {
        let templates = self
            .argument_templates
            .iter()
            .map(|(tool, args)| (tool.clone(), Value::Object(args.clone())))
            .collect::<Map<_, _>>();
        Value::Object(templates)
    }

    fn step_argument_templates_json(&self) -> Value {
        Value::Array(
            self.step_argument_templates
                .iter()
                .map(|template| {
                    template
                        .as_ref()
                        .map(|args| Value::Object(args.clone()))
                        .unwrap_or(Value::Null)
                })
                .collect(),
        )
    }

    fn to_json(&self) -> Value {
        json!({
            "source": self.source,
            "allowed_tools": self.allowed_tools_json(),
            "ordered_tools": self.ordered_tools_json(),
            "argument_templates": self.argument_templates_json(),
            "step_argument_templates": self.step_argument_templates_json(),
        })
    }

    fn model_facing_json(&self) -> Value {
        json!({
            "allowed_tools": self.allowed_tools_json(),
            "ordered_tools": self.ordered_tools_json(),
            "argument_templates": self.argument_templates_json(),
            "step_argument_templates": self.step_argument_templates_json(),
        })
    }
}

#[derive(Clone, Debug)]
struct ModelToolCallRejection {
    reason: String,
    terminal: bool,
    suggestion: &'static str,
}

impl ModelToolCallRejection {
    fn recoverable(reason: impl Into<String>, suggestion: &'static str) -> Self {
        Self {
            reason: reason.into(),
            terminal: false,
            suggestion,
        }
    }

    fn terminal(reason: impl Into<String>, suggestion: &'static str) -> Self {
        Self {
            reason: reason.into(),
            terminal: true,
            suggestion,
        }
    }
}

pub(crate) async fn run_from_cli(cli: LocalAgentCli) -> anyhow::Result<ExitCode> {
    let mut runner = Runner::new(cli).await?;
    let result = runner.run_loop().await;
    let exit_code = match result {
        Ok(()) => {
            runner.hold_open_before_session_close().await?;
            runner.write_success_status()?;
            ExitCode::SUCCESS
        }
        Err(error) => {
            let detail = format!("{error:#}");
            let code = error_code_from_detail(&detail);
            runner.write_failure(code, &detail).await?;
            ExitCode::from(1)
        }
    };
    let _ = runner.mcp.close().await;
    Ok(exit_code)
}

impl Runner {
    async fn new(cli: LocalAgentCli) -> anyhow::Result<Self> {
        if cli.max_turns == 0 {
            bail!("TOOL_PARAMS_INVALID: --local-agent-max-turns must be > 0");
        }
        if cli.timeout_ms == 0 {
            bail!("TOOL_PARAMS_INVALID: --local-agent-timeout-ms must be > 0");
        }
        if cli.context_char_limit < 1024 {
            bail!("TOOL_PARAMS_INVALID: --local-agent-context-char-limit must be >= 1024");
        }
        let model_name = non_empty(
            cli.model_name.as_deref(),
            "TOOL_PARAMS_INVALID: --local-agent-model is required",
        )?;
        let task = resolve_task(cli.task.as_deref(), cli.task_file.as_ref())?;
        let spawn_id = cli
            .spawn_id
            .clone()
            .unwrap_or_else(|| format!("agent-spawn-local-{}", Uuid::now_v7().simple()));
        validate_spawn_id(&spawn_id)?;
        let log_dir = match cli.log_dir.clone() {
            Some(path) => path,
            None => local_agent_spawn_root_dir()?.join(&spawn_id),
        };
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("create local-agent log dir {}", log_dir.display()))?;
        let stdout_path = log_dir.join("stdout.jsonl");
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)
            .with_context(|| format!("open {}", stdout_path.display()))?;
        let token = crate::http::load_token_value().context("load Synapse bearer token")?;
        let client_info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("synapse-local-model-agent", env!("CARGO_PKG_VERSION")),
        );
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(cli.mcp_url.clone())
                .auth_header(token.clone()),
        );
        let mcp = client_info
            .serve(transport)
            .await
            .context("initialize Synapse MCP local-agent session")?;
        let tools = mcp
            .peer()
            .list_all_tools()
            .await
            .context("client-parity tools/list failed")?;
        if tools.is_empty() {
            bail!("MODEL_TOOLS_UNSUPPORTED: Synapse tools/list returned zero tools");
        }
        let _health = call_mcp_tool_json(mcp.peer(), "health", Map::new())
            .await
            .context("call Synapse health tool")?;
        let mcp_session_id = current_mcp_session_id(mcp.peer()).await?;
        let mut list_args = Map::new();
        list_args.insert("name".to_owned(), Value::from(model_name.to_owned()));
        list_args.insert("include_disabled".to_owned(), Value::from(true));
        list_args.insert("limit".to_owned(), Value::from(10));
        let registry_value = call_mcp_tool_json(mcp.peer(), "local_model_list", list_args)
            .await
            .context("call Synapse local_model_list tool")?;
        let registry_result: LocalModelListResult = serde_json::from_value(registry_value)
            .context("decode local_model_list structured content")?;
        let registry = registry_result
            .rows
            .into_iter()
            .find(|row| row.name == model_name)
            .with_context(|| format!("LOCAL_MODEL_NOT_FOUND: registry row {model_name:?}"))?;
        validate_registry_row(&registry)?;
        let endpoint_url = chat_completions_endpoint(&registry, cli.allow_non_loopback)?;
        let endpoint_api_key = match registry.api_key_env_var.as_deref() {
            Some(name) => Some(
                std::env::var(name)
                    .with_context(|| format!("LOCAL_MODEL_API_KEY_MISSING: env {name}"))?,
            ),
            None => None,
        };
        let event_url = agent_event_url(&cli.mcp_url, &spawn_id)?;
        let tool_exposure = resolve_tool_exposure(&registry, tools.len());
        let task_tool_contract = infer_task_tool_contract(&task, &tools);
        let openai_tools = openai_tools_for_exposure(
            tool_exposure,
            &tools,
            task_tool_contract.as_ref(),
            &registry,
        )?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(cli.timeout_ms))
            .build()
            .context("build local-agent HTTP client")?;
        let conversation_id = format!("local-model-{}", Uuid::now_v7().simple());
        bind_requested_target(mcp.peer(), cli.target_json.as_deref()).await?;
        write_json_file(
            log_dir.join("local-model-runner.json"),
            &json!({
                "schema_version": 1,
                "spawn_id": spawn_id,
                "session_id": mcp_session_id.clone(),
                "registry_name": registry.name,
                "model": registry.model_id,
                "mcp_url": cli.mcp_url,
                "endpoint_url": endpoint_url.as_str(),
                "context_length": registry.context_length,
                "registry_max_tools": registry.max_tools,
                "runtime_preset": registry_runtime_preset(&registry),
                "tool_exposure": tool_exposure.as_str(),
                "mcp_tool_count": tools.len(),
                "openai_tool_count": openai_tools.len(),
                "task_tool_contract": task_tool_contract
                    .as_ref()
                    .map(TaskToolContract::to_json),
                "started_at_unix_ms": unix_time_ms_now(),
            }),
        )?;
        let task_started_path = log_dir.join("task-started.json");
        write_json_file(
            task_started_path.clone(),
            &json!({
                "schema_version": 1,
                "spawn_id": spawn_id,
                "cli": "local-model",
                "session_id": mcp_session_id.clone(),
                "status": "started",
                "health_ok": true,
                "target_ok": true,
                "assigned_prompt_present": true,
                "task_started_path": task_started_path.display().to_string(),
                "registry_name": registry.name,
                "model": registry.model_id,
                "conversation_id": conversation_id,
                "tool_count": tools.len(),
                "openai_tool_count": openai_tools.len(),
                "tool_exposure": tool_exposure.as_str(),
                "endpoint_url": endpoint_url.as_str(),
                "started_at_unix_ms": unix_time_ms_now(),
            }),
        )?;
        std::fs::write(log_dir.join("prompt.txt"), &task)
            .with_context(|| format!("write {}", log_dir.join("prompt.txt").display()))?;
        write_json_file(
            log_dir.join("completion-status.json"),
            &json!({
                "schema_version": 1,
                "spawn_id": spawn_id,
                "status": "running",
                "state": "running",
                "started_at_unix_ms": unix_time_ms_now(),
            }),
        )?;
        let mut system_content = system_prompt(tool_exposure, &tools);
        if let Some(contract) = &task_tool_contract {
            system_content.push_str("\n\n");
            system_content.push_str(&task_contract_prompt(contract));
        }
        let mut messages = Vec::new();
        messages.push(json!({
            "role": "system",
            "content": system_content,
        }));
        messages.push(json!({
            "role": "user",
            "content": task,
        }));
        let mut runner = Self {
            cli,
            spawn_id,
            log_dir,
            stdout,
            token,
            mcp,
            mcp_session_id,
            event_url,
            endpoint_url,
            endpoint_api_key,
            registry,
            tool_exposure,
            tools,
            openai_tools,
            messages,
            conversation_id,
            total_usage: Usage::default(),
            turn_count: 0,
            tool_call_count: 0,
            successful_tool_call_count: 0,
            parse_error_count: 0,
            invalid_tool_call_count: 0,
            tool_call_error_count: 0,
            truncated_context_count: 0,
            completed_after_tool: false,
            successful_workspace_puts: Vec::new(),
            task_tool_contract,
            completed_task_tools: BTreeSet::new(),
            completed_task_tool_counts: BTreeMap::new(),
            completed_task_tool_sources: BTreeMap::new(),
            approval_wait_elapsed: Duration::ZERO,
            http,
        };
        runner.write_line(json!({
            "type": "local.thread.started",
            "session_id": runner.mcp_session_id,
            "conversation_id": runner.conversation_id,
            "model": runner.registry.model_id,
            "registry_name": runner.registry.name,
            "tool_count": runner.tools.len(),
            "openai_tool_count": runner.openai_tools.len(),
            "tool_exposure": runner.tool_exposure.as_str(),
            "runtime_preset": registry_runtime_preset(&runner.registry),
            "task_tool_contract": runner
                .task_tool_contract
                .as_ref()
                .map(TaskToolContract::to_json),
        }))?;
        runner
            .post_event(json!({
                "event": "state_changed",
                "session_id": runner.mcp_session_id,
                "conversation_id": runner.conversation_id,
                "model": runner.registry.model_id,
                "registry_name": runner.registry.name,
                "state_to": "live",
                "reason_code": "local_agent_started",
                "tool_count": runner.tools.len(),
                "openai_tool_count": runner.openai_tools.len(),
                "tool_exposure": runner.tool_exposure.as_str(),
                "runtime_preset": registry_runtime_preset(&runner.registry),
                "task_tool_contract": runner
                    .task_tool_contract
                    .as_ref()
                    .map(TaskToolContract::to_json),
            }))
            .await?;
        Ok(runner)
    }

    async fn run_loop(&mut self) -> anyhow::Result<()> {
        let started = Instant::now();
        let mut used_any_tool = false;
        for turn in 1..=self.cli.max_turns {
            let active_elapsed = local_agent_active_elapsed(started, self.approval_wait_elapsed);
            if active_elapsed > Duration::from_millis(self.cli.timeout_ms) {
                bail!(
                    "LOCAL_AGENT_TIMEOUT: local-agent active timeout exceeded before turn {turn} \
                     (active_ms={} approval_wait_ms={} timeout_ms={})",
                    active_elapsed.as_millis(),
                    self.approval_wait_elapsed.as_millis(),
                    self.cli.timeout_ms,
                );
            }
            self.turn_count = turn;
            self.write_line(json!({
                "type": "local.turn.started",
                "conversation_id": self.conversation_id,
                "model": self.registry.model_id,
                "turn_index": turn,
            }))?;
            self.post_event(json!({
                "event": "turn_started",
                "session_id": self.mcp_session_id,
                "conversation_id": self.conversation_id,
                "model": self.registry.model_id,
                "registry_name": self.registry.name,
                "turn_index": turn,
            }))
            .await?;
            self.drain_steering_inbox().await?;
            self.truncate_context_if_needed().await?;
            let completion = self.chat_completion().await?;
            self.write_line(json!({
                "type": "local.assistant.message",
                "conversation_id": self.conversation_id,
                "model": self.registry.model_id,
                "turn_index": turn,
                "content": completion.content,
                "finish_reason": completion.finish_reason,
                "raw_response_sha256": completion.raw_sha256,
            }))?;
            self.messages.push(assistant_message(&completion));
            if let Some(usage) = completion.usage.clone() {
                self.total_usage.add(&usage);
                self.write_line(json!({
                    "type": "local.turn.finished",
                    "conversation_id": self.conversation_id,
                    "model": self.registry.model_id,
                    "turn_index": turn,
                    "finish_reason": completion.finish_reason,
                    "usage": usage,
                }))?;
                self.post_event(json!({
                    "event": "turn_finished",
                    "session_id": self.mcp_session_id,
                    "conversation_id": self.conversation_id,
                    "model": self.registry.model_id,
                    "registry_name": self.registry.name,
                    "turn_index": turn,
                    "finish_reason": completion.finish_reason,
                    "usage": usage,
                }))
                .await?;
            }
            if completion.tool_calls.is_empty() {
                // A plain assistant message with no tool call is a legitimate
                // completion: the model answered directly. This is valid on the
                // first turn (a prompt may not need tools) and after tool use
                // alike. Models that are genuinely incapable of tool-calling are
                // rejected at registration-probe time (MODEL_TOOLS_UNSUPPORTED),
                // not here — bailing on a correct direct answer was a false
                // failure that killed otherwise-successful agents. Only a turn
                // that yields neither a tool call nor any content is degenerate.
                let final_message = completion.content.trim();
                if final_message.is_empty() {
                    bail!(
                        "MODEL_EMPTY_COMPLETION: model returned neither a tool call nor message content on turn {turn} (finish_reason={:?}); used_any_tool={used_any_tool}",
                        completion.finish_reason
                    );
                }
                let missing_tools = missing_task_contract_tools(
                    &self.task_tool_contract,
                    &self.completed_task_tool_counts,
                );
                if !missing_tools.is_empty() {
                    bail!(
                        "MODEL_TASK_TOOL_CONTRACT_UNSATISFIED: model answered before successful required exact-tool call(s): {}",
                        missing_tools.join(", ")
                    );
                }
                if final_answer_after_failed_only_tools_should_fail(
                    used_any_tool,
                    self.successful_tool_call_count,
                    self.tool_call_error_count,
                    self.invalid_tool_call_count,
                    self.parse_error_count,
                ) {
                    bail!(
                        "MODEL_TASK_NO_SUCCESSFUL_TOOL_CALLS: model answered after failed/invalid tool call(s) without any successful Synapse tool call"
                    );
                }
                std::fs::write(self.log_dir.join("final-message.txt"), &completion.content)
                    .with_context(|| {
                        format!("write {}", self.log_dir.join("final-message.txt").display())
                    })?;
                self.post_event(json!({
                    "event": "exited",
                    "session_id": self.mcp_session_id,
                    "conversation_id": self.conversation_id,
                    "model": self.registry.model_id,
                    "registry_name": self.registry.name,
                    "end_state": "success",
                    "reason_code": "local_agent_completed",
                    "used_any_tool": used_any_tool,
                    "answered_without_tool_calls": !used_any_tool,
                }))
                .await?;
                return Ok(());
            }
            used_any_tool = true;
            for call in completion.tool_calls {
                self.execute_tool_call(call).await?;
                if self.completed_after_tool {
                    return Ok(());
                }
                self.drain_steering_inbox().await?;
            }
        }
        bail!(
            "LOCAL_AGENT_TURN_LIMIT: local model did not finish within {} turns",
            self.cli.max_turns
        )
    }

    async fn hold_open_before_session_close(&mut self) -> anyhow::Result<()> {
        if self.cli.hold_open_ms == 0 {
            return Ok(());
        }
        let started_unix_ms = unix_time_ms_now();
        self.write_line(json!({
            "type": "local.hold_open.started",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "session_id": self.mcp_session_id,
            "hold_open_ms": self.cli.hold_open_ms,
            "started_at_unix_ms": started_unix_ms,
            "source": "local_agent_mcp_session"
        }))?;
        tokio::time::sleep(Duration::from_millis(self.cli.hold_open_ms)).await;
        self.write_line(json!({
            "type": "local.hold_open.finished",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "session_id": self.mcp_session_id,
            "hold_open_ms": self.cli.hold_open_ms,
            "started_at_unix_ms": started_unix_ms,
            "finished_at_unix_ms": unix_time_ms_now(),
            "source": "local_agent_mcp_session"
        }))?;
        Ok(())
    }

    async fn execute_tool_call(&mut self, call: OpenAiToolCall) -> anyhow::Result<()> {
        self.tool_call_count = self.tool_call_count.saturating_add(1);
        let internalized_envelope = self.tool_exposure == ToolExposure::Internalized
            && call.name == INTERNALIZED_TOOL_CALL_ENVELOPE;
        let act_call_envelope = call.name == ACT_CALL_TOOL_CALL_ENVELOPE;
        let routed = call.name == SYNAPSE_TOOL_CALL || internalized_envelope || act_call_envelope;
        let catalog = call.name == SYNAPSE_TOOL_CATALOG;
        self.write_line(json!({
            "type": "local.tool_call.started",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "tool_call_id": call.id,
            "arguments": call.arguments,
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.post_event(json!({
            "event": "tool_call_started",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "tool_call_id": call.id,
            "tool_arguments": string_json_or_value(&call.arguments),
            "tool_exposure": self.tool_exposure.as_str(),
        }))
        .await?;

        if catalog {
            let result_value = match self.tool_catalog_value(&call.arguments) {
                Ok(value) => value,
                Err(error) => {
                    self.record_tool_parse_error(&call, error).await?;
                    return Ok(());
                }
            };
            let result_text = bounded_result_text(&result_value);
            self.messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": result_text,
            }));
            self.write_line(json!({
                "type": "local.tool_call.finished",
                "conversation_id": self.conversation_id,
                "model": self.registry.model_id,
                "turn_index": self.turn_count,
                "tool_name": call.name,
                "tool_call_id": call.id,
                "status": "ok",
                "result": result_value,
                "tool_exposure": self.tool_exposure.as_str(),
            }))?;
            self.post_event(json!({
                "event": "tool_call_finished",
                "session_id": self.mcp_session_id,
                "conversation_id": self.conversation_id,
                "model": self.registry.model_id,
                "registry_name": self.registry.name,
                "turn_index": self.turn_count,
                "tool_name": call.name,
                "tool_call_id": call.id,
                "tool_response": result_value,
                "error_code": "",
                "tool_exposure": self.tool_exposure.as_str(),
            }))
            .await?;
            return Ok(());
        }

        let (tool_name, args) = if call.name == SYNAPSE_TOOL_CALL {
            match parse_routed_tool_call(&call.arguments) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.record_tool_parse_error(&call, error).await?;
                    return Ok(());
                }
            }
        } else if internalized_envelope {
            match parse_internalized_tool_call_envelope(&call.arguments) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.record_tool_parse_error(&call, error).await?;
                    return Ok(());
                }
            }
        } else if act_call_envelope {
            match parse_act_call_tool_call_envelope(&call.arguments) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.record_tool_parse_error(&call, error).await?;
                    return Ok(());
                }
            }
        } else {
            match parse_tool_arguments(&call.arguments) {
                Ok(args) => (call.name.clone(), args),
                Err(error) => {
                    self.record_tool_parse_error(&call, error).await?;
                    return Ok(());
                }
            }
        };

        if self.should_defer_repeated_workspace_put_for_contract(&tool_name) {
            self.record_repeated_workspace_put_contract_deferred(&call, &tool_name, routed, &args)
                .await?;
            return Ok(());
        }

        if self.is_duplicate_successful_workspace_put(&tool_name, &args) {
            self.record_duplicate_workspace_put_completion(&call, &tool_name, routed, &args)
                .await?;
            return Ok(());
        }

        if self.should_defer_repeated_completed_task_tool_for_contract(&tool_name) {
            self.record_repeated_task_contract_tool_deferred(&call, &tool_name, routed, &args)
                .await?;
            return Ok(());
        }

        if let Some(reason) = task_contract_out_of_order_rejection(
            self.task_tool_contract.as_ref(),
            &self.completed_task_tool_counts,
            &tool_name,
        ) {
            let terminal = self
                .record_model_tool_call_invalid(&call, &tool_name, routed, &reason)
                .await?;
            if terminal {
                bail!("MODEL_TOOL_CALL_INVALID: {tool_name}: {}", reason.reason);
            }
            return Ok(());
        }

        let args = self
            .normalize_trusted_exact_contract_args(&call, &tool_name, args, routed)
            .await?;

        if let Some(reason) = model_tool_call_pre_gate_rejection(
            &tool_name,
            &args,
            self.synapse_tool_exists(&tool_name),
            self.task_tool_contract.as_ref(),
            &self.completed_task_tool_counts,
        ) {
            let terminal = self
                .record_model_tool_call_invalid(&call, &tool_name, routed, &reason)
                .await?;
            if terminal {
                bail!("MODEL_TOOL_CALL_INVALID: {tool_name}: {}", reason.reason);
            }
            return Ok(());
        }

        let args = self
            .normalize_local_agent_attribution_args(&call, &tool_name, args, routed)
            .await?;

        // Local-model agents are trusted autonomous workers. Prompt/exact-tool
        // contracts and tool-level invariants are the control surfaces; do not
        // pause normal local-model Synapse calls on a human approval queue.
        let args = match self.gate_tool_call(&tool_name, args, &call.id).await? {
            ToolGate::Allow(effective_args) => effective_args,
            ToolGate::Deny(reason) => {
                self.record_tool_gate_denied(&call, &tool_name, routed, &reason)
                    .await?;
                return Ok(());
            }
        };

        let result = match self
            .mcp
            .peer()
            .call_tool(CallToolRequestParams::new(tool_name.clone()).with_arguments(args.clone()))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                // A single failed tool call MUST NOT crash the agent. The
                // canonical tool-calling reliability pattern is to feed the
                // error back to the model as a tool result so it can
                // self-correct (retry with fixed arguments, pick a different
                // tool, or explain the limitation). Only a genuinely dead
                // transport is terminal. Crashing here previously killed agents
                // on the first recoverable error (e.g. a missing required
                // parameter), tearing down any process the agent had launched.
                let transport_terminal = tool_call_error_is_terminal(&error);
                let exact_required_tool_failed = task_contract_required_tool_failure_is_terminal(
                    self.task_tool_contract.as_ref(),
                    &self.completed_task_tool_counts,
                    &tool_name,
                    &args,
                );
                let fatal = transport_terminal || exact_required_tool_failed;
                let failure_class =
                    classify_synapse_tool_call_failure(&error, exact_required_tool_failed);
                let error_code = failure_class.error_code.as_str();
                let detail = format!("{error_code}: {tool_name}: {error}");
                self.tool_call_error_count = self.tool_call_error_count.saturating_add(1);
                // Structured, actionable feedback for the model (not a bare
                // exception string): names the tool, the failure, and the next
                // step the model should take.
                let model_feedback = json!({
                    "error": error_code,
                    "tool": tool_name,
                    "message": error.to_string(),
                    "error_code_source": failure_class.error_code_source,
                    "mcp_error": failure_class.mcp_error,
                    "recoverable": !fatal,
                    "terminal": fatal,
                    "suggestion": tool_failure_suggestion(fatal, self.tool_exposure),
                });
                let result_value = json!({
                    "error": detail,
                    "error_code": error_code,
                    "error_code_source": failure_class.error_code_source,
                    "tool": tool_name,
                    "recoverable": !fatal,
                    "terminal": fatal,
                    "mcp_error": failure_class.mcp_error,
                });
                self.messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": model_feedback.to_string(),
                }));
                self.write_line(json!({
                    "type": "local.tool_call.finished",
                    "conversation_id": self.conversation_id,
                    "model": self.registry.model_id,
                    "turn_index": self.turn_count,
                    "tool_name": call.name,
                    "routed_tool_name": if routed { Some(tool_name.as_str()) } else { None },
                    "tool_call_id": call.id,
                    "status": "error",
                    "error_code": error_code,
                    "terminal": fatal,
                    "result": result_value,
                    "tool_exposure": self.tool_exposure.as_str(),
                }))?;
                self.post_event(json!({
                    "event": "tool_call_finished",
                    "session_id": self.mcp_session_id,
                    "conversation_id": self.conversation_id,
                    "model": self.registry.model_id,
                    "registry_name": self.registry.name,
                    "turn_index": self.turn_count,
                    "tool_name": call.name,
                    "routed_tool_name": if routed { Some(tool_name.as_str()) } else { None },
                    "tool_call_id": call.id,
                    "tool_response": result_value,
                    "error_code": error_code,
                    "tool_exposure": self.tool_exposure.as_str(),
                }))
                .await?;
                if fatal {
                    // Transport is gone: every further call would fail the same
                    // way. Fail loudly with the exact cause.
                    bail!("{detail}");
                }
                // Recoverable: the model now has the error in context and the
                // loop continues. The overall run stays bounded by max_turns.
                return Ok(());
            }
        };
        let is_error = result.is_error.unwrap_or(false);
        let mut result_value = tool_result_value(&result);
        self.fail_if_tool_result_contains_control_shutdown(&tool_name, &result_value)
            .await?;
        if !is_error && tool_name == "workspace_put" {
            let readback = readback_workspace_put(self.mcp.peer(), &args)
                .await
                .context("workspace_put post-write readback")?;
            self.record_workspace_put_readback_task_completion(&readback);
            result_value = attach_workspace_put_readback(result_value, readback);
            self.successful_workspace_puts.push(args.clone());
        }
        if !is_error {
            self.successful_tool_call_count = self.successful_tool_call_count.saturating_add(1);
            self.record_completed_task_tool(&tool_name, "model_tool_call");
            self.attach_task_contract_progress(&mut result_value);
        }
        let result_text = bounded_result_text(&model_tool_result_value(
            &result_value,
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            &self.successful_workspace_puts,
        ));
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "content": result_text,
        }));
        self.write_line(json!({
            "type": "local.tool_call.finished",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name.as_str()) } else { None },
            "tool_call_id": call.id,
            "status": if is_error { "error" } else { "ok" },
            "result": result_value,
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.post_event(json!({
            "event": "tool_call_finished",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name.as_str()) } else { None },
            "tool_call_id": call.id,
            "tool_response": result_value,
            "error_code": if is_error { "SYNAPSE_TOOL_ERROR" } else { "" },
            "tool_exposure": self.tool_exposure.as_str(),
        }))
        .await?;
        if !is_error && self.should_complete_verified_task_contract() {
            self.record_verified_workspace_contract_completion().await?;
        }
        Ok(())
    }

    fn is_duplicate_successful_workspace_put(&self, tool_name: &str, args: &JsonObject) -> bool {
        tool_name == "workspace_put"
            && self
                .successful_workspace_puts
                .iter()
                .any(|successful| workspace_put_args_match(successful, args))
    }

    fn should_defer_repeated_workspace_put_for_contract(&self, tool_name: &str) -> bool {
        workspace_put_contract_repetition_should_defer(
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            tool_name,
        )
    }

    fn should_defer_repeated_completed_task_tool_for_contract(&self, tool_name: &str) -> bool {
        task_contract_completed_tool_repetition_should_defer(
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            tool_name,
        )
    }

    fn synapse_tool_exists(&self, tool_name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.name.as_ref() == tool_name)
    }

    async fn record_duplicate_workspace_put_completion(
        &mut self,
        call: &OpenAiToolCall,
        tool_name: &str,
        routed: bool,
        args: &JsonObject,
    ) -> anyhow::Result<()> {
        let missing_tools =
            missing_task_contract_tools(&self.task_tool_contract, &self.completed_task_tool_counts);
        let completion_deferred = !missing_tools.is_empty();
        let result_value = json!({
            "ok": true,
            "duplicate_suppressed": true,
            "reason": "workspace_put already succeeded and read back in this run",
            "arguments": args,
            "completion_deferred": completion_deferred,
            "missing_task_tools": missing_tools,
            "suggestion": if completion_deferred {
                "Do not repeat workspace_put. Call the missing exact-tool contract tool(s)."
            } else {
                "The duplicate write is already verified; finish the task."
            },
        });
        let result_text = bounded_result_text(&result_value);
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "content": result_text,
        }));
        self.write_line(json!({
            "type": "local.tool_call.finished",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "status": "ok",
            "result": result_value,
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.post_event(json!({
            "event": "tool_call_finished",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "tool_response": result_value,
            "error_code": "",
            "tool_exposure": self.tool_exposure.as_str(),
        }))
        .await?;
        if completion_deferred {
            return Ok(());
        }
        let final_message =
            "workspace_put already succeeded and was read back; duplicate suppressed.";
        std::fs::write(self.log_dir.join("final-message.txt"), final_message).with_context(
            || format!("write {}", self.log_dir.join("final-message.txt").display()),
        )?;
        self.write_line(json!({
            "type": "local.agent.completed",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "reason_code": "duplicate_workspace_put_suppressed",
            "final_message": final_message,
        }))?;
        self.post_event(json!({
            "event": "exited",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "end_state": "success",
            "reason_code": "local_agent_completed",
            "used_any_tool": true,
            "answered_without_tool_calls": false,
            "completion_reason": "duplicate_workspace_put_suppressed",
        }))
        .await?;
        self.completed_after_tool = true;
        Ok(())
    }

    async fn record_repeated_workspace_put_contract_deferred(
        &mut self,
        call: &OpenAiToolCall,
        tool_name: &str,
        routed: bool,
        args: &JsonObject,
    ) -> anyhow::Result<()> {
        let result_value = json!({
            "error": "WORKSPACE_PUT_ALREADY_COMPLETED",
            "recoverable": true,
            "executed": false,
            "reason": "workspace_put already succeeded and was read back in this run; this repeated workspace_put was not dispatched",
            "arguments": args,
            "task_contract_progress": task_contract_progress_value(
                &self.task_tool_contract,
                &self.completed_task_tool_counts,
                &self.successful_workspace_puts,
            ),
            "completion_deferred": true,
            "suggestion": "Do not call workspace_put again. Call the missing exact-tool contract tool(s), using the suggested arguments when present.",
        });
        let result_text = bounded_result_text(&model_tool_result_value(
            &json!({
                "error": "WORKSPACE_PUT_ALREADY_COMPLETED",
                "executed": false,
                "duplicate_suppressed": true,
                "message": "workspace_put already succeeded and was read back in this run",
            }),
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            &self.successful_workspace_puts,
        ));
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "content": result_text,
        }));
        self.write_line(json!({
            "type": "local.tool_call.finished",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "status": "error",
            "error_code": "WORKSPACE_PUT_ALREADY_COMPLETED",
            "terminal": false,
            "result": result_value,
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.post_event(json!({
            "event": "tool_call_finished",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "tool_response": result_value,
            "error_code": "WORKSPACE_PUT_ALREADY_COMPLETED",
            "terminal": false,
            "tool_exposure": self.tool_exposure.as_str(),
        }))
        .await?;
        Ok(())
    }

    async fn record_repeated_task_contract_tool_deferred(
        &mut self,
        call: &OpenAiToolCall,
        tool_name: &str,
        routed: bool,
        args: &JsonObject,
    ) -> anyhow::Result<()> {
        let result_value = json!({
            "ok": true,
            "executed": false,
            "duplicate_suppressed": true,
            "tool": tool_name,
            "reason": "exact-tool contract tool already completed in this run; this repeated call was not dispatched",
            "arguments": args,
            "task_contract_progress": task_contract_progress_value(
                &self.task_tool_contract,
                &self.completed_task_tool_counts,
                &self.successful_workspace_puts,
            ),
            "completion_deferred": true,
            "suggestion": "Do not repeat completed exact-tool contract tools. Call the suggested next missing tool with suggested_next_arguments when present.",
        });
        let result_text = bounded_result_text(&model_tool_result_value(
            &json!({
                "ok": true,
                "executed": false,
                "duplicate_suppressed": true,
                "name": tool_name,
                "message": "that function already completed; do not call it again",
            }),
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            &self.successful_workspace_puts,
        ));
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "content": result_text,
        }));
        self.write_line(json!({
            "type": "local.tool_call.finished",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "status": "ok",
            "result": result_value,
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.post_event(json!({
            "event": "tool_call_finished",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "tool_response": result_value,
            "error_code": "",
            "tool_exposure": self.tool_exposure.as_str(),
        }))
        .await?;
        Ok(())
    }

    /// Audit local-model calls that would be gated for Claude, then dispatch
    /// autonomously. The pre-gate above still rejects unknown tools, off-contract
    /// calls, and runner/operator-control tools before this point; target/lease
    /// and tool-level policies still fail closed during dispatch. What must not
    /// happen is a normal local-model tool call creating an `agent_permission`
    /// row or blocking on `approval_gate`.
    async fn gate_tool_call(
        &mut self,
        tool_name: &str,
        args: JsonObject,
        tool_use_id: &str,
    ) -> anyhow::Result<ToolGate> {
        let label = gate_tool_label(tool_name);
        if !crate::server::permission_policy::classify(&label, &Value::Object(args.clone()))
            .is_gate()
        {
            return Ok(ToolGate::Allow(args));
        }

        let exact_contract_authorized = self.cli.trusted_unattended_exact_contract
            && local_agent_exact_contract_gate_bypass_allowed(
                tool_name,
                &args,
                self.task_tool_contract.as_ref(),
                &self.completed_task_tool_counts,
            );
        let reason_code = if exact_contract_authorized {
            "trusted_unattended_exact_contract"
        } else {
            "local_model_autonomous_tool_call"
        };
        self.write_line(json!({
            "type": "local.tool_call.gate_bypassed",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": tool_name,
            "tool_call_id": tool_use_id,
            "reason_code": reason_code,
            "approval_gate_used": false,
            "exact_contract_authorized": exact_contract_authorized,
        }))?;
        self.post_event(json!({
            "event": "state_changed",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "state_to": "live",
            "reason_code": reason_code,
            "tool_name": tool_name,
            "tool_call_id": tool_use_id,
        }))
        .await?;
        Ok(ToolGate::Allow(args))
    }

    async fn normalize_trusted_exact_contract_args(
        &mut self,
        call: &OpenAiToolCall,
        tool_name: &str,
        args: JsonObject,
        routed: bool,
    ) -> anyhow::Result<JsonObject> {
        let Some(contract_args) = trusted_exact_contract_normalized_args(
            self.cli.trusted_unattended_exact_contract,
            tool_name,
            &args,
            self.task_tool_contract.as_ref(),
            &self.completed_task_tool_counts,
        ) else {
            return Ok(args);
        };
        let model_args = args;
        self.write_line(json!({
            "type": "local.tool_call.arguments_normalized",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": tool_name,
            "tool_call_id": call.id,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "reason_code": "trusted_unattended_exact_contract_arguments_normalized",
            "model_arguments": Value::Object(model_args),
            "contract_arguments": Value::Object(contract_args.clone()),
            "task_contract_progress": task_contract_progress_value(
                &self.task_tool_contract,
                &self.completed_task_tool_counts,
                &self.successful_workspace_puts,
            ),
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.post_event(json!({
            "event": "state_changed",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "state_to": "live",
            "reason_code": "trusted_unattended_exact_contract_arguments_normalized",
            "tool_name": tool_name,
            "tool_call_id": call.id,
        }))
        .await?;
        Ok(contract_args)
    }

    async fn normalize_local_agent_attribution_args(
        &mut self,
        call: &OpenAiToolCall,
        tool_name: &str,
        args: JsonObject,
        routed: bool,
    ) -> anyhow::Result<JsonObject> {
        let (attributed_args, changed) =
            add_agent_ask_operator_spawn_id(tool_name, args, &self.spawn_id);
        if !changed {
            return Ok(attributed_args);
        }
        self.write_line(json!({
            "type": "local.tool_call.arguments_normalized",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": tool_name,
            "tool_call_id": call.id,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "reason_code": "local_agent_spawn_id_attribution",
            "spawn_id": self.spawn_id,
            "attributed_arguments": Value::Object(attributed_args.clone()),
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.post_event(json!({
            "event": "state_changed",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "state_to": "live",
            "reason_code": "local_agent_spawn_id_attribution",
            "tool_name": tool_name,
            "tool_call_id": call.id,
            "spawn_id": self.spawn_id,
        }))
        .await?;
        Ok(attributed_args)
    }

    async fn record_model_tool_call_invalid(
        &mut self,
        call: &OpenAiToolCall,
        tool_name: &str,
        routed: bool,
        rejection: &ModelToolCallRejection,
    ) -> anyhow::Result<bool> {
        self.tool_call_error_count = self.tool_call_error_count.saturating_add(1);
        self.invalid_tool_call_count = self.invalid_tool_call_count.saturating_add(1);
        let retry_limit_exceeded =
            !rejection.terminal && self.invalid_tool_call_count > self.cli.tool_parse_retry_limit;
        let terminal = rejection.terminal || retry_limit_exceeded;
        let detail = if retry_limit_exceeded {
            format!(
                "MODEL_TOOL_CALL_INVALID: {tool_name}: {} (invalid tool-call retry limit {} exceeded)",
                rejection.reason, self.cli.tool_parse_retry_limit
            )
        } else {
            format!("MODEL_TOOL_CALL_INVALID: {tool_name}: {}", rejection.reason)
        };
        let suggestion = if retry_limit_exceeded {
            "Invalid tool-call retry limit exceeded; stop instead of retrying."
        } else {
            rejection.suggestion
        };
        let task_contract_progress = task_contract_progress_value(
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            &self.successful_workspace_puts,
        );
        let mut model_feedback = json!({
            "error": "MODEL_TOOL_CALL_INVALID",
            "tool": tool_name,
            "message": rejection.reason,
            "recoverable": !terminal,
            "retry_count": self.invalid_tool_call_count,
            "retry_limit": self.cli.tool_parse_retry_limit,
            "exact_function_contract": self
                .task_tool_contract
                .as_ref()
                .map(TaskToolContract::model_facing_json),
            "suggestion": suggestion,
        });
        if let Some(progress) = task_contract_model_status_value(
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            &self.successful_workspace_puts,
        ) {
            attach_model_contract_status_feedback(&mut model_feedback, progress);
        }
        let mut result_value = json!({ "error": detail });
        if let Some(progress) = task_contract_progress {
            if let Value::Object(result) = &mut result_value {
                result.insert("task_contract_progress".to_owned(), progress);
            }
        }
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "content": model_feedback.to_string(),
        }));
        self.write_line(json!({
            "type": "local.tool_call.finished",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "status": "error",
            "error_code": "MODEL_TOOL_CALL_INVALID",
            "terminal": terminal,
            "retry_count": self.invalid_tool_call_count,
            "retry_limit": self.cli.tool_parse_retry_limit,
            "result": result_value,
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.post_event(json!({
            "event": "tool_call_finished",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "tool_response": result_value,
            "error_code": "MODEL_TOOL_CALL_INVALID",
            "terminal": terminal,
            "retry_count": self.invalid_tool_call_count,
            "retry_limit": self.cli.tool_parse_retry_limit,
            "tool_exposure": self.tool_exposure.as_str(),
        }))
        .await?;
        Ok(terminal)
    }

    /// Feed an operator denial back to the model as the tool result (so it can
    /// pick another approach), and journal the gate-denied transition. The risky
    /// tool is NOT dispatched.
    async fn record_tool_gate_denied(
        &mut self,
        call: &OpenAiToolCall,
        tool_name: &str,
        routed: bool,
        reason: &str,
    ) -> anyhow::Result<()> {
        let feedback = json!({
            "error": "APPROVAL_DENIED",
            "tool": tool_name,
            "message": reason,
            "recoverable": true,
            "suggestion": "The operator denied this action. Do not retry it; choose a different approach or stop and explain why you cannot proceed.",
        });
        let result_value = json!({ "error": format!("APPROVAL_DENIED: {tool_name}: {reason}") });
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "content": feedback.to_string(),
        }));
        self.write_line(json!({
            "type": "local.tool_call.finished",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "status": "denied",
            "error_code": "APPROVAL_DENIED",
            "result": result_value,
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.post_event(json!({
            "event": "tool_call_finished",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "routed_tool_name": if routed { Some(tool_name) } else { None },
            "tool_call_id": call.id,
            "tool_response": result_value,
            "error_code": "APPROVAL_DENIED",
            "tool_exposure": self.tool_exposure.as_str(),
        }))
        .await?;
        Ok(())
    }

    async fn record_tool_parse_error(
        &mut self,
        call: &OpenAiToolCall,
        error: anyhow::Error,
    ) -> anyhow::Result<()> {
        self.parse_error_count = self.parse_error_count.saturating_add(1);
        let detail = format!("TOOL_CALL_ARGUMENTS_NOT_JSON: {error}");
        let task_contract_progress = task_contract_progress_value(
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            &self.successful_workspace_puts,
        );
        let mut result_value = json!({ "error": detail });
        if let Some(progress) = task_contract_progress.clone() {
            if let Value::Object(result) = &mut result_value {
                result.insert("task_contract_progress".to_owned(), progress);
            }
        }
        let mut model_feedback = json!({
            "error": "MODEL_TOOL_ARGUMENTS_INVALID",
            "tool": call.name,
            "message": detail,
            "recoverable": self.parse_error_count <= self.cli.tool_parse_retry_limit,
            "retry_count": self.parse_error_count,
            "retry_limit": self.cli.tool_parse_retry_limit,
        });
        if let Some(progress) = task_contract_model_status_value(
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            &self.successful_workspace_puts,
        ) {
            attach_model_contract_status_feedback(&mut model_feedback, progress);
        }
        self.write_line(json!({
            "type": "local.tool_parse_error",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "tool_call_id": call.id,
            "error_code": "MODEL_TOOL_ARGUMENTS_INVALID",
            "error_detail": detail,
            "tool_exposure": self.tool_exposure.as_str(),
        }))?;
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "content": model_feedback.to_string(),
        }));
        self.post_event(json!({
            "event": "tool_call_finished",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "turn_index": self.turn_count,
            "tool_name": call.name,
            "tool_call_id": call.id,
            "tool_response": result_value,
            "error_code": "MODEL_TOOL_ARGUMENTS_INVALID",
            "tool_exposure": self.tool_exposure.as_str(),
        }))
        .await?;
        if self.parse_error_count > self.cli.tool_parse_retry_limit {
            bail!(
                "MODEL_TOOLS_UNSUPPORTED: malformed tool-call arguments exceeded retry limit {}",
                self.cli.tool_parse_retry_limit
            );
        }
        Ok(())
    }

    fn tool_catalog_value(&self, raw_arguments: &str) -> anyhow::Result<Value> {
        let args = parse_tool_arguments(raw_arguments)?;
        let name_filter = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(50)
            .clamp(1, 200);
        let mut rows = Vec::new();
        for tool in &self.tools {
            let tool_name = tool.name.as_ref();
            let description = tool
                .description
                .as_ref()
                .map(|desc| desc.as_ref())
                .unwrap_or("");
            let matches_name = name_filter.is_none_or(|name| tool_name == name);
            let matches_query = query.as_ref().is_none_or(|query| {
                tool_name.to_ascii_lowercase().contains(query)
                    || description.to_ascii_lowercase().contains(query)
            });
            if matches_name && matches_query {
                rows.push(json!({
                    "name": tool_name,
                    "description": description,
                    "input_schema": Value::Object((*tool.input_schema).clone()),
                }));
                if rows.len() >= limit {
                    break;
                }
            }
        }
        Ok(json!({
            "source_of_truth": "MCP tools/list loaded by local-agent client",
            "tool_count": self.tools.len(),
            "returned_count": rows.len(),
            "limit": limit,
            "tool_exposure": self.tool_exposure.as_str(),
            "call_tool": SYNAPSE_TOOL_CALL,
            "tools": rows,
        }))
    }

    async fn fail_if_tool_result_contains_control_shutdown(
        &mut self,
        tool_name: &str,
        result_value: &Value,
    ) -> anyhow::Result<()> {
        let Some(message) = shutdown_message_from_tool_result(tool_name, result_value) else {
            return Ok(());
        };
        let steering_text = steering_payload_text(&message.payload);
        self.write_line(json!({
            "type": "local.steering.received",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "message_id": message.message_id,
            "kind": message.kind,
            "payload_summary": bounded_text(&steering_text, 2_000),
            "source_tool": tool_name,
            "delivery_path": "tool_result_control_filter",
        }))?;
        self.post_event(json!({
            "event": "state_changed",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "state_to": "live",
            "reason_code": "local_steering_received",
            "message_id": message.message_id,
            "message_kind": message.kind,
            "source_tool": tool_name,
        }))
        .await?;
        bail!(
            "LOCAL_AGENT_INTERRUPTED: control message {} kind={} requested shutdown",
            message.message_id,
            message.kind
        )
    }

    async fn drain_steering_inbox(&mut self) -> anyhow::Result<()> {
        let mut args = Map::new();
        args.insert("drain".to_owned(), Value::Bool(true));
        args.insert("max_messages".to_owned(), Value::from(16));
        let inbox = call_mcp_tool_json(self.mcp.peer(), "agent_inbox", args)
            .await
            .context("drain local-agent steering inbox")?;
        let messages = inbox
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if messages.is_empty() {
            return Ok(());
        }
        for message in messages {
            let message_id = message
                .get("message_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let kind = message
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("message")
                .trim()
                .to_owned();
            let payload = message.get("payload").cloned().unwrap_or(Value::Null);
            let steering_text = steering_payload_text(&payload);
            let content = format!(
                "Synapse control message {message_id} kind={kind}:\n{}",
                bounded_text(&steering_text, 8_000)
            );
            self.write_line(json!({
                "type": "local.steering.received",
                "session_id": self.mcp_session_id,
                "conversation_id": self.conversation_id,
                "model": self.registry.model_id,
                "turn_index": self.turn_count,
                "message_id": message_id,
                "kind": kind,
                "payload_summary": bounded_text(&steering_text, 2_000),
            }))?;
            self.post_event(json!({
                "event": "state_changed",
                "session_id": self.mcp_session_id,
                "conversation_id": self.conversation_id,
                "model": self.registry.model_id,
                "registry_name": self.registry.name,
                "state_to": "live",
                "reason_code": "local_steering_received",
                "message_id": message_id,
                "message_kind": kind,
            }))
            .await?;
            if steering_requests_shutdown(&kind, &payload) {
                bail!(
                    "LOCAL_AGENT_INTERRUPTED: control message {message_id} kind={kind} requested shutdown"
                );
            }
            self.messages.push(json!({
                "role": "user",
                "content": content,
            }));
        }
        Ok(())
    }

    async fn chat_completion(&self) -> anyhow::Result<ChatCompletion> {
        let stream = should_stream(self.cli.no_stream, self.tool_exposure);
        let turn_tools = self.openai_tools_for_current_turn()?;
        let turn_messages = self.messages_for_current_turn();
        let mut body = json!({
            "model": self.registry.model_id,
            "messages": turn_messages,
            "tools": turn_tools.tools,
            "tool_choice": "auto",
            "temperature": 0,
            "stream": stream,
        });
        if let Some(tool_choice) = turn_tools.tool_choice {
            body["tool_choice"] = tool_choice;
        }
        apply_runtime_preset(&self.registry, &mut body);
        if stream {
            body["stream_options"] = json!({"include_usage": true});
        }
        let mut request = self.http.post(self.endpoint_url.clone()).json(&body);
        if let Some(api_key) = &self.endpoint_api_key {
            request = request.bearer_auth(api_key);
        }
        let response = request
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("MODEL_ENDPOINT_UNREACHABLE: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("MODEL_ENDPOINT_UNREACHABLE: endpoint returned HTTP {status}: {body}");
        }
        if stream {
            parse_streaming_response(response).await
        } else {
            let text = response.text().await.context("read non-stream response")?;
            parse_non_stream_response(&text)
        }
    }

    fn messages_for_current_turn(&self) -> Vec<Value> {
        let mut messages = self.messages.clone();
        if matches!(
            self.tool_exposure,
            ToolExposure::Internalized | ToolExposure::Routed
        ) {
            if let Some(contract) = &self.task_tool_contract {
                if let Some(message) = task_contract_next_instruction_message(
                    contract,
                    &self.completed_task_tool_counts,
                ) {
                    messages.push(message);
                }
            }
        }
        messages
    }

    fn openai_tools_for_current_turn(&self) -> anyhow::Result<TurnToolSelection> {
        if matches!(
            self.tool_exposure,
            ToolExposure::Internalized | ToolExposure::Routed
        ) {
            if let Some(contract) = &self.task_tool_contract {
                return exact_contract_turn_tools(
                    &self.tools,
                    contract,
                    &self.completed_task_tool_counts,
                );
            }
        }
        Ok(TurnToolSelection {
            tools: self.openai_tools.clone(),
            tool_choice: None,
        })
    }

    async fn truncate_context_if_needed(&mut self) -> anyhow::Result<()> {
        let current = serde_json::to_string(&self.messages)
            .context("serialize local-agent context for length check")?;
        if current.chars().count() <= self.cli.context_char_limit {
            return Ok(());
        }
        let before_chars = current.chars().count();
        while self.messages.len() > 2 {
            let current = serde_json::to_string(&self.messages)?;
            if current.chars().count() <= self.cli.context_char_limit {
                break;
            }
            self.messages.remove(2);
        }
        let after_chars = serde_json::to_string(&self.messages)?.chars().count();
        self.truncated_context_count = self.truncated_context_count.saturating_add(1);
        self.write_line(json!({
            "type": "local.context.truncated",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "before_chars": before_chars,
            "after_chars": after_chars,
            "limit_chars": self.cli.context_char_limit,
        }))?;
        self.post_event(json!({
            "event": "state_changed",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "state_to": "live",
            "reason_code": "local_context_truncated",
            "before_chars": before_chars,
            "after_chars": after_chars,
            "limit_chars": self.cli.context_char_limit,
        }))
        .await?;
        if after_chars > self.cli.context_char_limit {
            bail!(
                "LOCAL_AGENT_CONTEXT_OVERFLOW: context is {after_chars} chars after truncation, limit {}",
                self.cli.context_char_limit
            );
        }
        Ok(())
    }

    async fn post_event(&self, body: Value) -> anyhow::Result<()> {
        let response = self
            .http
            .post(self.event_url.clone())
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .context("POST local model event ingress")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("AGENT_EVENT_INGRESS_WRITE_FAILED: HTTP {status}: {text}");
        }
        Ok(())
    }

    fn write_line(&mut self, value: Value) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.stdout, &value).context("write local-agent stdout JSON")?;
        self.stdout
            .write_all(b"\n")
            .context("write local-agent stdout newline")?;
        self.stdout.flush().context("flush local-agent stdout")?;
        Ok(())
    }

    fn write_success_status(&self) -> anyhow::Result<()> {
        write_json_file(
            self.log_dir.join("completion-status.json"),
            &json!({
                "schema_version": 1,
                "spawn_id": self.spawn_id,
                "status": "ok",
                "state": "complete",
                "exit_code": 0,
                "turn_count": self.turn_count,
                "tool_call_count": self.tool_call_count,
                "successful_tool_call_count": self.successful_tool_call_count,
                "parse_error_count": self.parse_error_count,
                "invalid_tool_call_count": self.invalid_tool_call_count,
                "tool_call_error_count": self.tool_call_error_count,
                "truncated_context_count": self.truncated_context_count,
                "task_tool_contract": self
                    .task_tool_contract
                    .as_ref()
                    .map(TaskToolContract::to_json),
                "completed_task_tools": self
                    .completed_task_tools
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
                "completed_task_tool_counts": self.completed_task_tool_counts.clone(),
                "completed_task_tool_sources": self.completed_task_tool_sources.clone(),
                "usage": self.total_usage,
                "completed_at_unix_ms": unix_time_ms_now(),
            }),
        )
    }

    async fn write_failure(&mut self, code: &str, detail: &str) -> anyhow::Result<()> {
        let _ = self.write_line(json!({
            "type": "local.error",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "turn_index": self.turn_count,
            "error_code": code,
            "error_detail": detail,
        }));
        let _ = self
            .post_event(json!({
                "event": "exited",
                "session_id": self.mcp_session_id,
                "conversation_id": self.conversation_id,
                "model": self.registry.model_id,
                "registry_name": self.registry.name,
                "end_state": "error",
                "reason_code": code,
                "error_code": code,
            }))
            .await;
        write_json_file(
            self.log_dir.join("completion-status.json"),
            &json!({
                "schema_version": 1,
                "spawn_id": self.spawn_id,
                "status": "failed",
                "state": "dead",
                "exit_code": 1,
                "error_code": code,
                "error_message": detail,
                "turn_count": self.turn_count,
                "tool_call_count": self.tool_call_count,
                "successful_tool_call_count": self.successful_tool_call_count,
                "parse_error_count": self.parse_error_count,
                "invalid_tool_call_count": self.invalid_tool_call_count,
                "tool_call_error_count": self.tool_call_error_count,
                "truncated_context_count": self.truncated_context_count,
                "task_tool_contract": self
                    .task_tool_contract
                    .as_ref()
                    .map(TaskToolContract::to_json),
                "completed_task_tools": self
                    .completed_task_tools
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
                "completed_task_tool_sources": self.completed_task_tool_sources.clone(),
                "usage": self.total_usage,
                "completed_at_unix_ms": unix_time_ms_now(),
            }),
        )
    }

    fn record_completed_task_tool(&mut self, tool_name: &str, source: &str) {
        let Some(contract) = &self.task_tool_contract else {
            return;
        };
        if contract.allowed_tools.contains(tool_name) {
            let count = self
                .completed_task_tool_counts
                .entry(tool_name.to_owned())
                .or_insert(0);
            *count = count.saturating_add(1);
            let occurrence_key = format!("{tool_name}#{}", *count);
            self.completed_task_tools.insert(tool_name.to_owned());
            self.completed_task_tool_sources
                .entry(occurrence_key)
                .or_insert_with(|| source.to_owned());
        }
    }

    fn record_workspace_put_readback_task_completion(&mut self, readback: &Value) {
        if readback.get("tool").and_then(Value::as_str) != Some("workspace_get") {
            return;
        }
        if readback.get("matched").and_then(Value::as_bool) != Some(true) {
            return;
        }
        self.record_completed_task_tool("workspace_get", "workspace_put_post_write_readback");
    }

    fn attach_task_contract_progress(&self, result_value: &mut Value) {
        let Some(progress) = task_contract_progress_value(
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            &self.successful_workspace_puts,
        ) else {
            return;
        };
        match result_value {
            Value::Object(map) => {
                map.insert("task_contract_progress".to_owned(), progress);
            }
            _ => {
                *result_value = json!({
                    "result": result_value,
                    "task_contract_progress": progress,
                });
            }
        }
    }

    fn should_complete_verified_task_contract(&self) -> bool {
        verified_workspace_contract_complete(
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
        ) || verified_workspace_checkpoint_contract_complete(
            &self.task_tool_contract,
            &self.completed_task_tool_counts,
            &self.successful_workspace_puts,
        )
    }

    async fn record_verified_workspace_contract_completion(&mut self) -> anyhow::Result<()> {
        let final_message = final_message_from_successful_workspace_puts(
            &self.successful_workspace_puts,
            "workspace_put/workspace_get exact-tool contract completed with verified readback",
        );
        std::fs::write(self.log_dir.join("final-message.txt"), &final_message).with_context(
            || format!("write {}", self.log_dir.join("final-message.txt").display()),
        )?;
        self.write_line(json!({
            "type": "local.agent.completed",
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "reason_code": "task_tool_contract_verified",
            "final_message": final_message,
            "completed_task_tools": self
                .completed_task_tools
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<_>>(),
            "completed_task_tool_counts": self.completed_task_tool_counts.clone(),
            "completed_task_tool_sources": self.completed_task_tool_sources.clone(),
        }))?;
        self.post_event(json!({
            "event": "exited",
            "session_id": self.mcp_session_id,
            "conversation_id": self.conversation_id,
            "model": self.registry.model_id,
            "registry_name": self.registry.name,
            "end_state": "success",
            "reason_code": "local_agent_completed",
            "used_any_tool": true,
            "answered_without_tool_calls": false,
            "completion_reason": "task_tool_contract_verified",
            "completed_task_tools": self
                .completed_task_tools
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<_>>(),
            "completed_task_tool_counts": self.completed_task_tool_counts.clone(),
            "completed_task_tool_sources": self.completed_task_tool_sources.clone(),
        }))
        .await?;
        self.completed_after_tool = true;
        Ok(())
    }
}

async fn current_mcp_session_id(
    peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
) -> anyhow::Result<String> {
    let mut args = Map::new();
    args.insert("drain".to_owned(), Value::Bool(false));
    args.insert("max_messages".to_owned(), Value::from(1));
    let inbox = call_mcp_tool_json(peer, "agent_inbox", args)
        .await
        .context("read local-agent MCP session id through agent_inbox")?;
    let session_id = inbox
        .get("this_session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "HTTP_SESSION_INVALID: agent_inbox did not return this_session_id for local-agent session"
            )
        })?;
    Ok(session_id.to_owned())
}

async fn bind_requested_target(
    peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
    target_json: Option<&str>,
) -> anyhow::Result<()> {
    let Some(raw_target) = target_json.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    let target: Value = serde_json::from_str(raw_target)
        .with_context(|| "TOOL_PARAMS_INVALID: --local-agent-target-json must be JSON")?;
    let mut args = Map::new();
    args.insert("target".to_owned(), target.clone());
    call_mcp_tool_json(peer, "set_target", args)
        .await
        .context("bind local-agent target through set_target")?;
    let readback = call_mcp_tool_json(peer, "get_target", Map::new())
        .await
        .context("read local-agent target through get_target")?;
    let current = readback.get("current").cloned().unwrap_or(Value::Null);
    if current != target {
        bail!("TARGET_BIND_READBACK_MISMATCH: expected {target}, got {current}");
    }
    Ok(())
}

async fn call_mcp_tool_json(
    peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
    name: &str,
    arguments: JsonObject,
) -> anyhow::Result<Value> {
    let result = peer
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
        .await
        .with_context(|| format!("MCP tool {name} call failed"))?;
    if result.is_error.unwrap_or(false) {
        bail!(
            "MCP tool {name} returned isError=true: {}",
            tool_result_value(&result)
        );
    }
    Ok(tool_result_value(&result))
}

#[derive(Clone, Debug, PartialEq)]
struct WorkspacePutReadbackPlan {
    arguments: JsonObject,
    expected_value: Value,
}

#[derive(Clone, Debug, PartialEq)]
struct TurnToolSelection {
    tools: Vec<Value>,
    tool_choice: Option<Value>,
}

fn model_tool_call_pre_gate_rejection(
    tool_name: &str,
    args: &JsonObject,
    tool_present: bool,
    task_tool_contract: Option<&TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> Option<ModelToolCallRejection> {
    if !tool_present {
        return Some(ModelToolCallRejection::recoverable(
            format!(
                "{tool_name} is not present in Synapse tools/list; local models must emit a real Synapse tool name"
            ),
            "Retry with a real Synapse tool name requested by the task and exact JSON arguments.",
        ));
    }
    if let Some(contract) = task_tool_contract {
        if !contract.allowed_tools.contains(tool_name) {
            return Some(ModelToolCallRejection::recoverable(
                format!(
                    "{tool_name} is outside this task's exact-tool contract; allowed tools: {}",
                    contract.allowed_tools_display()
                ),
                "Retry using only an allowed task tool with the exact user-provided arguments.",
            ));
        }
        if let Some(rejection) =
            task_contract_argument_rejection(contract, completed_task_tool_counts, tool_name, args)
        {
            return Some(rejection);
        }
    }
    if local_agent_runner_operator_control_tool(tool_name) {
        return Some(ModelToolCallRejection::terminal(
            format!("{tool_name} is runner/operator-control; local models must not call it"),
            "Stop. Runner/operator-control tools are not available to local-model workers.",
        ));
    }
    match tool_name {
        "workspace_put" => {
            if !args.contains_key("value") && !args.contains_key("artifact") {
                return Some(ModelToolCallRejection::recoverable(
                    "workspace_put requires at least one of value or artifact before approval",
                    "Retry workspace_put with the exact key and value or artifact from the task; do not add expected_version unless replacing an existing row.",
                ));
            }
            None
        }
        _ => None,
    }
}

fn add_agent_ask_operator_spawn_id(
    tool_name: &str,
    mut args: JsonObject,
    spawn_id: &str,
) -> (JsonObject, bool) {
    if tool_name != "agent_ask_operator" {
        return (args, false);
    }
    let should_add = match args.get("spawn_id") {
        None | Some(Value::Null) => true,
        Some(Value::String(value)) => value.trim().is_empty(),
        Some(_) => false,
    };
    if !should_add {
        return (args, false);
    }
    args.insert("spawn_id".to_owned(), Value::String(spawn_id.to_owned()));
    (args, true)
}

fn local_agent_exact_contract_gate_bypass_allowed(
    tool_name: &str,
    args: &JsonObject,
    task_tool_contract: Option<&TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> bool {
    let Some(contract) = task_tool_contract else {
        return false;
    };
    if !contract.allowed_tools.contains(tool_name) {
        return false;
    }
    let Some((step_index, next_tool)) =
        next_missing_task_contract_step(contract, completed_task_tool_counts)
    else {
        return false;
    };
    if next_tool != tool_name {
        return false;
    }
    if crate::server::permission_policy::classify(
        &gate_tool_label(tool_name),
        &Value::Object(args.clone()),
    )
    .destructive()
    {
        return false;
    }
    if local_agent_exact_contract_gate_bypass_denied_tool(tool_name) {
        return false;
    }
    match contract
        .step_argument_templates
        .get(step_index)
        .and_then(Option::as_ref)
    {
        Some(expected) => expected == args,
        None => false,
    }
}

fn trusted_exact_contract_normalized_args(
    trusted: bool,
    tool_name: &str,
    args: &JsonObject,
    task_tool_contract: Option<&TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> Option<JsonObject> {
    if !trusted {
        return None;
    }
    let contract = task_tool_contract?;
    if !contract.allowed_tools.contains(tool_name) {
        return None;
    }
    let (step_index, next_tool) =
        next_missing_task_contract_step(contract, completed_task_tool_counts)?;
    if next_tool != tool_name {
        return None;
    }
    if local_agent_exact_contract_gate_bypass_denied_tool(tool_name) {
        return None;
    }
    let expected = contract
        .step_argument_templates
        .get(step_index)
        .and_then(Option::as_ref)?;
    if expected == args {
        return None;
    }
    if crate::server::permission_policy::classify(
        &gate_tool_label(tool_name),
        &Value::Object(expected.clone()),
    )
    .destructive()
    {
        return None;
    }
    Some(expected.clone())
}

fn local_agent_exact_contract_gate_bypass_denied_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "approval_decide"
            | "approval_request"
            | "approval_gate"
            | "act_run_shell"
            | "act_run_shell_cancel"
            | "act_run_shell_start"
            | "act_spawn_agent"
            | "agent_send"
            | "agent_send_broadcast"
            | "agent_interrupt"
            | "agent_kill"
            | "agent_pause"
            | "agent_respawn"
            | "agent_resume"
            | "agent_steer"
            | "agent_template_delete"
            | "agent_template_put"
            | "act_click"
            | "act_clipboard"
            | "act_combo"
            | "act_focus_window"
            | "act_keymap"
            | "act_launch"
            | "act_pad"
            | "act_press"
            | "act_scroll"
            | "act_set_field_text"
            | "act_set_value"
            | "act_stroke"
            | "act_type"
            | "control_lease_acquire"
            | "control_lease_handoff"
            | "control_lease_release"
            | "fleet_stop"
            | "local_model_probe"
            | "local_model_register"
            | "local_model_remove"
            | "local_model_update"
            | "task_cancel"
            | "task_claim"
            | "task_create"
            | "task_dispatch_once"
            | "task_reconcile"
            | "task_update"
            | "tool_profile_set"
    )
}

fn local_agent_runner_operator_control_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "approval_decide"
            | "approval_gate"
            | "approval_request"
            | "act_spawn_agent"
            | "agent_send"
            | "agent_send_broadcast"
            | "agent_interrupt"
            | "agent_kill"
            | "agent_pause"
            | "agent_respawn"
            | "agent_resume"
            | "agent_steer"
            | "agent_template_delete"
            | "agent_template_put"
            | "fleet_stop"
            | "local_model_probe"
            | "local_model_register"
            | "local_model_remove"
            | "local_model_update"
            | "task_cancel"
            | "task_claim"
            | "task_create"
            | "task_dispatch_once"
            | "task_reconcile"
            | "task_update"
            | "tool_profile_set"
    )
}

fn task_contract_argument_rejection(
    contract: &TaskToolContract,
    completed_task_tool_counts: &BTreeMap<String, usize>,
    tool_name: &str,
    args: &JsonObject,
) -> Option<ModelToolCallRejection> {
    let (step_index, next_tool) =
        next_missing_task_contract_step(contract, completed_task_tool_counts)?;
    if next_tool != tool_name {
        return None;
    }
    let expected = contract
        .step_argument_templates
        .get(step_index)
        .and_then(Option::as_ref)?;
    if expected == args {
        return None;
    }
    let missing = expected
        .keys()
        .filter(|key| !args.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    let extra = args
        .keys()
        .filter(|key| !expected.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    let changed = expected
        .iter()
        .filter(|(key, expected_value)| {
            args.get(*key)
                .is_some_and(|actual_value| actual_value != *expected_value)
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    Some(ModelToolCallRejection::recoverable(
        format!(
            "{tool_name} arguments do not match this exact-tool contract; expected {}, got {}; missing_keys=[{}], extra_keys=[{}], changed_keys=[{}]",
            stable_json_object_text(expected),
            stable_json_object_text(args),
            missing.join(", "),
            extra.join(", "),
            changed.join(", ")
        ),
        "Retry with contract_status.next_function.arguments exactly; do not add, omit, or change keys.",
    ))
}

fn stable_json_object_text(args: &JsonObject) -> String {
    serde_json::to_string(&Value::Object(args.clone())).unwrap_or_else(|_| "{}".to_owned())
}

fn infer_task_tool_contract(task: &str, tools: &[Tool]) -> Option<TaskToolContract> {
    let lower = task.to_ascii_lowercase();
    let exact_tool_phrase = [
        "exact-contract synapse task",
        "use exactly",
        "use only the exact tool calls",
        "call exactly",
        "execute exactly these mcp tools",
        "execute exactly these synapse tools",
        "execute exactly these real synapse tools",
        "exactly one synapse mcp tool",
        "required tool calls exactly in order",
        "use exactly these real synapse tools",
        "use exactly these synapse tools",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));
    if !exact_tool_phrase {
        return None;
    }
    let mut mentioned_tools = tools
        .iter()
        .flat_map(|tool| {
            let name = tool.name.as_ref();
            let positions = task_tool_name_positions(task, name)
                .into_iter()
                .filter(|position| !task_tool_mention_is_denied(task, *position))
                .collect::<Vec<_>>();
            let Some(first_position) = positions.first().copied() else {
                return Vec::new();
            };
            if task_tool_argument_template(task, first_position).is_some() {
                positions
                    .into_iter()
                    .filter(|position| task_tool_argument_template(task, *position).is_some())
                    .map(|position| (position, name.to_owned()))
                    .collect::<Vec<_>>()
            } else {
                vec![(first_position, name.to_owned())]
            }
        })
        .collect::<Vec<_>>();
    mentioned_tools.sort_by(|(left_position, left_name), (right_position, right_name)| {
        left_position
            .cmp(right_position)
            .then_with(|| left_name.cmp(right_name))
    });
    let ordered_tools = mentioned_tools
        .iter()
        .map(|(_, name)| name.clone())
        .collect::<Vec<_>>();
    let allowed_tools = ordered_tools.iter().cloned().collect::<BTreeSet<_>>();
    let argument_templates_by_name = mentioned_tools
        .iter()
        .map(|(_, name)| {
            (
                name.clone(),
                task_tool_argument_templates_for_name(task, name),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut argument_template_indexes = BTreeMap::<String, usize>::new();
    let step_argument_templates = mentioned_tools
        .iter()
        .map(|(position, name)| {
            task_tool_argument_template(task, *position).or_else(|| {
                let index = argument_template_indexes.entry(name.clone()).or_insert(0);
                let template = argument_templates_by_name
                    .get(name)
                    .and_then(|templates| templates.get(*index))
                    .cloned();
                if template.is_some() {
                    *index += 1;
                }
                template
            })
        })
        .collect::<Vec<_>>();
    let argument_templates = mentioned_tools
        .iter()
        .filter_map(|(_, name)| {
            task_tool_argument_template_for_name(task, name).map(|args| (name.clone(), args))
        })
        .collect::<BTreeMap<_, _>>();
    if allowed_tools.is_empty() {
        return None;
    }
    Some(TaskToolContract {
        allowed_tools,
        ordered_tools,
        argument_templates,
        step_argument_templates,
        source: "task_exact_tool_phrase",
    })
}

fn task_tool_name_positions(task: &str, tool_name: &str) -> Vec<usize> {
    let mut search_start = 0;
    let mut positions = Vec::new();
    while let Some(offset) = task[search_start..].find(tool_name) {
        let start = search_start + offset;
        let end = start + tool_name.len();
        let before_ok = start == 0
            || task[..start]
                .chars()
                .next_back()
                .is_none_or(|ch| !is_tool_name_char(ch));
        let after_ok = end == task.len()
            || task[end..]
                .chars()
                .next()
                .is_none_or(|ch| !is_tool_name_char(ch));
        if before_ok && after_ok {
            positions.push(start);
        }
        search_start = end;
    }
    positions
}

fn task_tool_mention_is_denied(task: &str, tool_position: usize) -> bool {
    let line_start = task[..tool_position]
        .rfind('\n')
        .map(|offset| offset + 1)
        .unwrap_or(0);
    let line_end = task[tool_position..]
        .find('\n')
        .map(|offset| tool_position + offset)
        .unwrap_or(task.len());
    let line = task[line_start..line_end].to_ascii_lowercase();
    let tool_offset = tool_position.saturating_sub(line_start);
    let before_tool = &line[..tool_offset.min(line.len())];
    let denial_markers = [
        "do not call",
        "do not use",
        "don't call",
        "don't use",
        "never call",
        "never use",
        "avoid",
        "forbidden",
        "prohibited",
        "not allowed",
        "unless task names",
    ];
    denial_markers
        .iter()
        .any(|marker| before_tool.contains(marker) || line.contains(marker))
}

fn is_tool_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn task_tool_argument_template(task: &str, tool_position: usize) -> Option<JsonObject> {
    let line_end = task[tool_position..]
        .find('\n')
        .map(|offset| tool_position + offset)
        .unwrap_or(task.len());
    let line_tail = &task[tool_position..line_end];
    let object_offset = line_tail.find('{')?;
    let object_start = tool_position + object_offset;
    parse_json_object_at(task, object_start).ok()
}

fn task_tool_argument_template_for_name(task: &str, tool_name: &str) -> Option<JsonObject> {
    task_tool_name_positions(task, tool_name)
        .into_iter()
        .filter(|position| !task_tool_mention_is_denied(task, *position))
        .find_map(|position| task_tool_argument_template(task, position))
}

fn task_tool_argument_templates_for_name(task: &str, tool_name: &str) -> Vec<JsonObject> {
    task_tool_name_positions(task, tool_name)
        .into_iter()
        .filter(|position| !task_tool_mention_is_denied(task, *position))
        .filter_map(|position| task_tool_argument_template(task, position))
        .collect()
}

fn parse_json_object_at(input: &str, object_start: usize) -> anyhow::Result<JsonObject> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in input[object_start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth = depth.saturating_add(1),
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = object_start + offset + ch.len_utf8();
                    let value: Value = serde_json::from_str(&input[object_start..end])?;
                    return value
                        .as_object()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("argument template must be a JSON object"));
                }
            }
            _ => {}
        }
    }
    bail!("argument template JSON object is unterminated")
}

fn missing_task_contract_tools(
    task_tool_contract: &Option<TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> Vec<String> {
    task_tool_contract
        .as_ref()
        .map(|contract| {
            missing_task_contract_tools_for_contract(contract, completed_task_tool_counts)
        })
        .unwrap_or_default()
}

fn missing_task_contract_tools_for_contract(
    contract: &TaskToolContract,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut remaining_counts = completed_task_tool_counts.clone();
    contract
        .ordered_tools
        .iter()
        .filter_map(|tool| {
            let count = remaining_counts.entry(tool.clone()).or_insert(0);
            if *count > 0 {
                *count -= 1;
                None
            } else {
                Some(tool.clone())
            }
        })
        .collect()
}

fn required_task_contract_tool_count(
    task_tool_contract: &Option<TaskToolContract>,
    tool_name: &str,
) -> usize {
    task_tool_contract
        .as_ref()
        .map(|contract| {
            contract
                .ordered_tools
                .iter()
                .filter(|tool| tool.as_str() == tool_name)
                .count()
        })
        .unwrap_or_default()
}

fn completed_task_contract_tool_count(
    completed_task_tool_counts: &BTreeMap<String, usize>,
    tool_name: &str,
) -> usize {
    completed_task_tool_counts
        .get(tool_name)
        .copied()
        .unwrap_or_default()
}

fn completed_task_contract_tools(
    contract: &TaskToolContract,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut remaining_counts = completed_task_tool_counts.clone();
    contract
        .ordered_tools
        .iter()
        .filter_map(|tool| {
            let count = remaining_counts.entry(tool.clone()).or_insert(0);
            if *count > 0 {
                *count -= 1;
                Some(tool.clone())
            } else {
                None
            }
        })
        .collect()
}

fn final_answer_after_failed_only_tools_should_fail(
    used_any_tool: bool,
    successful_tool_call_count: u64,
    tool_call_error_count: u32,
    invalid_tool_call_count: u32,
    parse_error_count: u32,
) -> bool {
    used_any_tool
        && successful_tool_call_count == 0
        && (tool_call_error_count > 0 || invalid_tool_call_count > 0 || parse_error_count > 0)
}

fn next_missing_task_contract_tool<'a>(
    contract: &'a TaskToolContract,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> Option<&'a str> {
    next_missing_task_contract_step(contract, completed_task_tool_counts).map(|(_, tool)| tool)
}

fn next_missing_task_contract_step<'a>(
    contract: &'a TaskToolContract,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> Option<(usize, &'a str)> {
    let mut remaining_counts = completed_task_tool_counts.clone();
    contract
        .ordered_tools
        .iter()
        .enumerate()
        .find_map(|(index, tool)| {
            let count = remaining_counts.entry(tool.clone()).or_insert(0);
            if *count > 0 {
                *count -= 1;
                None
            } else {
                Some((index, tool.as_str()))
            }
        })
}

fn task_contract_next_instruction_message(
    contract: &TaskToolContract,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> Option<Value> {
    let (step_index, next_tool) =
        next_missing_task_contract_step(contract, completed_task_tool_counts)?;
    let arguments = contract
        .step_argument_templates
        .get(step_index)
        .and_then(Option::as_ref)
        .cloned()
        .unwrap_or_default();
    let arguments_text =
        serde_json::to_string(&Value::Object(arguments)).unwrap_or_else(|_| "{}".to_owned());
    Some(json!({
        "role": "user",
        "content": format!(
            "NEXT_REQUIRED_FUNCTION\nname: {next_tool}\narguments: {arguments_text}\nReturn exactly one OpenAI tool call for this function now. Any other function name is off-contract and no tool will run."
        ),
    }))
}

fn workspace_put_contract_repetition_should_defer(
    task_tool_contract: &Option<TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
    tool_name: &str,
) -> bool {
    tool_name == "workspace_put"
        && completed_task_contract_tool_count(completed_task_tool_counts, "workspace_put")
            >= required_task_contract_tool_count(task_tool_contract, "workspace_put")
        && !missing_task_contract_tools(task_tool_contract, completed_task_tool_counts).is_empty()
}

fn task_contract_completed_tool_repetition_should_defer(
    task_tool_contract: &Option<TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
    tool_name: &str,
) -> bool {
    let Some(contract) = task_tool_contract else {
        return false;
    };
    contract.allowed_tools.contains(tool_name)
        && completed_task_contract_tool_count(completed_task_tool_counts, tool_name)
            >= required_task_contract_tool_count(task_tool_contract, tool_name)
        && !missing_task_contract_tools(task_tool_contract, completed_task_tool_counts).is_empty()
}

fn task_contract_required_tool_failure_is_terminal(
    contract: Option<&TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
    tool_name: &str,
    args: &JsonObject,
) -> bool {
    let Some(contract) = contract else {
        return false;
    };
    let Some((step_index, next_tool)) =
        next_missing_task_contract_step(contract, completed_task_tool_counts)
    else {
        return false;
    };
    if next_tool != tool_name {
        return false;
    }
    let Some(expected) = contract
        .step_argument_templates
        .get(step_index)
        .and_then(Option::as_ref)
    else {
        return false;
    };
    expected == &contract_failure_comparable_args(tool_name, expected, args)
}

fn contract_failure_comparable_args(
    tool_name: &str,
    expected: &JsonObject,
    args: &JsonObject,
) -> JsonObject {
    let mut comparable = args.clone();
    if tool_name == "agent_ask_operator" && !expected.contains_key("spawn_id") {
        comparable.remove("spawn_id");
    }
    comparable
}

fn task_contract_out_of_order_rejection(
    contract: Option<&TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
    tool_name: &str,
) -> Option<ModelToolCallRejection> {
    let contract = contract?;
    if !contract.allowed_tools.contains(tool_name)
        || completed_task_contract_tool_count(completed_task_tool_counts, tool_name)
            >= required_task_contract_tool_count(&Some(contract.clone()), tool_name)
    {
        return None;
    }
    let remaining = missing_task_contract_tools_for_contract(contract, completed_task_tool_counts);
    let expected = remaining.first()?;
    if expected == tool_name {
        return None;
    }
    Some(ModelToolCallRejection::recoverable(
        format!(
            "{tool_name} is out of order for this exact-tool contract; next required tool is {expected}; remaining order: {}",
            remaining.join(" -> ")
        ),
        "Retry with the next required function from contract_status.next_function and its exact JSON arguments.",
    ))
}

fn verified_workspace_contract_complete(
    task_tool_contract: &Option<TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> bool {
    let Some(contract) = task_tool_contract else {
        return false;
    };
    contract.allowed_tools.len() == 2
        && contract.allowed_tools.contains("workspace_put")
        && contract.allowed_tools.contains("workspace_get")
        && missing_task_contract_tools(task_tool_contract, completed_task_tool_counts).is_empty()
}

fn verified_workspace_checkpoint_contract_complete(
    task_tool_contract: &Option<TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
    successful_workspace_puts: &[JsonObject],
) -> bool {
    let Some(contract) = task_tool_contract else {
        return false;
    };
    contract.allowed_tools.contains("workspace_put")
        && !successful_workspace_puts.is_empty()
        && missing_task_contract_tools(task_tool_contract, completed_task_tool_counts).is_empty()
}

fn final_message_from_successful_workspace_puts(
    successful_workspace_puts: &[JsonObject],
    fallback: &str,
) -> String {
    successful_workspace_puts
        .last()
        .and_then(|args| args.get("value"))
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_else(|| fallback.to_owned())
}

fn task_contract_progress_value(
    task_tool_contract: &Option<TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
    successful_workspace_puts: &[JsonObject],
) -> Option<Value> {
    let contract = task_tool_contract.as_ref()?;
    let missing_tools = missing_task_contract_tools(task_tool_contract, completed_task_tool_counts);
    let all_required_tools_complete = missing_tools.is_empty();
    let completed_tools = completed_task_contract_tools(contract, completed_task_tool_counts)
        .into_iter()
        .map(Value::String)
        .collect::<Vec<_>>();
    let next_step_index = completed_tools.len();
    let mut progress = json!({
        "source": contract.source,
        "allowed_tools": contract.allowed_tools_json(),
        "ordered_tools": contract.ordered_tools_json(),
        "completed_tools": completed_tools,
        "completed_tool_counts": completed_task_tool_counts,
        "missing_tools": missing_tools,
        "all_required_tools_complete": all_required_tools_complete,
    });
    if let Value::Object(map) = &mut progress {
        if let Some(next_tool) = map
            .get("missing_tools")
            .and_then(Value::as_array)
            .and_then(|missing| missing.first())
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        {
            map.insert(
                "suggested_next_tool".to_owned(),
                Value::String(next_tool.clone()),
            );
            if let Some(arguments) = contract
                .step_argument_templates
                .get(next_step_index)
                .and_then(Option::as_ref)
                .map(|args| Value::Object(args.clone()))
            {
                map.insert("suggested_next_arguments".to_owned(), arguments);
                map.insert(
                    "suggestion".to_owned(),
                    Value::String(format!(
                        "Next call {next_tool} with suggested_next_arguments."
                    )),
                );
                return Some(progress);
            }
            if next_tool == "workspace_get" {
                if let Some(arguments) = successful_workspace_puts.last().and_then(|put_args| {
                    workspace_put_readback_plan(put_args)
                        .ok()
                        .map(|plan| Value::Object(plan.arguments))
                }) {
                    map.insert("suggested_next_arguments".to_owned(), arguments);
                    map.insert(
                        "suggestion".to_owned(),
                        Value::String(
                            "Next call workspace_get with suggested_next_arguments; do not call workspace_put again."
                                .to_owned(),
                        ),
                    );
                    return Some(progress);
                }
            }
            map.insert(
                "suggestion".to_owned(),
                Value::String(format!(
                    "Next call {next_tool} with the exact task arguments."
                )),
            );
        } else {
            map.insert(
                "suggestion".to_owned(),
                Value::String(
                    "All exact-tool contract tools are complete; answer with the requested final message."
                        .to_owned(),
                ),
            );
        }
    }
    Some(progress)
}

fn task_contract_model_status_value(
    task_tool_contract: &Option<TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
    successful_workspace_puts: &[JsonObject],
) -> Option<Value> {
    let contract = task_tool_contract.as_ref()?;
    let remaining = missing_task_contract_tools(task_tool_contract, completed_task_tool_counts);
    let completed = completed_task_contract_tools(contract, completed_task_tool_counts)
        .into_iter()
        .map(Value::String)
        .collect::<Vec<_>>();
    let next_step_index = completed.len();
    let all_required_functions_complete = remaining.is_empty();
    let mut status = json!({
        "allowed_function_names": contract.allowed_tools_json(),
        "ordered_function_names": contract.ordered_tools_json(),
        "completed_function_names": completed,
        "completed_function_counts": completed_task_tool_counts,
        "remaining_function_names": remaining,
        "all_required_functions_complete": all_required_functions_complete,
    });
    let Value::Object(map) = &mut status else {
        return Some(status);
    };
    let next_tool = remaining.first().cloned();
    if let Some(next_tool) = next_tool {
        let next_arguments = if let Some(arguments) = contract
            .step_argument_templates
            .get(next_step_index)
            .and_then(Option::as_ref)
            .map(|args| Value::Object(args.clone()))
        {
            arguments
        } else if next_tool == "workspace_get" {
            successful_workspace_puts
                .last()
                .and_then(|put_args| {
                    workspace_put_readback_plan(put_args)
                        .ok()
                        .map(|plan| Value::Object(plan.arguments))
                })
                .unwrap_or_else(|| Value::Object(Map::new()))
        } else {
            Value::Object(Map::new())
        };
        map.insert(
            "next_function".to_owned(),
            json!({
                "name": next_tool,
                "arguments": next_arguments,
            }),
        );
        map.insert(
            "instruction".to_owned(),
            Value::String(format!(
                "Return exactly one function call now: name={next_tool}; arguments=next_function.arguments. Do not call any other name and do not use positional arrays."
            )),
        );
    } else {
        map.insert(
            "instruction".to_owned(),
            Value::String(
                "All required function calls are complete; return the final answer without another tool call."
                    .to_owned(),
            ),
        );
    }
    Some(status)
}

fn strip_model_hidden_contract_fields(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let filtered = map
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "task_contract_progress"
                            | "missing_task_tools"
                            | "completed_task_tool_sources"
                    )
                })
                .map(|(key, value)| (key.clone(), strip_model_hidden_contract_fields(value)))
                .collect::<Map<_, _>>();
            Value::Object(filtered)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(strip_model_hidden_contract_fields)
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn model_tool_result_value(
    result_value: &Value,
    task_tool_contract: &Option<TaskToolContract>,
    completed_task_tool_counts: &BTreeMap<String, usize>,
    successful_workspace_puts: &[JsonObject],
) -> Value {
    let Some(status) = task_contract_model_status_value(
        task_tool_contract,
        completed_task_tool_counts,
        successful_workspace_puts,
    ) else {
        return strip_model_hidden_contract_fields(result_value);
    };
    let mut value = json!({
        "result": strip_model_hidden_contract_fields(result_value),
        "contract_status": status,
    });
    if let Some(status) = value.get("contract_status").cloned() {
        attach_model_contract_status_feedback(&mut value, status);
    }
    value
}

fn attach_model_contract_status_feedback(model_feedback: &mut Value, status: Value) {
    let Value::Object(feedback) = model_feedback else {
        return;
    };
    if let Some(next_function) = status.get("next_function").cloned() {
        let instruction = status
            .get("instruction")
            .cloned()
            .unwrap_or_else(|| Value::String("Call next_function now.".to_owned()));
        feedback.insert("next_function".to_owned(), next_function.clone());
        feedback.insert(
            "call_next".to_owned(),
            json!({
                "function": next_function,
                "instruction": instruction,
            }),
        );
    }
    feedback.insert("contract_status".to_owned(), status);
}

fn task_contract_prompt(contract: &TaskToolContract) -> String {
    let mut prompt = format!(
        "Exact-tool contract active.\n\
Allowed tool names: {}.\n\
Required tool order: {}.\n\
Use only these tool names exactly; a semantically similar tool name is invalid.\n\
For OpenAI tool_calls, function.name must be the exact tool name and function.arguments must be the JSON object; no wrapper and no positional array.\n\
Copy the task's argument keys and values exactly into the allowed tool call.\n\
If workspace_put is allowed and the task gives a value, its arguments must include run_id, key, and value.\n\
Do not add expected_version unless the task explicitly asks to replace an existing row.",
        contract.allowed_tools_display(),
        contract.ordered_tools_display()
    );
    if let Some(guidance) = browser_target_contract_guidance(contract) {
        prompt.push('\n');
        prompt.push_str(&guidance);
    }
    prompt
}

fn browser_target_contract_guidance(contract: &TaskToolContract) -> Option<String> {
    let hints = contract
        .ordered_tools
        .iter()
        .filter_map(|tool| match tool.as_str() {
            "cdp_open_tab" => Some("cdp_open_tab opens the owned Chrome tab"),
            "target_claim" => Some("target_claim claims the returned owned target"),
            "browser_set_value" => Some("browser_set_value changes a field in the owned tab"),
            "target_act" => Some("target_act mutates the owned target/lane"),
            "cdp_target_info" => Some("cdp_target_info reads typed page text/vitals"),
            "browser_evaluate" => Some("browser_evaluate reads back DOM state"),
            "workspace_put" => Some("workspace_put records the verified result"),
            _ => None,
        })
        .collect::<Vec<_>>();
    if hints.is_empty() {
        return None;
    }
    Some(format!(
        "Browser/target route hints: {}. Any unlisted status, probe, control, or semantically similar tool name fails this contract.",
        hints.join("; ")
    ))
}

async fn readback_workspace_put(
    peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
    put_args: &JsonObject,
) -> anyhow::Result<Value> {
    let plan = workspace_put_readback_plan(put_args)?;
    let readback = call_mcp_tool_json(peer, "workspace_get", plan.arguments.clone()).await?;
    workspace_put_readback_record(&plan, &readback)
}

fn workspace_put_readback_plan(put_args: &JsonObject) -> anyhow::Result<WorkspacePutReadbackPlan> {
    let key = put_args
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("WORKSPACE_PUT_READBACK_ARGS_INVALID: missing key"))?;
    let expected_value = put_args
        .get("value")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("WORKSPACE_PUT_READBACK_ARGS_INVALID: missing value"))?;
    let mut arguments = Map::new();
    arguments.insert("key".to_owned(), Value::String(key.to_owned()));
    if let Some(run_id) = put_args.get("run_id").and_then(Value::as_str) {
        arguments.insert("run_id".to_owned(), Value::String(run_id.to_owned()));
    }
    Ok(WorkspacePutReadbackPlan {
        arguments,
        expected_value,
    })
}

fn workspace_put_args_match(left: &JsonObject, right: &JsonObject) -> bool {
    left == right
}

fn workspace_put_readback_record(
    plan: &WorkspacePutReadbackPlan,
    readback: &Value,
) -> anyhow::Result<Value> {
    let actual_value = readback
        .get("entry")
        .and_then(|entry| entry.get("value"))
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("WORKSPACE_PUT_READBACK_INVALID: workspace_get missing entry.value")
        })?;
    if actual_value != plan.expected_value {
        bail!(
            "WORKSPACE_PUT_READBACK_MISMATCH: expected {}, got {}",
            plan.expected_value,
            actual_value
        );
    }
    Ok(json!({
        "tool": "workspace_get",
        "arguments": plan.arguments.clone(),
        "expected_value": plan.expected_value.clone(),
        "actual_value": actual_value,
        "matched": true,
        "storage_readback": readback.get("storage_readback").cloned().unwrap_or(Value::Null),
    }))
}

fn attach_workspace_put_readback(mut result_value: Value, readback: Value) -> Value {
    match &mut result_value {
        Value::Object(map) => {
            map.insert("post_write_readback".to_owned(), readback);
            result_value
        }
        _ => json!({
            "tool_result": result_value,
            "post_write_readback": readback,
        }),
    }
}

fn tool_result_value(result: &rmcp::model::CallToolResult) -> Value {
    if let Some(value) = &result.structured_content {
        return value.clone();
    }
    let text = result
        .content
        .iter()
        .filter_map(content_text)
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    }
}

fn content_text(content: &Content) -> Option<String> {
    content.as_text().map(|text| text.text.clone())
}

fn openai_tool_from_mcp(tool: &Tool) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name.as_ref(),
            "description": tool.description.as_ref().map(|desc| desc.as_ref()).unwrap_or("Synapse MCP tool"),
            "parameters": Value::Object((*tool.input_schema).clone()),
        }
    })
}

fn routed_harness_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": SYNAPSE_TOOL_CATALOG,
                "description": "Read the live Synapse MCP tool catalog loaded by this agent. Use name for an exact tool or query to search names/descriptions.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Exact Synapse MCP tool name to inspect."
                        },
                        "query": {
                            "type": "string",
                            "description": "Case-insensitive search across Synapse MCP tool names and descriptions."
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 200,
                            "description": "Maximum catalog rows to return."
                        }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": SYNAPSE_TOOL_CALL,
                "description": "Call any real Synapse MCP tool by name with a JSON object of arguments. The target tool must come from synapse_tool_catalog.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Real Synapse MCP tool name to call."
                        },
                        "arguments": {
                            "type": "object",
                            "description": "JSON object passed to that Synapse MCP tool.",
                            "additionalProperties": true
                        }
                    },
                    "required": ["name", "arguments"]
                }
            }
        }),
    ]
}

fn openai_tools_for_exposure(
    tool_exposure: ToolExposure,
    tools: &[Tool],
    task_tool_contract: Option<&TaskToolContract>,
    registry: &LocalModelRegistryRow,
) -> anyhow::Result<Vec<Value>> {
    match tool_exposure {
        ToolExposure::Direct => Ok(tools.iter().map(openai_tool_from_mcp).collect::<Vec<_>>()),
        ToolExposure::Routed => Ok(routed_harness_tools()),
        ToolExposure::Internalized => {
            contract_tools_for_internalized(tools, task_tool_contract, registry)
        }
    }
}

fn contract_tools_for_internalized(
    tools: &[Tool],
    task_tool_contract: Option<&TaskToolContract>,
    registry: &LocalModelRegistryRow,
) -> anyhow::Result<Vec<Value>> {
    let Some(contract) = task_tool_contract else {
        return Ok(Vec::new());
    };
    if let Some(max_tools) = registry.max_tools {
        if contract.ordered_tools.len() > max_tools {
            bail!(
                "MODEL_TOOLS_UNSUPPORTED: exact-tool contract needs {} tools but registry max_tools is {}",
                contract.ordered_tools.len(),
                max_tools
            );
        }
    }
    contract
        .ordered_tools
        .iter()
        .map(|name| {
            openai_tool_by_name(tools, name).with_context(|| {
                format!("MODEL_TOOLS_UNSUPPORTED: contract tool {name} missing from tools/list")
            })
        })
        .collect()
}

fn openai_tool_by_name(tools: &[Tool], name: &str) -> Option<Value> {
    tools
        .iter()
        .find(|tool| tool.name.as_ref() == name)
        .map(openai_tool_from_mcp)
}

fn exact_contract_turn_tools(
    tools: &[Tool],
    contract: &TaskToolContract,
    completed_task_tool_counts: &BTreeMap<String, usize>,
) -> anyhow::Result<TurnToolSelection> {
    let Some(next_tool) = next_missing_task_contract_tool(contract, completed_task_tool_counts)
    else {
        return Ok(TurnToolSelection {
            tools: Vec::new(),
            tool_choice: None,
        });
    };
    let tool = openai_tool_by_name(tools, next_tool).with_context(|| {
        format!("MODEL_TOOLS_UNSUPPORTED: contract tool {next_tool} missing from tools/list")
    })?;
    Ok(TurnToolSelection {
        tools: vec![tool],
        tool_choice: Some(json!({
            "type": "function",
            "function": {"name": next_tool},
        })),
    })
}

fn resolve_tool_exposure(row: &LocalModelRegistryRow, tool_count: usize) -> ToolExposure {
    // Internalized models carry the surface in their weights — never inject a
    // catalog, regardless of max_tools. This takes precedence over the routed
    // fallback (which exists only for providers that cap tool count).
    if registry_runtime_preset(row) == "internalized_no_catalog" {
        return ToolExposure::Internalized;
    }
    match row.max_tools {
        Some(max_tools) if tool_count > max_tools => ToolExposure::Routed,
        _ => ToolExposure::Direct,
    }
}

/// Classifies an rmcp `call_tool` failure as terminal (the transport is gone,
/// so every subsequent call would fail identically) versus recoverable (the
/// server responded with an error, timed out, or returned an unexpected shape —
/// the model can be told and can try again). A recoverable error is fed back to
/// the model instead of crashing the agent.
fn tool_call_error_is_terminal(error: &ServiceError) -> bool {
    matches!(
        error,
        ServiceError::TransportClosed
            | ServiceError::TransportSend(_)
            | ServiceError::Cancelled { .. }
    )
}

fn parse_routed_tool_call(raw: &str) -> anyhow::Result<(String, JsonObject)> {
    let mut args = parse_tool_arguments(raw)?;
    let name = args
        .remove("name")
        .and_then(|value| value.as_str().map(str::trim).map(ToOwned::to_owned))
        .filter(|value| !value.is_empty())
        .context("routed tool call requires non-empty name")?;
    let arguments = match args.remove("arguments") {
        Some(Value::Object(map)) => map,
        Some(Value::String(raw)) => {
            let value: Value =
                serde_json::from_str(&raw).context("routed tool arguments string is not JSON")?;
            value
                .as_object()
                .cloned()
                .context("routed tool arguments string must decode to a JSON object")?
        }
        Some(other) => {
            bail!("routed tool arguments must be a JSON object, got {other}");
        }
        None => Map::new(),
    };
    Ok((name, arguments))
}

fn parse_internalized_tool_call_envelope(raw: &str) -> anyhow::Result<(String, JsonObject)> {
    let mut envelope = parse_tool_arguments(raw)?;
    let name = envelope
        .remove("name")
        .or_else(|| envelope.remove("tool_name"))
        .or_else(|| envelope.remove("function"))
        .or_else(|| envelope.remove("function_name"))
        .and_then(|value| value.as_str().map(str::trim).map(ToOwned::to_owned))
        .filter(|value| !value.is_empty())
        .context("internalized tool_call envelope requires non-empty tool_name")?;
    let arguments = match envelope
        .remove("arguments")
        .or_else(|| envelope.remove("args"))
    {
        Some(Value::Object(map)) => map,
        Some(Value::String(raw)) => {
            let value: Value = serde_json::from_str(&raw)
                .context("internalized tool_call arguments string is not JSON")?;
            value
                .as_object()
                .cloned()
                .context("internalized tool_call arguments string must decode to a JSON object")?
        }
        Some(Value::Array(_)) => bail!(
            "internalized tool_call args must be a JSON object with exact argument keys; positional arrays are ambiguous and rejected fail-closed"
        ),
        Some(other) => bail!("internalized tool_call arguments must be a JSON object, got {other}"),
        None => Map::new(),
    };
    Ok((name, arguments))
}

fn parse_act_call_tool_call_envelope(raw: &str) -> anyhow::Result<(String, JsonObject)> {
    let mut envelope = parse_tool_arguments(raw)?;
    let name = envelope
        .remove("tool_name")
        .and_then(|value| value.as_str().map(str::trim).map(ToOwned::to_owned))
        .filter(|value| !value.is_empty())
        .context("act_call envelope requires non-empty tool_name")?;
    let arguments = match envelope
        .remove("arguments")
        .or_else(|| envelope.remove("args"))
    {
        Some(value) => {
            if !envelope.is_empty() {
                bail!(
                    "act_call envelope must not mix args/arguments with flattened delegated keys"
                );
            }
            tool_argument_value_to_object(value, "act_call envelope arguments")?
        }
        None => envelope,
    };
    Ok((name, arguments))
}

fn tool_argument_value_to_object(value: Value, context: &str) -> anyhow::Result<JsonObject> {
    match value {
        Value::Object(map) => Ok(map),
        Value::String(raw) => {
            let value: Value = serde_json::from_str(&raw)
                .with_context(|| format!("{context} string is not JSON"))?;
            value
                .as_object()
                .cloned()
                .with_context(|| format!("{context} string must decode to a JSON object"))
        }
        Value::Array(_) => bail!(
            "{context} must be a JSON object with exact argument keys; positional arrays are ambiguous and rejected fail-closed"
        ),
        other => bail!("{context} must be a JSON object, got {other}"),
    }
}

fn resolve_task(task: Option<&str>, task_file: Option<&PathBuf>) -> anyhow::Result<String> {
    if let Some(task) = task.filter(|value| !value.trim().is_empty()) {
        return Ok(task.to_owned());
    }
    if let Some(path) = task_file {
        let task = std::fs::read_to_string(path)
            .with_context(|| format!("read task file {}", path.display()))?;
        if task.trim().is_empty() {
            bail!("TOOL_PARAMS_INVALID: task file {} is empty", path.display());
        }
        return Ok(task);
    }
    bail!("TOOL_PARAMS_INVALID: --local-agent-task or --local-agent-task-file is required")
}

fn non_empty(value: Option<&str>, detail: &str) -> anyhow::Result<String> {
    value
        .filter(|text| !text.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!(detail.to_owned()))
}

fn validate_spawn_id(spawn_id: &str) -> anyhow::Result<()> {
    if !spawn_id.starts_with("agent-spawn-") {
        bail!("TOOL_PARAMS_INVALID: spawn id must start with agent-spawn-");
    }
    if spawn_id.len() > 128 {
        bail!("TOOL_PARAMS_INVALID: spawn id exceeds 128 chars");
    }
    if !spawn_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        bail!("TOOL_PARAMS_INVALID: spawn id must contain only ASCII alphanumerics and dashes");
    }
    Ok(())
}

fn validate_registry_row(row: &LocalModelRegistryRow) -> anyhow::Result<()> {
    if !row.enabled {
        bail!(
            "LOCAL_MODEL_DISABLED: registry row {:?} is disabled",
            row.name
        );
    }
    if row.api_shape != "open_ai_chat_completions" {
        bail!(
            "LOCAL_MODEL_API_SHAPE_UNSUPPORTED: {:?} is not open_ai_chat_completions",
            row.api_shape
        );
    }
    let Some(probe) = &row.last_probe else {
        bail!(
            "LOCAL_MODEL_UNPROBED: registry row {:?} has no last_probe",
            row.name
        );
    };
    if !probe.healthy {
        bail!(
            "LOCAL_MODEL_UNHEALTHY: {:?}: {} {}",
            row.name,
            probe.error_code.as_deref().unwrap_or("unknown"),
            probe.error_detail.as_deref().unwrap_or("")
        );
    }
    let probe_age_ms = unix_time_ms_now().saturating_sub(probe.observed_at_unix_ms);
    if probe_age_ms > LOCAL_MODEL_SPAWN_MAX_PROBE_AGE_MS {
        bail!(
            "LOCAL_MODEL_PROBE_STALE: registry row {:?} last healthy probe age {}ms exceeds {}ms; run local_model_probe after verifying endpoint process/socket SoT",
            row.name,
            probe_age_ms,
            LOCAL_MODEL_SPAWN_MAX_PROBE_AGE_MS
        );
    }
    Ok(())
}

fn registry_runtime_preset(row: &LocalModelRegistryRow) -> &str {
    row.runtime_preset
        .as_deref()
        .unwrap_or("open_ai_compatible")
}

fn should_stream(cli_no_stream: bool, tool_exposure: ToolExposure) -> bool {
    !cli_no_stream && tool_exposure != ToolExposure::Internalized
}

fn apply_runtime_preset(row: &LocalModelRegistryRow, body: &mut Value) {
    match registry_runtime_preset(row) {
        "deepseek_v4_flash_non_thinking" => {
            body["thinking"] = json!({ "type": "disabled" });
        }
        "deepseek_v4_reasoning" => {
            body["thinking"] = json!({ "type": "enabled" });
            body["reasoning_effort"] = json!("max");
            if let Some(object) = body.as_object_mut() {
                object.remove("tool_choice");
            }
        }
        "internalized_no_catalog" => {
            // Surface is in the weights: send NO full catalog. Exact-tool
            // contract runs may still carry a bounded task-local tool list; it
            // keeps function-name selection grounded without reintroducing the
            // huge catalog that internalization exists to avoid.
            if let Some(object) = body.as_object_mut() {
                let has_bounded_tools = object
                    .get("tools")
                    .and_then(Value::as_array)
                    .is_some_and(|tools| !tools.is_empty());
                if !has_bounded_tools {
                    object.remove("tools");
                    object.remove("tool_choice");
                }
            }
        }
        _ => {}
    }
}

fn chat_completions_endpoint(
    row: &LocalModelRegistryRow,
    allow_non_loopback: bool,
) -> anyhow::Result<Url> {
    let mut url = Url::parse(&row.base_url)
        .with_context(|| format!("LOCAL_MODEL_ENDPOINT_INVALID: {:?}", row.base_url))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        bail!("LOCAL_MODEL_ENDPOINT_INVALID: endpoint scheme must be http or https");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("LOCAL_MODEL_ENDPOINT_INVALID: endpoint must not contain query or fragment");
    }
    if !(row.allow_non_loopback || allow_non_loopback || is_loopback_url(&url)) {
        bail!(
            "LOCAL_MODEL_ENDPOINT_NON_LOOPBACK: non-loopback endpoints require explicit allowance"
        );
    }
    let path = url.path().trim_end_matches('/');
    let next = if path.ends_with("/chat/completions") {
        path.to_owned()
    } else if path.is_empty() || path == "/" {
        "/v1/chat/completions".to_owned()
    } else {
        format!("{path}/chat/completions")
    };
    url.set_path(&next);
    Ok(url)
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|addr| addr.is_loopback()),
        None => false,
    }
}

fn agent_event_url(mcp_url: &str, spawn_id: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(mcp_url).context("parse local-agent MCP URL")?;
    url.set_path("/agent-events");
    url.set_query(Some(&format!(
        "spawn_id={spawn_id}&source=local_model_runner"
    )));
    Ok(url)
}

fn parse_tool_arguments(raw: &str) -> anyhow::Result<JsonObject> {
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(raw)?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("tool arguments must be a JSON object"))
}

fn string_json_or_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

fn assistant_message(completion: &ChatCompletion) -> Value {
    if completion.tool_calls.is_empty() {
        json!({
            "role": "assistant",
            "content": completion.content,
        })
    } else {
        let tool_calls = completion
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments,
                    }
                })
            })
            .collect::<Vec<_>>();
        json!({
            "role": "assistant",
            "content": completion.content,
            "tool_calls": tool_calls,
        })
    }
}

fn parse_non_stream_response(text: &str) -> anyhow::Result<ChatCompletion> {
    let value: Value = serde_json::from_str(text)
        .with_context(|| format!("MODEL_RESPONSE_INVALID_JSON: {}", bounded_text(text, 4000)))?;
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| anyhow::anyhow!("MODEL_RESPONSE_INVALID: missing choices[0]"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| anyhow::anyhow!("MODEL_RESPONSE_INVALID: missing choices[0].message"))?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.iter().filter_map(parse_tool_call_value).collect())
        .unwrap_or_default();
    Ok(ChatCompletion {
        content,
        tool_calls,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        usage: value.get("usage").and_then(Usage::from_value),
        raw_sha256: sha256_hex(text.as_bytes()),
    })
}

fn parse_tool_call_value(value: &Value) -> Option<OpenAiToolCall> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("call_missing_id")
        .to_owned();
    let function = value.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?.to_owned();
    let arguments = match function.get("arguments") {
        Some(Value::String(text)) => text.to_owned(),
        Some(value) => value.to_string(),
        None => "{}".to_owned(),
    };
    Some(OpenAiToolCall {
        id,
        name,
        arguments,
    })
}

async fn parse_streaming_response(response: reqwest::Response) -> anyhow::Result<ChatCompletion> {
    let mut stream = response.bytes_stream();
    let mut pending = String::new();
    let mut raw = Vec::new();
    let mut content = String::new();
    let mut calls: BTreeMap<u64, StreamingToolCall> = BTreeMap::new();
    let mut usage = None;
    let mut finish_reason = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read streaming response chunk")?;
        raw.extend_from_slice(&chunk);
        pending.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = pending.find('\n') {
            let mut line = pending[..index].trim_end_matches('\r').to_owned();
            pending = pending[index + 1..].to_owned();
            line = line.trim().to_owned();
            if !line.starts_with("data:") {
                continue;
            }
            let data = line.trim_start_matches("data:").trim();
            if data == "[DONE]" || data.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(data)
                .with_context(|| format!("MODEL_STREAM_CHUNK_INVALID_JSON: {data}"))?;
            if let Some(next_usage) = value.get("usage").and_then(Usage::from_value) {
                usage = Some(next_usage);
            }
            let Some(choice) = value
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
            else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                finish_reason = Some(reason.to_owned());
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(piece) = delta.get("content").and_then(Value::as_str) {
                content.push_str(piece);
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for value in tool_calls {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let entry = calls.entry(index).or_default();
                    if let Some(id) = value.get("id").and_then(Value::as_str) {
                        entry.id = Some(id.to_owned());
                    }
                    if let Some(function) = value.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            entry.name = Some(name.to_owned());
                        }
                        match function.get("arguments") {
                            Some(Value::String(part)) => entry.arguments.push_str(part),
                            Some(other) => entry.arguments.push_str(&other.to_string()),
                            None => {}
                        }
                    }
                }
            }
        }
    }
    let tool_calls = calls
        .into_iter()
        .map(|(index, call)| OpenAiToolCall {
            id: call.id.unwrap_or_else(|| format!("call_{index}")),
            name: call.name.unwrap_or_default(),
            arguments: call.arguments,
        })
        .filter(|call| !call.name.is_empty())
        .collect::<Vec<_>>();
    Ok(ChatCompletion {
        content,
        tool_calls,
        finish_reason,
        usage,
        raw_sha256: sha256_hex(&raw),
    })
}

fn bounded_result_text(value: &Value) -> String {
    bounded_text(&value.to_string(), 16_000)
}

#[derive(Clone, Debug, PartialEq)]
struct ShutdownMailboxMessage {
    message_id: String,
    kind: String,
    payload: Value,
}

fn shutdown_message_from_tool_result(
    tool_name: &str,
    result_value: &Value,
) -> Option<ShutdownMailboxMessage> {
    for message in mailbox_messages_from_tool_result(tool_name, result_value) {
        let kind = message
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("message")
            .trim()
            .to_owned();
        let payload = message.get("payload").cloned().unwrap_or(Value::Null);
        if steering_requests_shutdown(&kind, &payload) {
            let message_id = message
                .get("message_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            return Some(ShutdownMailboxMessage {
                message_id,
                kind,
                payload,
            });
        }
    }
    None
}

fn mailbox_messages_from_tool_result<'a>(
    tool_name: &str,
    result_value: &'a Value,
) -> Vec<&'a Value> {
    match tool_name {
        "agent_inbox" => result_value
            .get("messages")
            .and_then(Value::as_array)
            .map(|messages| messages.iter().collect())
            .unwrap_or_default(),
        "agent_wait" => result_value
            .get("inbox")
            .and_then(|inbox| inbox.get("messages"))
            .and_then(Value::as_array)
            .map(|messages| messages.iter().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_owned()
    } else {
        value.chars().take(max_chars).collect::<String>()
    }
}

fn steering_payload_text(payload: &Value) -> String {
    if let Some(text) = payload.as_str() {
        return text.to_owned();
    }
    for field in [
        "text",
        "message",
        "content",
        "prompt",
        "instruction",
        "instructions",
        "body",
        "command",
    ] {
        if let Some(text) = payload.get(field).and_then(Value::as_str) {
            return text.to_owned();
        }
    }
    payload.to_string()
}

fn steering_requests_shutdown(kind: &str, payload: &Value) -> bool {
    let kind = kind.trim().to_ascii_lowercase();
    if matches!(
        kind.as_str(),
        "kill" | "stop" | "cancel" | "interrupt" | "shutdown"
    ) {
        return true;
    }
    for field in ["command", "action", "control"] {
        if let Some(value) = payload.get(field).and_then(Value::as_str) {
            let value = value.trim().to_ascii_lowercase();
            if matches!(
                value.as_str(),
                "kill" | "stop" | "cancel" | "interrupt" | "shutdown"
            ) {
                return true;
            }
        }
    }
    false
}

fn write_json_file(path: PathBuf, value: &Value) -> anyhow::Result<()> {
    let encoded = serde_json::to_vec_pretty(value).context("serialize local-agent JSON file")?;
    std::fs::write(&path, encoded).with_context(|| format!("write {}", path.display()))
}

fn local_agent_spawn_root_dir() -> anyhow::Result<PathBuf> {
    let local_appdata = std::env::var_os("LOCALAPPDATA")
        .context("LOCAL_MODEL_AGENT_LOG_ROOT_UNAVAILABLE: LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(local_appdata)
        .join("Synapse")
        .join("agent-spawns"))
}

/// Behavioral doctrine prepended to every spawned agent's conversation (the
/// system message). Compressed per `docs/compressionprompt.md`: the load-bearing
/// constraints and the routed meta-tool names are kept verbatim; meta-framing,
/// restatement, and why-prose are cut; imperative present tense; one rule per
/// line; controlled vocabulary ("tool", not "function/MCP tool" interchangeably).
///
/// Tool *awareness* is intentionally NOT injected here: spawned local models
/// carry the Synapse surface in their LoRA'd weights, and API models receive the
/// full tool schemas from the MCP attachment (Direct exposure). The manifest
/// approach is therefore retired; this prompt only governs behavior.
fn system_prompt(tool_exposure: ToolExposure, tools: &[Tool]) -> String {
    const BASE: &str = "Synapse agent. Use attached MCP tools to inspect/change state.\nRules:\n- Never invent tool results.\n- Stored artifact -> read back before success.\n- post_write_readback.matched=true => success; do not repeat write.\n- Summarize after required tools succeed.";
    const INTERNALIZED_BASE: &str = "Synapse agent. Return one OpenAI tool_call and no prose when a tool is needed.\nRules:\n- function.name = exact task-requested Synapse tool name.\n- function.arguments = JSON object string with exact argument keys/values; never a positional array.\n- Do not use wrapper, probe, control, status, retry, or reconcile calls unless task names them.\n- workspace_put requires key and value or artifact; do not add expected_version unless task says replace.\n- Never invent tool results.\n- Stored artifact -> read back before success.\n- post_write_readback.matched=true => success; do not repeat write.";
    match tool_exposure {
        // Direct: the model holds every tool schema natively, so it just needs
        // to know they are there and callable by name.
        ToolExposure::Direct => format!(
            "{BASE}\n\nEvery Synapse tool is attached directly; call any by name and read its schema from your tool list."
        ),
        // Routed: a capped provider sees only the two meta-tools, so the
        // indirection mechanism is load-bearing and stated exactly.
        ToolExposure::Routed => {
            let names = tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{BASE}\n\nYour provider caps tool count below the full Synapse surface, so tools route through two meta-tools:\n- {SYNAPSE_TOOL_CATALOG} — list/inspect tools (name=<tool> for one, query=<text> to search).\n- {SYNAPSE_TOOL_CALL} {{name, arguments}} — execute any real Synapse tool.\nThis reaches every Synapse tool (perception, action, agent, task, dashboard, local-model). Tools: {names}",
            )
        }
        // Internalized: the surface is in the weights and the model was trained
        // on bare user turns — keep the prompt to behavioral rules only, with no
        // tool framing, to stay on-distribution and near-empty.
        ToolExposure::Internalized => INTERNALIZED_BASE.to_string(),
    }
}

/// One-line, exposure-aware recovery hint fed back to the model when a tool call
/// fails. Compressed (imperative, no meta) and *correct per exposure*: only
/// Routed sessions actually have `synapse_tool_catalog`, so Direct (API-model)
/// sessions must not be told to use a tool that is not attached.
fn tool_failure_suggestion(fatal: bool, exposure: ToolExposure) -> &'static str {
    if fatal {
        return "Connection to the Synapse tool server was lost; the run is ending.";
    }
    match exposure {
        ToolExposure::Routed => {
            "Tool call failed. Read the message, re-check the tool's schema with synapse_tool_catalog, then retry with corrected arguments or pick a different tool."
        }
        // Direct and Internalized both carry the schemas (attached vs. in
        // weights); neither has the routed catalog meta-tool to consult.
        ToolExposure::Direct | ToolExposure::Internalized => {
            "Tool call failed. Read the message, re-check the tool's input schema, then retry with corrected arguments or pick a different tool."
        }
    }
}

/// Local-model dispatch verdict. Production currently only returns `Allow`;
/// `Deny` remains for parser/unit coverage of legacy approval verdicts.
#[allow(dead_code)]
enum ToolGate {
    /// Approved (by the operator or by auto-allow). Carries the EFFECTIVE
    /// arguments — the operator's edited args when they approved-with-edits
    /// (#1030), otherwise the original input.
    Allow(JsonObject),
    /// Denied (operator declined, gate timed out, or the gate could not answer).
    /// Carries the reason fed back to the model; the tool is NOT dispatched.
    Deny(String),
}

/// The tool-name form the harness hands the approval policy/gate so a bare
/// Synapse tool name (e.g. `act_type`) classifies as the MCP tool it is
/// (`mcp__synapse__act_type`) — otherwise `permission_policy::classify` would
/// hit its unknown-bare-name branch and gate every call indiscriminately.
fn gate_tool_label(tool_name: &str) -> String {
    format!("mcp__synapse__{tool_name}")
}

/// Parse the `approval_gate` verdict value (`{"behavior":"allow","updatedInput":…}`
/// or `{"behavior":"deny","message":…}`). Fail-closed: an unknown/!object/missing
/// behavior is an error the caller turns into a denial — never a silent allow.
#[cfg(test)]
fn parse_gate_verdict(verdict: &Value, fallback: &JsonObject) -> anyhow::Result<ToolGate> {
    match verdict.get("behavior").and_then(Value::as_str) {
        Some("allow") => {
            let args = match verdict.get("updatedInput") {
                Some(Value::Object(map)) => map.clone(),
                None | Some(Value::Null) => fallback.clone(),
                Some(other) => {
                    bail!("approval_gate updatedInput must be a JSON object, got {other}")
                }
            };
            Ok(ToolGate::Allow(args))
        }
        Some("deny") => Ok(ToolGate::Deny(
            verdict
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("denied by operator")
                .to_owned(),
        )),
        other => bail!("approval_gate returned an unknown behavior {other:?}: {verdict}"),
    }
}

fn local_agent_active_elapsed(started: Instant, approval_wait_elapsed: Duration) -> Duration {
    started.elapsed().saturating_sub(approval_wait_elapsed)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SynapseToolCallFailureClass {
    error_code: String,
    error_code_source: &'static str,
    mcp_error: Option<Value>,
}

fn classify_synapse_tool_call_failure(
    error: &ServiceError,
    exact_required_tool_failed: bool,
) -> SynapseToolCallFailureClass {
    if let ServiceError::McpError(mcp_error) = error {
        if let Some(code) = mcp_error_data_code(mcp_error) {
            return SynapseToolCallFailureClass {
                error_code: code.to_owned(),
                error_code_source: "mcp_error.data.code",
                mcp_error: Some(mcp_error_value(mcp_error)),
            };
        }
        return SynapseToolCallFailureClass {
            error_code: "SYNAPSE_TOOL_CALL_FAILED".to_owned(),
            error_code_source: "mcp_error_without_data_code",
            mcp_error: Some(mcp_error_value(mcp_error)),
        };
    }

    if exact_required_tool_failed {
        return SynapseToolCallFailureClass {
            error_code: "MODEL_TASK_REQUIRED_TOOL_FAILED".to_owned(),
            error_code_source: "task_contract_required_tool",
            mcp_error: None,
        };
    }

    SynapseToolCallFailureClass {
        error_code: "SYNAPSE_TOOL_CALL_FAILED".to_owned(),
        error_code_source: "service_error",
        mcp_error: None,
    }
}

fn mcp_error_data_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .filter(|code| is_low_cardinality_error_code(code))
}

fn is_low_cardinality_error_code(code: &str) -> bool {
    !code.is_empty()
        && code.chars().count() <= synapse_core::AGENT_EVENT_MAX_REASON_CHARS
        && code
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn mcp_error_value(error: &ErrorData) -> Value {
    json!({
        "jsonrpc_code": error.code.0,
        "message": error.message.as_ref(),
        "data": error.data,
    })
}

fn error_code_from_detail(detail: &str) -> &str {
    for code in [
        "MODEL_ENDPOINT_UNREACHABLE",
        "MODEL_TOOLS_UNSUPPORTED",
        "MODEL_EMPTY_COMPLETION",
        "MODEL_TOOL_CALL_INVALID",
        "MODEL_TASK_REQUIRED_TOOL_FAILED",
        "LOCAL_AGENT_TIMEOUT",
        "LOCAL_AGENT_CONTEXT_OVERFLOW",
        "LOCAL_AGENT_INTERRUPTED",
        "LOCAL_AGENT_TURN_LIMIT",
        "AGENT_EVENT_INGRESS_WRITE_FAILED",
        "LOCAL_MODEL_UNHEALTHY",
        "LOCAL_MODEL_DISABLED",
        "LOCAL_MODEL_UNPROBED",
        "LOCAL_MODEL_PROBE_STALE",
        "LOCAL_MODEL_ENDPOINT_NON_LOOPBACK",
        "SYNAPSE_TOOL_CALL_FAILED",
        "TOOL_PARAMS_INVALID",
    ] {
        if detail.contains(code) {
            return code;
        }
    }
    "LOCAL_MODEL_AGENT_FAILED"
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn unix_time_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use reqwest::header;

    use super::*;

    fn completed_counts<const N: usize>(tools: [&str; N]) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for tool in tools {
            *counts.entry(tool.to_owned()).or_insert(0) += 1;
        }
        counts
    }

    #[test]
    fn agent_wait_tool_result_kill_is_runner_control() {
        let result = json!({
            "ok": true,
            "timed_out": false,
            "inbox": {
                "messages": [{
                    "message_id": "agentmsg-1",
                    "kind": "kill",
                    "payload": { "reason": "operator acceptance" }
                }]
            }
        });

        assert_eq!(
            shutdown_message_from_tool_result("agent_wait", &result),
            Some(ShutdownMailboxMessage {
                message_id: "agentmsg-1".to_owned(),
                kind: "kill".to_owned(),
                payload: json!({ "reason": "operator acceptance" }),
            })
        );
    }

    #[test]
    fn agent_wait_tool_result_normal_steer_stays_model_visible() {
        let result = json!({
            "ok": true,
            "timed_out": false,
            "inbox": {
                "messages": [{
                    "message_id": "agentmsg-2",
                    "kind": "steer",
                    "payload": { "instruction": "write the marker row" }
                }]
            }
        });

        assert_eq!(
            shutdown_message_from_tool_result("agent_wait", &result),
            None
        );
    }

    #[test]
    fn tool_call_server_error_is_recoverable_not_terminal() {
        // The paint-death case: gemma called capture_screenshot without the
        // required `path`, so the server answered with a JSON-RPC error
        // (-32099 / invalid params). That is an McpError — the transport is
        // fine and the model can be told and retry. It MUST NOT be terminal.
        let server_error = ServiceError::McpError(rmcp::model::ErrorData::invalid_params(
            "missing field `path`",
            None,
        ));
        assert!(
            !tool_call_error_is_terminal(&server_error),
            "a server-side invalid-params error is recoverable; the agent must feed it back and continue"
        );

        // Timeouts and unexpected responses are also recoverable retries.
        assert!(!tool_call_error_is_terminal(&ServiceError::Timeout {
            timeout: std::time::Duration::from_millis(1),
        }));
        assert!(!tool_call_error_is_terminal(
            &ServiceError::UnexpectedResponse
        ));
    }

    #[test]
    fn tool_call_transport_loss_is_terminal() {
        // A dead pipe cannot recover; every further call would fail identically.
        assert!(tool_call_error_is_terminal(&ServiceError::TransportClosed));
        assert!(tool_call_error_is_terminal(&ServiceError::Cancelled {
            reason: Some("peer gone".to_owned()),
        }));
    }

    #[test]
    fn routed_tool_call_parses_real_tool_name_and_arguments() -> anyhow::Result<()> {
        let (name, args) = parse_routed_tool_call(
            r#"{"name":"workspace_put","arguments":{"run_id":"issue985","key":"ok","value":{"actual":true}}}"#,
        )?;
        assert_eq!(name, "workspace_put");
        assert_eq!(args["run_id"], "issue985");
        assert_eq!(args["value"]["actual"], true);
        Ok(())
    }

    #[test]
    fn internalized_tool_call_envelope_parses_only_object_arguments() -> anyhow::Result<()> {
        let (name, args) = parse_internalized_tool_call_envelope(
            r##"{"tool_name":"cdp_open_tab","args":{"url":"http://127.0.0.1:8896/fixture.html","window_hwnd":2427400}}"##,
        )?;
        assert_eq!(name, "cdp_open_tab");
        assert_eq!(args["url"], "http://127.0.0.1:8896/fixture.html");
        assert_eq!(args["window_hwnd"], 2427400);

        let (name, args) = parse_internalized_tool_call_envelope(
            r#"{"function":"target_claim","args":{"ttl_ms":120000}}"#,
        )?;
        assert_eq!(name, "target_claim");
        assert_eq!(args["ttl_ms"], 120000);

        let (name, args) = parse_internalized_tool_call_envelope(
            r#"{"function_name":"target_claim","args":{"ttl_ms":120000}}"#,
        )?;
        assert_eq!(name, "target_claim");
        assert_eq!(args["ttl_ms"], 120000);

        let array = parse_internalized_tool_call_envelope(
            r#"{"tool_name":"cdp_open_tab","args":["http://127.0.0.1:8896/fixture.html","2427400"]}"#,
        )
        .expect_err("positional args are ambiguous and must fail closed");
        assert!(array.to_string().contains("positional arrays"));
        Ok(())
    }

    #[test]
    fn act_call_tool_call_envelope_parses_flattened_and_nested_arguments() -> anyhow::Result<()> {
        let (name, args) = parse_act_call_tool_call_envelope(
            r#"{"tool_name":"synapse_probe","nonce":"probe-123"}"#,
        )?;
        assert_eq!(name, "synapse_probe");
        assert_eq!(args["nonce"], "probe-123");

        let (name, args) = parse_act_call_tool_call_envelope(
            r#"{"tool_name":"workspace_get","args":{"run_id":"issue1265","key":"safe-shell"}}"#,
        )?;
        assert_eq!(name, "workspace_get");
        assert_eq!(args["run_id"], "issue1265");
        assert_eq!(args["key"], "safe-shell");

        let mixed = parse_act_call_tool_call_envelope(
            r#"{"tool_name":"workspace_get","args":{"key":"safe-shell"},"run_id":"issue1265"}"#,
        )
        .expect_err("mixing nested and flattened args is ambiguous");
        assert!(mixed.to_string().contains("must not mix"));

        let array = parse_act_call_tool_call_envelope(
            r#"{"tool_name":"workspace_get","args":["safe-shell"]}"#,
        )
        .expect_err("positional args are ambiguous and must fail closed");
        assert!(array.to_string().contains("positional arrays"));
        Ok(())
    }

    #[test]
    fn assistant_tool_call_message_keeps_string_content_for_chat_templates() {
        let completion = ChatCompletion {
            content: String::new(),
            tool_calls: vec![OpenAiToolCall {
                id: "call-1".to_owned(),
                name: "workspace_get".to_owned(),
                arguments: r#"{"key":"probe"}"#.to_owned(),
            }],
            ..ChatCompletion::default()
        };
        let message = assistant_message(&completion);
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["content"], "");
        assert!(message["content"].is_string());
        assert_eq!(
            message["tool_calls"][0]["function"]["name"],
            "workspace_get"
        );
    }

    #[test]
    fn workspace_put_readback_plan_uses_exact_key_and_run_id() -> anyhow::Result<()> {
        let put_args: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1053",
            "key": "known-key",
            "value": {"expected": 4, "actual": 4},
        }))?;
        let plan = workspace_put_readback_plan(&put_args)?;
        assert_eq!(plan.arguments["run_id"], "issue1053");
        assert_eq!(plan.arguments["key"], "known-key");
        assert_eq!(plan.expected_value["actual"], 4);
        Ok(())
    }

    #[test]
    fn workspace_put_readback_record_fails_on_value_mismatch() -> anyhow::Result<()> {
        let put_args: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1053",
            "key": "known-key",
            "value": {"expected": 4, "actual": 4},
        }))?;
        let plan = workspace_put_readback_plan(&put_args)?;
        let mismatch = json!({
            "entry": {
                "value": {"expected": 4, "actual": 5},
            },
            "storage_readback": {
                "value_sha256": "sha256:synthetic",
            },
        });
        let error = workspace_put_readback_record(&plan, &mismatch)
            .expect_err("mismatched readback must fail");
        assert!(
            error
                .to_string()
                .contains("WORKSPACE_PUT_READBACK_MISMATCH")
        );
        Ok(())
    }

    #[test]
    fn workspace_put_readback_record_preserves_storage_hash() -> anyhow::Result<()> {
        let put_args: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1053",
            "key": "known-key",
            "value": {"expected": 4, "actual": 4},
        }))?;
        let plan = workspace_put_readback_plan(&put_args)?;
        let readback = json!({
            "entry": {
                "value": {"expected": 4, "actual": 4},
            },
            "storage_readback": {
                "value_sha256": "sha256:synthetic",
            },
        });
        let record = workspace_put_readback_record(&plan, &readback)?;
        assert_eq!(record["matched"], true);
        assert_eq!(record["actual_value"]["actual"], 4);
        assert_eq!(
            record["storage_readback"]["value_sha256"],
            "sha256:synthetic"
        );
        Ok(())
    }

    #[test]
    fn workspace_put_args_match_only_exact_duplicates() -> anyhow::Result<()> {
        let first: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1054",
            "key": "known-key",
            "value": {"expected": "done", "actual": "done"},
        }))?;
        let same: JsonObject = serde_json::from_value(json!({
            "key": "known-key",
            "value": {"actual": "done", "expected": "done"},
            "run_id": "issue1054",
        }))?;
        let different: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1054",
            "key": "known-key",
            "value": {"expected": "done", "actual": "changed"},
        }))?;
        assert!(workspace_put_args_match(&first, &same));
        assert!(!workspace_put_args_match(&first, &different));
        Ok(())
    }

    #[test]
    fn model_tool_call_pre_gate_rejects_workspace_put_without_value_or_artifact()
    -> anyhow::Result<()> {
        let args: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1055",
            "key": "missing-value",
        }))?;
        let reason = model_tool_call_pre_gate_rejection(
            "workspace_put",
            &args,
            true,
            None,
            &BTreeMap::new(),
        )
        .expect("workspace_put without value/artifact must fail before approval");
        assert!(
            reason
                .reason
                .contains("requires at least one of value or artifact")
        );
        assert!(!reason.terminal);

        let with_value: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1055",
            "key": "ok",
            "value": null,
        }))?;
        assert!(
            model_tool_call_pre_gate_rejection(
                "workspace_put",
                &with_value,
                true,
                None,
                &BTreeMap::new()
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn model_tool_call_pre_gate_rejects_unknown_tool_before_approval() -> anyhow::Result<()> {
        let args: JsonObject = serde_json::from_value(json!({
            "malformed_output": true,
        }))?;
        let reason =
            model_tool_call_pre_gate_rejection("agent_retry", &args, false, None, &BTreeMap::new())
                .expect("invented tool names must fail before approval");
        assert!(reason.reason.contains("not present in Synapse tools/list"));
        assert!(reason.reason.contains("real Synapse tool name"));
        assert!(!reason.terminal);
        Ok(())
    }

    #[test]
    fn model_tool_call_pre_gate_rejects_local_model_approval_control() -> anyhow::Result<()> {
        let args: JsonObject = serde_json::from_value(json!({
            "approval_id": "apr1-test",
            "decision": "decline",
        }))?;
        let reason = model_tool_call_pre_gate_rejection(
            "approval_decide",
            &args,
            true,
            None,
            &BTreeMap::new(),
        )
        .expect("local models must not self-decide approval rows");
        assert!(reason.reason.contains("runner/operator-control"));
        assert!(reason.terminal);
        assert!(
            model_tool_call_pre_gate_rejection(
                "approval_gate",
                &args,
                true,
                None,
                &BTreeMap::new()
            )
            .is_some()
        );
        assert!(
            model_tool_call_pre_gate_rejection("agent_send", &args, true, None, &BTreeMap::new())
                .is_some()
        );
        assert!(
            model_tool_call_pre_gate_rejection(
                "workspace_get",
                &args,
                true,
                None,
                &BTreeMap::new()
            )
            .is_none()
        );
        let question_args: JsonObject = serde_json::from_value(json!({
            "question": "Which synthetic value should be used?",
            "timeout_ms": 1000,
            "notify": false,
            "suppress_popup": true,
        }))?;
        assert!(
            model_tool_call_pre_gate_rejection(
                "agent_ask_operator",
                &question_args,
                true,
                None,
                &BTreeMap::new()
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn agent_ask_operator_args_receive_local_spawn_attribution() -> anyhow::Result<()> {
        let args: JsonObject = serde_json::from_value(json!({
            "question": "Which synthetic value should be used?",
            "timeout_ms": 1000,
            "notify": false,
            "suppress_popup": true,
        }))?;
        let (attributed, changed) =
            add_agent_ask_operator_spawn_id("agent_ask_operator", args, "agent-spawn-local-test");
        assert!(changed);
        assert_eq!(
            attributed.get("spawn_id").and_then(Value::as_str),
            Some("agent-spawn-local-test")
        );

        let existing: JsonObject = serde_json::from_value(json!({
            "question": "Already attributed?",
            "spawn_id": "agent-spawn-explicit",
        }))?;
        let (existing, changed) = add_agent_ask_operator_spawn_id(
            "agent_ask_operator",
            existing,
            "agent-spawn-local-test",
        );
        assert!(!changed);
        assert_eq!(
            existing.get("spawn_id").and_then(Value::as_str),
            Some("agent-spawn-explicit")
        );

        let malformed: JsonObject = serde_json::from_value(json!({
            "question": "Malformed attribution should fail in the tool schema.",
            "spawn_id": 7,
        }))?;
        let (malformed, changed) = add_agent_ask_operator_spawn_id(
            "agent_ask_operator",
            malformed,
            "agent-spawn-local-test",
        );
        assert!(!changed);
        assert_eq!(malformed.get("spawn_id"), Some(&json!(7)));

        let other: JsonObject = serde_json::from_value(json!({
            "key": "x",
        }))?;
        let (other, changed) =
            add_agent_ask_operator_spawn_id("workspace_get", other, "agent-spawn-local-test");
        assert!(!changed);
        assert!(other.get("spawn_id").is_none());
        Ok(())
    }

    #[test]
    fn model_tool_call_pre_gate_rejects_local_model_dispatcher_control() -> anyhow::Result<()> {
        let args: JsonObject = serde_json::from_value(json!({
            "task_id": "synapse",
            "concurrency_cap": 1,
        }))?;
        let reason = model_tool_call_pre_gate_rejection(
            "task_dispatch_once",
            &args,
            true,
            None,
            &BTreeMap::new(),
        )
        .expect("dispatcher tools must fail before MCP dispatch");
        assert!(reason.reason.contains("runner/operator-control"));
        assert!(reason.terminal);

        let nested_spawn: JsonObject = serde_json::from_value(json!({
            "cli": "local_model",
            "model_ref": "qwen8v2-tool-live",
            "prompt": "nested"
        }))?;
        let spawn_reason = model_tool_call_pre_gate_rejection(
            "act_spawn_agent",
            &nested_spawn,
            true,
            None,
            &BTreeMap::new(),
        )
        .expect("nested spawn is runner control for local-model workers");
        assert!(spawn_reason.reason.contains("runner/operator-control"));
        assert!(spawn_reason.terminal);
        Ok(())
    }

    #[test]
    fn exact_tool_contract_recognizes_execute_exactly_mcp_prompt() -> anyhow::Result<()> {
        let tools = tools_named([
            "cdp_open_tab",
            "target_claim",
            "browser_set_value",
            "target_act",
            "cdp_target_info",
            "workspace_put",
            "task_dispatch_once",
        ]);
        let task = "Exact-contract Synapse task. Execute exactly these MCP tools in order:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/issue1220-lane.html?agent=ramp-001\",\"window_hwnd\":7996006}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. browser_set_value {\"selector\":\"#agent-input\",\"text\":\"ramp-001\"}\n\
4. target_act {\"selector\":\"#mark\",\"verb\":\"click\",\"wait_timeout_ms\":10000}\n\
5. cdp_target_info {}\n\
6. workspace_put {\"run_id\":\"issue1220-ramp5-20260620-0225\",\"key\":\"ramp-001\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools)
            .expect("real #1220 exact-contract wording must infer a contract");

        assert_eq!(
            contract.ordered_tools,
            vec![
                "cdp_open_tab",
                "target_claim",
                "browser_set_value",
                "target_act",
                "cdp_target_info",
                "workspace_put"
            ]
        );
        assert!(!contract.allowed_tools.contains("task_dispatch_once"));
        assert_eq!(
            contract
                .argument_templates
                .get("cdp_open_tab")
                .and_then(|args| args.get("window_hwnd"))
                .and_then(Value::as_i64),
            Some(7996006)
        );

        let dispatcher_args: JsonObject = serde_json::from_value(json!({"task_id": "synapse"}))?;
        let rejection = model_tool_call_pre_gate_rejection(
            "task_dispatch_once",
            &dispatcher_args,
            true,
            Some(&contract),
            &BTreeMap::new(),
        )
        .expect("dispatcher drift must not dispatch under an exact contract");
        assert!(
            rejection
                .reason
                .contains("outside this task's exact-tool contract")
        );
        Ok(())
    }

    #[test]
    fn exact_tool_contract_detects_operator_exact_tool_calls_phrase() {
        let tools = tools_named(["cdp_open_tab", "target_claim", "workspace_put"]);
        let task = "Use only the exact tool calls below, in order:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/issue1220-lane.html\",\"window_hwnd\":1704314}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. workspace_put {\"run_id\":\"issue1220-ramp5\",\"key\":\"agent-a\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools)
            .expect("operator exact-tool-call wording must infer a contract");

        assert_eq!(
            contract.ordered_tools,
            vec!["cdp_open_tab", "target_claim", "workspace_put"]
        );
        assert_eq!(
            contract
                .argument_templates
                .get("workspace_put")
                .and_then(|args| args.get("key"))
                .and_then(Value::as_str),
            Some("agent-a")
        );
    }

    #[test]
    fn exact_tool_contract_detects_required_tool_calls_exactly_in_order_phrase() {
        let tools = tools_named(["cdp_open_tab", "target_claim", "workspace_put"]);
        let task = "The required tool calls exactly in order are:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/issue1220-lane.html?agent=b\",\"window_hwnd\":1704314}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. workspace_put {\"run_id\":\"issue1220-ramp5\",\"key\":\"agent-b\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools)
            .expect("required-tool-calls-exactly wording must infer a contract");

        assert_eq!(
            contract.ordered_tools,
            vec!["cdp_open_tab", "target_claim", "workspace_put"]
        );
        assert_eq!(
            contract
                .argument_templates
                .get("cdp_open_tab")
                .and_then(|args| args.get("window_hwnd"))
                .and_then(Value::as_i64),
            Some(1704314)
        );
    }

    #[test]
    fn exact_tool_contract_rejects_off_task_tools_without_approval() -> anyhow::Result<()> {
        let tools = tools_named(["workspace_put", "workspace_get", "local_model_probe"]);
        let task = "Use exactly these real Synapse tools, with no prose before tool calls:\n\
1. Call workspace_put with key \"issue1222/key\" and value {\"ok\":true}.\n\
2. Call workspace_get with key \"issue1222/key\" to read it back.";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        assert!(contract.allowed_tools.contains("workspace_put"));
        assert!(contract.allowed_tools.contains("workspace_get"));
        assert!(!contract.allowed_tools.contains("local_model_probe"));

        let args = JsonObject::new();
        let rejection = model_tool_call_pre_gate_rejection(
            "local_model_probe",
            &args,
            true,
            Some(&contract),
            &BTreeMap::new(),
        )
        .expect("off-task tool must be rejected before approval");
        assert!(!rejection.terminal);
        assert!(
            rejection
                .reason
                .contains("outside this task's exact-tool contract")
        );
        assert!(rejection.reason.contains("workspace_put"));
        Ok(())
    }

    #[test]
    fn exact_tool_contract_reports_missing_required_tools() {
        let tools = tools_named(["workspace_put", "workspace_get", "local_model_probe"]);
        let task = "Use exactly these real Synapse tools:\n\
1. Call workspace_put with key \"issue1222/key\" and value {\"ok\":true}.\n\
2. Call workspace_get with key \"issue1222/key\".";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let completed = completed_counts(["workspace_put"]);

        let missing = missing_task_contract_tools(&Some(contract), &completed);

        assert_eq!(missing, vec!["workspace_get".to_owned()]);
    }

    #[test]
    fn exact_tool_contract_defers_repeated_workspace_put_when_get_missing() {
        let tools = tools_named(["workspace_put", "workspace_get"]);
        let task = "Use exactly these real Synapse tools:\n\
1. Call workspace_put with run_id \"issue1222\", key \"issue1222/key\", and value \"ok\".\n\
2. Call workspace_get with run_id \"issue1222\" and key \"issue1222/key\".";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let mut completed = completed_counts(["workspace_put"]);

        assert!(workspace_put_contract_repetition_should_defer(
            &Some(contract.clone()),
            &completed,
            "workspace_put"
        ));

        completed = completed_counts(["workspace_put", "workspace_get"]);
        assert!(!workspace_put_contract_repetition_should_defer(
            &Some(contract),
            &completed,
            "workspace_put"
        ));
    }

    #[test]
    fn exact_tool_contract_defers_repeated_completed_browser_tool() {
        let tools = tools_named([
            "cdp_open_tab",
            "target_claim",
            "target_act",
            "browser_evaluate",
            "workspace_put",
        ]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
cdp_open_tab, target_claim, target_act, browser_evaluate, workspace_put.";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let mut completed = completed_counts(["cdp_open_tab"]);

        assert!(task_contract_completed_tool_repetition_should_defer(
            &Some(contract.clone()),
            &completed,
            "cdp_open_tab"
        ));
        assert!(!task_contract_completed_tool_repetition_should_defer(
            &Some(contract.clone()),
            &completed,
            "target_claim"
        ));
        assert!(!task_contract_completed_tool_repetition_should_defer(
            &Some(contract.clone()),
            &completed,
            "act_press"
        ));

        for tool in [
            "target_claim",
            "target_act",
            "browser_evaluate",
            "workspace_put",
        ] {
            *completed.entry(tool.to_owned()).or_insert(0) += 1;
        }
        assert!(!task_contract_completed_tool_repetition_should_defer(
            &Some(contract),
            &completed,
            "cdp_open_tab"
        ));
    }

    #[test]
    fn exact_tool_contract_terminal_on_fixed_next_tool_failure() -> anyhow::Result<()> {
        let tools = tools_named([
            "cdp_open_tab",
            "target_claim",
            "browser_set_value",
            "target_act",
            "workspace_put",
        ]);
        let task = "Exact-contract Synapse task. Execute exactly these MCP tools in order:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/fixture.html\",\"window_hwnd\":2427400}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. browser_set_value {\"selector\":\"#agent-input\",\"text\":\"ramp-004\"}\n\
4. target_act {\"selector\":\"#mark\",\"verb\":\"click\",\"wait_timeout_ms\":10000}\n\
5. workspace_put {\"run_id\":\"issue1280\",\"key\":\"ramp-004\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let completed = completed_counts(["cdp_open_tab", "target_claim"]);
        let exact_args: JsonObject =
            serde_json::from_value(json!({"selector":"#agent-input","text":"ramp-004"}))?;
        assert!(
            task_contract_required_tool_failure_is_terminal(
                Some(&contract),
                &completed,
                "browser_set_value",
                &exact_args,
            ),
            "the next required fixed-args tool cannot be retried forever after the daemon already rejected it"
        );

        let drifted_args: JsonObject =
            serde_json::from_value(json!({"selector":"#agent-input","text":"wrong"}))?;
        assert!(
            !task_contract_required_tool_failure_is_terminal(
                Some(&contract),
                &completed,
                "browser_set_value",
                &drifted_args,
            ),
            "argument drift is handled by the pre-gate contract rejection path"
        );
        let out_of_order_args: JsonObject = serde_json::from_value(json!({
            "selector":"#mark",
            "verb":"click",
            "wait_timeout_ms":10000
        }))?;
        assert!(
            !task_contract_required_tool_failure_is_terminal(
                Some(&contract),
                &completed,
                "target_act",
                &out_of_order_args,
            ),
            "allowed but out-of-order tools remain governed by the out-of-order rejection path"
        );
        assert!(
            !task_contract_required_tool_failure_is_terminal(
                None,
                &completed,
                "browser_set_value",
                &exact_args,
            ),
            "ordinary non-contract tool errors stay recoverable"
        );
        Ok(())
    }

    #[test]
    fn exact_tool_contract_terminal_on_agent_question_failure_after_spawn_attribution()
    -> anyhow::Result<()> {
        let tools = tools_named(["agent_ask_operator"]);
        let task = "Exact-contract Synapse task. Use exactly these real Synapse tools in this exact order:\n\
1. agent_ask_operator {\"question\":\"\",\"context\":\"REGRESSION-1028-EMPTY\",\"timeout_ms\":120000,\"notify\":false,\"suppress_popup\":true}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let model_args: JsonObject = serde_json::from_value(json!({
            "question": "",
            "context": "REGRESSION-1028-EMPTY",
            "timeout_ms": 120000,
            "notify": false,
            "suppress_popup": true,
        }))?;
        let (dispatched_args, changed) =
            add_agent_ask_operator_spawn_id("agent_ask_operator", model_args, "agent-spawn-empty");
        assert!(changed);
        assert!(
            task_contract_required_tool_failure_is_terminal(
                Some(&contract),
                &BTreeMap::new(),
                "agent_ask_operator",
                &dispatched_args,
            ),
            "runner-added spawn_id must not make a fixed exact-contract agent_ask_operator failure recoverable forever"
        );
        Ok(())
    }

    #[test]
    fn final_answer_after_failed_only_tools_is_not_success() {
        assert!(final_answer_after_failed_only_tools_should_fail(
            true, 0, 1, 0, 0,
        ));
        assert!(final_answer_after_failed_only_tools_should_fail(
            true, 0, 0, 1, 0,
        ));
        assert!(final_answer_after_failed_only_tools_should_fail(
            true, 0, 0, 0, 1,
        ));
        assert!(!final_answer_after_failed_only_tools_should_fail(
            true, 1, 1, 0, 0,
        ));
        assert!(!final_answer_after_failed_only_tools_should_fail(
            false, 0, 0, 0, 0,
        ));
    }

    #[test]
    fn exact_tool_contract_rejects_out_of_order_allowed_tool_before_approval() {
        let tools = tools_named([
            "cdp_open_tab",
            "target_claim",
            "browser_set_value",
            "target_act",
            "cdp_target_info",
            "workspace_put",
        ]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/fixture.html\",\"window_hwnd\":2427400}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. browser_set_value {\"selector\":\"#lane-value\",\"text\":\"x\"}\n\
4. target_act {\"selector\":\"#mark\",\"verb\":\"click\",\"wait_timeout_ms\":10000}\n\
5. cdp_target_info {}\n\
6. workspace_put {\"run_id\":\"issue1246\",\"key\":\"browser-target\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let completed = completed_counts(["cdp_open_tab", "target_claim", "browser_set_value"]);

        let rejection =
            task_contract_out_of_order_rejection(Some(&contract), &completed, "workspace_put")
                .expect("workspace_put is allowed but not next");
        assert!(rejection.reason.contains("out of order"));
        assert!(
            rejection
                .reason
                .contains("next required tool is target_act")
        );
        assert!(!rejection.terminal);
        assert!(
            task_contract_out_of_order_rejection(Some(&contract), &completed, "target_act")
                .is_none()
        );
        assert!(
            task_contract_out_of_order_rejection(Some(&contract), &completed, "act_press")
                .is_none()
        );
    }

    #[test]
    fn exact_tool_contract_rejects_argument_drift_before_approval() -> anyhow::Result<()> {
        let tools = tools_named([
            "cdp_open_tab",
            "target_claim",
            "browser_set_value",
            "target_act",
            "cdp_target_info",
            "workspace_put",
        ]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/fixture.html\",\"window_hwnd\":2427400}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. browser_set_value {\"selector\":\"#lane-value\",\"text\":\"issue1246\"}\n\
4. target_act {\"selector\":\"#mark\",\"verb\":\"click\",\"wait_timeout_ms\":10000}\n\
5. cdp_target_info {}\n\
6. workspace_put {\"run_id\":\"issue1246\",\"key\":\"browser-target\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");

        let exact_empty = Map::new();
        assert!(
            model_tool_call_pre_gate_rejection(
                "cdp_target_info",
                &exact_empty,
                true,
                Some(&contract),
                &completed_counts([
                    "cdp_open_tab",
                    "target_claim",
                    "browser_set_value",
                    "target_act"
                ])
            )
            .is_none()
        );

        let extra_args: JsonObject = serde_json::from_value(json!({"include": []}))?;
        let rejection = model_tool_call_pre_gate_rejection(
            "cdp_target_info",
            &extra_args,
            true,
            Some(&contract),
            &completed_counts([
                "cdp_open_tab",
                "target_claim",
                "browser_set_value",
                "target_act",
            ]),
        )
        .expect("extra argument must fail before approval");
        assert!(
            rejection
                .reason
                .contains("expected {}, got {\"include\":[]}")
        );
        assert!(rejection.reason.contains("extra_keys=[include]"));
        assert!(!rejection.terminal);

        let missing_args: JsonObject =
            serde_json::from_value(json!({"run_id":"issue1246","key":"browser-target"}))?;
        let rejection = model_tool_call_pre_gate_rejection(
            "workspace_put",
            &missing_args,
            true,
            Some(&contract),
            &completed_counts([
                "cdp_open_tab",
                "target_claim",
                "browser_set_value",
                "target_act",
                "cdp_target_info",
            ]),
        )
        .expect("missing exact value must fail before approval");
        assert!(rejection.reason.contains("missing_keys=[value]"));

        let changed_args: JsonObject = serde_json::from_value(json!({
            "run_id":"issue1246",
            "key":"browser-target",
            "value":{"ok":false}
        }))?;
        let rejection = model_tool_call_pre_gate_rejection(
            "workspace_put",
            &changed_args,
            true,
            Some(&contract),
            &completed_counts([
                "cdp_open_tab",
                "target_claim",
                "browser_set_value",
                "target_act",
                "cdp_target_info",
            ]),
        )
        .expect("changed exact value must fail before approval");
        assert!(rejection.reason.contains("changed_keys=[value]"));
        Ok(())
    }

    #[test]
    fn exact_tool_contract_tracks_duplicate_tool_steps_by_occurrence() -> anyhow::Result<()> {
        let tools = tools_named(["target_act", "workspace_put"]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. target_act {\"verb\":\"set_field\",\"role\":\"document\",\"name\":\"Text editor\",\"text\":\"hello im gemma\",\"wait_timeout_ms\":10000}\n\
2. target_act {\"verb\":\"save\",\"path\":\"C:\\\\Temp\\\\issue1034.txt\",\"text\":\"hello im gemma\",\"wait_timeout_ms\":5000}\n\
3. workspace_put {\"run_id\":\"issue1034\",\"key\":\"gemma\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");

        assert_eq!(
            contract.ordered_tools,
            vec!["target_act", "target_act", "workspace_put"]
        );
        assert_eq!(
            contract.step_argument_templates[0]
                .as_ref()
                .and_then(|args| args.get("verb"))
                .and_then(Value::as_str),
            Some("set_field")
        );
        assert_eq!(
            contract.step_argument_templates[1]
                .as_ref()
                .and_then(|args| args.get("verb"))
                .and_then(Value::as_str),
            Some("save")
        );

        let completed = completed_counts(["target_act"]);
        assert_eq!(
            missing_task_contract_tools(&Some(contract.clone()), &completed),
            vec!["target_act".to_owned(), "workspace_put".to_owned()]
        );
        let save_args: JsonObject = serde_json::from_value(json!({
            "verb": "save",
            "path": "C:\\Temp\\issue1034.txt",
            "text": "hello im gemma",
            "wait_timeout_ms": 5000
        }))?;
        assert!(
            model_tool_call_pre_gate_rejection(
                "target_act",
                &save_args,
                true,
                Some(&contract),
                &completed,
            )
            .is_none(),
            "second target_act occurrence must validate against the save template"
        );
        Ok(())
    }

    #[test]
    fn exact_contract_gate_bypass_requires_exact_template_match() -> anyhow::Result<()> {
        let tools = tools_named([
            "cdp_open_tab",
            "target_claim",
            "browser_set_value",
            "target_act",
            "cdp_target_info",
            "workspace_put",
        ]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/fixture.html\",\"window_hwnd\":2427400}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. browser_set_value {\"selector\":\"#lane-value\",\"text\":\"issue1263\"}\n\
4. target_act {\"selector\":\"#mark\",\"verb\":\"click\",\"wait_timeout_ms\":10000}\n\
5. cdp_target_info {}\n\
6. workspace_put {\"run_id\":\"issue1263\",\"key\":\"browser-target\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        let exact_set: JsonObject = serde_json::from_value(json!({
            "selector": "#lane-value",
            "text": "issue1263"
        }))?;
        assert!(local_agent_exact_contract_gate_bypass_allowed(
            "browser_set_value",
            &exact_set,
            Some(&contract),
            &completed_counts(["cdp_open_tab", "target_claim"])
        ));
        assert!(!local_agent_exact_contract_gate_bypass_allowed(
            "browser_set_value",
            &exact_set,
            None,
            &BTreeMap::new()
        ));

        let changed_set: JsonObject = serde_json::from_value(json!({
            "selector": "#lane-value",
            "text": "different"
        }))?;
        assert!(!local_agent_exact_contract_gate_bypass_allowed(
            "browser_set_value",
            &changed_set,
            Some(&contract),
            &completed_counts(["cdp_open_tab", "target_claim"])
        ));

        let exact_info = Map::new();
        assert!(local_agent_exact_contract_gate_bypass_allowed(
            "cdp_target_info",
            &exact_info,
            Some(&contract),
            &completed_counts([
                "cdp_open_tab",
                "target_claim",
                "browser_set_value",
                "target_act"
            ])
        ));
        let extra_info: JsonObject = serde_json::from_value(json!({"include": []}))?;
        assert!(!local_agent_exact_contract_gate_bypass_allowed(
            "cdp_target_info",
            &extra_info,
            Some(&contract),
            &completed_counts([
                "cdp_open_tab",
                "target_claim",
                "browser_set_value",
                "target_act"
            ])
        ));
        Ok(())
    }

    #[test]
    fn trusted_exact_contract_normalizes_next_tool_args_to_template() -> anyhow::Result<()> {
        let tools = tools_named([
            "cdp_open_tab",
            "target_claim",
            "browser_set_value",
            "target_act",
            "cdp_target_info",
            "workspace_put",
        ]);
        let task = "Use only the exact tool calls below, in order:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/issue1220-lane.html?agent=ramp10-003\",\"window_hwnd\":1704314}\n\
2. target_claim {\"ttl_ms\":600000}\n\
3. browser_set_value {\"selector\":\"#agent-input\",\"text\":\"ramp10-003\"}\n\
4. target_act {\"selector\":\"#mark\",\"verb\":\"click\",\"wait_timeout_ms\":10000}\n\
5. cdp_target_info {}\n\
6. workspace_put {\"run_id\":\"issue1220-ramp10-20260620-1043\",\"key\":\"ramp10-003\",\"value\":{\"ok\":true,\"agent\":\"ramp10-003\",\"expected_status\":\"ramp10-003:ramp10-003\"}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        let completed = completed_counts([
            "cdp_open_tab",
            "target_claim",
            "browser_set_value",
            "target_act",
        ]);
        let drifted_info: JsonObject = serde_json::from_value(json!({
            "cdp_target_id": "chrome-tab:600753094",
            "window_hwnd": 1704314
        }))?;
        let normalized = trusted_exact_contract_normalized_args(
            true,
            "cdp_target_info",
            &drifted_info,
            Some(&contract),
            &completed,
        )
        .expect("next safe tool drift should normalize to contract template");

        assert!(normalized.is_empty());
        assert!(
            model_tool_call_pre_gate_rejection(
                "cdp_target_info",
                &normalized,
                true,
                Some(&contract),
                &completed
            )
            .is_none()
        );
        assert!(local_agent_exact_contract_gate_bypass_allowed(
            "cdp_target_info",
            &normalized,
            Some(&contract),
            &completed
        ));
        assert!(!local_agent_exact_contract_gate_bypass_allowed(
            "cdp_target_info",
            &drifted_info,
            Some(&contract),
            &completed
        ));
        Ok(())
    }

    #[test]
    fn trusted_exact_contract_normalization_refuses_operator_control_tools() -> anyhow::Result<()> {
        let tools = tools_named(["act_run_shell", "workspace_put"]);
        let task = "Use only the exact tool calls below, in order:\n\
1. act_run_shell {\"command\":\"powershell.exe\",\"args\":[\"-NoLogo\",\"-NoProfile\",\"-Command\",\"Write-Output ok\"],\"execution_mode\":\"inline\",\"timeout_ms\":1000}\n\
2. workspace_put {\"run_id\":\"issue1220\",\"key\":\"shell-edge\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        let drifted_shell: JsonObject = serde_json::from_value(json!({
            "command": "powershell.exe",
            "args": ["-NoLogo", "-NoProfile", "-Command", "Write-Output ok"],
            "execution_mode": "inline",
            "timeout_ms": 1000,
            "unexpected": true
        }))?;

        assert!(
            trusted_exact_contract_normalized_args(
                true,
                "act_run_shell",
                &drifted_shell,
                Some(&contract),
                &BTreeMap::new(),
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn trusted_exact_contract_normalization_requires_trust_and_next_tool() -> anyhow::Result<()> {
        let tools = tools_named(["cdp_target_info", "workspace_put"]);
        let task = "Use only the exact tool calls below, in order:\n\
1. cdp_target_info {}\n\
2. workspace_put {\"run_id\":\"issue1220\",\"key\":\"status\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        let drifted_info: JsonObject = serde_json::from_value(json!({"include": []}))?;
        let completed = completed_counts(["cdp_target_info"]);

        assert!(
            trusted_exact_contract_normalized_args(
                false,
                "cdp_target_info",
                &drifted_info,
                Some(&contract),
                &BTreeMap::new(),
            )
            .is_none()
        );
        assert!(
            trusted_exact_contract_normalized_args(
                true,
                "cdp_target_info",
                &drifted_info,
                Some(&contract),
                &completed,
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn exact_contract_diagnostic_does_not_label_raw_foreground_tools() -> anyhow::Result<()> {
        let tools = tools_named(["act_type"]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. act_type {\"text\":\"do-not-bypass\",\"verify_delta\":true}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        let args: JsonObject = serde_json::from_value(json!({
            "text": "do-not-bypass",
            "verify_delta": true
        }))?;

        assert!(
            model_tool_call_pre_gate_rejection(
                "act_type",
                &args,
                true,
                Some(&contract),
                &BTreeMap::new()
            )
            .is_none()
        );
        assert!(!local_agent_exact_contract_gate_bypass_allowed(
            "act_type",
            &args,
            Some(&contract),
            &BTreeMap::new()
        ));
        Ok(())
    }

    #[test]
    fn exact_contract_diagnostic_does_not_label_shell_or_operator_control() -> anyhow::Result<()> {
        let tools = tools_named([
            "act_run_shell",
            "act_spawn_agent",
            "agent_kill",
            "approval_request",
            "local_model_update",
        ]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. act_run_shell {\"command\":\"powershell.exe\",\"args\":[\"-NoLogo\",\"-Command\",\"Remove-Item -LiteralPath C:\\\\temp\\\\x -Recurse\"],\"execution_mode\":\"inline\"}\n\
2. act_spawn_agent {\"prompt\":\"spawn nested\",\"cli\":\"local_model\",\"model_ref\":\"qwen8v2-tool-live\"}\n\
3. agent_kill {\"target_id\":\"agent-spawn-danger\"}\n\
4. approval_request {\"summary\":\"do not bypass\"}\n\
5. local_model_update {\"name\":\"qwen8v2-tool-live\",\"enabled\":false}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        for (tool_name, args) in [
            (
                "act_run_shell",
                json!({
                    "command": "powershell.exe",
                    "args": ["-NoLogo", "-Command", "Remove-Item -LiteralPath C:\\temp\\x -Recurse"],
                    "execution_mode": "inline"
                }),
            ),
            (
                "act_spawn_agent",
                json!({
                    "prompt": "spawn nested",
                    "cli": "local_model",
                    "model_ref": "qwen8v2-tool-live"
                }),
            ),
            ("agent_kill", json!({"target_id": "agent-spawn-danger"})),
            ("approval_request", json!({"summary": "do not bypass"})),
            (
                "local_model_update",
                json!({"name": "qwen8v2-tool-live", "enabled": false}),
            ),
        ] {
            let object = args.as_object().cloned().expect("test args object");
            assert!(
                !local_agent_exact_contract_gate_bypass_allowed(
                    tool_name,
                    &object,
                    Some(&contract),
                    &BTreeMap::new()
                ),
                "{tool_name} must not be labeled exact-contract-authorized"
            );
        }
        Ok(())
    }

    #[test]
    fn exact_contract_gate_bypass_allows_prompt_authorized_target_act_run_shell()
    -> anyhow::Result<()> {
        let tools = tools_named(["target_act", "workspace_put"]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. target_act {\"verb\":\"run_shell\",\"command\":\"powershell.exe\",\"args\":[\"-NoLogo\",\"-NoProfile\",\"-Command\",\"Get-ChildItem\"],\"timeout_ms\":10000,\"working_dir\":\"C:\\\\code\\\\Synapse\"}\n\
2. workspace_put {\"run_id\":\"issue1265\",\"key\":\"prompt-authorized-shell\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        let args: JsonObject = serde_json::from_value(json!({
            "verb": "run_shell",
            "command": "powershell.exe",
            "args": ["-NoLogo", "-NoProfile", "-Command", "Get-ChildItem"],
            "timeout_ms": 10000,
            "working_dir": "C:\\code\\Synapse"
        }))?;

        assert!(local_agent_exact_contract_gate_bypass_allowed(
            "target_act",
            &args,
            Some(&contract),
            &BTreeMap::new()
        ));
        Ok(())
    }

    #[test]
    fn exact_contract_gate_bypass_denies_drifted_target_act_run_shell_args() -> anyhow::Result<()> {
        let tools = tools_named(["target_act", "workspace_put"]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. target_act {\"verb\":\"run_shell\",\"command\":\"whoami.exe\",\"timeout_ms\":10000,\"working_dir\":\"C:\\\\code\\\\Synapse\"}\n\
2. workspace_put {\"run_id\":\"issue1265\",\"key\":\"safe-shell\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        let args: JsonObject = serde_json::from_value(json!({
            "verb": "run_shell",
            "command": "whoami.exe",
            "timeout_ms": 10000,
            "working_dir": "C:\\code\\Synapse",
            "unexpected": true
        }))?;

        assert!(!local_agent_exact_contract_gate_bypass_allowed(
            "target_act",
            &args,
            Some(&contract),
            &BTreeMap::new()
        ));
        Ok(())
    }

    #[test]
    fn local_agent_timeout_budget_excludes_approval_wait() {
        let started = Instant::now()
            .checked_sub(Duration::from_mins(4))
            .expect("test duration must fit within Instant range");
        let active = local_agent_active_elapsed(started, Duration::from_mins(3));
        assert!(active < Duration::from_secs(90), "{active:?}");

        let fully_blocked = local_agent_active_elapsed(started, Duration::from_mins(5));
        assert_eq!(fully_blocked, Duration::ZERO);
    }

    #[test]
    fn local_agent_timeout_error_code_is_not_endpoint_unreachable() {
        let detail = "LOCAL_AGENT_TIMEOUT: local-agent active timeout exceeded before turn 2";
        assert_eq!(error_code_from_detail(detail), "LOCAL_AGENT_TIMEOUT");
        assert_eq!(
            error_code_from_detail("MODEL_TASK_REQUIRED_TOOL_FAILED: browser_set_value: synthetic"),
            "MODEL_TASK_REQUIRED_TOOL_FAILED"
        );
    }

    #[test]
    fn synapse_tool_call_failure_uses_nested_mcp_error_code() {
        let error = ServiceError::McpError(ErrorData::new(
            rmcp::model::ErrorCode(-32099),
            "act_launch refused notepad.exe",
            Some(json!({
                "code": "ACTION_TARGET_INVALID",
                "reason": "shared_tabbed_app_existing_window_risk",
                "resolution": "create or select a provably owned native tab/document target first"
            })),
        ));

        let class = classify_synapse_tool_call_failure(&error, false);

        assert_eq!(class.error_code, "ACTION_TARGET_INVALID");
        assert_eq!(class.error_code_source, "mcp_error.data.code");
        let mcp_error = class.mcp_error.expect("MCP error envelope retained");
        assert_eq!(mcp_error["jsonrpc_code"], json!(-32099));
        assert_eq!(
            mcp_error["message"],
            json!("act_launch refused notepad.exe")
        );
        assert_eq!(
            mcp_error["data"]["reason"],
            json!("shared_tabbed_app_existing_window_risk")
        );
    }

    #[test]
    fn synapse_tool_call_failure_rejects_unbounded_nested_error_code() {
        let error = ServiceError::McpError(ErrorData::new(
            rmcp::model::ErrorCode(-32099),
            "tool failed with unbounded detail",
            Some(json!({
                "code": "not a bounded machine code with spaces",
                "reason": "bad_code_shape"
            })),
        ));

        let class = classify_synapse_tool_call_failure(&error, false);

        assert_eq!(class.error_code, "SYNAPSE_TOOL_CALL_FAILED");
        assert_eq!(class.error_code_source, "mcp_error_without_data_code");
        assert_eq!(
            class.mcp_error.expect("MCP error envelope retained")["data"]["reason"],
            json!("bad_code_shape")
        );
    }

    #[test]
    fn exact_tool_contract_progress_suggests_workspace_get_arguments() -> anyhow::Result<()> {
        let tools = tools_named(["workspace_put", "workspace_get"]);
        let task = "Use exactly these real Synapse tools:\n\
1. Call workspace_put with run_id \"issue1222\", key \"issue1222/key\", and value \"ok\".\n\
2. Call workspace_get with run_id \"issue1222\" and key \"issue1222/key\".";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let completed = completed_counts(["workspace_put"]);
        let successful_put: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1222",
            "key": "issue1222/key",
            "value": "ok",
        }))?;

        let progress = task_contract_progress_value(
            &Some(contract),
            &completed,
            std::slice::from_ref(&successful_put),
        )
        .expect("progress");

        assert_eq!(progress["missing_tools"], json!(["workspace_get"]));
        assert_eq!(progress["suggested_next_tool"], json!("workspace_get"));
        assert_eq!(
            progress["suggested_next_arguments"],
            json!({"key": "issue1222/key", "run_id": "issue1222"})
        );
        assert!(
            progress["suggestion"]
                .as_str()
                .unwrap()
                .contains("do not call workspace_put again")
        );
        Ok(())
    }

    #[test]
    fn exact_tool_contract_preserves_browser_target_task_order() {
        let tools = tools_named([
            "browser_evaluate",
            "workspace_put",
            "target_claim",
            "cdp_open_tab",
            "target_act",
        ]);
        let task = "Use exactly these real Synapse tools:\n\
1. Call cdp_open_tab with url \"http://127.0.0.1:8893/issue1246.html\".\n\
2. Call target_claim for the returned target.\n\
3. Call target_act to set the field.\n\
4. Call browser_evaluate to read #status.\n\
5. Call workspace_put with run_id \"issue1246\", key \"browser-target\", and value {\"ok\":true}.";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");

        assert_eq!(
            contract.ordered_tools,
            vec![
                "cdp_open_tab",
                "target_claim",
                "target_act",
                "browser_evaluate",
                "workspace_put",
            ]
        );
        assert_eq!(
            missing_task_contract_tools(&Some(contract.clone()), &BTreeMap::new()),
            vec![
                "cdp_open_tab",
                "target_claim",
                "target_act",
                "browser_evaluate",
                "workspace_put",
            ]
        );
        let completed = completed_counts(["cdp_open_tab"]);
        let progress = task_contract_progress_value(&Some(contract), &completed, &[])
            .expect("progress after first browser tool");
        assert_eq!(progress["suggested_next_tool"], json!("target_claim"));
        assert_eq!(
            progress["missing_tools"],
            json!([
                "target_claim",
                "target_act",
                "browser_evaluate",
                "workspace_put"
            ])
        );
    }

    #[test]
    fn exact_tool_contract_ignores_negative_tool_mentions_before_ordered_list() {
        let tools = tools_named([
            "act_press",
            "act_type",
            "browser_evaluate",
            "browser_set_value",
            "cdp_open_tab",
            "cdp_target_info",
            "target_act",
            "target_claim",
            "workspace_put",
        ]);
        let task = "Use exactly these real Synapse tools in this exact order.\n\
Do not call task_retry, browser_evaluate, act_type, act_press, or raw foreground tools.\n\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/issue1246-browser-target.html\",\"window_hwnd\":2427400}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. browser_set_value {\"selector\":\"#lane-value\",\"text\":\"issue1246\"}\n\
4. target_act {\"verb\":\"click\",\"selector\":\"#mark\",\"wait_timeout_ms\":10000}\n\
5. cdp_target_info {}\n\
6. workspace_put {\"run_id\":\"issue1246\",\"key\":\"browser-target\",\"value\":{\"ok\":true}}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");

        assert_eq!(
            contract.ordered_tools,
            vec![
                "cdp_open_tab",
                "target_claim",
                "browser_set_value",
                "target_act",
                "cdp_target_info",
                "workspace_put",
            ]
        );
        assert!(!contract.allowed_tools.contains("browser_evaluate"));
        assert!(!contract.allowed_tools.contains("act_type"));
        assert!(!contract.allowed_tools.contains("act_press"));
        let progress = task_contract_progress_value(&Some(contract), &BTreeMap::new(), &[])
            .expect("initial progress");
        assert_eq!(progress["suggested_next_tool"], json!("cdp_open_tab"));
    }

    #[test]
    fn exact_tool_contract_progress_includes_exact_task_arguments() {
        let tools = tools_named(["cdp_open_tab", "target_claim"]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
cdp_open_tab, target_claim.\n\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/issue1246-browser-target.html\",\"window_hwnd\":2427400}\n\
2. target_claim {\"ttl_ms\":120000}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let progress = task_contract_progress_value(&Some(contract.clone()), &BTreeMap::new(), &[])
            .expect("initial progress");

        assert_eq!(progress["suggested_next_tool"], json!("cdp_open_tab"));
        assert_eq!(
            progress["suggested_next_arguments"],
            json!({
                "url": "http://127.0.0.1:8896/issue1246-browser-target.html",
                "window_hwnd": 2427400,
            })
        );
        assert_eq!(
            contract.argument_templates["target_claim"],
            serde_json::from_value::<JsonObject>(json!({"ttl_ms": 120000})).unwrap()
        );
    }

    #[test]
    fn model_facing_exact_contract_feedback_hides_internal_task_keys() {
        let tools = tools_named(["cdp_open_tab", "target_claim"]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
cdp_open_tab, target_claim.\n\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/issue1246.html\",\"window_hwnd\":2427400}\n\
2. target_claim {\"ttl_ms\":120000}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let completed = completed_counts(["cdp_open_tab"]);
        let internal_progress =
            task_contract_progress_value(&Some(contract.clone()), &completed, &[])
                .expect("internal progress still exists for logs");
        let model_value = model_tool_result_value(
            &json!({
                "ok": true,
                "task_contract_progress": internal_progress,
            }),
            &Some(contract),
            &completed,
            &[],
        );
        let text = model_value.to_string();

        assert!(!text.contains("task_exact_tool_phrase"));
        assert!(!text.contains("task_contract_progress"));
        assert!(!text.contains("suggested_next_tool"));
        assert!(text.contains("target_claim"));
        assert_eq!(
            model_value["contract_status"]["next_function"]["name"],
            json!("target_claim")
        );
        assert_eq!(model_value["next_function"]["name"], json!("target_claim"));
        assert_eq!(
            model_value["contract_status"]["next_function"]["arguments"],
            json!({"ttl_ms": 120000})
        );
        assert_eq!(
            model_value["call_next"]["function"]["arguments"],
            json!({"ttl_ms": 120000})
        );
    }

    #[test]
    fn exact_tool_contract_prompt_guides_browser_target_route_without_raw_foreground() {
        let tools = tools_named([
            "cdp_open_tab",
            "target_claim",
            "browser_set_value",
            "target_act",
            "cdp_target_info",
            "browser_evaluate",
            "workspace_put",
            "act_press",
        ]);
        let task = "Use exactly these real Synapse tools:\n\
1. Call cdp_open_tab.\n\
2. Call target_claim.\n\
3. Call browser_set_value.\n\
4. Call target_act.\n\
5. Call cdp_target_info.\n\
6. Call workspace_put.";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");

        let prompt = task_contract_prompt(&contract);

        assert!(prompt.contains(
            "Required tool order: cdp_open_tab -> target_claim -> browser_set_value -> target_act -> cdp_target_info -> workspace_put."
        ));
        assert!(prompt.contains("cdp_open_tab opens the owned Chrome tab"));
        assert!(prompt.contains("target_claim claims the returned owned target"));
        assert!(prompt.contains("browser_set_value changes a field"));
        assert!(prompt.contains("cdp_target_info reads typed page text/vitals"));
        assert!(prompt.contains("workspace_put records the verified result"));
        assert!(!prompt.contains("act_press"));
    }

    #[test]
    fn verified_browser_checkpoint_contract_completes_after_workspace_readback()
    -> anyhow::Result<()> {
        let tools = tools_named([
            "cdp_open_tab",
            "target_claim",
            "target_act",
            "browser_evaluate",
            "workspace_put",
        ]);
        let task = "Use exactly these real Synapse tools:\n\
1. Call cdp_open_tab.\n\
2. Call target_claim.\n\
3. Call target_act.\n\
4. Call browser_evaluate.\n\
5. Call workspace_put.";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let completed = completed_counts([
            "cdp_open_tab",
            "target_claim",
            "target_act",
            "browser_evaluate",
            "workspace_put",
        ]);

        assert!(!verified_workspace_contract_complete(
            &Some(contract.clone()),
            &completed
        ));
        assert!(!verified_workspace_checkpoint_contract_complete(
            &Some(contract.clone()),
            &completed,
            &[]
        ));
        let successful_put: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1246",
            "key": "browser-target",
            "value": {"ok": true},
        }))?;
        assert!(verified_workspace_checkpoint_contract_complete(
            &Some(contract),
            &completed,
            &[successful_put]
        ));
        Ok(())
    }

    #[test]
    fn verified_workspace_contract_completes_after_put_readback_and_get_credit()
    -> anyhow::Result<()> {
        let tools = tools_named(["workspace_put", "workspace_get"]);
        let task = "Use exactly these real Synapse tools:\n\
1. Call workspace_put with run_id \"issue1222\", key \"issue1222/key\", and value \"ok\".\n\
2. Call workspace_get with run_id \"issue1222\" and key \"issue1222/key\".";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");
        let mut completed = completed_counts(["workspace_put"]);
        assert!(!verified_workspace_contract_complete(
            &Some(contract.clone()),
            &completed
        ));

        completed = completed_counts(["workspace_put", "workspace_get"]);
        assert!(verified_workspace_contract_complete(
            &Some(contract),
            &completed
        ));
        Ok(())
    }

    #[test]
    fn verified_workspace_contract_final_message_uses_verified_written_value() -> anyhow::Result<()>
    {
        let successful_put: JsonObject = serde_json::from_value(json!({
            "run_id": "issue1222",
            "key": "issue1222/key",
            "value": "ok",
        }))?;

        assert_eq!(
            final_message_from_successful_workspace_puts(&[successful_put], "fallback"),
            "ok"
        );
        assert_eq!(
            final_message_from_successful_workspace_puts(&[], "fallback"),
            "fallback"
        );
        Ok(())
    }

    #[test]
    fn exact_tool_contract_prompt_lists_only_allowed_tools() {
        let tools = tools_named(["workspace_put", "workspace_get", "local_model_probe"]);
        let task = "Use exactly these real Synapse tools:\n\
1. Call workspace_put with key \"issue1222/key\" and value {\"ok\":true}.\n\
2. Call workspace_get with key \"issue1222/key\".";
        let contract = infer_task_tool_contract(task, &tools).expect("exact tool contract");

        let prompt = task_contract_prompt(&contract);

        assert!(prompt.contains("Allowed tool names: workspace_get, workspace_put."));
        assert!(prompt.contains("semantically similar tool name is invalid"));
        assert!(prompt.contains("run_id, key, and value"));
        assert!(!prompt.contains("local_model_probe"));
    }

    #[test]
    fn tool_exposure_routes_when_provider_tool_cap_is_lower_than_synapse_surface() {
        let mut row = test_local_agent_row();
        row.max_tools = Some(128);
        assert_eq!(resolve_tool_exposure(&row, 141), ToolExposure::Routed);
        assert_eq!(resolve_tool_exposure(&row, 128), ToolExposure::Direct);
    }

    #[test]
    fn internalized_preset_never_injects_a_catalog_regardless_of_max_tools() {
        // An internalized model carries the surface in its weights; the harness
        // must inject ZERO tools even when the surface exceeds any cap.
        let mut row = test_local_agent_row();
        row.runtime_preset = Some("internalized_no_catalog".to_owned());
        row.max_tools = Some(16);
        assert_eq!(resolve_tool_exposure(&row, 141), ToolExposure::Internalized);
        assert_eq!(resolve_tool_exposure(&row, 1), ToolExposure::Internalized);
        // The behavioral prompt carries no tool framing (no catalog meta-tools).
        let prompt = system_prompt(ToolExposure::Internalized, &[]);
        assert!(!prompt.contains(SYNAPSE_TOOL_CATALOG));
        assert!(!prompt.contains("attached MCP tools"));
        assert!(!prompt.contains("attached directly"));
        assert!(prompt.contains("one OpenAI tool_call and no prose"));
        assert!(prompt.contains("function.name = exact task-requested Synapse tool name"));
        assert!(prompt.contains("exact argument keys"));
        assert!(prompt.contains("post_write_readback.matched=true"));
        // Internalized serving endpoints are validated through non-streaming
        // probes and may return JSON even when asked to stream; keep the runner
        // on the same non-streaming path so tool_calls are parsed.
        assert!(!should_stream(false, ToolExposure::Internalized));
        assert!(!should_stream(true, ToolExposure::Internalized));
        assert!(should_stream(false, ToolExposure::Direct));
        assert!(!should_stream(true, ToolExposure::Direct));
        // The request body for an internalized session must drop tools+choice.
        let mut body = json!({"tools": [], "tool_choice": "auto", "model": "x"});
        apply_runtime_preset(&row, &mut body);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn internalized_exact_contract_exposes_only_contract_tools() -> anyhow::Result<()> {
        let mut row = test_local_agent_row();
        row.runtime_preset = Some("internalized_no_catalog".to_owned());
        row.max_tools = Some(16);
        let tools = tools_named(["workspace_put", "workspace_get", "local_model_probe"]);
        let task = "Use exactly these real Synapse tools:\n\
1. workspace_put {\"run_id\":\"issue1246\",\"key\":\"k\",\"value\":\"v\"}\n\
2. workspace_get {\"run_id\":\"issue1246\",\"key\":\"k\"}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");

        let exposed =
            openai_tools_for_exposure(ToolExposure::Internalized, &tools, Some(&contract), &row)?;

        let exposed_names = exposed
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>();
        assert_eq!(exposed_names, vec!["workspace_put", "workspace_get"]);
        assert!(!exposed_names.contains(&"local_model_probe"));

        let mut body = json!({"tools": exposed, "tool_choice": "auto", "model": "x"});
        apply_runtime_preset(&row, &mut body);
        assert_eq!(body["tool_choice"], json!("auto"));
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        Ok(())
    }

    #[test]
    fn internalized_exact_contract_turn_exposes_only_next_function() -> anyhow::Result<()> {
        let tools = tools_named(["cdp_open_tab", "target_claim", "browser_set_value"]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/fixture.html\",\"window_hwnd\":2427400}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. browser_set_value {\"selector\":\"#lane-value\",\"text\":\"issue1246\"}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        let mut completed = completed_counts(["cdp_open_tab"]);

        let selection = exact_contract_turn_tools(&tools, &contract, &completed)?;
        let exposed_names = selection
            .tools
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>();
        assert_eq!(exposed_names, vec!["target_claim"]);
        assert_eq!(
            selection.tool_choice,
            Some(json!({
                "type": "function",
                "function": {"name": "target_claim"},
            }))
        );

        completed = completed_counts(["cdp_open_tab", "target_claim", "browser_set_value"]);
        let complete_selection = exact_contract_turn_tools(&tools, &contract, &completed)?;
        assert!(complete_selection.tools.is_empty());
        assert!(complete_selection.tool_choice.is_none());
        Ok(())
    }

    #[test]
    fn internalized_exact_contract_turn_instruction_names_only_next_function() {
        let tools = tools_named(["cdp_open_tab", "target_claim", "browser_set_value"]);
        let task = "Use exactly these real Synapse tools in this exact order:\n\
1. cdp_open_tab {\"url\":\"http://127.0.0.1:8896/fixture.html\",\"window_hwnd\":2427400}\n\
2. target_claim {\"ttl_ms\":120000}\n\
3. browser_set_value {\"selector\":\"#lane-value\",\"text\":\"issue1246\"}";
        let contract = infer_task_tool_contract(task, &tools).expect("exact contract");
        let completed = completed_counts(["cdp_open_tab", "target_claim"]);

        let message = task_contract_next_instruction_message(&contract, &completed)
            .expect("next instruction");
        let content = message["content"].as_str().expect("content");
        assert!(content.contains("NEXT_REQUIRED_FUNCTION"));
        assert!(content.contains("name: browser_set_value"));
        assert!(content.contains(r##""selector":"#lane-value""##));
        assert!(content.contains(r#""text":"issue1246""#));
        assert!(!content.contains("name: cdp_open_tab"));
    }

    #[test]
    fn deepseek_runtime_presets_shape_chat_body() {
        let mut row = test_local_agent_row();
        row.runtime_preset = Some("deepseek_v4_flash_non_thinking".to_owned());
        let mut flash = json!({"tool_choice": "auto"});
        apply_runtime_preset(&row, &mut flash);
        assert_eq!(flash["thinking"]["type"], "disabled");
        assert!(flash.get("tool_choice").is_some());

        row.runtime_preset = Some("deepseek_v4_reasoning".to_owned());
        let mut reasoning = json!({"tool_choice": "auto"});
        apply_runtime_preset(&row, &mut reasoning);
        assert_eq!(reasoning["thinking"]["type"], "enabled");
        assert_eq!(reasoning["reasoning_effort"], "max");
        assert!(reasoning.get("tool_choice").is_none());
    }

    fn prompt_test_tools() -> Vec<Tool> {
        tools_named(["observe", "act_type", "agent_send"])
    }

    fn tools_named<const N: usize>(names: [&str; N]) -> Vec<Tool> {
        names
            .iter()
            .map(|name| {
                serde_json::from_value(json!({
                    "name": name,
                    "description": "synthetic",
                    "inputSchema": {"type": "object", "properties": {}},
                }))
                .expect("synthetic Tool")
            })
            .collect()
    }

    /// Round-trip proxy for compressionprompt.md §5.5: every load-bearing
    /// constraint from the original verbose prompt must survive the compression,
    /// and the result must actually be shorter (compression, not bloat). Also
    /// pins the Direct/Routed correctness invariant: a Direct (API-model) session
    /// is never told about `synapse_tool_catalog`, which it does not have.
    #[test]
    fn system_prompt_keeps_load_bearing_constraints_and_compresses() {
        // The exact verbose prompts this change replaced (origin/main 3537242).
        const OLD_BASE: &str = "You are a local Synapse agent. Use the provided MCP tools to inspect and change state. Never invent tool results. When a task asks for a stored artifact, call the relevant Synapse tool and then read it back. Finish with a concise summary only after the needed tool calls have succeeded.";
        let old_direct = format!(
            "{OLD_BASE}\n\nAll Synapse MCP tools from this session's strict tools/list are attached directly as model tools."
        );
        let old_routed = format!(
            "{OLD_BASE}\n\nThis provider has a lower function-count cap than the live Synapse tool surface, so tools are exposed through a routed harness. Call synapse_tool_catalog to inspect exact tool schemas and call synapse_tool with a real Synapse tool name plus arguments to execute it. The routed harness can call every real Synapse MCP tool loaded by this session, including file, shell, browser/perception, agent, dashboard, and local-model tools. Live tool names: observe, act_type, agent_send"
        );

        let tools = prompt_test_tools();
        let direct = system_prompt(ToolExposure::Direct, &tools);
        let routed = system_prompt(ToolExposure::Routed, &tools);

        // Load-bearing behavioral constraints survive verbatim in both modes.
        for prompt in [&direct, &routed] {
            assert!(prompt.contains("Synapse agent"), "identity");
            assert!(prompt.contains("inspect/change state"), "capability");
            assert!(
                prompt.contains("Never invent tool results"),
                "no-fabrication"
            );
            assert!(prompt.contains("read back"), "read-back verification");
            assert!(
                prompt.contains("post_write_readback.matched=true"),
                "driver-readback terminal success"
            );
            assert!(
                prompt.contains("Summarize after required tools succeed"),
                "summary-gating"
            );
        }
        // Direct mode must NOT advertise the routed-only meta-tools (it has the
        // real schemas attached; synapse_tool_catalog is not in its tool list).
        assert!(!direct.contains(SYNAPSE_TOOL_CATALOG));
        assert!(!direct.contains(SYNAPSE_TOOL_CALL));
        // Routed mode keeps the indirection mechanism and the live names.
        assert!(routed.contains(SYNAPSE_TOOL_CATALOG));
        assert!(routed.contains(SYNAPSE_TOOL_CALL));
        for tool in &tools {
            assert!(routed.contains(tool.name.as_ref()));
        }

        // Compression: char count is the tiktoken-absent proxy (~4 chars/token).
        println!(
            "readback=prompt_compress direct old={} new={} routed old={} new={}",
            old_direct.len(),
            direct.len(),
            old_routed.len(),
            routed.len()
        );
        assert!(
            direct.len() < old_direct.len(),
            "Direct prompt must be shorter than the verbose original"
        );
        assert!(
            routed.len() < old_routed.len(),
            "Routed prompt must be shorter than the verbose original"
        );
    }

    #[test]
    fn tool_failure_suggestion_is_exposure_aware() {
        // Direct sessions have no synapse_tool_catalog — must not be told to use it.
        let direct = tool_failure_suggestion(false, ToolExposure::Direct);
        assert!(!direct.contains("synapse_tool_catalog"));
        assert!(direct.contains("input schema"));
        // Routed sessions do have it — the hint names it.
        let routed = tool_failure_suggestion(false, ToolExposure::Routed);
        assert!(routed.contains("synapse_tool_catalog"));
        // A dead transport is terminal regardless of exposure.
        assert!(tool_failure_suggestion(true, ToolExposure::Direct).contains("run is ending"));
        assert!(tool_failure_suggestion(true, ToolExposure::Routed).contains("run is ending"));
    }

    #[test]
    fn gate_label_makes_bare_synapse_tools_classify_as_mcp_tools() {
        use crate::server::permission_policy::classify;
        // A hazardous action tool must gate; a read-only one must auto-allow —
        // proving the harness shares the daemon's single classification authority.
        assert_eq!(
            gate_tool_label("act_run_shell"),
            "mcp__synapse__act_run_shell"
        );
        assert!(
            classify(&gate_tool_label("act_run_shell"), &json!({})).is_gate(),
            "act_run_shell must be gated"
        );
        assert!(
            classify(&gate_tool_label("act_type"), &json!({})).is_gate(),
            "act_type (typing into apps) must be gated"
        );
        assert!(
            !classify(&gate_tool_label("observe"), &json!({})).is_gate(),
            "observe is read-only and must auto-allow"
        );
        assert!(
            !classify(&gate_tool_label("read_text"), &json!({})).is_gate(),
            "read_text is read-only and must auto-allow"
        );
    }

    #[test]
    fn parse_gate_verdict_allow_prefers_operator_edited_args() {
        let fallback: JsonObject =
            serde_json::from_value(json!({"command": "original"})).expect("fallback object");
        // Approve-with-edits (#1030): the edited args win.
        let edited = json!({"behavior": "allow", "updatedInput": {"command": "edited"}});
        match parse_gate_verdict(&edited, &fallback).expect("allow") {
            ToolGate::Allow(args) => assert_eq!(args["command"], "edited"),
            ToolGate::Deny(reason) => panic!("expected allow, got deny: {reason}"),
        }
        // Plain allow (no updatedInput) falls back to the original input.
        let plain = json!({"behavior": "allow"});
        match parse_gate_verdict(&plain, &fallback).expect("allow") {
            ToolGate::Allow(args) => assert_eq!(args["command"], "original"),
            ToolGate::Deny(reason) => panic!("expected allow, got deny: {reason}"),
        }
    }

    #[test]
    fn parse_gate_verdict_deny_carries_reason() {
        let fallback = JsonObject::new();
        let deny = json!({"behavior": "deny", "message": "not safe right now"});
        match parse_gate_verdict(&deny, &fallback).expect("deny parses") {
            ToolGate::Deny(reason) => assert_eq!(reason, "not safe right now"),
            ToolGate::Allow(_) => panic!("expected deny"),
        }
    }

    #[test]
    fn parse_gate_verdict_is_fail_closed_on_garbage() {
        let fallback = JsonObject::new();
        // Unknown behavior, missing behavior, and a non-object updatedInput must
        // all error (the caller turns the error into a denial) — never allow.
        assert!(parse_gate_verdict(&json!({"behavior": "maybe"}), &fallback).is_err());
        assert!(parse_gate_verdict(&json!({}), &fallback).is_err());
        assert!(
            parse_gate_verdict(
                &json!({"behavior": "allow", "updatedInput": "not-an-object"}),
                &fallback
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn local_model_endpoint_env_probe_requires_real_tool_call() -> anyhow::Result<()> {
        let base_url = match std::env::var("SYNAPSE_LOCAL_AGENT_ITEST_BASE_URL") {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                eprintln!(
                    "SKIP_LOCAL_MODEL_ENDPOINT_ITEST: set SYNAPSE_LOCAL_AGENT_ITEST_BASE_URL and SYNAPSE_LOCAL_AGENT_ITEST_MODEL to run against a real OpenAI-compatible local endpoint"
                );
                return Ok(());
            }
        };
        let model_id = match std::env::var("SYNAPSE_LOCAL_AGENT_ITEST_MODEL") {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                eprintln!(
                    "SKIP_LOCAL_MODEL_ENDPOINT_ITEST: SYNAPSE_LOCAL_AGENT_ITEST_MODEL is absent"
                );
                return Ok(());
            }
        };
        let allow_non_loopback = std::env::var("SYNAPSE_LOCAL_AGENT_ITEST_ALLOW_NON_LOOPBACK")
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);
        let row = LocalModelRegistryRow {
            name: "env-local-agent-itest".to_owned(),
            base_url,
            model_id: model_id.clone(),
            enabled: true,
            allow_non_loopback,
            api_key_env_var: None,
            api_shape: "open_ai_chat_completions".to_owned(),
            runtime_preset: None,
            context_length: None,
            max_tools: None,
            last_probe: Some(LocalModelProbe {
                observed_at_unix_ms: unix_time_ms_now(),
                healthy: true,
                error_code: None,
                error_detail: None,
            }),
        };
        validate_registry_row(&row)?;
        let endpoint = chat_completions_endpoint(&row, allow_non_loopback)?;
        let nonce = format!("local-agent-itest-{}", Uuid::now_v7().simple());
        let request_body = json!({
            "model": model_id,
            "messages": [
                {
                    "role": "system",
                    "content": "Return no prose. Call the requested tool exactly once."
                },
                {
                    "role": "user",
                    "content": format!("Call synapse_probe with nonce {nonce:?}.")
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "synapse_probe",
                        "description": "Echo the provided nonce to prove structured tool calling works.",
                        "parameters": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "nonce": { "type": "string" }
                            },
                            "required": ["nonce"]
                        }
                    }
                }
            ],
            "tool_choice": {
                "type": "function",
                "function": { "name": "synapse_probe" }
            },
            "stream": false,
            "temperature": 0,
            "max_tokens": 128
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_mins(1))
            .build()?;
        let mut request = client
            .post(endpoint.clone())
            .header(header::ACCEPT, "application/json")
            .json(&request_body);
        if let Ok(token) = std::env::var("SYNAPSE_LOCAL_AGENT_ITEST_API_KEY") {
            if !token.trim().is_empty() {
                request = request.bearer_auth(token);
            }
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("real local endpoint {endpoint} request failed"))?;
        let status = response.status();
        let body = response.text().await.context("read endpoint response")?;
        assert!(
            status.is_success(),
            "real local endpoint returned HTTP {status}: {}",
            bounded_text(&body, 4000)
        );
        let completion = parse_non_stream_response(&body)?;
        let call = completion
            .tool_calls
            .iter()
            .find(|call| call.name == "synapse_probe")
            .context("real endpoint did not return a synapse_probe tool call")?;
        let args: Value = serde_json::from_str(&call.arguments)
            .context("real endpoint returned malformed tool arguments")?;
        assert_eq!(
            args.get("nonce").and_then(Value::as_str),
            Some(nonce.as_str()),
            "real endpoint tool call nonce must match"
        );
        eprintln!(
            "LOCAL_MODEL_ENDPOINT_ITEST_OK endpoint={} model={} prompt_tokens={:?} completion_tokens={:?}",
            endpoint,
            row.model_id,
            completion.usage.as_ref().map(|usage| usage.prompt_tokens),
            completion
                .usage
                .as_ref()
                .map(|usage| usage.completion_tokens)
        );
        Ok(())
    }

    fn test_local_agent_row() -> LocalModelRegistryRow {
        LocalModelRegistryRow {
            name: "deepseek-flash".to_owned(),
            base_url: "https://api.deepseek.com".to_owned(),
            model_id: "deepseek-v4-flash".to_owned(),
            enabled: true,
            allow_non_loopback: true,
            api_key_env_var: Some("DEEPSEEK_API_KEY".to_owned()),
            api_shape: "open_ai_chat_completions".to_owned(),
            runtime_preset: Some("deepseek_v4_flash_non_thinking".to_owned()),
            context_length: Some(1_000_000),
            max_tools: Some(128),
            last_probe: Some(LocalModelProbe {
                observed_at_unix_ms: unix_time_ms_now(),
                healthy: true,
                error_code: None,
                error_detail: None,
            }),
        }
    }

    #[test]
    fn validate_registry_row_refuses_stale_healthy_probe() {
        let mut row = test_local_agent_row();
        if let Some(probe) = row.last_probe.as_mut() {
            probe.observed_at_unix_ms =
                unix_time_ms_now().saturating_sub(LOCAL_MODEL_SPAWN_MAX_PROBE_AGE_MS + 1);
        }
        let error = validate_registry_row(&row).expect_err("stale healthy probe must be refused");
        assert!(
            error.to_string().contains("LOCAL_MODEL_PROBE_STALE"),
            "unexpected error: {error:?}"
        );
    }
}
