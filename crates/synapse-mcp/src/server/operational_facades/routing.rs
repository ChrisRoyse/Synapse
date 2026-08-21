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
        description = "Public storage facade for the <=40 MCP surface. operation=inspect/summary are read-only Calyx vault reports; operation=snapshot_gc_status is an O(1) independent read of the live vault's physical MVCC reclamation counters; snapshot_open/read/release expose bounded Aster MVCC time-travel through an explicit process-local lease and metadata-only exact logical-row readback; operation=anchors decodes physical Calyx Anchors CF rows for an exact source row; operation=row_read returns ONE exact physical CF_OBSERVATIONS row through its typed decoder; operation=search_rebuild rebuilds a persisted Calyx search generation; operation=transcript_order_status independently compares the complete retained CF_AGENT_TRANSCRIPTS set to CF_AGENT_TRANSCRIPT_ORDER and returns digests plus a revision token; operation=transcript_order_rebuild is maintenance-gated and accepts only that exact token, persists crash-recovery intent, rebuilds the derived projection, fully reconciles it, and publishes readable metadata; operation=panel_lifecycle reads or maintenance-gates revision-guarded Registry mutations; operation=intelligence exposes typed Calyx analysis sub-operations, including intelligence.operation=view_registry_measure (READ_STORAGE+WRITE_STORAGE unattended measurement that atomically publishes a bounded causal-view Registry row plus Assay Ledger entry) and intelligence.operation=view_registry_read (READ_STORAGE-only independent exact Registry/Ledger integrity readback); operation=gc_once, backup, and other state-changing operations are maintenance-gated and return physical readback. Synthetic probe-row writes are debug-only raw routes and are not part of this production facade. Unknown operations and mismatched operation payloads fail closed."
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
        description = "Public prompt-injection hygiene facade for the <=40 MCP surface. Read operations flags/report are normal-profile visible. scan_text without persistence is read-only; scan_text persist=true and scan_storage write CF_KV flag rows and are maintenance-gated with readback. operation=vault_verify is a read-only scheduled whole-vault verification (restore verifier + provenance hash chain, incremental by default, full_chain=true for a whole-Ledger re-hash) that runs under the vault maintenance guard shared with backup/erase and fails closed with SYNAPSE_HYGIENE_VAULT_VERIFY_FAILED on any non-green surface."
    )]
    pub async fn hygiene(
        &self,
        params: Parameters<HygieneParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<HygieneResponse>, ErrorData> {
        hygiene::handle(self, params, request_context).await
    }

    #[tool(
        description = "Public setup facade for the <=40 MCP surface. operation=status/doctor read host setup Source-of-Truth files, daemon pid/bind, and Codex MCP config. operation=repair is maintenance-gated. operation=launchd_service action=status/restart reads or maintenance-gates restart of the exact macOS gui/<euid>/com.synapse.mcp LaunchAgent with durable before/after reconciliation; it never accepts a caller-supplied command or label. operation=host_transition action=status/configure/preflight/execute uses kernel boot identity, every durable shell status, configured WSL production leases/checkpoints, persisted authorization/intent records, and Windows Event 1074 to fail closed around planned restart/poweroff."
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
