use rmcp::{RoleServer, service::RequestContext};
use serde_json::{Value, json};

use crate::{
    m3::{
        audit_export::export_audit_bundle,
        demo_recording::{demo_record_status_snapshot, start_demo_recording, stop_demo_recording},
        profile_registry::query_audit_intelligence,
        replay::record_replay,
    },
    server::{
        ErrorData, Json, Parameters, SynapseService,
        command_audit::{
            CommandAuditInput, CommandAuditLegacyProbeRepairParams,
            command_audit_error_from_error_data,
        },
        context::mcp_session_id_from_request_context,
        tool, tool_router,
    },
};

use super::{
    AUDIT_SOT, AUDIT_TOOL, DEFAULT_ARTIFACT_MAX_BYTES, DEFAULT_ARTIFACT_MAX_RECORDS, REPLAY_SOT,
    REPLAY_TOOL,
    artifact::inspect_replay_artifact,
    command_query::summarize_command_query,
    errors::{delegate_error, missing_spec, params_error},
    lifecycle::{lifecycle_path, read_lifecycle_tail},
    response::{audit_response, replay_response},
    types::{
        AuditLedgerEntryReadback, AuditLegacyProbeRepairResponse, AuditOperation, AuditParams,
        AuditRepairRowReadback, AuditReproduceResponse, AuditResponse,
        AuditSealAdjudicationResponse, AuditVerifyChainResponse, ReplayArtifactInspectParams,
        ReplayOperation, ReplayParams, ReplayResponse,
    },
    validation::{validate_audit_params, validate_replay_params},
};

/// Physical Source of Truth for the ledger-backed audit operations.
const LEDGER_SOT: &str =
    "CF_LEDGER append-only provenance hash chain + raw_commitment checkpoint-cohort Merkle seals";
#[tool_router(router = audit_replay_facade_tool_router, vis = "pub(in crate::server)")]
impl SynapseService {
    #[tool(
        description = "Public audit facade for the <=40 MCP surface. operation=command_query reads bounded CF_ACTION_LOG metadata without raw payloads (default is newest-first: with no start_key_hex/start_ts_ns it returns the most recent matches as a complete page and reports has_older + oldest_returned_ts_ns; supplying start_ts_ns or start_key_hex switches to forward paging) and fails closed with bounded key/value hash, length, and physical-revision diagnostics for every invalid row; repair_legacy_probe_row is an explicit maintenance-only, exact revision-guarded cleanup for positively identified #1540 synthetic envelopes and atomically appends a canonical repair audit row; lifecycle_events/lifecycle_exits read sanitized daemon JSONL ledgers; profile_intelligence summarizes profile-linked audit rows; export_bundle writes a redacted local bundle only with explicit consent; verify_chain re-walks and re-hashes the CF_LEDGER provenance hash chain, then verifies every raw_commitment Merkle cohort seal against the physical commitment CF (full or an incremental Ledger from_seq/to_seq window, with optional read_seq entry readback), and returns a fail-closed intact/broken/corrupt verdict plus the unsealed checkpoint-tail count; adjudicate_raw_commitment_seal is a maintenance-only, digest-guarded governance write for a cohort seal that is damaged beyond repair: a seal lives in the payload of an append-only Ledger entry, so a torn one can never be rewritten without breaking the chain it protects, and the verifier latches on its first seal failure - meaning one torn seal silently stops every later seal from being checked at all, forever. Adjudicating appends an Admin entry naming that one seal and the byte-exact diagnostic, after which verification of every later cohort resumes. It repairs nothing and hides nothing: the damage stays in the chain and in every readback, the cohort is never counted as verified, and the vault can never again report verified while the exception stands. It requires the exact raw_commitment_failure_sha256 that verify_chain currently reports, so an exception cannot outlive the exact damage it was authorized against; reproduce re-derives a record's recorded provenance binding by cx_id and bounds drift to a genuine ledger entry. Each operation requires exactly its matching payload object (pass verify_chain:{} for a full-chain verify)."
    )]
    pub async fn audit(
        &self,
        params: Parameters<AuditParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AuditResponse>, ErrorData> {
        let operation = validate_audit_params(&params.0)?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = AUDIT_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=audit"
        );
        match operation {
            AuditOperation::CommandQuery => {
                let spec = params
                    .0
                    .command_query
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), AUDIT_SOT))?;
                let response = self
                    .command_audit_query(spec.into())
                    .map_err(|error| delegate_error(AUDIT_TOOL, operation.as_str(), "CF_ACTION_LOG", AUDIT_SOT, error, "tighten the audit filters or repair CF_ACTION_LOG before retrying command_query"))?;
                let sanitized = summarize_command_query(response)?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "CF_ACTION_LOG scanned_rows={} returned_count={}",
                        sanitized.scanned_rows, sanitized.returned_count
                    ),
                    |out| out.command_query = Some(sanitized),
                )))
            }
            AuditOperation::RepairLegacyProbeRow => {
                let spec = params
                    .0
                    .repair_legacy_probe_row
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), AUDIT_SOT))?;
                crate::server::operational_facades::policy::require_maintenance_profile(
                    self,
                    &request_context,
                    AUDIT_TOOL,
                    operation.as_str(),
                    &spec.key_sha256,
                    AUDIT_SOT,
                )?;
                self.require_m3_permissions(
                    AUDIT_TOOL,
                    &crate::m3::permissions::required([
                        crate::m3::permissions::Permission::ReadStorage,
                        crate::m3::permissions::Permission::WriteStorage,
                    ]),
                )?;
                let actor_session_id = mcp_session_id_from_request_context(&request_context)?;
                let repaired = self.command_audit_repair_legacy_probe_row(
                    CommandAuditLegacyProbeRepairParams {
                        key_len_bytes: spec.key_len_bytes,
                        key_sha256: spec.key_sha256,
                        value_len_bytes: spec.value_len_bytes,
                        value_sha256: spec.value_sha256,
                        expected_revision_sha256: spec.expected_revision_sha256,
                        reason: spec.reason,
                        actor_session_id,
                    },
                )?;
                let response = AuditLegacyProbeRepairResponse {
                    source_of_truth: repaired.source_of_truth.to_owned(),
                    legacy_marker: repaired.legacy_marker,
                    previous_key_len_bytes: repaired.previous_key_len_bytes,
                    previous_key_sha256: repaired.previous_key_sha256,
                    previous_value_len_bytes: repaired.previous_value_len_bytes,
                    previous_value_sha256: repaired.previous_value_sha256,
                    previous_revision_sha256: repaired.previous_revision_sha256,
                    source_row_absent: repaired.source_row_absent,
                    committed_seq: repaired.committed_seq,
                    repair_audit: AuditRepairRowReadback {
                        cf_name: repaired.repair_audit.cf_name.to_owned(),
                        key_hex: repaired.repair_audit.key_hex,
                        value_len_bytes: repaired.repair_audit.value_len_bytes,
                        value_sha256: repaired.repair_audit.value_sha256,
                    },
                };
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "CF_ACTION_LOG legacy_key_sha256={} source_row_absent={} repair_key_hex={}",
                        response.previous_key_sha256,
                        response.source_row_absent,
                        response.repair_audit.key_hex
                    ),
                    |out| out.repair_legacy_probe_row = Some(response),
                )))
            }
            AuditOperation::LifecycleEvents => {
                let spec = params.0.lifecycle_events.unwrap_or_default();
                let path = lifecycle_path("tool_events_path")?;
                let response = read_lifecycle_tail(&path, &spec)?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "{} lines_read={} returned_count={}",
                        path.display(),
                        response.total_lines_read,
                        response.returned_count
                    ),
                    |out| out.lifecycle_events = Some(response),
                )))
            }
            AuditOperation::LifecycleExits => {
                let spec = params.0.lifecycle_exits.unwrap_or_default();
                let path = lifecycle_path("exit_events_path")?;
                let response = read_lifecycle_tail(&path, &spec)?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "{} lines_read={} returned_count={}",
                        path.display(),
                        response.total_lines_read,
                        response.returned_count
                    ),
                    |out| out.lifecycle_exits = Some(response),
                )))
            }
            AuditOperation::ProfileIntelligence => {
                let spec = params
                    .0
                    .profile_intelligence
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), AUDIT_SOT))?;
                self.require_m3_permissions(
                    AUDIT_TOOL,
                    &crate::m3::profile_registry::required_permissions_audit(&spec),
                )?;
                let reflex_runtime = self.reflex_runtime().map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        "reflex_runtime",
                        AUDIT_SOT,
                        error,
                        "repair M3 storage initialization before retrying profile_intelligence",
                    )
                })?;
                let response = query_audit_intelligence(&reflex_runtime, &spec).map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        &spec.profile_id,
                        AUDIT_SOT,
                        error,
                        "inspect profile id and audit CF health before retrying profile_intelligence",
                    )
                })?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "profile_id={} max_rows={}",
                        response.profile_id, response.max_rows
                    ),
                    |out| out.profile_intelligence = Some(response),
                )))
            }
            AuditOperation::ExportBundle => {
                let spec = params
                    .0
                    .export_bundle
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), AUDIT_SOT))?;
                self.require_m3_permissions(
                    AUDIT_TOOL,
                    &crate::m3::audit_export::required_permissions_bundle(&spec),
                )?;
                let reflex_runtime = self.reflex_runtime().map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        "reflex_runtime",
                        AUDIT_SOT,
                        error,
                        "repair M3 storage initialization before retrying export_bundle",
                    )
                })?;
                let response = export_audit_bundle(&reflex_runtime, &spec).map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        &spec.profile_id,
                        AUDIT_SOT,
                        error,
                        "provide explicit enabled strict consent and inspect consent/output-file readbacks",
                    )
                })?;
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "manifest={} rows={} redacted_fields={}",
                        response.manifest_path, response.rows_exported, response.redacted_fields
                    ),
                    |out| out.export_bundle = Some(response),
                )))
            }
            AuditOperation::AdjudicateRawCommitmentSeal => {
                let spec = params
                    .0
                    .adjudicate_raw_commitment_seal
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), LEDGER_SOT))?;
                crate::server::operational_facades::policy::require_maintenance_profile(
                    self,
                    &request_context,
                    AUDIT_TOOL,
                    operation.as_str(),
                    &spec.expected_failure_sha256,
                    LEDGER_SOT,
                )?;
                self.require_m3_permissions(
                    AUDIT_TOOL,
                    &crate::m3::permissions::required([
                        crate::m3::permissions::Permission::ReadStorage,
                        crate::m3::permissions::Permission::WriteStorage,
                    ]),
                )?;
                let db = self.m3_storage()?;
                let ledger_seq = spec.ledger_seq;
                let expected = spec.expected_failure_sha256.clone();
                let reason = spec.reason.clone();
                // The guard re-verifies the whole chain before writing, so this
                // is as heavy as a full verify_chain and must not sit on a
                // runtime worker serving MCP.
                let receipt = tokio::task::spawn_blocking(move || {
                    db.adjudicate_calyx_raw_commitment_seal(ledger_seq, &expected, &reason)
                })
                .await
                .map_err(|join_error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        "calyx_ledger",
                        LEDGER_SOT,
                        crate::m1::mcp_error(
                            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                            format!(
                                "adjudicate_raw_commitment_seal blocking task failed to join: {join_error}"
                            ),
                        ),
                        "inspect daemon logs; the seal adjudication task terminated abnormally",
                    )
                })?
                .map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        "calyx_ledger",
                        LEDGER_SOT,
                        crate::m1::mcp_error(error.code(), error.to_string()),
                        "re-run audit operation=verify_chain and adjudicate the exact seal and raw_commitment_failure_sha256 it reports",
                    )
                })?;
                let response = AuditSealAdjudicationResponse {
                    adjudicated_ledger_seq: receipt.adjudicated_ledger_seq,
                    diagnostic: receipt.diagnostic,
                    diagnostic_sha256: receipt.diagnostic_sha256,
                    reason: receipt.reason,
                    adjudication_ledger_seq: receipt.adjudication_ledger_seq,
                    adjudication_entry_hash: receipt.adjudication_entry_hash,
                };
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "CF_LEDGER adjudicated_seal_seq={} adjudication_ledger_seq={} adjudication_entry_hash={}",
                        response.adjudicated_ledger_seq,
                        response.adjudication_ledger_seq,
                        response.adjudication_entry_hash
                    ),
                    |out| out.adjudicate_raw_commitment_seal = Some(response),
                )))
            }
            AuditOperation::VerifyChain => {
                let spec = params
                    .0
                    .verify_chain
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), LEDGER_SOT))?;
                let range = match (spec.from_seq, spec.to_seq) {
                    (None, None) => None,
                    (Some(from), Some(to)) => Some((from, to)),
                    _ => {
                        return Err(params_error(
                            AUDIT_TOOL,
                            operation.as_str(),
                            "from_seq",
                            LEDGER_SOT,
                            "provide both from_seq and to_seq for an incremental window, or neither for a full-chain verify",
                        ));
                    }
                };
                let read_seq = spec.read_seq;
                let db = self.m3_storage()?;
                // Full-chain verification re-scans and re-hashes the whole
                // physical Ledger CF; it must never occupy a Tokio runtime worker
                // serving MCP. Offload to the blocking pool.
                let (verify, entry) = tokio::task::spawn_blocking(move || {
                    let verify = db.verify_calyx_ledger_chain(range)?;
                    let entry = match read_seq {
                        Some(seq) => Some(db.read_calyx_ledger_entry(seq)?),
                        None => None,
                    };
                    Ok::<_, synapse_storage::StorageError>((verify, entry))
                })
                .await
                .map_err(|join_error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        "calyx_ledger",
                        LEDGER_SOT,
                        crate::m1::mcp_error(
                            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                            format!("verify_chain blocking task failed to join: {join_error}"),
                        ),
                        "inspect daemon logs; the ledger verify task terminated abnormally",
                    )
                })?
                .map_err(|error| {
                    let remediation = error.remediation().unwrap_or(
                        "inspect the physical CF_LEDGER hash chain and restore from a verified backup before trusting audit output",
                    );
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        "calyx_ledger",
                        LEDGER_SOT,
                        crate::m1::mcp_error(error.code(), error.to_string()),
                        remediation,
                    )
                })?;
                let response = verify_chain_response(&verify, entry.as_ref());
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "CF_LEDGER verdict={} head_height={} verified=[{}..{}) entries={} raw_commitments={} raw_sealed={} raw_pending={} raw_seals={}",
                        response.verdict,
                        response.head_height,
                        response.verified_from_seq,
                        response.verified_to_seq,
                        response.entry_count,
                        response.raw_commitment_count,
                        response.raw_commitment_sealed_count,
                        response.raw_commitment_pending_count,
                        response.raw_commitment_seal_count,
                    ),
                    |out| out.verify_chain = Some(response),
                )))
            }
            AuditOperation::Reproduce => {
                let spec = params
                    .0
                    .reproduce
                    .ok_or_else(|| missing_spec(AUDIT_TOOL, operation.as_str(), LEDGER_SOT))?;
                let cx_id = spec.cx_id.trim().to_owned();
                if cx_id.is_empty() {
                    return Err(params_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        "cx_id",
                        LEDGER_SOT,
                        "cx_id must not be empty",
                    ));
                }
                let db = self.m3_storage()?;
                let cx_for_task = cx_id.clone();
                let report = tokio::task::spawn_blocking(move || {
                    db.reproduce_calyx_record(&cx_for_task)
                })
                .await
                .map_err(|join_error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        &cx_id,
                        LEDGER_SOT,
                        crate::m1::mcp_error(
                            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                            format!("reproduce blocking task failed to join: {join_error}"),
                        ),
                        "inspect daemon logs; the reproduce task terminated abnormally",
                    )
                })?
                .map_err(|error| {
                    delegate_error(
                        AUDIT_TOOL,
                        operation.as_str(),
                        &cx_id,
                        LEDGER_SOT,
                        crate::m1::mcp_error(error.code(), error.to_string()),
                        "confirm the cx_id exists and inspect its provenance ledger entry before retrying reproduce",
                    )
                })?;
                let response = reproduce_response(&report);
                Ok(Json(audit_response(
                    operation,
                    format!(
                        "CF_LEDGER reproduce cx_id={} reproduced={} recorded_seq={} drift={}",
                        response.cx_id, response.reproduced, response.recorded_seq, response.drift,
                    ),
                    |out| out.reproduce = Some(response),
                )))
            }
        }
    }

    #[tool(
        description = "Public replay facade for the <=40 MCP surface. operation=record writes a replay JSONL file and immediately inspects it; demo_status/demo_start/demo_stop manage explicit UIA demo recording through CF_KV/CF_TIMELINE; artifact_inspect validates replay JSONL bytes and structure without raw payload dumps."
    )]
    pub async fn replay(
        &self,
        params: Parameters<ReplayParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ReplayResponse>, ErrorData> {
        let operation = validate_replay_params(&params.0)?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = REPLAY_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=replay"
        );
        match operation {
            ReplayOperation::Record => {
                let spec = params
                    .0
                    .record
                    .ok_or_else(|| missing_spec(REPLAY_TOOL, operation.as_str(), REPLAY_SOT))?;
                self.require_m3_permissions(
                    REPLAY_TOOL,
                    &crate::m3::replay::required_permissions(&spec),
                )?;
                let sse_state = self.sse_state().map_err(|error| {
                    delegate_error(
                        REPLAY_TOOL,
                        operation.as_str(),
                        "sse_state",
                        REPLAY_SOT,
                        error,
                        "repair SSE state initialization before retrying replay record",
                    )
                })?;
                let response = record_replay(self.m1_state.clone(), sse_state, &spec)
                    .await
                    .map_err(|error| {
                        delegate_error(
                            REPLAY_TOOL,
                            operation.as_str(),
                            spec.path.as_deref().unwrap_or("default_replay_path"),
                            REPLAY_SOT,
                            error,
                            "fix replay target/format/path and inspect the replay artifact root",
                        )
                    })?;
                let artifact = inspect_replay_artifact(&ReplayArtifactInspectParams {
                    path: response.path.clone(),
                    max_bytes: DEFAULT_ARTIFACT_MAX_BYTES,
                    max_records: DEFAULT_ARTIFACT_MAX_RECORDS,
                })?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "{} bytes={} records_read={}",
                        artifact.path, artifact.bytes, artifact.records_read
                    ),
                    |out| {
                        out.record = Some(response);
                        out.artifact_readback = Some(artifact);
                    },
                )))
            }
            ReplayOperation::DemoStatus => {
                let _spec = params.0.demo_status.unwrap_or_default();
                let status = demo_record_status_snapshot(&self.m3_state).map_err(|error| {
                    delegate_error(
                        REPLAY_TOOL,
                        operation.as_str(),
                        "timeline/demo-record/v1",
                        REPLAY_SOT,
                        error,
                        "inspect CF_KV timeline/demo-record/v1 and retry demo_status",
                    )
                })?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "{} armed={} expired_active_row={}",
                        status.source_of_truth, status.armed, status.expired_active_row
                    ),
                    |out| out.demo_status = Some(status),
                )))
            }
            ReplayOperation::DemoStart => {
                let spec = params
                    .0
                    .demo_start
                    .ok_or_else(|| missing_spec(REPLAY_TOOL, operation.as_str(), REPLAY_SOT))?;
                self.require_m3_permissions(
                    REPLAY_TOOL,
                    &crate::m3::demo_recording::required_permissions_start(&spec),
                )?;
                let by_session = mcp_session_id_from_request_context(&request_context)?
                    .unwrap_or_else(|| "stdio".to_owned());
                let command_payload = json!({
                    "profile_id": &spec.profile_id,
                    "duration_ms": spec.duration_ms,
                    "path": &spec.path,
                    "label": &spec.label,
                });
                let command_before = json!({
                    "source_of_truth": REPLAY_SOT,
                    "by_session": &by_session,
                    "operation": "demo_start",
                });
                self.command_audit_intent(CommandAuditInput::mcp(
                    REPLAY_TOOL,
                    "demo_start",
                    Some(by_session.clone()),
                    Some(by_session.clone()),
                    command_payload.clone(),
                    command_before.clone(),
                    Value::Null,
                    "pending",
                ))?;
                let result = start_demo_recording(&self.m3_state, &spec, &by_session);
                match &result {
                    Ok(response) => {
                        self.command_audit_final(CommandAuditInput::mcp(
                            REPLAY_TOOL,
                            "demo_start",
                            Some(by_session.clone()),
                            Some(by_session),
                            command_payload,
                            command_before,
                            json!({
                                "source_of_truth": REPLAY_SOT,
                                "demo_id": response.demo_id,
                                "replay_path": response.replay_path,
                                "persisted": response.persisted,
                                "marker_row_written": response.marker_row_written,
                            }),
                            "ok",
                        ))?;
                    }
                    Err(error) => {
                        self.command_audit_final(
                            CommandAuditInput::mcp(
                                REPLAY_TOOL,
                                "demo_start",
                                Some(by_session.clone()),
                                Some(by_session),
                                command_payload,
                                command_before,
                                json!({
                                    "source_of_truth": REPLAY_SOT,
                                    "operation": "demo_start",
                                }),
                                "error",
                            )
                            .with_error(command_audit_error_from_error_data(error)),
                        )?;
                    }
                }
                let response = result.map_err(|error| {
                    delegate_error(
                        REPLAY_TOOL,
                        operation.as_str(),
                        &spec.profile_id,
                        REPLAY_SOT,
                        error,
                        "fix demo profile/duration/path and inspect CF_KV/CF_TIMELINE rows",
                    )
                })?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "demo_id={} persisted={} marker_row_written={}",
                        response.demo_id, response.persisted, response.marker_row_written
                    ),
                    |out| out.demo_start = Some(response),
                )))
            }
            ReplayOperation::DemoStop => {
                let spec = params
                    .0
                    .demo_stop
                    .ok_or_else(|| missing_spec(REPLAY_TOOL, operation.as_str(), REPLAY_SOT))?;
                self.require_m3_permissions(
                    REPLAY_TOOL,
                    &crate::m3::demo_recording::required_permissions_stop(&spec),
                )?;
                let by_session = mcp_session_id_from_request_context(&request_context)?
                    .unwrap_or_else(|| "stdio".to_owned());
                let command_payload = json!({
                    "demo_id": &spec.demo_id,
                });
                let command_before = json!({
                    "source_of_truth": REPLAY_SOT,
                    "by_session": &by_session,
                    "operation": "demo_stop",
                });
                self.command_audit_intent(CommandAuditInput::mcp(
                    REPLAY_TOOL,
                    "demo_stop",
                    Some(by_session.clone()),
                    Some(by_session.clone()),
                    command_payload.clone(),
                    command_before.clone(),
                    Value::Null,
                    "pending",
                ))?;
                let result = stop_demo_recording(&self.m3_state, &spec, &by_session);
                match &result {
                    Ok(response) => {
                        self.command_audit_final(CommandAuditInput::mcp(
                            REPLAY_TOOL,
                            "demo_stop",
                            Some(by_session.clone()),
                            Some(by_session),
                            command_payload,
                            command_before,
                            json!({
                                "source_of_truth": REPLAY_SOT,
                                "demo_id": response.demo_id,
                                "replay_path": response.replay_path,
                                "records_written": response.records_written,
                                "bytes": response.bytes,
                            }),
                            "ok",
                        ))?;
                    }
                    Err(error) => {
                        self.command_audit_final(
                            CommandAuditInput::mcp(
                                REPLAY_TOOL,
                                "demo_stop",
                                Some(by_session.clone()),
                                Some(by_session),
                                command_payload,
                                command_before,
                                json!({
                                    "source_of_truth": REPLAY_SOT,
                                    "operation": "demo_stop",
                                }),
                                "error",
                            )
                            .with_error(command_audit_error_from_error_data(error)),
                        )?;
                    }
                }
                let response = result.map_err(|error| {
                    delegate_error(
                        REPLAY_TOOL,
                        operation.as_str(),
                        spec.demo_id.as_deref().unwrap_or("active_demo_recording"),
                        REPLAY_SOT,
                        error,
                        "inspect active demo status and CF_TIMELINE DemoMarker rows before retrying demo_stop",
                    )
                })?;
                let artifact = inspect_replay_artifact(&ReplayArtifactInspectParams {
                    path: response.replay_path.clone(),
                    max_bytes: DEFAULT_ARTIFACT_MAX_BYTES,
                    max_records: DEFAULT_ARTIFACT_MAX_RECORDS,
                })?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "{} bytes={} records_read={}",
                        artifact.path, artifact.bytes, artifact.records_read
                    ),
                    |out| {
                        out.demo_stop = Some(response);
                        out.artifact_readback = Some(artifact);
                    },
                )))
            }
            ReplayOperation::ArtifactInspect => {
                let spec = params
                    .0
                    .artifact_inspect
                    .ok_or_else(|| missing_spec(REPLAY_TOOL, operation.as_str(), REPLAY_SOT))?;
                let response = inspect_replay_artifact(&spec)?;
                Ok(Json(replay_response(
                    operation,
                    format!(
                        "{} bytes={} records_read={}",
                        response.path, response.bytes, response.records_read
                    ),
                    |out| out.artifact_readback = Some(response),
                )))
            }
        }
    }
}

fn ledger_entry_readback(
    readback: &synapse_calyx::SynapseCalyxLedgerEntryReadback,
) -> AuditLedgerEntryReadback {
    AuditLedgerEntryReadback {
        seq: readback.seq,
        present: readback.present,
        kind: readback.kind.clone(),
        subject: readback.subject.clone(),
        actor: readback.actor.clone(),
        ts: readback.ts,
        prev_hash: readback.prev_hash.clone(),
        entry_hash: readback.entry_hash.clone(),
        payload_len: readback.payload_len,
        payload_sha256: readback.payload_sha256.clone(),
        self_verifies: readback.self_verifies,
    }
}

fn verify_chain_response(
    verify: &synapse_calyx::SynapseCalyxLedgerVerifyReport,
    entry: Option<&synapse_calyx::SynapseCalyxLedgerEntryReadback>,
) -> AuditVerifyChainResponse {
    AuditVerifyChainResponse {
        source_of_truth: LEDGER_SOT.to_owned(),
        intact: verify.intact,
        verdict: verify.verdict.clone(),
        head_height: verify.head_height,
        verified_from_seq: verify.verified_from_seq,
        verified_to_seq: verify.verified_to_seq,
        entry_count: verify.entry_count,
        tip_hash: verify.tip_hash.clone(),
        quarantine_seq: verify.quarantine_seq,
        broken_expected_hash: verify.broken_expected_hash.clone(),
        broken_found_hash: verify.broken_found_hash.clone(),
        corrupt_reason: verify.corrupt_reason.clone(),
        raw_commitments_intact: verify.raw_commitments_intact,
        raw_commitment_seal_count: verify.raw_commitment_seal_count,
        raw_commitment_count: verify.raw_commitment_count,
        raw_commitment_sealed_count: verify.raw_commitment_sealed_count,
        raw_commitment_pending_count: verify.raw_commitment_pending_count,
        raw_commitment_coverage_from_seq: verify.raw_commitment_coverage_from_seq,
        raw_commitment_sealed_through_seq: verify.raw_commitment_sealed_through_seq,
        raw_commitment_first_pending_seq: verify.raw_commitment_first_pending_seq,
        raw_commitment_failure: verify.raw_commitment_failure.clone(),
        raw_commitment_failure_sha256: verify.raw_commitment_failure_sha256.clone(),
        raw_commitment_adjudicated_count: verify.raw_commitment_adjudicated_count,
        raw_commitment_adjudicated_exceptions: verify.raw_commitment_adjudicated_exceptions.clone(),
        raw_commitment_failed_seal_count: verify.raw_commitment_failed_seal_count,
        raw_commitment_failed_seal_examples: verify.raw_commitment_failed_seal_examples.clone(),
        raw_commitment_uncovered_count: verify.raw_commitment_uncovered_count,
        reader_lease_duration_ms: verify.reader_lease_duration_ms,
        reader_lease_renewal_count: verify.reader_lease_renewal_count,
        covers_full_history: verify.covers_full_history,
        chain_origin: verify.chain_origin.clone(),
        history_coverage: verify.history_coverage.clone(),
        attested_from_seq: verify.attested_from_seq,
        vault_generation: verify.vault_generation,
        vault_reset_count: verify.vault_reset_count,
        predecessor_vault_id: verify.predecessor_vault_id.clone(),
        predecessor_high_water_seq: verify.predecessor_high_water_seq,
        entry_readback: entry.map(ledger_entry_readback),
    }
}

fn reproduce_response(
    report: &synapse_calyx::SynapseCalyxReproduceReport,
) -> AuditReproduceResponse {
    AuditReproduceResponse {
        source_of_truth: LEDGER_SOT.to_owned(),
        cx_id: report.cx_id.clone(),
        reproduced: report.reproduced,
        recorded_seq: report.recorded_seq,
        recorded_hash: report.recorded_hash.clone(),
        input_hash: report.input_hash.clone(),
        entry_present: report.entry_present,
        entry_hash: report.entry_hash.clone(),
        entry_self_verifies: report.entry_self_verifies,
        subject_matches: report.subject_matches,
        coverage: report.coverage.clone(),
        coverage_matches: report.coverage_matches,
        drift: report.drift.clone(),
    }
}
