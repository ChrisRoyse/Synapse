use super::{
    ActSpawnAgentRequest, ActSpawnAgentResponse, AgentSpawnTaskStartedParams,
    AgentSpawnTaskStartedResponse, ErrorData, Json, Parameters, SynapseService,
    agent_control::{
        AgentInterruptParams, AgentInterruptResponse, AgentKillParams, AgentKillResponse,
        AgentPauseParams, AgentRespawnParams, AgentRespawnResponse, AgentSteerParams,
        AgentSteerResponse, AgentSuspendResponse,
    },
    agent_mailbox::{
        AgentInboxParams, AgentInboxResponse, AgentMailboxRepairParams, AgentMailboxRepairResponse,
        AgentReceiptsParams, AgentReceiptsResponse, AgentSendBroadcastParams,
        AgentSendBroadcastResponse, AgentSendParams, AgentSendResponse, AgentWaitParams,
        AgentWaitResponse,
    },
    agent_query::{AgentQueryParams, AgentQueryResponse},
    agent_stats::{AgentStatsParams, AgentStatsResponse},
    agent_tasks::{
        EmptyParams, TaskCancelParams, TaskClaimParams, TaskCreateParams, TaskDispatchOnceParams,
        TaskDispatchOnceResponse, TaskGetResponse, TaskIdParams, TaskListParams, TaskListResponse,
        TaskMutationResponse, TaskNextParams, TaskNextResponse, TaskQueueStateRepairParams,
        TaskQueueStateRepairResponse, TaskReconcileResponse, TaskRowRepairParams,
        TaskRowRepairResponse, TaskSequenceRepairParams, TaskSequenceRepairResponse,
        TaskUpdateParams,
    },
    agent_templates::{
        AgentTemplateDeleteParams, AgentTemplateDeleteResponse, AgentTemplateGetParams,
        AgentTemplateGetResponse, AgentTemplateListParams, AgentTemplateListResponse,
        AgentTemplatePutParams, AgentTemplatePutResponse,
    },
    tool,
    tool_profiles::ToolProfileKind,
    tool_router,
};

use rmcp::{RoleServer, model::ErrorCode, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use synapse_core::error_codes;
use synapse_storage::{RevisionGuard, cf, constellations, decode_json};

const AGENT_TOOL: &str = "agent";
const TASK_TOOL: &str = "task";
const AGENT_SOURCE_OF_TRUTH: &str = "%LOCALAPPDATA%\\synapse\\agent-spawns + CF_AGENT_EVENTS/CF_AGENT_TRANSCRIPTS + CF_KV mailbox rows/durable queue state/template rows";
const TASK_SOURCE_OF_TRUTH: &str =
    "CF_KV agent task rows + guarded enqueue watermark + agent task event/readback rows";

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
    MailboxRepair,
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
    RecommendTools,
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
            Self::MailboxRepair => "mailbox_repair",
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
            Self::RecommendTools => "recommend_tools",
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
    pub mailbox_repair: Option<AgentMailboxRepairParams>,
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
    #[serde(default)]
    pub recommend_tools: Option<AgentToolRecommendParams>,
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
    pub mailbox_repair: Option<AgentMailboxRepairResponse>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommend_tools: Option<AgentToolRecommendResponse>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentToolRecommendParams {
    pub task_class: String,
    #[schemars(range(min = 1, max = 100000))]
    pub min_evidence: usize,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentToolEvidence {
    pub tool: String,
    pub successes: u64,
    pub failures: u64,
    pub evidence_count: u64,
    pub expected_success: f64,
    pub success_ci95_low: f64,
    pub success_ci95_high: f64,
    pub outcome_information_bits: f64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentToolRecommendResponse {
    pub task_class: String,
    pub grounding: String,
    pub evidence_count: u64,
    pub task_attempts_considered: u64,
    pub recommended_tools: Vec<String>,
    pub discouraged_tools: Vec<String>,
    pub tools: Vec<AgentToolEvidence>,
    pub failure_mode_arrows: Vec<Value>,
    pub failure_mode_arrow_grounding: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal_map_context: Option<Value>,
    pub decision_id: String,
    pub decision_row_key: String,
    pub decision_row_sha256: String,
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
    RepairSequence,
    RepairQueueState,
    RepairRow,
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
            Self::RepairSequence => "repair_sequence",
            Self::RepairQueueState => "repair_queue_state",
            Self::RepairRow => "repair_row",
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
    pub repair_sequence: Option<TaskSequenceRepairParams>,
    #[serde(default)]
    pub repair_queue_state: Option<TaskQueueStateRepairParams>,
    #[serde(default)]
    pub repair_row: Option<TaskRowRepairParams>,
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
    pub repair_sequence: Option<TaskSequenceRepairResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_queue_state: Option<TaskQueueStateRepairResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_row: Option<TaskRowRepairResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_once: Option<TaskDispatchOnceResponse>,
}

#[tool_router(router = agent_facade_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Facade for spawned-agent lifecycle, mailbox, stats, templates, and controls in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Every mutating operation delegates to the real lifecycle/mailbox/template/control implementation and returns its physical source-of-truth readback. mailbox_repair is an explicit break-glass/full-capability-only guarded metadata repair; normal mailbox use fails closed on corrupt or drifted queue state."
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
            AgentOperation::MailboxRepair => {
                let spec = params
                    .0
                    .mailbox_repair
                    .ok_or_else(|| missing_agent_spec("mailbox_repair"))?;
                let source_id = spec.recipient_session_id.clone();
                require_queue_repair_profile(
                    self,
                    &request_context,
                    AGENT_TOOL,
                    operation.as_str(),
                    &source_id,
                    AGENT_SOURCE_OF_TRUTH,
                )?;
                let response = self.mailbox_repair_state_impl(spec).map_err(|error| {
                    agent_delegate_error(
                        operation,
                        source_id,
                        error,
                        "inspect the raw global/recipient state rows and supply monotonic floors from the last known-good physical readback",
                    )
                })?;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "CF_KV mailbox state repaired global_seq={} recipient={} count={} generation={} committed_seq={}",
                        response.repaired_global_sequence,
                        response.recipient_session_id,
                        response.repaired_recipient_count,
                        response.repaired_recipient_generation,
                        response.committed_seq
                    ),
                    |out| out.mailbox_repair = Some(response),
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
            AgentOperation::RecommendTools => {
                let spec = params
                    .0
                    .recommend_tools
                    .ok_or_else(|| missing_agent_spec("recommend_tools"))?;
                self.require_m3_permissions(
                    AGENT_TOOL,
                    &crate::m3::permissions::required([
                        crate::m3::permissions::Permission::ReadStorage,
                        crate::m3::permissions::Permission::WriteStorage,
                    ]),
                )?;
                let db = self.m3_storage()?;
                let response = recommend_tools(&db, &spec.task_class, spec.min_evidence)?;
                self.audit_action_ok_with_details_for_request(
                    "steering_tool_recommend",
                    &serde_json::to_value(&response).map_err(|error| {
                        crate::m1::mcp_error(
                            error_codes::TOOL_INTERNAL_ERROR,
                            format!("STEERING_TOOL_DECISION_AUDIT_ENCODE_FAILED: {error}"),
                        )
                    })?,
                    &request_context,
                )?;
                Ok(Json(agent_response(
                    operation,
                    format!(
                        "CF_AGENT_EVENTS outcomes={} grounding={} decision_row={}",
                        response.evidence_count, response.grounding, response.decision_row_key
                    ),
                    |out| out.recommend_tools = Some(response),
                )))
            }
        }
    }

    #[tool(
        description = "Facade for durable agent task queue operations in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Mutating operations return physical task/coordination readback from the real implementation. repair_sequence, repair_queue_state, and repair_row are explicit break-glass/full-capability-only revision-guarded recovery operations; normal queue use fails closed on malformed or drifted state."
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
            TaskOperation::RepairSequence => {
                let spec = params
                    .0
                    .repair_sequence
                    .ok_or_else(|| missing_task_spec("repair_sequence"))?;
                require_queue_repair_profile(
                    self,
                    &request_context,
                    TASK_TOOL,
                    operation.as_str(),
                    TASK_SEQUENCE_KEY_SOURCE_ID,
                    TASK_SOURCE_OF_TRUTH,
                )?;
                let response = self
                    .task_repair_sequence_impl(spec)
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            "sequence_watermark",
                            error,
                            "inspect the raw watermark and task rows, then supply a monotonic floor from the last known-good physical readback",
                        )
                    })?;
                Ok(Json(task_response(
                    operation,
                    format!(
                        "CF_KV task sequence repaired={} observed_max={} committed_seq={}",
                        response.repaired_sequence,
                        response.observed_max_task_sequence,
                        response.committed_seq
                    ),
                    |out| out.repair_sequence = Some(response),
                )))
            }
            TaskOperation::RepairQueueState => {
                let spec = params
                    .0
                    .repair_queue_state
                    .ok_or_else(|| missing_task_spec("repair_queue_state"))?;
                require_queue_repair_profile(
                    self,
                    &request_context,
                    TASK_TOOL,
                    operation.as_str(),
                    "agent-task/v2/meta/queue_state",
                    TASK_SOURCE_OF_TRUTH,
                )?;
                let response = self
                    .task_repair_queue_state_impl(spec)
                    .map_err(|error| {
                        task_delegate_error(
                            operation,
                            "queue_state",
                            error,
                            "inspect the raw queue-state revision and task rows, then supply that exact revision and a monotonic generation floor",
                        )
                    })?;
                Ok(Json(task_response(
                    operation,
                    format!(
                        "CF_KV task queue state repaired={} observed_max={} committed_seq={:?}",
                        response.repaired_generation,
                        response.observed_max_task_generation,
                        response.committed_seq
                    ),
                    |out| out.repair_queue_state = Some(response),
                )))
            }
            TaskOperation::RepairRow => {
                let spec = params
                    .0
                    .repair_row
                    .ok_or_else(|| missing_task_spec("repair_row"))?;
                let source_id = spec.task_id.clone();
                require_queue_repair_profile(
                    self,
                    &request_context,
                    TASK_TOOL,
                    operation.as_str(),
                    source_id.as_str(),
                    TASK_SOURCE_OF_TRUTH,
                )?;
                let response = self.task_repair_row_impl(spec).map_err(|error| {
                    task_delegate_error(
                        operation,
                        source_id,
                        error,
                        "inspect the exact raw row revision plus queue/watermark state and provide a complete invariant-valid replacement",
                    )
                })?;
                Ok(Json(task_response(
                    operation,
                    format!(
                        "CF_KV task row repaired task_id={} generation={} committed_seq={:?}",
                        response.task.task_id,
                        response.task.mutation_generation,
                        response.committed_seq
                    ),
                    |out| out.repair_row = Some(response),
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

const TASK_SEQUENCE_KEY_SOURCE_ID: &str = "agent-task/v1/meta/last_enqueue_seq";

fn require_queue_repair_profile(
    service: &SynapseService,
    request_context: &RequestContext<RoleServer>,
    tool_name: &'static str,
    operation: &'static str,
    source_id: &str,
    source_of_truth: &'static str,
) -> Result<(), ErrorData> {
    let session_id = super::context::mcp_session_id_from_request_context(request_context)?;
    let snapshot = service.tool_profile_snapshot(session_id.as_deref())?;
    if matches!(
        snapshot.profile,
        ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability
    ) {
        return Ok(());
    }
    Err(ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool_name} operation={operation} is not allowed for profile {}",
            snapshot.profile.as_str()
        ),
        Some(json!({
            "code": error_codes::TOOL_PROFILE_POLICY_DENIED,
            "tool": tool_name,
            "operation": operation,
            "source_id": source_id,
            "profile": snapshot.profile.as_str(),
            "source_of_truth": source_of_truth,
            "remediation": "switch to an explicit break_glass or full_capability profile with operator intent before repairing durable queue metadata",
        })),
    ))
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
            ("mailbox_repair", params.mailbox_repair.is_some()),
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
            ("recommend_tools", params.recommend_tools.is_some()),
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
            ("repair_sequence", params.repair_sequence.is_some()),
            ("repair_queue_state", params.repair_queue_state.is_some()),
            ("repair_row", params.repair_row.is_some()),
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
        mailbox_repair: None,
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
        recommend_tools: None,
    };
    populate(&mut response);
    response
}

#[derive(Default)]
struct ToolOutcomeCounts {
    success: u64,
    failure: u64,
}

struct SteeringCausalEvidence {
    arrows: Vec<Value>,
    grounding: String,
    context: Option<Value>,
}

fn steering_causal_evidence(
    db: &synapse_storage::Db,
    task_class: &str,
    counts: &BTreeMap<String, ToolOutcomeCounts>,
) -> Result<SteeringCausalEvidence, ErrorData> {
    const MAX_FAILURE_MODE_ARROWS: usize = 256;
    let window_ns = i64::try_from(
        synapse_storage::derived_state::CAUSAL_MAP_WINDOW.as_nanos(),
    )
    .map_err(|_| {
        crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "STEERING_CAUSAL_MAP_WINDOW_OVERFLOW: configured causal-map window does not fit signed nanoseconds; remediation=repair the derived-state causal-map window contract",
        )
    })?;
    let mut params =
        synapse_calyx::SynapseCalyxTemporalParams::new(constellations::SYN_MCP_USAGE_PANEL_VERSION);
    params.max_records = synapse_storage::derived_state::CAUSAL_MAP_MAX_RECORDS;
    // Causal-map pointers are normalized by bounded window span, not wall-clock
    // endpoints. The independent reader follows the latest pointer and validates
    // the artifact against its own exact closed source window.
    params.since_ts_ns = Some(0);
    params.until_ts_ns = Some(window_ns);
    params.group_key = Some("mcp_usage_tool".to_owned());
    params.bin_seconds = synapse_storage::derived_state::CAUSAL_MAP_BIN_SECONDS;
    params.max_lag = synapse_storage::derived_state::CAUSAL_MAP_MAX_LAG;

    let report = match db.read_temporal_causal_map_intelligence(
        &params,
        synapse_storage::derived_state::CAUSAL_MAP_FDR_ALPHA,
    ) {
        Ok(report) => report,
        Err(error) if error.code() == "SYNAPSE_CALYX_CAUSAL_MAP_NOT_BUILT" => {
            let target_id = format!(
                "{}:{}",
                constellations::SYN_MCP_USAGE_PANEL_NAME,
                "mcp_usage_tool"
            );
            let maintenance_action = synapse_storage::derived_state::derived_state_readback()
                .last_causal_map_actions
                .get(&target_id)
                .cloned();
            tracing::warn!(
                code = "STEERING_CAUSAL_MAP_NOT_BUILT",
                task_class,
                target_id,
                maintenance_action = ?maintenance_action,
                detail = %error,
                "tool steering has no physically published causal-map generation; the decision remains explicitly provisional"
            );
            return Ok(SteeringCausalEvidence {
                arrows: Vec::new(),
                grounding: "provisional_causal_map_not_built".to_owned(),
                context: Some(json!({
                    "schema": "synapse.steering.causal_map_context.v1",
                    "status": "not_built",
                    "panel_name": constellations::SYN_MCP_USAGE_PANEL_NAME,
                    "panel_version": constellations::SYN_MCP_USAGE_PANEL_VERSION,
                    "group_key": "mcp_usage_tool",
                    "maintenance_target": target_id,
                    "maintenance_action": maintenance_action,
                    "error": {
                        "code": error.code(),
                        "message": error.to_string(),
                        "remediation": error.remediation(),
                    },
                    "structural_effect_identified": false,
                })),
            });
        }
        Err(error) => {
            return Err(crate::m1::mcp_error(
                error.code(),
                format!(
                    "STEERING_CAUSAL_MAP_READ_FAILED: task_class={task_class} detail={error}; remediation={}",
                    error.remediation().unwrap_or(
                        "preserve the Graph pointer/artifact and repair the exact causal-map read failure"
                    )
                ),
            ));
        }
    };
    if !report.physical_readback_matches
        || !report.pointer_readback_matches
        || !report.artifact.all_requested_records_loaded
        || !report.artifact.all_stream_pairs_enumerated
        || report.artifact.structural_effect_identified
        || report.artifact.evidence_class != "observational_predictive"
    {
        return Err(crate::m1::mcp_error(
            error_codes::STORAGE_READ_FAILED,
            "STEERING_CAUSAL_MAP_CONTRACT_INVALID: the persisted map lacks complete physical readback/pair coverage or misstates observational evidence as a structural effect; remediation=preserve and rebuild the exact Graph causal-map generation",
        ));
    }

    // Agent transcripts preserve the client-qualified tool name
    // (`mcp__synapse__agent`), while MCP usage records persist the daemon route
    // (`agent`). Join those identities explicitly; unrelated local tools such
    // as PowerShell have no MCP-usage stream and are never coerced into one.
    let mut causal_counts: BTreeMap<String, ToolOutcomeCounts> = BTreeMap::new();
    for stream in &report.artifact.streams {
        for (observed_tool, outcome) in counts {
            if steering_tool_matches_route(observed_tool, &stream.name) {
                let aggregate = causal_counts.entry(stream.name.clone()).or_default();
                aggregate.success = aggregate.success.saturating_add(outcome.success);
                aggregate.failure = aggregate.failure.saturating_add(outcome.failure);
            }
        }
    }
    let failed_tools = causal_counts
        .iter()
        .filter(|(_, outcome)| outcome.failure > 0)
        .map(|(tool, _)| tool.as_str())
        .collect::<BTreeSet<_>>();
    let relevant_pairs = report
        .artifact
        .pairs
        .iter()
        .filter(|pair| {
            failed_tools.contains(pair.group_a.as_str())
                || failed_tools.contains(pair.group_b.as_str())
        })
        .collect::<Vec<_>>();
    if relevant_pairs.len() > MAX_FAILURE_MODE_ARROWS {
        return Err(crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "STEERING_CAUSAL_ARROW_LIMIT_EXCEEDED: {} complete relevant pairs exceed declared response limit {MAX_FAILURE_MODE_ARROWS}; remediation=raise the explicit response budget after measuring payload cost, never truncate causal evidence",
                relevant_pairs.len()
            ),
        ));
    }
    let arrows = relevant_pairs
        .iter()
        .map(|pair| {
            json!({
                "schema": "synapse.steering.failure_mode_arrow.v2",
                "task_class": task_class,
                "scope": "global_mcp_usage_rolling_window_with_task_class_outcome_overlay",
                "group_a": pair.group_a,
                "group_b": pair.group_b,
                "events_a": pair.events_a,
                "events_b": pair.events_b,
                "task_class_outcomes_a": steering_tool_outcome_json(causal_counts.get(&pair.group_a)),
                "task_class_outcomes_b": steering_tool_outcome_json(causal_counts.get(&pair.group_b)),
                "transfer_entropy": pair.transfer_entropy,
                "granger_a_to_b": pair.granger_a_to_b,
                "granger_b_to_a": pair.granger_b_to_a,
                "signed_lag_correlation": pair.cross_correlation,
                "convergent_cross_mapping": pair.convergent_cross_mapping,
                "temporal_cross_k": pair.temporal_cross_k,
                "evidence_class": report.artifact.evidence_class,
                "structural_effect_identified": false,
                "interpretation": "observed temporal association involving a tool with task-class failures; this does not identify the tool as a structural cause of failure",
            })
        })
        .collect::<Vec<_>>();
    let relevant_fdr_families = report
        .artifact
        .fdr_families
        .iter()
        .map(|family| {
            let decisions = family
                .decisions
                .iter()
                .filter(|decision| {
                    relevant_pairs.iter().any(|pair| {
                        steering_hypothesis_matches_pair(
                            &decision.hypothesis,
                            &pair.group_a,
                            &pair.group_b,
                        )
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "name": family.name,
                "method": family.method,
                "assumptions": family.assumptions,
                "alpha": family.alpha,
                "complete_family_hypotheses_tested": family.hypotheses_tested,
                "relevant_decisions": decisions,
            })
        })
        .collect::<Vec<_>>();
    let grounding = if failed_tools.is_empty() {
        "observational_causal_map_present_no_task_class_failures"
    } else if arrows.is_empty() {
        "observational_causal_map_present_no_matching_failure_pairs"
    } else {
        "observational_predictive_causal_map"
    };
    let context = json!({
        "schema": "synapse.steering.causal_map_context.v1",
        "status": "physically_verified",
        "panel_name": constellations::SYN_MCP_USAGE_PANEL_NAME,
        "panel_version": report.artifact.panel_version,
        "group_key": report.artifact.group_key,
        "pair_scope": report.artifact.pair_scope,
        "window": {
            "since_ts_ns": report.artifact.since_ts_ns,
            "until_ts_ns": report.artifact.until_ts_ns,
            "bin_seconds": report.artifact.bin_seconds,
            "max_lag": report.artifact.max_lag,
            "source_records": report.artifact.source_records,
            "source_fingerprint_sha256": report.artifact.source_fingerprint_sha256,
        },
        "coverage": {
            "streams": report.artifact.streams,
            "expected_pair_count": report.artifact.expected_pair_count,
            "all_requested_records_loaded": report.artifact.all_requested_records_loaded,
            "all_stream_pairs_enumerated": report.artifact.all_stream_pairs_enumerated,
            "resource_accounting": report.artifact.resource_accounting,
        },
        "global_estimators": {
            "pc_stable_skeleton": report.artifact.pc_stable_skeleton,
            "partial_correlation_network": report.artifact.partial_correlation_network,
            "hawkes_branching_graph": report.artifact.hawkes_branching_graph,
        },
        "bh_fdr_families": relevant_fdr_families,
        "evidence_class": report.artifact.evidence_class,
        "structural_effect_identified": report.artifact.structural_effect_identified,
        "structural_identification_reason": report.artifact.structural_identification_reason,
        "identification_requirements": report.artifact.identification_requirements,
        "physical_source_of_truth": {
            "column_family": "Graph",
            "artifact_key_hex": report.graph_key_hex,
            "artifact_sha256": report.graph_value_sha256,
            "pointer_key_hex": report.pointer_key_hex,
            "pointer_sha256": report.pointer_value_sha256,
            "physical_readback_matches": report.physical_readback_matches,
            "pointer_readback_matches": report.pointer_readback_matches,
        },
    });
    Ok(SteeringCausalEvidence {
        arrows,
        grounding: grounding.to_owned(),
        context: Some(context),
    })
}

fn steering_tool_outcome_json(outcome: Option<&ToolOutcomeCounts>) -> Value {
    outcome.map_or_else(
        || json!({ "observed_in_task_class": false, "successes": 0, "failures": 0 }),
        |outcome| {
            json!({
                "observed_in_task_class": true,
                "successes": outcome.success,
                "failures": outcome.failure,
            })
        },
    )
}

fn steering_tool_matches_route(observed_tool: &str, route: &str) -> bool {
    observed_tool == route
        || observed_tool
            .strip_prefix("mcp__synapse__")
            .is_some_and(|name| name == route)
}

fn steering_hypothesis_matches_pair(hypothesis: &str, group_a: &str, group_b: &str) -> bool {
    [
        format!("{group_a}->{group_b}@"),
        format!("{group_b}->{group_a}@"),
        format!("{group_a}<->{group_b}@"),
        format!("{group_b}<->{group_a}@"),
        format!("{group_a}<->{group_b}|"),
        format!("{group_b}<->{group_a}|"),
    ]
    .iter()
    .any(|prefix| hypothesis.starts_with(prefix))
}

fn recommend_tools(
    db: &synapse_storage::Db,
    task_class: &str,
    min_evidence: usize,
) -> Result<AgentToolRecommendResponse, ErrorData> {
    const MAX_EVENT_ROWS: usize = 2_000_000;
    const PAGE_ROWS: usize = 8_192;
    let task_class = task_class.trim();
    if task_class.is_empty() || min_evidence == 0 || min_evidence > 100_000 {
        return Err(crate::m1::mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "agent recommend_tools requires a nonblank task_class and min_evidence in 1..=100000",
        ));
    }
    let tasks = SynapseService::read_all_tasks(db)?;
    let mut spawn_ids = BTreeSet::new();
    let mut task_attempts_considered = 0_u64;
    for task in tasks.iter().filter(|task| task.template_id == task_class) {
        for attempt in &task.attempts {
            if !matches!(
                attempt.outcome,
                super::agent_tasks::AttemptOutcome::Succeeded
                    | super::agent_tasks::AttemptOutcome::Failed
            ) {
                continue;
            }
            if let Some(spawn_id) = attempt.spawn_id.as_deref() {
                spawn_ids.insert(spawn_id.to_owned());
                task_attempts_considered = task_attempts_considered.saturating_add(1);
            }
        }
    }

    let mut counts: BTreeMap<String, ToolOutcomeCounts> = BTreeMap::new();
    let mut scanned = 0_usize;
    let mut start = Vec::new();
    while !spawn_ids.is_empty() {
        let (rows, more) = db
            .scan_cf_from(cf::CF_AGENT_EVENTS, &start, PAGE_ROWS)
            .map_err(|error| crate::m1::mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_EVENT_ROWS {
                return Err(crate::m1::mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "STEERING_TOOL_EVENT_SCAN_BUDGET_EXHAUSTED: scanned more than {MAX_EVENT_ROWS} CF_AGENT_EVENTS rows; remediation=add a task-class tool-outcome aggregate before retrying this corpus"
                    ),
                ));
            }
            let event: synapse_core::AgentEventRecord = decode_json(value).map_err(|error| {
                crate::m1::mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "STEERING_TOOL_EVENT_ROW_INVALID: key_hex={} detail={error}; remediation=repair or quarantine the corrupt CF_AGENT_EVENTS row",
                        constellations::hex_encode(key)
                    ),
                )
            })?;
            if event.kind != synapse_core::AgentEventKind::ToolCallFinished
                || !event
                    .spawn_id
                    .as_deref()
                    .is_some_and(|spawn_id| spawn_ids.contains(spawn_id))
            {
                continue;
            }
            let tool = event
                .attributes
                .tool_name
                .as_deref()
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
                .ok_or_else(|| {
                    crate::m1::mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "STEERING_TOOL_NAME_ABSENT: terminal tool event key_hex={} has no tool name; remediation=repair agent-event ingestion before measuring tool outcomes",
                            constellations::hex_encode(key)
                        ),
                    )
                })?;
            let failed = super::agent_events::tool_call_error_present(&event);
            let cell = counts.entry(tool.to_owned()).or_default();
            if failed {
                cell.failure = cell.failure.saturating_add(1);
            } else {
                cell.success = cell.success.saturating_add(1);
            }
        }
        if !more {
            break;
        }
        let Some((last, _)) = rows.last() else { break };
        start = last.clone();
        start.push(0);
    }

    let total_success = counts.values().map(|cell| cell.success).sum::<u64>();
    let total_failure = counts.values().map(|cell| cell.failure).sum::<u64>();
    let total = total_success.saturating_add(total_failure);
    let mut tools = counts
        .iter()
        .map(|(tool, cell)| {
            let n = cell.success.saturating_add(cell.failure);
            let (low, high) = steering_wilson_interval(cell.success, n);
            AgentToolEvidence {
                tool: tool.clone(),
                successes: cell.success,
                failures: cell.failure,
                evidence_count: n,
                expected_success: (cell.success as f64 + 1.0) / (n as f64 + 2.0),
                success_ci95_low: low,
                success_ci95_high: high,
                outcome_information_bits: steering_indicator_mi(
                    cell.success,
                    cell.failure,
                    total_success,
                    total_failure,
                ),
            }
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| {
        right
            .expected_success
            .total_cmp(&left.expected_success)
            .then_with(|| right.evidence_count.cmp(&left.evidence_count))
            .then_with(|| left.tool.cmp(&right.tool))
    });
    let grounding = if total as usize >= min_evidence && tools.len() >= 2 {
        "grounded"
    } else {
        "provisional_insufficient_evidence"
    };
    let recommended_tools = tools
        .iter()
        .filter(|tool| tool.evidence_count as usize >= min_evidence && tool.success_ci95_low >= 0.5)
        .map(|tool| tool.tool.clone())
        .collect::<Vec<_>>();
    let discouraged_tools = tools
        .iter()
        .filter(|tool| tool.evidence_count as usize >= min_evidence && tool.success_ci95_high < 0.5)
        .map(|tool| tool.tool.clone())
        .collect::<Vec<_>>();
    let causal_evidence = steering_causal_evidence(db, task_class, &counts)?;
    let observed_ns = super::agent_events::unix_time_ns_now();
    let seed = serde_json::to_vec(&(
        &task_class,
        observed_ns,
        &tools,
        &causal_evidence.arrows,
        &causal_evidence.context,
    ))
    .map_err(|error| {
        crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("STEERING_TOOL_DECISION_ENCODE_FAILED: {error}"),
        )
    })?;
    let decision_id = steering_sha256(&seed);
    let decision_row_key = format!("steering/v1/decision/tool/{observed_ns}/{decision_id}");
    let row = serde_json::to_vec(&json!({
        "schema": "synapse.steering.tool_decision.v1",
        "decision_id": decision_id,
        "observed_unix_ns": observed_ns,
        "task_class": task_class,
        "grounding": grounding,
        "evidence_count": total,
        "task_attempts_considered": task_attempts_considered,
        "recommended_tools": recommended_tools,
        "discouraged_tools": discouraged_tools,
        "tools": tools,
        "failure_mode_arrows": causal_evidence.arrows,
        "failure_mode_arrow_grounding": causal_evidence.grounding,
        "causal_map_context": causal_evidence.context,
    }))
    .map_err(|error| {
        crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("STEERING_TOOL_DECISION_ENCODE_FAILED: {error}"),
        )
    })?;
    let mutation = db
        .mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [RevisionGuard::new(decision_row_key.as_bytes(), None)],
            std::iter::empty::<Vec<u8>>(),
            [(decision_row_key.as_bytes(), row.as_slice())],
        )
        .map_err(|error| {
            crate::m1::mcp_error(
                error.code(),
                format!("STEERING_TOOL_DECISION_COMMIT_FAILED: {error}"),
            )
        })?;
    if !mutation.applied {
        return Err(crate::m1::mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            "STEERING_TOOL_DECISION_ID_COLLISION: append-only decision key already exists",
        ));
    }
    let readback = db
        .get_cf(cf::CF_KV, decision_row_key.as_bytes())
        .map_err(|error| crate::m1::mcp_error(error.code(), error.to_string()))?;
    if readback.as_deref() != Some(row.as_slice()) {
        return Err(crate::m1::mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            "STEERING_TOOL_DECISION_READBACK_MISMATCH: committed bytes differ from the requested decision",
        ));
    }
    Ok(AgentToolRecommendResponse {
        task_class: task_class.to_owned(),
        grounding: grounding.to_owned(),
        evidence_count: total,
        task_attempts_considered,
        recommended_tools,
        discouraged_tools,
        tools,
        failure_mode_arrows: causal_evidence.arrows,
        failure_mode_arrow_grounding: causal_evidence.grounding,
        causal_map_context: causal_evidence.context,
        decision_id,
        decision_row_key,
        decision_row_sha256: steering_sha256(&row),
    })
}

fn steering_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn steering_wilson_interval(success: u64, total: u64) -> (f64, f64) {
    if total == 0 {
        return (0.0, 1.0);
    }
    let n = total as f64;
    let p = success as f64 / n;
    let z = 1.959_963_984_540_054_f64;
    let denominator = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denominator;
    let half = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / denominator;
    ((center - half).max(0.0), (center + half).min(1.0))
}

fn steering_indicator_mi(ms: u64, mf: u64, total_s: u64, total_f: u64) -> f64 {
    let total = total_s.saturating_add(total_f);
    if total == 0 {
        return 0.0;
    }
    let cells = [
        (ms, ms.saturating_add(mf), total_s),
        (mf, ms.saturating_add(mf), total_f),
        (
            total_s.saturating_sub(ms),
            total.saturating_sub(ms.saturating_add(mf)),
            total_s,
        ),
        (
            total_f.saturating_sub(mf),
            total.saturating_sub(ms.saturating_add(mf)),
            total_f,
        ),
    ];
    cells
        .into_iter()
        .filter(|(joint, row, col)| *joint > 0 && *row > 0 && *col > 0)
        .map(|(joint, row, col)| {
            let pxy = joint as f64 / total as f64;
            pxy * ((joint as f64 * total as f64) / (row as f64 * col as f64)).log2()
        })
        .sum::<f64>()
        .max(0.0)
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
        repair_sequence: None,
        repair_queue_state: None,
        repair_row: None,
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
