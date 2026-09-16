use rmcp::{RoleServer, service::RequestContext};

use crate::server::{ErrorData, Json, Parameters, SynapseService, tool, tool_router};

use super::{
    hygiene, model, setup, storage, telemetry,
    types::{
        HygieneParams, HygieneResponse, ModelParams, ModelResponse, SetupParams, SetupResponse,
        StorageParams, StorageResponse, TelemetryParams, TelemetryResponse,
    },
};

#[tool_router(router = operational_facade_tool_router, vis = "pub(in crate::server)")]
impl SynapseService {
    #[tool(
        description = "Public storage facade for the <=40 MCP surface. operation=inspect/summary are read-only RocksDB CF reports; operation=gc_once is maintenance-gated and returns separate CF row-count/readback evidence. Synthetic probe-row writes are debug-only raw routes and are not part of this production facade. Unknown operations and mismatched operation payloads fail closed."
    )]
    pub async fn storage(
        &self,
        params: Parameters<StorageParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<StorageResponse>, ErrorData> {
        storage::handle(self, params, request_context).await
    }

    #[tool(
        description = "Public local-model facade for the <=40 MCP surface. operation=list/status read CF_KV registry rows; operation=probe writes real endpoint probe evidence; register/update/remove are maintenance-gated and require physical CF_KV readback."
    )]
    pub async fn model(
        &self,
        params: Parameters<ModelParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ModelResponse>, ErrorData> {
        model::handle(self, params, request_context).await
    }

    #[tool(
        description = "Public prompt-injection hygiene facade for the <=40 MCP surface. Read operations flags/report are normal-profile visible. scan_text without persistence is read-only; scan_text persist=true and scan_storage write CF_KV flag rows and are maintenance-gated with readback."
    )]
    pub async fn hygiene(
        &self,
        params: Parameters<HygieneParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<HygieneResponse>, ErrorData> {
        hygiene::handle(self, params, request_context).await
    }

    #[tool(
        description = "Public setup facade for the <=40 MCP surface. operation=status/doctor read host setup Source-of-Truth files, daemon pid/bind, and Codex MCP config. operation=repair is maintenance-gated and refuses normal-agent self-repair instead of silently mutating the running daemon."
    )]
    pub async fn setup(
        &self,
        params: Parameters<SetupParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<SetupResponse>, ErrorData> {
        setup::handle(self, params, request_context).await
    }

    #[tool(
        description = "Public telemetry facade for the <=40 MCP surface. operation=status returns profile/tool counts, model-facing tool payload bytes/token estimates, operation-level lifecycle usage aggregates, storage CF counters, and ingress counters from physical SoTs."
    )]
    pub async fn telemetry(
        &self,
        params: Parameters<TelemetryParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TelemetryResponse>, ErrorData> {
        telemetry::handle(self, params, request_context).await
    }
}
