use std::{
    collections::BTreeMap,
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
    RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientInfo, Content, Implementation, JsonObject,
        Tool,
    },
    service::RunningService,
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
    pub context_char_limit: usize,
    pub tool_parse_retry_limit: u32,
    pub no_stream: bool,
    pub allow_non_loopback: bool,
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
    parse_error_count: u32,
    truncated_context_count: u32,
    http: reqwest::Client,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolExposure {
    Direct,
    Routed,
}

impl ToolExposure {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Routed => "routed",
        }
    }
}

pub(crate) async fn run_from_cli(cli: LocalAgentCli) -> anyhow::Result<ExitCode> {
    let mut runner = Runner::new(cli).await?;
    let result = runner.run_loop().await;
    let exit_code = match result {
        Ok(()) => {
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
        let openai_tools = match tool_exposure {
            ToolExposure::Direct => tools.iter().map(openai_tool_from_mcp).collect::<Vec<_>>(),
            ToolExposure::Routed => routed_harness_tools(),
        };
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
        let mut messages = Vec::new();
        messages.push(json!({
            "role": "system",
            "content": system_prompt(tool_exposure, &tools),
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
            parse_error_count: 0,
            truncated_context_count: 0,
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
            }))
            .await?;
        Ok(runner)
    }

    async fn run_loop(&mut self) -> anyhow::Result<()> {
        let started = Instant::now();
        let mut used_any_tool = false;
        for turn in 1..=self.cli.max_turns {
            if started.elapsed() > Duration::from_millis(self.cli.timeout_ms) {
                bail!(
                    "MODEL_ENDPOINT_UNREACHABLE: local-agent timeout exceeded before turn {turn}"
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
                if used_any_tool {
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
                    }))
                    .await?;
                    return Ok(());
                }
                bail!("MODEL_TOOLS_UNSUPPORTED: model returned no tool calls on the first turn");
            }
            used_any_tool = true;
            for call in completion.tool_calls {
                self.execute_tool_call(call).await?;
                self.drain_steering_inbox().await?;
            }
        }
        bail!(
            "LOCAL_AGENT_TURN_LIMIT: local model did not finish within {} turns",
            self.cli.max_turns
        )
    }

    async fn execute_tool_call(&mut self, call: OpenAiToolCall) -> anyhow::Result<()> {
        self.tool_call_count = self.tool_call_count.saturating_add(1);
        let routed = call.name == SYNAPSE_TOOL_CALL;
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

        let (tool_name, args) = if routed {
            match parse_routed_tool_call(&call.arguments) {
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

        let result = match self
            .mcp
            .peer()
            .call_tool(CallToolRequestParams::new(tool_name.clone()).with_arguments(args))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let detail = format!("SYNAPSE_TOOL_CALL_FAILED: {tool_name}: {error}");
                let result_value = json!({ "error": detail });
                self.messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": detail,
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
                    "error_code": "SYNAPSE_TOOL_CALL_FAILED",
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
                    "error_code": "SYNAPSE_TOOL_CALL_FAILED",
                    "tool_exposure": self.tool_exposure.as_str(),
                }))
                .await?;
                bail!("{detail}");
            }
        };
        let is_error = result.is_error.unwrap_or(false);
        let result_value = tool_result_value(&result);
        self.fail_if_tool_result_contains_control_shutdown(&tool_name, &result_value)
            .await?;
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
        Ok(())
    }

    async fn record_tool_parse_error(
        &mut self,
        call: &OpenAiToolCall,
        error: anyhow::Error,
    ) -> anyhow::Result<()> {
        self.parse_error_count = self.parse_error_count.saturating_add(1);
        let detail = format!("TOOL_CALL_ARGUMENTS_NOT_JSON: {error}");
        let result_value = json!({ "error": detail });
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
            "content": detail,
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
        let mut body = json!({
            "model": self.registry.model_id,
            "messages": self.messages,
            "tools": self.openai_tools,
            "tool_choice": "auto",
            "temperature": 0,
            "stream": !self.cli.no_stream,
        });
        apply_runtime_preset(&self.registry, &mut body);
        if !self.cli.no_stream {
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
        if self.cli.no_stream {
            let text = response.text().await.context("read non-stream response")?;
            parse_non_stream_response(&text)
        } else {
            parse_streaming_response(response).await
        }
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
                "parse_error_count": self.parse_error_count,
                "truncated_context_count": self.truncated_context_count,
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
                "parse_error_count": self.parse_error_count,
                "truncated_context_count": self.truncated_context_count,
                "usage": self.total_usage,
                "completed_at_unix_ms": unix_time_ms_now(),
            }),
        )
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

fn resolve_tool_exposure(row: &LocalModelRegistryRow, tool_count: usize) -> ToolExposure {
    match row.max_tools {
        Some(max_tools) if tool_count > max_tools => ToolExposure::Routed,
        _ => ToolExposure::Direct,
    }
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
    Ok(())
}

fn registry_runtime_preset(row: &LocalModelRegistryRow) -> &str {
    row.runtime_preset
        .as_deref()
        .unwrap_or("open_ai_compatible")
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
            "content": if completion.content.is_empty() { Value::Null } else { Value::String(completion.content.clone()) },
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

fn system_prompt(tool_exposure: ToolExposure, tools: &[Tool]) -> String {
    let base = "You are a local Synapse agent. Use the provided MCP tools to inspect and change state. Never invent tool results. When a task asks for a stored artifact, call the relevant Synapse tool and then read it back. Finish with a concise summary only after the needed tool calls have succeeded.";
    match tool_exposure {
        ToolExposure::Direct => format!(
            "{base}\n\nAll Synapse MCP tools from this session's strict tools/list are attached directly as model tools."
        ),
        ToolExposure::Routed => {
            let names = tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{base}\n\nThis provider has a lower function-count cap than the live Synapse tool surface, so tools are exposed through a routed harness. Call {catalog} to inspect exact tool schemas and call {call_tool} with a real Synapse tool name plus arguments to execute it. The routed harness can call every real Synapse MCP tool loaded by this session, including file, shell, browser/perception, agent, dashboard, and local-model tools. Live tool names: {names}",
                catalog = SYNAPSE_TOOL_CATALOG,
                call_tool = SYNAPSE_TOOL_CALL,
            )
        }
    }
}

fn error_code_from_detail(detail: &str) -> &str {
    for code in [
        "MODEL_ENDPOINT_UNREACHABLE",
        "MODEL_TOOLS_UNSUPPORTED",
        "LOCAL_AGENT_CONTEXT_OVERFLOW",
        "LOCAL_AGENT_INTERRUPTED",
        "LOCAL_AGENT_TURN_LIMIT",
        "AGENT_EVENT_INGRESS_WRITE_FAILED",
        "LOCAL_MODEL_UNHEALTHY",
        "LOCAL_MODEL_DISABLED",
        "LOCAL_MODEL_UNPROBED",
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
    fn tool_exposure_routes_when_provider_tool_cap_is_lower_than_synapse_surface() {
        let mut row = test_local_agent_row();
        row.max_tools = Some(128);
        assert_eq!(resolve_tool_exposure(&row, 141), ToolExposure::Routed);
        assert_eq!(resolve_tool_exposure(&row, 128), ToolExposure::Direct);
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
            .timeout(Duration::from_secs(60))
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
                healthy: true,
                error_code: None,
                error_detail: None,
            }),
        }
    }
}
