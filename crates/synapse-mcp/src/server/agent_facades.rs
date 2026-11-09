use super::{
    ActSpawnAgentRequest, ActSpawnAgentResponse, AgentSpawnTaskStartedParams,
    AgentSpawnTaskStartedResponse, ErrorData, Json, Parameters, SynapseService,
    agent_control::{
        AgentInterruptParams, AgentInterruptResponse, AgentKillParams, AgentKillResponse,
        AgentPauseParams, AgentRespawnParams, AgentRespawnResponse, AgentSteerParams,
        AgentSteerResponse, AgentSuspendResponse,
    },
    agent_mailbox::{
        AgentInboxParams, AgentInboxResponse, AgentReceiptsParams, AgentReceiptsResponse,
        AgentSendBroadcastParams, AgentSendBroadcastResponse, AgentSendParams, AgentSendResponse,
        AgentWaitParams, AgentWaitResponse,
    },
    agent_query::{AgentQueryParams, AgentQueryResponse},
    agent_stats::{AgentStatsParams, AgentStatsResponse},
    agent_tasks::{
        EmptyParams, TaskCancelParams, TaskClaimParams, TaskCreateParams, TaskDispatchOnceParams,
        TaskDispatchOnceResponse, TaskGetResponse, TaskIdParams, TaskListParams, TaskListResponse,
        TaskMutationResponse, TaskNextParams, TaskNextResponse, TaskReconcileResponse,
        TaskUpdateParams,
    },
    agent_templates::{
        AgentTemplateDeleteParams, AgentTemplateDeleteResponse, AgentTemplateGetParams,
        AgentTemplateGetResponse, AgentTemplateListParams, AgentTemplateListResponse,
        AgentTemplatePutParams, AgentTemplatePutResponse,
    },
    tool, tool_router,
};

use rmcp::{RoleServer, model::ErrorCode, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;

const AGENT_TOOL: &str = "agent";
const TASK_TOOL: &str = "task";
const AGENT_SOURCE_OF_TRUTH: &str = "%LOCALAPPDATA%\\synapse\\agent-spawns + CF_AGENT_EVENTS/CF_AGENT_TRANSCRIPTS + CF_KV mailbox/template rows";
const TASK_SOURCE_OF_TRUTH: &str = "CF_KV agent task rows + agent task event/readback rows";

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperation {
    Spawn,
    Query,
    Send,
    Inbox,
    Wait,
    Broadcast,
    Receipts,
    Stats,
    TemplatePut,
    TemplateGet,
    TemplateList,
    TemplateDelete,
    TaskStarted,
    Interrupt,
    Kill,
    Steer,
    Pause,
    Resume,
    Respawn,
}

impl AgentOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Query => "query",
            Self::Send => "send",
            Self::Inbox => "inbox",
            Self::Wait => "wait",
            Self::Broadcast => "broadcast",
            Self::Receipts => "receipts",
            Self::Stats => "stats",
            Self::TemplatePut => "template_put",
            Self::TemplateGet => "template_get",
            Self::TemplateList => "template_list",
            Self::TemplateDelete => "template_delete",
            Self::TaskStarted => "task_started",
            Self::Interrupt => "interrupt",
            Self::Kill => "kill",
            Self::Steer => "steer",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Respawn => "respawn",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentParams {
    pub operation: AgentOperation,
    #[serde(default)]
    pub spawn: Option<ActSpawnAgentRequest>,
    #[serde(default)]
    pub query: Option<AgentQueryParams>,
    #[serde(default)]
    pub send: Option<AgentSendParams>,
    #[serde(default)]
    pub inbox: Option<AgentInboxParams>,
    #[serde(default)]
    pub wait: Option<AgentWaitParams>,
    #[serde(default)]
    pub broadcast: Option<AgentSendBroadcastParams>,
    #[serde(default)]
    pub receipts: Option<AgentReceiptsParams>,
    #[serde(default)]
    pub stats: Option<AgentStatsParams>,
    #[serde(default)]
    pub template_put: Option<AgentTemplatePutParams>,
    #[serde(default)]
    pub template_get: Option<AgentTemplateGetParams>,
    #[serde(default)]
    pub template_list: Option<AgentTemplateListParams>,
    #[serde(default)]
    pub template_delete: Option<AgentTemplateDeleteParams>,
    #[serde(default)]
    pub task_started: Option<AgentSpawnTaskStartedParams>,
    #[serde(default)]
    pub interrupt: Option<AgentInterruptParams>,
    #[serde(default)]
    pub kill: Option<AgentKillParams>,
    #[serde(default)]
    pub steer: Option<AgentSteerParams>,
    #[serde(default)]
    pub pause: Option<AgentPauseParams>,
    #[serde(default)]
    pub resume: Option<AgentPauseParams>,
    #[serde(default)]
    pub respawn: Option<AgentRespawnParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentResponse {
    pub operation: AgentOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn: Option<ActSpawnAgentResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<AgentQueryResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<AgentSendResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox: Option<AgentInboxResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<AgentWaitResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broadcast: Option<AgentSendBroadcastResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipts: Option<AgentReceiptsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<AgentStatsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_put: Option<AgentTemplatePutResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_get: Option<AgentTemplateGetResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_list: Option<AgentTemplateListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_delete: Option<AgentTemplateDeleteResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_started: Option<AgentSpawnTaskStartedResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt: Option<AgentInterruptResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill: Option<AgentKillResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steer: Option<AgentSteerResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<AgentSuspendResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<AgentSuspendResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub respawn: Option<AgentRespawnResponse>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOperation {
    Create,
    Get,
    Update,
    Claim,
    Cancel,
    List,
    Next,
    Reconcile,
    DispatchOnce,
}

impl TaskOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Get => "get",
            Self::Update => "update",
            Self::Claim => "claim",
            Self::Cancel => "cancel",
            Self::List => "list",
            Self::Next => "next",
            Self::Reconcile => "reconcile",
            Self::DispatchOnce => "dispatch_once",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskParams {
    pub operation: TaskOperation,
    #[serde(default)]
    pub create: Option<TaskCreateParams>,
    #[serde(default)]
    pub get: Option<TaskIdParams>,
    #[serde(default)]
    pub update: Option<TaskUpdateParams>,
    #[serde(default)]
    pub claim: Option<TaskClaimParams>,
    #[serde(default)]
    pub cancel: Option<TaskCancelParams>,
    #[serde(default)]
    pub list: Option<TaskListParams>,
    #[serde(default)]
    pub next: Option<TaskNextParams>,
    #[serde(default)]
    pub reconcile: Option<EmptyParams>,
    #[serde(default)]
    pub dispatch_once: Option<TaskDispatchOnceParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskResponse {
    pub operation: TaskOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<TaskMutationResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub get: Option<TaskGetResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<TaskMutationResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<TaskMutationResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<TaskMutationResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<TaskListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<TaskNextResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconcile: Option<TaskReconcileResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_once: Option<TaskDispatchOnceResponse>,
}

#[tool_router(router = agent_facade_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Facade for spawned-agent lifecycle, mailbox, stats, templates, and controls in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Every mutating operation delegates to the real lifecycle/mailbox/template/control implementation and returns its physical source-of-truth readback."
    )]
    pub async fn agent(
        &self,
        params: Parameters<AgentParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentResponse>, ErrorData> {
        validate_agent_facade_params(&params.0)?;
        let operation = params.0.operation;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = AGENT_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=agent"
        );
        match operation {
            AgentOperation::Spawn => {
                let spec = params.0.spawn.ok_or_else(|| missing_agent_spec("spawn"))?;
                let source_id = spec
                    .template_id
                    .clone()
                    .or_else(|| spec.prompt.clone())
                    .unwrap_or_else(|| "direct_spawn".to_owned());
                let response = self
                    .act_spawn_agent(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect the spawn directory, readiness artifact, session registry, and CF_AGENT_EVENTS rows before retrying",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    spawn_readback(&response),
                    |out| {
                        out.spawn = Some(response);
                    },
                )))
            }
            AgentOperation::Query => {
                let spec = params.0.query.ok_or_else(|| missing_agent_spec("query"))?;
                let source_id = spec.session_id.clone();
                let response = self
                    .agent_query(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide a real MCP session id or agent-spawn id and inspect CF_AGENT_EVENTS/CF_AGENT_TRANSCRIPTS",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    query_readback(&response),
                    |out| out.query = Some(response),
                )))
            }
            AgentOperation::Send => {
                let spec = params.0.send.ok_or_else(|| missing_agent_spec("send"))?;
                let source_id = spec.to_session.clone();
                let response = self
                    .agent_send(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "resolve the recipient to a live MCP session and inspect the mailbox CF_KV row",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "CF_KV mailbox row={} bytes={} sha256={}",
                        response.storage_readback.row_key,
                        response.storage_readback.value_len_bytes,
                        response.storage_readback.value_sha256
                    ),
                    |out| out.send = Some(response),
                )))
            }
            AgentOperation::Inbox => {
                let spec = params.0.inbox.ok_or_else(|| missing_agent_spec("inbox"))?;
                let response = self
                    .agent_inbox(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            "current_session",
                            error,
                            "inspect this session's mailbox CF_KV rows and retry with a valid filter",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "CF_KV mailbox scan session={} returned={} deleted={} remaining={}",
                        response.this_session_id,
                        response.returned_count,
                        response.deleted_count,
                        response.queue_depth_after
                    ),
                    |out| out.inbox = Some(response),
                )))
            }
            AgentOperation::Wait => {
                let spec = params.0.wait.ok_or_else(|| missing_agent_spec("wait"))?;
                let response = self
                    .agent_wait(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            "current_session",
                            error,
                            "inspect mailbox rows and timeout_ms before retrying wait",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "CF_KV mailbox wait session={} waited_ms={} timed_out={} returned={}",
                        response.inbox.this_session_id,
                        response.waited_ms,
                        response.timed_out,
                        response.inbox.returned_count
                    ),
                    |out| out.wait = Some(response),
                )))
            }
            AgentOperation::Broadcast => {
                let spec = params
                    .0
                    .broadcast
                    .ok_or_else(|| missing_agent_spec("broadcast"))?;
                let source_id = spec.kind.clone();
                let response = self
                    .agent_send_broadcast(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect resolved recipients and per-recipient mailbox row readbacks",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "CF_KV mailbox broadcast delivered={} skipped={} resolved={}",
                        response.delivered_count,
                        response.skipped_count,
                        response.resolved_recipients
                    ),
                    |out| out.broadcast = Some(response),
                )))
            }
            AgentOperation::Receipts => {
                let spec = params
                    .0
                    .receipts
                    .ok_or_else(|| missing_agent_spec("receipts"))?;
                let response = self
                    .agent_receipts(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            "current_session",
                            error,
                            "inspect this session's receipt-box CF_KV rows before retrying",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "CF_KV receipt scan session={} returned={} deleted={}",
                        response.this_session_id, response.returned_count, response.deleted_count
                    ),
                    |out| out.receipts = Some(response),
                )))
            }
            AgentOperation::Stats => {
                let spec = params.0.stats.ok_or_else(|| missing_agent_spec("stats"))?;
                let response = self.agent_stats(Parameters(spec)).await.map_err(|error| {
                    agent_delegate_error(
                        operation,
                        "fleet_or_agent",
                        error,
                        "inspect CF_AGENT_EVENTS scan bounds and requested group_by before retrying",
                    )
                })?.0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "CF_AGENT_EVENTS stats scan rows={} agents={}",
                        response.scanned_rows, response.agents_total
                    ),
                    |out| out.stats = Some(response),
                )))
            }
            AgentOperation::TemplatePut => {
                let spec = params
                    .0
                    .template_put
                    .ok_or_else(|| missing_agent_spec("template_put"))?;
                let source_id = spec.template_id.clone();
                let response = self
                    .agent_template_put(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect the template CF_KV row and fix template_id/model/prompt",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    template_rows_readback(&response.written_rows),
                    |out| out.template_put = Some(response),
                )))
            }
            AgentOperation::TemplateGet => {
                let spec = params
                    .0
                    .template_get
                    .ok_or_else(|| missing_agent_spec("template_get"))?;
                let source_id = spec.template_id.clone();
                let response = self
                    .agent_template_get(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide a durable template_id that exists in CF_KV",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!("CF_KV template row={}", response.row_key),
                    |out| out.template_get = Some(response),
                )))
            }
            AgentOperation::TemplateList => {
                let spec = params
                    .0
                    .template_list
                    .ok_or_else(|| missing_agent_spec("template_list"))?;
                let response = self
                    .agent_template_list(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            "templates",
                            error,
                            "inspect the CF_KV template prefix scan and max limit",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!("CF_KV template prefix scan count={}", response.count),
                    |out| out.template_list = Some(response),
                )))
            }
            AgentOperation::TemplateDelete => {
                let spec = params
                    .0
                    .template_delete
                    .ok_or_else(|| missing_agent_spec("template_delete"))?;
                let source_id = spec.template_id.clone();
                let response = self
                    .agent_template_delete(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an existing template_id and verify the CF_KV row is absent after delete",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!("deleted CF_KV template row={}", response.deleted_row_key),
                    |out| out.template_delete = Some(response),
                )))
            }
            AgentOperation::TaskStarted => {
                let spec = params
                    .0
                    .task_started
                    .ok_or_else(|| missing_agent_spec("task_started"))?;
                let source_id = spec.spawn_id.clone();
                let response = self
                    .agent_spawn_task_started(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect the spawn directory, manifest, MCP session id, and task-started artifact before retrying",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "task-started artifact path={} spawn_id={} session_id={} readiness_source={}",
                        response.task_started_path,
                        response.spawn_id,
                        response.session_id,
                        response.readiness_source
                    ),
                    |out| out.task_started = Some(response),
                )))
            }
            AgentOperation::Interrupt => {
                let spec = params
                    .0
                    .interrupt
                    .ok_or_else(|| missing_agent_spec("interrupt"))?;
                let source_id = spec.session_id.clone();
                let response = self
                    .agent_interrupt(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect clean-channel outcomes and process readback before retrying",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    agent_control_readback("interrupt", &response),
                    |out| out.interrupt = Some(response),
                )))
            }
            AgentOperation::Kill => {
                let spec = params.0.kill.ok_or_else(|| missing_agent_spec("kill"))?;
                let source_id = spec.session_id.clone();
                let response = self
                    .agent_kill(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect before/after process readback and agent event rows",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "agent kill process readback requested_id={} killed={} already_dead={} live_after={} orphans={}",
                        response.requested_id,
                        response.killed,
                        response.already_dead,
                        response.process_after.live_process_ids.len(),
                        response.orphan_process_ids.len()
                    ),
                    |out| out.kill = Some(response),
                )))
            }
            AgentOperation::Steer => {
                let spec = params.0.steer.ok_or_else(|| missing_agent_spec("steer"))?;
                let source_id = spec.session_id.clone();
                let response = self
                    .agent_steer(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect steering channel outcomes and receipt/mailbox rows",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "agent steer channel readback requested_id={} delivered={} channels={}",
                        response.requested_id,
                        response.delivered,
                        response.channels.len()
                    ),
                    |out| out.steer = Some(response),
                )))
            }
            AgentOperation::Pause => {
                let spec = params.0.pause.ok_or_else(|| missing_agent_spec("pause"))?;
                let source_id = spec.session_id.clone();
                let response = self
                    .agent_pause(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect thread suspension readback for the target process tree",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "agent pause thread readback requested_id={} ok={} live_processes={} applied={} failed={} all_suspended={}",
                        response.requested_id,
                        response.ok,
                        response.suspend.live_process_ids.len(),
                        response.suspend.applied_process_ids.len(),
                        response.suspend.failed.len(),
                        response.suspend.all_suspended
                    ),
                    |out| out.pause = Some(response),
                )))
            }
            AgentOperation::Resume => {
                let spec = params
                    .0
                    .resume
                    .ok_or_else(|| missing_agent_spec("resume"))?;
                let source_id = spec.session_id.clone();
                let response = self
                    .agent_resume(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect thread resume readback for the target process tree",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "agent resume thread readback requested_id={} ok={} live_processes={} applied={} failed={} all_running={}",
                        response.requested_id,
                        response.ok,
                        response.suspend.live_process_ids.len(),
                        response.suspend.applied_process_ids.len(),
                        response.suspend.failed.len(),
                        response.suspend.all_running
                    ),
                    |out| out.resume = Some(response),
                )))
            }
            AgentOperation::Respawn => {
                let spec = params
                    .0
                    .respawn
                    .ok_or_else(|| missing_agent_spec("respawn"))?;
                let source_id = spec.session_id.clone();
                let response = self
                    .agent_respawn(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        agent_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect the prior spawn manifest, new spawn directory, and lineage rows",
                        )
                    })?
                    .0;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "agent respawn prior_session={} prior_spawn={:?} new_spawn={} new_session={} prior_killed={} prior_already_dead={}",
                        response.prior_session_id,
                        response.prior_spawn_id,
                        response.new_spawn_id,
                        response.new_session_id,
                        response.prior_killed,
                        response.prior_already_dead
                    ),
                    |out| out.respawn = Some(response),
                )))
            }
        }
    }

    #[tool(
        description = "Facade for durable agent task queue operations in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Mutating operations return the task row readback from the real task implementation."
    )]
    pub async fn task(
        &self,
        params: Parameters<TaskParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskResponse>, ErrorData> {
        validate_task_facade_params(&params.0)?;
        let operation = params.0.operation;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TASK_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=task"
        );
        match operation {
            TaskOperation::Create => {
                let spec = params.0.create.ok_or_else(|| missing_task_spec("create"))?;
                let source_id = spec.task_id.clone();
                let response = self
                    .task_create(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix task_id/template_id/title and inspect the written CF_KV task row",
                        )
                    })?
                    .0;
                Ok(Json(task_response(
                    operation,
                    task_row_readback("created", &response),
                    |out| out.create = Some(response),
                )))
            }
            TaskOperation::Get => {
                let spec = params.0.get.ok_or_else(|| missing_task_spec("get"))?;
                let source_id = spec.task_id.clone();
                let response = self
                    .task_get(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide a durable task_id that exists in the task row store",
                        )
                    })?
                    .0;
                Ok(Json(task_response(
                    operation,
                    format!("CF_KV task row task_id={}", response.task.task_id),
                    |out| out.get = Some(response),
                )))
            }
            TaskOperation::Update => {
                let spec = params.0.update.ok_or_else(|| missing_task_spec("update"))?;
                let source_id = spec.task_id.clone();
                let response = self
                    .task_update(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            source_id,
                            error,
                            "read the current task state, use a valid transition, and inspect the written row",
                        )
                    })?
                    .0;
                Ok(Json(task_response(
                    operation,
                    task_row_readback("updated", &response),
                    |out| out.update = Some(response),
                )))
            }
            TaskOperation::Claim => {
                let spec = params.0.claim.ok_or_else(|| missing_task_spec("claim"))?;
                let source_id = spec.task_id.clone();
                let response = self
                    .task_claim(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            source_id,
                            error,
                            "claim only todo tasks with a real session id and inspect the written task row",
                        )
                    })?
                    .0;
                Ok(Json(task_response(
                    operation,
                    task_row_readback("claimed", &response),
                    |out| out.claim = Some(response),
                )))
            }
            TaskOperation::Cancel => {
                let spec = params.0.cancel.ok_or_else(|| missing_task_spec("cancel"))?;
                let source_id = spec.task_id.clone();
                let response = self
                    .task_cancel(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            source_id,
                            error,
                            "cancel only non-terminal tasks and inspect the terminal task row",
                        )
                    })?
                    .0;
                Ok(Json(task_response(
                    operation,
                    task_row_readback("cancelled", &response),
                    |out| out.cancel = Some(response),
                )))
            }
            TaskOperation::List => {
                let spec = params.0.list.ok_or_else(|| missing_task_spec("list"))?;
                let response = self
                    .task_list(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            "tasks",
                            error,
                            "inspect the task prefix scan and requested state/max filter",
                        )
                    })?
                    .0;
                Ok(Json(task_response(
                    operation,
                    format!(
                        "CF_KV task prefix scan count={} reconciled_orphans={}",
                        response.count,
                        response.reconciled_orphans.len()
                    ),
                    |out| out.list = Some(response),
                )))
            }
            TaskOperation::Next => {
                let spec = params.0.next.ok_or_else(|| missing_task_spec("next"))?;
                let response = self
                    .task_next(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            "dispatcher",
                            error,
                            "inspect in-flight task rows and concurrency cap before retrying",
                        )
                    })?
                    .0;
                Ok(Json(task_response(
                    operation,
                    format!(
                        "task dispatcher decision={} in_flight={} cap={}",
                        response.decision, response.in_flight, response.concurrency_cap
                    ),
                    |out| out.next = Some(response),
                )))
            }
            TaskOperation::Reconcile => {
                let spec = params
                    .0
                    .reconcile
                    .ok_or_else(|| missing_task_spec("reconcile"))?;
                let response = self
                    .task_reconcile(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            "tasks",
                            error,
                            "inspect in-progress task rows, spawn completion artifacts, and live sessions",
                        )
                    })?
                    .0;
                Ok(Json(task_response(
                    operation,
                    format!(
                        "task reconcile scanned_in_progress={} flagged_orphans={}",
                        response.scanned_in_progress,
                        response.flagged_orphans.len()
                    ),
                    |out| out.reconcile = Some(response),
                )))
            }
            TaskOperation::DispatchOnce => {
                let spec = params
                    .0
                    .dispatch_once
                    .ok_or_else(|| missing_task_spec("dispatch_once"))?;
                let response = self
                    .task_dispatch_once(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            "dispatcher",
                            error,
                            "inspect task row, spawn directory, readiness artifact, and failed attempt record",
                        )
                    })?
                    .0;
                Ok(Json(task_response(
                    operation,
                    format!(
                        "task dispatch decision={} spawn={}",
                        response.decision,
                        response
                            .spawn
                            .as_ref()
                            .map(|spawn| spawn.spawn_id.as_str())
                            .unwrap_or("<none>")
                    ),
                    |out| out.dispatch_once = Some(response),
                )))
            }
        }
    }
}

fn validate_agent_facade_params(params: &AgentParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        AGENT_TOOL,
        params.operation.as_str(),
        &[
            ("spawn", params.spawn.is_some()),
            ("query", params.query.is_some()),
            ("send", params.send.is_some()),
            ("inbox", params.inbox.is_some()),
            ("wait", params.wait.is_some()),
            ("broadcast", params.broadcast.is_some()),
            ("receipts", params.receipts.is_some()),
            ("stats", params.stats.is_some()),
            ("template_put", params.template_put.is_some()),
            ("template_get", params.template_get.is_some()),
            ("template_list", params.template_list.is_some()),
            ("template_delete", params.template_delete.is_some()),
            ("task_started", params.task_started.is_some()),
            ("interrupt", params.interrupt.is_some()),
            ("kill", params.kill.is_some()),
            ("steer", params.steer.is_some()),
            ("pause", params.pause.is_some()),
            ("resume", params.resume.is_some()),
            ("respawn", params.respawn.is_some()),
        ],
    )
}

fn validate_task_facade_params(params: &TaskParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        TASK_TOOL,
        params.operation.as_str(),
        &[
            ("create", params.create.is_some()),
            ("get", params.get.is_some()),
            ("update", params.update.is_some()),
            ("claim", params.claim.is_some()),
            ("cancel", params.cancel.is_some()),
            ("list", params.list.is_some()),
            ("next", params.next.is_some()),
            ("reconcile", params.reconcile.is_some()),
            ("dispatch_once", params.dispatch_once.is_some()),
        ],
    )
}

fn validate_exact_operation_spec(
    tool: &'static str,
    operation: &'static str,
    specs: &[(&'static str, bool)],
) -> Result<(), ErrorData> {
    let present = specs
        .iter()
        .filter_map(|(name, is_present)| is_present.then_some(*name))
        .collect::<Vec<_>>();
    if !present.contains(&operation) {
        return Err(facade_params_error(
            tool,
            operation,
            format!("{tool} operation={operation} requires a matching {operation} spec"),
            format!("pass {operation}={{...}} and no other operation spec"),
        ));
    }
    if present.len() != 1 {
        return Err(facade_params_error(
            tool,
            operation,
            format!("{tool} operation={operation} received invalid operation specs {present:?}"),
            format!("pass exactly one operation-specific spec matching {operation}"),
        ));
    }
    Ok(())
}

fn missing_agent_spec(operation: &'static str) -> ErrorData {
    facade_params_error(
        AGENT_TOOL,
        operation,
        format!("agent operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn missing_task_spec(operation: &'static str) -> ErrorData {
    facade_params_error(
        TASK_TOOL,
        operation,
        format!("task operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn facade_params_error(
    tool: &'static str,
    operation: &'static str,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> ErrorData {
    mcp_error_with_data(
        error_codes::TOOL_PARAMS_INVALID,
        message.into(),
        json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "source_of_truth": "typed facade params before delegated operation",
            "remediation": remediation.into(),
        }),
    )
}

fn agent_delegate_error(
    operation: AgentOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    delegate_error(
        AGENT_TOOL,
        operation.as_str(),
        AGENT_SOURCE_OF_TRUTH,
        source_id,
        error,
        remediation,
    )
}

fn task_delegate_error(
    operation: TaskOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    delegate_error(
        TASK_TOOL,
        operation.as_str(),
        TASK_SOURCE_OF_TRUTH,
        source_id,
        error,
        remediation,
    )
}

fn delegate_error(
    tool: &'static str,
    operation: &'static str,
    source_of_truth: &'static str,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let source_id = source_id.into();
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause_data = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "tool": tool,
            "operation": operation,
            "source_of_truth": source_of_truth,
            "source_id": source_id,
            "remediation": remediation,
            "cause": cause_data,
        })),
    )
}

fn mcp_error_with_data(_code: &'static str, message: String, data: Value) -> ErrorData {
    ErrorData::new(ErrorCode(-32099), message, Some(data))
}

fn agent_response(
    operation: AgentOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut AgentResponse),
) -> AgentResponse {
    let mut response = AgentResponse {
        operation,
        source_of_truth: format!(
            "{AGENT_SOURCE_OF_TRUTH} + delegated agent operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        spawn: None,
        query: None,
        send: None,
        inbox: None,
        wait: None,
        broadcast: None,
        receipts: None,
        stats: None,
        template_put: None,
        template_get: None,
        template_list: None,
        template_delete: None,
        task_started: None,
        interrupt: None,
        kill: None,
        steer: None,
        pause: None,
        resume: None,
        respawn: None,
    };
    populate(&mut response);
    response
}

fn task_response(
    operation: TaskOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut TaskResponse),
) -> TaskResponse {
    let mut response = TaskResponse {
        operation,
        source_of_truth: format!(
            "{TASK_SOURCE_OF_TRUTH} + delegated task operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        create: None,
        get: None,
        update: None,
        claim: None,
        cancel: None,
        list: None,
        next: None,
        reconcile: None,
        dispatch_once: None,
    };
    populate(&mut response);
    response
}

fn spawn_readback(response: &ActSpawnAgentResponse) -> String {
    format!(
        "spawn_id={} session_id={} task_readiness={} stdout={} stderr={}",
        response.spawn_id,
        response.session_id,
        response.task_readiness_source,
        response.log_paths.stdout_path,
        response.log_paths.stderr_path
    )
}

fn query_readback(response: &AgentQueryResponse) -> String {
    format!(
        "CF_AGENT_EVENTS/CF_AGENT_TRANSCRIPTS scan found={} events={} transcripts={}",
        response.found, response.scan.events_matched, response.scan.transcript_rows_scanned
    )
}

fn template_rows_readback(rows: &[super::agent_templates::TemplateRowReadback]) -> String {
    let rows = rows
        .iter()
        .map(|row| {
            format!(
                "{}:{}:{}:{}",
                row.cf_name, row.row_key, row.value_len_bytes, row.value_sha256
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("CF_KV template row writeback [{rows}]")
}

fn agent_control_readback(action: &'static str, response: &AgentInterruptResponse) -> String {
    format!(
        "agent {action} channel readback requested_id={} delivered={} channels={}",
        response.requested_id,
        response.delivered,
        response.channels.len()
    )
}

fn task_row_readback(action: &'static str, response: &TaskMutationResponse) -> String {
    format!(
        "CF_KV task {action} row={} bytes={} task_id={}",
        response.written_row.row_key, response.written_row.value_len_bytes, response.task.task_id
    )
}
