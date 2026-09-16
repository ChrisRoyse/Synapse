use super::super::{ErrorData, Json, Parameters, SynapseService, tool, tool_router};
use super::{assist, reality, routine, verification};

use rmcp::{RoleServer, service::RequestContext};

#[tool_router(router = routine_assist_facade_tool_router, vis = "pub(in crate::server)")]
impl SynapseService {
    #[tool(
        description = "Facade for routine mining, listing, inspection, lifecycle updates, feedback, labels, automation candidate generation, and armed routine ticks in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Delegates to the existing routine_* implementation paths and returns CF_ROUTINES/CF_ROUTINE_STATE/CF_KV readback metadata."
    )]
    pub async fn routine(
        &self,
        params: Parameters<routine::RoutineParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<routine::RoutineResponse>, ErrorData> {
        routine::handle(self, params, request_context).await
    }

    #[tool(
        description = "Facade for intent and suggestion assist operations in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Delegates to intent_current/intent_detect_tick/suggestion_tick/suggestion_list/suggestion_accept and returns CF_KV suggestion plus intent-tracker readback metadata."
    )]
    pub async fn assist(
        &self,
        params: Parameters<assist::AssistParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<assist::AssistResponse>, ErrorData> {
        assist::handle(self, params, request_context).await
    }

    #[tool(
        description = "Facade for delta-first reality baseline, delta, and audit operations in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Delegates to reality_baseline/observe_delta/reality_audit and returns CF_KV reality row readback metadata."
    )]
    pub async fn reality(
        &self,
        params: Parameters<reality::RealityParams>,
    ) -> Result<Json<reality::RealityResponse>, ErrorData> {
        reality::handle(self, params).await
    }

    #[tool(
        description = "Facade for verification inbox, polling, audit, binding, and source-list operations in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Delegates to verification_* implementation paths and returns CF_KV audit/binding readback metadata."
    )]
    pub async fn verification(
        &self,
        params: Parameters<verification::VerificationParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<verification::VerificationResponse>, ErrorData> {
        verification::handle(self, params, request_context).await
    }
}
