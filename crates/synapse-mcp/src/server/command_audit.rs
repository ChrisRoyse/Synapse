use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::{ErrorData, model::ErrorCode};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use synapse_storage::{
    RevisionGuard,
    action_log::{
        ACTION_LOG_KEY_LEN, ActionLogRowDiagnostic, ActionLogRowKind, COMMAND_AUDIT_ROW_KIND,
        diagnostic_for_invalid_row, validate_action_log_row,
    },
    cf,
};

use super::SynapseService;
use crate::m1::mcp_error;

const COMMAND_AUDIT_SCHEMA_VERSION: u32 = 1;
const COMMAND_AUDIT_PAYLOAD_MAX_BYTES: usize = 8192;
const COMMAND_AUDIT_SNAPSHOT_SCAN_LIMIT: usize = 1000;
const COMMAND_AUDIT_SNAPSHOT_ROW_LIMIT: usize = 100;
const COMMAND_AUDIT_QUERY_DEFAULT_LIMIT: usize = 100;
const COMMAND_AUDIT_QUERY_MAX_LIMIT: usize = 250;
const COMMAND_AUDIT_QUERY_DEFAULT_SCAN_LIMIT: usize = 1000;
const COMMAND_AUDIT_QUERY_MAX_SCAN_LIMIT: usize = 5000;
const COMMAND_AUDIT_QUERY_BATCH_ROWS: usize = 256;
const COMMAND_AUDIT_INTEGRITY_EXAMPLE_LIMIT: usize = 8;

static COMMAND_AUDIT_SEQ: AtomicU32 = AtomicU32::new(0);

#[derive(Default)]
struct CommandAuditIntegrityFailures {
    count: usize,
    examples: Vec<CommandAuditIntegrityFailure>,
}

#[derive(Clone, Debug, Serialize)]
struct CommandAuditIntegrityFailure {
    #[serde(flatten)]
    diagnostic: ActionLogRowDiagnostic,
    physical_revision_sha256: Option<String>,
    revision_readback_error: Option<String>,
}

impl CommandAuditIntegrityFailures {
    fn observe(
        &mut self,
        db: &synapse_storage::Db,
        key: &[u8],
        value: &[u8],
        error: &synapse_storage::action_log::ActionLogCodecError,
    ) {
        self.count = self.count.saturating_add(1);
        if self.examples.len() < COMMAND_AUDIT_INTEGRITY_EXAMPLE_LIMIT {
            let diagnostic = diagnostic_for_invalid_row(key, value, error);
            let (physical_revision_sha256, revision_readback_error) = match db
                .get_cf_revisioned(cf::CF_ACTION_LOG, key)
            {
                Ok(Some(physical)) => (Some(revision_sha256_text(&physical.revision_sha256)), None),
                Ok(None) => (
                    None,
                    Some("physical row became absent during exact revision readback".to_owned()),
                ),
                Err(read_error) => (
                    None,
                    Some(format!(
                        "exact physical revision readback failed code={} detail={read_error}",
                        read_error.code()
                    )),
                ),
            };
            tracing::error!(
                code = "COMMAND_AUDIT_INTEGRITY_FAILURE",
                failure_code = diagnostic.failure_code,
                key_len_bytes = diagnostic.key_len_bytes,
                key_sha256 = %diagnostic.key_sha256,
                value_len_bytes = diagnostic.value_len_bytes,
                value_sha256 = %diagnostic.value_sha256,
                physical_revision_sha256 = physical_revision_sha256.as_deref(),
                revision_readback_error = revision_readback_error.as_deref(),
                failure_detail = %diagnostic.failure_detail,
                "audit read found an invalid CF_ACTION_LOG row and will fail closed"
            );
            self.examples.push(CommandAuditIntegrityFailure {
                diagnostic,
                physical_revision_sha256,
                revision_readback_error,
            });
        }
    }

    fn fail_if_any(&self, scanned_rows: usize, operation: &'static str) -> Result<(), ErrorData> {
        if self.count == 0 {
            return Ok(());
        }
        let failures_omitted = self.count.saturating_sub(self.examples.len());
        tracing::error!(
            code = "COMMAND_AUDIT_INTEGRITY_FAILED",
            operation,
            scanned_rows,
            failure_count = self.count,
            failures_reported = self.examples.len(),
            failures_omitted,
            "CF_ACTION_LOG read refused noncanonical physical rows"
        );
        Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "audit operation={operation} found noncanonical CF_ACTION_LOG rows; no partial result was returned"
            ),
            Some(json!({
                "code": synapse_core::error_codes::STORAGE_READ_FAILED,
                "failure_code": "COMMAND_AUDIT_INTEGRITY_FAILED",
                "operation": operation,
                "source_id": cf::CF_ACTION_LOG,
                "source_of_truth": "CF_ACTION_LOG physical rows",
                "scanned_rows": scanned_rows,
                "failure_count": self.count,
                "failure_examples": self.examples,
                "failure_example_limit": COMMAND_AUDIT_INTEGRITY_EXAMPLE_LIMIT,
                "failures_omitted": failures_omitted,
                "raw_key_value_omitted": true,
                "page_complete": false,
                "remediation": "stop trusting audit output; use audit operation=repair_legacy_probe_row only for a positively identified #1540 synthetic row, passing the exact hashes, lengths, and physical revision from this error; otherwise preserve the row and investigate its writer before any mutation",
            })),
        ))
    }
}

#[derive(Clone, Debug)]
pub(super) struct CommandAuditInput {
    pub tool: &'static str,
    pub verb: &'static str,
    pub channel: &'static str,
    pub actor_session_id: Option<String>,
    pub target_session_id: Option<String>,
    pub target: Option<Value>,
    pub payload: Value,
    pub before: Value,
    pub after: Value,
    pub outcome: &'static str,
    pub error: Option<CommandAuditError>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CommandAuditError {
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommandAuditRowReadback {
    pub cf_name: &'static str,
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    /// Exact content-addressed Calyx constellation measured from this physical
    /// action-log row. Supported action-family Oracles must use this identity;
    /// reconstructing a query from a tool name or audit id is forbidden.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constellation_cx_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommandAuditSnapshot {
    pub source_of_truth: &'static str,
    pub scanned_rows: usize,
    pub row_count: usize,
    pub rows: Vec<CommandAuditSnapshotRow>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommandAuditSnapshotRow {
    pub key_hex: String,
    pub audit_id: String,
    pub ts_ns: u64,
    pub phase: String,
    pub actor_session_id: Option<String>,
    pub tool: String,
    pub verb: String,
    pub channel: String,
    pub target_session_id: Option<String>,
    pub payload_sha256: Option<String>,
    pub payload_bounded: Option<Value>,
    pub payload_truncated: bool,
    pub target: Option<Value>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub outcome: String,
    pub error_code: Option<String>,
    pub source_of_truth: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CommandAuditQueryParams {
    pub limit: Option<usize>,
    pub scan_limit: Option<usize>,
    pub start_key_hex: Option<String>,
    pub start_ts_ns: Option<u64>,
    pub end_ts_ns: Option<u64>,
    pub session_id: Option<String>,
    pub tool: Option<String>,
    pub status: Option<String>,
    pub error_code: Option<String>,
    pub row_kind: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct CommandAuditLegacyProbeRepairParams {
    pub key_len_bytes: u64,
    pub key_sha256: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub expected_revision_sha256: String,
    pub reason: String,
    pub actor_session_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommandAuditLegacyProbeRepairResponse {
    pub source_of_truth: &'static str,
    pub legacy_marker: String,
    pub previous_key_len_bytes: u64,
    pub previous_key_sha256: String,
    pub previous_value_len_bytes: u64,
    pub previous_value_sha256: String,
    pub previous_revision_sha256: String,
    pub source_row_absent: bool,
    pub committed_seq: Option<u64>,
    pub repair_audit: CommandAuditRowReadback,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommandAuditQueryResponse {
    pub source_of_truth: &'static str,
    pub cf_name: &'static str,
    pub filters: CommandAuditQueryFilters,
    pub limit: usize,
    pub scan_limit: usize,
    pub scanned_rows: usize,
    pub matched_rows: usize,
    pub returned_count: usize,
    pub corrupt_row_count: usize,
    pub noncanonical_key_count: usize,
    pub partial: bool,
    pub exhausted: bool,
    pub start_key_hex: Option<String>,
    pub next_start_key_hex: Option<String>,
    /// Iteration direction actually applied. `"newest_first"` is the unwindowed
    /// default (no `start_key_hex`/`start_ts_ns`): it returns the most recent
    /// matches as a complete page. `"oldest_first"` is explicit forward paging
    /// (a `start_key_hex` or `start_ts_ns` was supplied) and keeps the
    /// fail-closed partial-page contract. #1550.
    pub scan_order: &'static str,
    /// True when matches older than the returned window exist. For newest-first
    /// this is an honest "there is more history", NOT a failure — page older by
    /// passing `end_ts_ns = oldest_returned_ts_ns`.
    pub has_older: bool,
    /// Timestamp of the oldest row returned this page (newest-first only), so a
    /// caller can continue older without guessing a `start_ts_ns` a priori.
    pub oldest_returned_ts_ns: Option<u64>,
    pub rows: Vec<CommandAuditQueryRow>,
}

pub(crate) const AUDIT_SCAN_ORDER_NEWEST_FIRST: &str = "newest_first";
pub(crate) const AUDIT_SCAN_ORDER_OLDEST_FIRST: &str = "oldest_first";

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommandAuditQueryFilters {
    pub start_ts_ns: Option<u64>,
    pub end_ts_ns: Option<u64>,
    pub session_id: Option<String>,
    pub tool: Option<String>,
    pub status: Option<String>,
    pub error_code: Option<String>,
    pub row_kind: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CommandAuditQueryRow {
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub row_kind: String,
    pub audit_id: String,
    pub ts_ns: u64,
    pub ts_ns_text: String,
    pub phase: Option<String>,
    pub status: Option<String>,
    pub outcome: Option<String>,
    pub session_id: Option<String>,
    pub actor_session_id: Option<String>,
    pub target_session_id: Option<String>,
    pub tool: String,
    pub verb: Option<String>,
    pub channel: Option<String>,
    pub error_code: Option<String>,
    pub payload_sha256: Option<String>,
    pub payload_truncated: Option<bool>,
    pub source_of_truth: Value,
    pub record: Value,
}

impl CommandAuditInput {
    pub(super) fn mcp(
        tool: &'static str,
        verb: &'static str,
        actor_session_id: Option<String>,
        target_session_id: Option<String>,
        payload: Value,
        before: Value,
        after: Value,
        outcome: &'static str,
    ) -> Self {
        Self {
            tool,
            verb,
            channel: "mcp",
            actor_session_id,
            target_session_id,
            target: None,
            payload,
            before,
            after,
            outcome,
            error: None,
        }
    }

    pub(super) fn with_target(mut self, target: Value) -> Self {
        self.target = Some(target);
        self
    }

    pub(super) fn with_error(mut self, error: CommandAuditError) -> Self {
        self.error = Some(error);
        self
    }

    pub(super) fn with_channel(mut self, channel: &'static str) -> Self {
        self.channel = channel;
        self
    }
}

impl SynapseService {
    pub(super) fn command_audit_intent(
        &self,
        input: CommandAuditInput,
    ) -> Result<CommandAuditRowReadback, ErrorData> {
        self.write_command_audit_row("intent", input)
    }

    pub(super) fn command_audit_final(
        &self,
        input: CommandAuditInput,
    ) -> Result<CommandAuditRowReadback, ErrorData> {
        self.write_command_audit_row("final", input)
    }

    pub(crate) fn command_audit_snapshot(&self) -> Result<CommandAuditSnapshot, ErrorData> {
        let db = self.m3_storage()?;
        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            command_audit_internal_error("reflex runtime lock poisoned while reading command audit")
        })?;
        let rows = runtime
            .storage_cf_tail_rows(cf::CF_ACTION_LOG, COMMAND_AUDIT_SNAPSHOT_SCAN_LIMIT)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let scanned_rows = rows.len();
        let mut parsed = Vec::new();
        let mut integrity_failures = CommandAuditIntegrityFailures::default();
        for (key, value) in rows.into_iter().rev() {
            let validated = match validate_action_log_row(&key, &value) {
                Ok(validated) => validated,
                Err(error) => {
                    integrity_failures.observe(&db, &key, &value, &error);
                    continue;
                }
            };
            if validated.row_kind != ActionLogRowKind::CommandAudit {
                continue;
            }
            if parsed.len() < COMMAND_AUDIT_SNAPSHOT_ROW_LIMIT {
                parsed.push(command_audit_snapshot_row(&key, &validated.value));
            }
        }
        integrity_failures.fail_if_any(scanned_rows, "command_snapshot")?;
        Ok(CommandAuditSnapshot {
            source_of_truth: cf::CF_ACTION_LOG,
            scanned_rows,
            row_count: parsed.len(),
            rows: parsed,
        })
    }

    pub(crate) fn command_audit_query(
        &self,
        params: CommandAuditQueryParams,
    ) -> Result<CommandAuditQueryResponse, ErrorData> {
        let limit = audit_query_limit(
            params.limit,
            COMMAND_AUDIT_QUERY_DEFAULT_LIMIT,
            COMMAND_AUDIT_QUERY_MAX_LIMIT,
            "limit",
        )?;
        let scan_limit = audit_query_limit(
            params.scan_limit,
            COMMAND_AUDIT_QUERY_DEFAULT_SCAN_LIMIT,
            COMMAND_AUDIT_QUERY_MAX_SCAN_LIMIT,
            "scan_limit",
        )?;
        let row_kind = normalize_row_kind_filter(params.row_kind.as_deref())?;
        let filters = CommandAuditQueryFilters {
            start_ts_ns: params.start_ts_ns,
            end_ts_ns: params.end_ts_ns,
            session_id: normalized_filter(params.session_id),
            tool: normalized_filter(params.tool),
            status: normalized_filter(params.status),
            error_code: normalized_filter(params.error_code),
            row_kind,
        };
        if let (Some(start), Some(end)) = (filters.start_ts_ns, filters.end_ts_ns) {
            if start > end {
                return Err(command_audit_params_error(
                    "audit query start_ts_ns must be <= end_ts_ns",
                ));
            }
        }

        // #1550: the natural unwindowed "what did X just do?" call supplies
        // neither a cursor nor a start timestamp. Oldest-first from an empty key
        // exhausts scan_limit deep in weeks-old history and hard-errors, so
        // default to a newest-first scan that returns the most recent matches as
        // a complete page. Any explicit cursor or start window keeps the forward
        // paging contract below unchanged.
        let start_key_hex_param = normalized_filter(params.start_key_hex);
        if start_key_hex_param.is_none() && filters.start_ts_ns.is_none() {
            return self.command_audit_query_newest_first(limit, scan_limit, filters);
        }
        let start_key = match start_key_hex_param {
            Some(start_key_hex) => {
                decode_hex(&start_key_hex).map_err(command_audit_params_error)?
            }
            None => filters
                .start_ts_ns
                .map(|start_ts_ns| command_audit_key(start_ts_ns, 0))
                .unwrap_or_default(),
        };
        let start_key_hex = (!start_key.is_empty()).then(|| hex_encode(&start_key));

        let db = self.m3_storage()?;
        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            command_audit_internal_error("reflex runtime lock poisoned while querying action audit")
        })?;

        let mut cursor = start_key;
        let mut scanned_rows = 0_usize;
        let mut matched_rows = 0_usize;
        let mut integrity_failures = CommandAuditIntegrityFailures::default();
        let mut returned = Vec::new();
        let mut next_start_key_hex = None;
        let mut more_after_window = false;
        let mut more_matching_rows = false;
        let mut stopped_at_end_ts = false;

        while scanned_rows < scan_limit {
            let remaining_scan = scan_limit.saturating_sub(scanned_rows);
            let batch_limit = remaining_scan.min(COMMAND_AUDIT_QUERY_BATCH_ROWS);
            if batch_limit == 0 {
                break;
            }
            let (batch, has_more) = runtime
                .storage_cf_rows_from(cf::CF_ACTION_LOG, &cursor, batch_limit)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            if batch.is_empty() {
                more_after_window = false;
                break;
            }
            more_after_window = has_more;
            let mut last_scanned_key: Option<Vec<u8>> = None;
            for (key, value) in batch {
                scanned_rows = scanned_rows.saturating_add(1);
                last_scanned_key = Some(key.clone());
                let validated = match validate_action_log_row(&key, &value) {
                    Ok(validated) => validated,
                    Err(error) => {
                        integrity_failures.observe(&db, &key, &value, &error);
                        continue;
                    }
                };
                let ts_ns = validated.ts_ns;
                let row = validated.value;
                if filters.end_ts_ns.is_some_and(|end| ts_ns > end) {
                    stopped_at_end_ts = true;
                    break;
                }
                if !audit_row_matches(&row, &filters) {
                    continue;
                }
                if returned.len() >= limit {
                    more_matching_rows = true;
                    next_start_key_hex = Some(hex_encode(&key));
                    break;
                }
                matched_rows = matched_rows.saturating_add(1);
                returned.push(command_audit_query_row(&key, &value, row));
                next_start_key_hex = Some(hex_encode(&key_after(&key)));
            }

            if let Some(last_key) = last_scanned_key {
                let resume_key = key_after(&last_key);
                if !more_matching_rows {
                    next_start_key_hex = Some(hex_encode(&resume_key));
                }
                cursor = resume_key;
            }

            if stopped_at_end_ts || more_matching_rows || !more_after_window {
                break;
            }
        }

        integrity_failures.fail_if_any(scanned_rows, "command_query")?;

        let scan_budget_exhausted =
            scanned_rows >= scan_limit && more_after_window && !stopped_at_end_ts;
        let partial = scan_budget_exhausted || more_matching_rows;
        if !partial {
            next_start_key_hex = None;
        }
        let returned_count = returned.len();
        Ok(CommandAuditQueryResponse {
            source_of_truth: "CF_ACTION_LOG bounded scan",
            cf_name: cf::CF_ACTION_LOG,
            filters,
            limit,
            scan_limit,
            scanned_rows,
            matched_rows,
            returned_count,
            corrupt_row_count: 0,
            noncanonical_key_count: 0,
            partial,
            exhausted: !partial,
            start_key_hex,
            next_start_key_hex,
            scan_order: AUDIT_SCAN_ORDER_OLDEST_FIRST,
            has_older: partial,
            oldest_returned_ts_ns: None,
            rows: returned,
        })
    }

    pub(crate) fn command_audit_repair_legacy_probe_row(
        &self,
        params: CommandAuditLegacyProbeRepairParams,
    ) -> Result<CommandAuditLegacyProbeRepairResponse, ErrorData> {
        validate_sha256_text(&params.key_sha256, "key_sha256")?;
        validate_sha256_text(&params.value_sha256, "value_sha256")?;
        let expected_revision =
            parse_sha256_text(&params.expected_revision_sha256, "expected_revision_sha256")?;
        if params.key_len_bytes == 0 || params.key_len_bytes > 512 {
            return Err(command_audit_params_error(
                "repair_legacy_probe_row key_len_bytes must be between 1 and 512",
            ));
        }
        if params.value_len_bytes == 0 || params.value_len_bytes > 1_048_576 {
            return Err(command_audit_params_error(
                "repair_legacy_probe_row value_len_bytes must be between 1 and 1048576",
            ));
        }
        let reason = params.reason.trim();
        if reason.is_empty() || reason.chars().count() > 512 {
            return Err(command_audit_params_error(
                "repair_legacy_probe_row reason must contain 1..=512 characters",
            ));
        }

        let db = self.m3_storage()?;
        let rows = db
            .scan_cf_tail(cf::CF_ACTION_LOG, COMMAND_AUDIT_QUERY_MAX_SCAN_LIMIT)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let mut matches = rows
            .into_iter()
            .filter(|(key, _value)| sha256_hex(key) == params.key_sha256)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(mcp_error(
                synapse_core::error_codes::STORAGE_CORRUPTED,
                format!(
                    "COMMAND_AUDIT_LEGACY_REPAIR_IDENTITY_UNRESOLVED: expected exactly one live row with key_sha256={} in the bounded {}-row CF_ACTION_LOG tail, found {}; no mutation occurred; remediation=rerun command_query for a fresh exact integrity diagnostic and investigate any omitted failure before retrying",
                    params.key_sha256,
                    COMMAND_AUDIT_QUERY_MAX_SCAN_LIMIT,
                    matches.len()
                ),
            ));
        }
        let (legacy_key, legacy_value) = matches.pop().ok_or_else(|| {
            command_audit_internal_error(
                "legacy repair identity selection became empty after exact cardinality validation",
            )
        })?;
        if legacy_key.len() as u64 != params.key_len_bytes
            || legacy_value.len() as u64 != params.value_len_bytes
            || sha256_hex(&legacy_value) != params.value_sha256
        {
            return Err(mcp_error(
                synapse_core::error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "COMMAND_AUDIT_LEGACY_REPAIR_CONTENT_MISMATCH: expected key_len={} value_len={} value_sha256={}, actual key_len={} value_len={} value_sha256={}; no mutation occurred; remediation=use one fresh command_query diagnostic without changing any field",
                    params.key_len_bytes,
                    params.value_len_bytes,
                    params.value_sha256,
                    legacy_key.len(),
                    legacy_value.len(),
                    sha256_hex(&legacy_value)
                ),
            ));
        }
        let legacy_marker = validate_issue1540_probe_row(&legacy_key, &legacy_value)?;
        let physical = db
            .get_cf_revisioned(cf::CF_ACTION_LOG, &legacy_key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .ok_or_else(|| {
                mcp_error(
                    synapse_core::error_codes::STORAGE_WRITE_FAILED,
                    "COMMAND_AUDIT_LEGACY_REPAIR_ROW_DISAPPEARED: exact physical row became absent before revision guard acquisition; no mutation occurred",
                )
            })?;
        if physical.value.as_deref() != Some(legacy_value.as_slice()) {
            return Err(mcp_error(
                synapse_core::error_codes::STORAGE_WRITE_FAILED,
                "COMMAND_AUDIT_LEGACY_REPAIR_LOGICAL_READBACK_MISMATCH: exact revisioned value differs from the bounded scan result; no mutation occurred",
            ));
        }
        if physical.revision_sha256 != expected_revision {
            return Err(mcp_error(
                synapse_core::error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "COMMAND_AUDIT_LEGACY_REPAIR_REVISION_MISMATCH: expected_revision_sha256={} actual_revision_sha256={}; no mutation occurred; remediation=rerun command_query and use its fresh exact physical revision",
                    params.expected_revision_sha256,
                    revision_sha256_text(&physical.revision_sha256)
                ),
            ));
        }

        let (repair_ts_ns, repair_seq) = next_command_audit_key_parts();
        let repair_key = command_audit_key(repair_ts_ns, repair_seq);
        let repair_key_hex = hex_encode(&repair_key);
        let mut audit_context = self.current_action_audit_context()?;
        audit_context.session_id = params.actor_session_id.clone();
        let payload = json!({
            "legacy_marker": legacy_marker,
            "key_len_bytes": params.key_len_bytes,
            "key_sha256": params.key_sha256,
            "value_len_bytes": params.value_len_bytes,
            "value_sha256": params.value_sha256,
            "previous_revision_sha256": params.expected_revision_sha256,
            "reason": reason,
        });
        let payload_bytes = synapse_storage::encode_json(&payload).map_err(|error| {
            command_audit_internal_error(format!("legacy repair payload encode failed: {error}"))
        })?;
        let repair_record = json!({
            "schema_version": COMMAND_AUDIT_SCHEMA_VERSION,
            "row_kind": COMMAND_AUDIT_ROW_KIND,
            "audit_id": format!("{repair_ts_ns:020}-{repair_seq:010}"),
            "ts_ns": repair_ts_ns,
            "seq": repair_seq,
            "phase": "final",
            "actor": {
                "channel": "mcp",
                "tool": "audit",
                "session_id": params.actor_session_id,
            },
            "audit_context": audit_context,
            "tool": "audit",
            "verb": "repair_legacy_probe_row",
            "channel": "mcp",
            "target_session_id": Value::Null,
            "target": Value::Null,
            "payload_sha256": sha256_hex(&payload_bytes),
            "payload_bytes": payload_bytes.len(),
            "payload_bounded": payload,
            "payload_truncated": false,
            "payload_hash_scope": "redacted_payload",
            "redacted": false,
            "redactions": [],
            "before": {
                "source_row_present": true,
                "key_sha256": params.key_sha256,
                "value_sha256": params.value_sha256,
                "physical_revision_sha256": params.expected_revision_sha256,
            },
            "after": {
                "source_row_present": false,
            },
            "outcome": "ok",
            "error_code": Value::Null,
            "error": Value::Null,
            "source_of_truth": {
                "cf_name": cf::CF_ACTION_LOG,
                "row_kind": COMMAND_AUDIT_ROW_KIND,
                "retention": "24h",
                "key_hex": repair_key_hex,
                "repaired_legacy_key_sha256": params.key_sha256,
            },
        });
        let repair_value = synapse_storage::encode_json(&repair_record).map_err(|error| {
            command_audit_internal_error(format!("legacy repair audit encode failed: {error}"))
        })?;

        let outcome = db.mutate_batch_if_revisions_pressure_bypass(
            cf::CF_ACTION_LOG,
            [
                RevisionGuard::new(legacy_key.clone(), Some(expected_revision)),
                RevisionGuard::new(repair_key.clone(), None),
            ],
            [legacy_key.clone()],
            [(repair_key.clone(), repair_value.clone())],
        );
        let committed_seq = match outcome {
            Ok(outcome) if outcome.applied => Some(outcome.committed_seq),
            Ok(outcome) => {
                return Err(mcp_error(
                    synapse_core::error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "COMMAND_AUDIT_LEGACY_REPAIR_CONFLICT: conflict_guard_index={:?} expected_revision_sha256={} actual_revision_sha256={}; no repair was applied; remediation=rerun command_query and rebase on the current exact row",
                        outcome
                            .conflict
                            .as_ref()
                            .map(|conflict| conflict.guard_index),
                        outcome
                            .conflict
                            .as_ref()
                            .and_then(|conflict| conflict.expected_revision_sha256)
                            .map_or_else(
                                || "absent".to_owned(),
                                |value| revision_sha256_text(&value)
                            ),
                        outcome
                            .conflict
                            .as_ref()
                            .and_then(|conflict| conflict.actual_revision_sha256)
                            .map_or_else(
                                || "absent".to_owned(),
                                |value| revision_sha256_text(&value)
                            ),
                    ),
                ));
            }
            Err(error) => {
                let legacy_after = db
                    .get_cf_revisioned(cf::CF_ACTION_LOG, &legacy_key)
                    .map_err(|read_error| {
                        mcp_error(
                            read_error.code(),
                            format!(
                                "COMMAND_AUDIT_LEGACY_REPAIR_COMMIT_AMBIGUOUS: commit failed ({error}) and source-row readback failed ({read_error})"
                            ),
                        )
                    })?;
                let repair_after = db
                    .get_cf(cf::CF_ACTION_LOG, &repair_key)
                    .map_err(|read_error| {
                        mcp_error(
                            read_error.code(),
                            format!(
                                "COMMAND_AUDIT_LEGACY_REPAIR_COMMIT_AMBIGUOUS: commit failed ({error}) and repair-audit readback failed ({read_error})"
                            ),
                        )
                    })?;
                if legacy_after.is_none()
                    && repair_after.as_deref() == Some(repair_value.as_slice())
                {
                    tracing::warn!(
                        code = "COMMAND_AUDIT_LEGACY_REPAIR_AMBIGUOUS_COMMIT_RECONCILED",
                        legacy_key_sha256 = %params.key_sha256,
                        repair_key_hex = %repair_key_hex,
                        "separate exact physical readback proved the guarded repair committed"
                    );
                    None
                } else {
                    return Err(mcp_error(
                        error.code(),
                        format!(
                            "COMMAND_AUDIT_LEGACY_REPAIR_NOT_COMMITTED: guarded atomic repair failed: {error}; exact source/repair readback did not prove the requested state"
                        ),
                    ));
                }
            }
        };

        let source_row_absent = db
            .get_cf_revisioned(cf::CF_ACTION_LOG, &legacy_key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .is_none();
        if !source_row_absent {
            return Err(mcp_error(
                synapse_core::error_codes::STORAGE_CORRUPTED,
                "COMMAND_AUDIT_LEGACY_REPAIR_DELETE_READBACK_FAILED: guarded commit returned applied but the exact legacy row remains present",
            ));
        }
        let repair_readback = db
            .get_cf(cf::CF_ACTION_LOG, &repair_key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .ok_or_else(|| {
                mcp_error(
                    synapse_core::error_codes::STORAGE_CORRUPTED,
                    "COMMAND_AUDIT_LEGACY_REPAIR_AUDIT_READBACK_MISSING: exact canonical repair audit row is absent after commit",
                )
            })?;
        if repair_readback != repair_value {
            return Err(mcp_error(
                synapse_core::error_codes::STORAGE_CORRUPTED,
                "COMMAND_AUDIT_LEGACY_REPAIR_AUDIT_READBACK_MISMATCH: exact canonical repair audit bytes differ after commit",
            ));
        }
        let repair_audit = CommandAuditRowReadback {
            cf_name: cf::CF_ACTION_LOG,
            key_hex: repair_key_hex,
            value_len_bytes: repair_readback.len() as u64,
            value_sha256: sha256_hex(&repair_readback),
            // The revision-guarded legacy cleanup writes one atomic raw repair
            // audit rather than publishing a causal action observation.
            constellation_cx_id: None,
        };
        tracing::warn!(
            code = "COMMAND_AUDIT_LEGACY_PROBE_ROW_REPAIRED",
            legacy_marker,
            legacy_key_sha256 = %params.key_sha256,
            legacy_value_sha256 = %params.value_sha256,
            previous_revision_sha256 = %params.expected_revision_sha256,
            repair_key_hex = %repair_audit.key_hex,
            repair_value_sha256 = %repair_audit.value_sha256,
            committed_seq,
            "exact revision-guarded #1540 probe cleanup committed with separate physical readback"
        );
        Ok(CommandAuditLegacyProbeRepairResponse {
            source_of_truth: "CF_ACTION_LOG exact legacy-row absence + canonical repair audit row",
            legacy_marker,
            previous_key_len_bytes: params.key_len_bytes,
            previous_key_sha256: params.key_sha256,
            previous_value_len_bytes: params.value_len_bytes,
            previous_value_sha256: params.value_sha256,
            previous_revision_sha256: params.expected_revision_sha256,
            source_row_absent,
            committed_seq,
            repair_audit,
        })
    }

    /// Newest-first bounded tail scan of `CF_ACTION_LOG` for the unwindowed
    /// default (#1550). Reuses the reverse-tail primitive already backing
    /// `command_audit_snapshot`, walks the most recent `scan_limit` rows from
    /// newest to oldest, and returns up to `limit` matches as a **complete
    /// page** — filling `limit` (or capping `scan_limit`) reports `has_older`
    /// honestly instead of hard-erroring the way forward paging must.
    fn command_audit_query_newest_first(
        &self,
        limit: usize,
        scan_limit: usize,
        filters: CommandAuditQueryFilters,
    ) -> Result<CommandAuditQueryResponse, ErrorData> {
        let db = self.m3_storage()?;
        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            command_audit_internal_error(
                "reflex runtime lock poisoned while querying action audit (newest-first)",
            )
        })?;
        // Ascending (oldest->newest) tail of at most scan_limit rows; iterate it
        // in reverse to emit newest-first. `tail_capped` means older rows exist
        // beyond this window.
        let tail = runtime
            .storage_cf_tail_rows(cf::CF_ACTION_LOG, scan_limit)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let tail_capped = tail.len() >= scan_limit;

        let mut scanned_rows = 0_usize;
        let mut matched_rows = 0_usize;
        let mut integrity_failures = CommandAuditIntegrityFailures::default();
        let mut returned = Vec::new();
        let mut has_older = false;

        for (key, value) in tail.into_iter().rev() {
            scanned_rows = scanned_rows.saturating_add(1);
            let validated = match validate_action_log_row(&key, &value) {
                Ok(validated) => validated,
                Err(error) => {
                    integrity_failures.observe(&db, &key, &value, &error);
                    continue;
                }
            };
            let ts_ns = validated.ts_ns;
            let key_seq = validated.seq;
            let row = validated.value;
            // In newest-first mode end_ts_ns is an upper bound: skip rows newer
            // than it (they are outside the requested window), keep scanning down.
            if filters.end_ts_ns.is_some_and(|end| ts_ns > end) {
                continue;
            }
            if !audit_row_matches(&row, &filters) {
                continue;
            }
            if returned.len() >= limit {
                has_older = true;
                continue;
            }
            matched_rows = matched_rows.saturating_add(1);
            returned.push((ts_ns, key_seq, command_audit_query_row(&key, &value, row)));
        }
        integrity_failures.fail_if_any(scanned_rows, "command_query")?;
        returned.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        let newest_start_key_hex = returned
            .first()
            .map(|(_ts_ns, _seq, row)| row.key_hex.clone());
        let oldest_returned_ts_ns = returned.last().map(|(ts_ns, _seq, _row)| *ts_ns);
        let returned: Vec<CommandAuditQueryRow> = returned
            .into_iter()
            .map(|(_ts_ns, _seq, row)| row)
            .collect();

        // If we consumed the whole scan window without filling `limit` but the
        // window itself was capped, older matches may still exist beyond it.
        if !has_older && tail_capped {
            has_older = true;
        }
        let returned_count = returned.len();
        Ok(CommandAuditQueryResponse {
            source_of_truth: "CF_ACTION_LOG newest-first bounded tail scan",
            cf_name: cf::CF_ACTION_LOG,
            filters,
            limit,
            scan_limit,
            scanned_rows,
            matched_rows,
            returned_count,
            corrupt_row_count: 0,
            noncanonical_key_count: 0,
            partial: false,
            exhausted: !has_older,
            start_key_hex: newest_start_key_hex,
            next_start_key_hex: None,
            scan_order: AUDIT_SCAN_ORDER_NEWEST_FIRST,
            has_older,
            oldest_returned_ts_ns,
            rows: returned,
        })
    }

    fn write_command_audit_row(
        &self,
        phase: &'static str,
        input: CommandAuditInput,
    ) -> Result<CommandAuditRowReadback, ErrorData> {
        let (ts_ns, seq) = next_command_audit_key_parts();
        let key = command_audit_key(ts_ns, seq);
        let key_hex = hex_encode(&key);
        let tool = input.tool;
        let verb = input.verb;
        let channel = input.channel;
        let outcome = input.outcome;
        let mut audit_context = self.current_action_audit_context()?;
        let actor_session_id = input
            .actor_session_id
            .clone()
            .or_else(crate::http::current_mcp_session_id)
            .or_else(|| audit_context.session_id.clone());
        audit_context.session_id = actor_session_id.clone();
        let payload_record = bounded_redacted_payload(&input.payload);
        let value = json!({
            "schema_version": COMMAND_AUDIT_SCHEMA_VERSION,
            "row_kind": COMMAND_AUDIT_ROW_KIND,
            "audit_id": format!("{ts_ns:020}-{seq:010}"),
            "ts_ns": ts_ns,
            "seq": seq,
            "phase": phase,
            "actor": {
                "channel": channel,
                "tool": tool,
                "session_id": actor_session_id,
                "profile_id": audit_context.profile_id,
                "profile_version": audit_context.profile_version,
                "profile_schema_version": audit_context.profile_schema_version,
            },
            "audit_context": audit_context,
            "tool": tool,
            "verb": verb,
            "channel": channel,
            "target_session_id": input.target_session_id.clone(),
            "target": input.target.clone(),
            "payload_sha256": payload_record.sha256,
            "payload_bytes": payload_record.bytes,
            "payload_bounded": payload_record.value,
            "payload_truncated": payload_record.truncated,
            "payload_hash_scope": "redacted_payload",
            "redacted": payload_record.redacted,
            "redactions": payload_record.redactions,
            "before": input.before.clone(),
            "after": input.after.clone(),
            "outcome": outcome,
            "error_code": input.error.as_ref().and_then(|error| error.code.clone()),
            "error": input.error.clone(),
            "source_of_truth": {
                "cf_name": cf::CF_ACTION_LOG,
                "row_kind": COMMAND_AUDIT_ROW_KIND,
                "retention": "24h",
                "key_hex": key_hex,
            },
        });
        let encoded = synapse_storage::encode_json(&value).map_err(|error| {
            command_audit_internal_error(format!("command audit row encode failed: {error}"))
        })?;
        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            command_audit_internal_error("reflex runtime lock poisoned while writing command audit")
        })?;
        let constellation_reports = runtime
            .storage_put_action_log_rows(vec![(key.clone(), encoded.clone())])
            .map_err(|error| {
                command_audit_internal_error(format!("command audit write failed: {error}"))
            })?;
        let [constellation_report] = constellation_reports.as_slice() else {
            return Err(command_audit_internal_error(format!(
                "command audit constellation measurement returned {} reports for one row key_hex={key_hex}",
                constellation_reports.len()
            )));
        };
        if constellation_report.source_key_hex != key_hex {
            return Err(command_audit_internal_error(format!(
                "command audit constellation source key mismatch: row={key_hex} measured={}",
                constellation_report.source_key_hex
            )));
        }
        let (readback_rows, _has_more) = runtime
            .storage_cf_rows_from(cf::CF_ACTION_LOG, &key, 1)
            .map_err(|error| {
            command_audit_internal_error(format!("command audit readback failed: {error}"))
        })?;
        let Some((read_key, read_value)) = readback_rows.first() else {
            return Err(command_audit_internal_error(format!(
                "command audit readback missing row key_hex={key_hex}"
            )));
        };
        if read_key != &key || read_value != &encoded {
            return Err(command_audit_internal_error(format!(
                "command audit readback mismatch key_hex={key_hex}"
            )));
        }
        let readback = CommandAuditRowReadback {
            cf_name: cf::CF_ACTION_LOG,
            key_hex,
            value_len_bytes: read_value.len() as u64,
            value_sha256: sha256_hex(read_value),
            constellation_cx_id: Some(constellation_report.cx_id.clone()),
        };
        tracing::info!(
            code = "COMMAND_AUDIT_RECORDED",
            tool,
            verb,
            phase,
            outcome,
            key_hex = %readback.key_hex,
            "command audit row written and read back"
        );
        Ok(readback)
    }
}

pub(super) fn command_audit_error_from_error_data(error: &ErrorData) -> CommandAuditError {
    CommandAuditError {
        code: error_data_code(error).map(str::to_owned),
        message: error.message.to_string(),
        data: error.data.clone(),
    }
}

#[derive(Debug)]
struct BoundedPayload {
    value: Value,
    sha256: String,
    bytes: usize,
    truncated: bool,
    redacted: bool,
    redactions: Vec<String>,
}

fn bounded_redacted_payload(payload: &Value) -> BoundedPayload {
    let mut redactions = Vec::new();
    let value = redact_value(payload, "$", &mut redactions);
    let encoded = serde_json::to_vec(&value).unwrap_or_else(|_error| b"null".to_vec());
    let bytes = encoded.len();
    let sha256 = sha256_hex(&encoded);
    let truncated = bytes > COMMAND_AUDIT_PAYLOAD_MAX_BYTES;
    let value = if truncated {
        let prefix =
            String::from_utf8_lossy(&encoded[..COMMAND_AUDIT_PAYLOAD_MAX_BYTES]).to_string();
        json!({
            "truncated_utf8_prefix": prefix,
            "omitted_bytes": bytes.saturating_sub(COMMAND_AUDIT_PAYLOAD_MAX_BYTES),
        })
    } else {
        value
    };
    BoundedPayload {
        value,
        sha256,
        bytes,
        truncated,
        redacted: !redactions.is_empty(),
        redactions,
    }
}

fn redact_value(value: &Value, path: &str, redactions: &mut Vec<String>) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let child_path = format!("{path}.{key}");
                if sensitive_key(key) {
                    redactions.push(child_path);
                    out.insert(key.clone(), Value::String("[REDACTED]".to_owned()));
                } else {
                    out.insert(key.clone(), redact_value(value, &child_path, redactions));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| redact_value(item, &format!("{path}[{index}]"), redactions))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "token",
        "password",
        "secret",
        "api_key",
        "apikey",
        "authorization",
        "bearer",
        "cookie",
        "credential",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn command_audit_snapshot_row(key: &[u8], row: &Value) -> CommandAuditSnapshotRow {
    let actor = row.get("actor").and_then(Value::as_object);
    CommandAuditSnapshotRow {
        key_hex: hex_encode(key),
        audit_id: string_field(row, "audit_id"),
        ts_ns: row.get("ts_ns").and_then(Value::as_u64).unwrap_or_default(),
        phase: string_field(row, "phase"),
        actor_session_id: actor
            .and_then(|actor| actor.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        tool: string_field(row, "tool"),
        verb: string_field(row, "verb"),
        channel: string_field(row, "channel"),
        target_session_id: row
            .get("target_session_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        payload_sha256: row
            .get("payload_sha256")
            .and_then(Value::as_str)
            .map(str::to_owned),
        payload_bounded: row.get("payload_bounded").cloned(),
        payload_truncated: row
            .get("payload_truncated")
            .and_then(Value::as_bool)
            .unwrap_or_default(),
        target: row.get("target").cloned(),
        before: row.get("before").cloned(),
        after: row.get("after").cloned(),
        outcome: string_field(row, "outcome"),
        error_code: row
            .get("error_code")
            .and_then(Value::as_str)
            .map(str::to_owned),
        source_of_truth: row.get("source_of_truth").cloned(),
    }
}

fn command_audit_query_row(key: &[u8], encoded_value: &[u8], row: Value) -> CommandAuditQueryRow {
    let row_kind = audit_row_kind(&row).to_owned();
    CommandAuditQueryRow {
        key_hex: hex_encode(key),
        value_len_bytes: encoded_value.len() as u64,
        value_sha256: sha256_hex(encoded_value),
        row_kind: row_kind.clone(),
        audit_id: string_field(&row, "audit_id"),
        ts_ns: audit_row_ts_ns(&row),
        ts_ns_text: audit_row_ts_ns(&row).to_string(),
        phase: optional_string_field(&row, "phase"),
        status: optional_string_field(&row, "status"),
        outcome: optional_string_field(&row, "outcome"),
        session_id: optional_string_field(&row, "session_id")
            .or_else(|| audit_context_session_id(&row)),
        actor_session_id: actor_session_id(&row),
        target_session_id: optional_string_field(&row, "target_session_id"),
        tool: string_field(&row, "tool"),
        verb: optional_string_field(&row, "verb"),
        channel: optional_string_field(&row, "channel"),
        error_code: optional_string_field(&row, "error_code"),
        payload_sha256: optional_string_field(&row, "payload_sha256"),
        payload_truncated: row.get("payload_truncated").and_then(Value::as_bool),
        source_of_truth: row.get("source_of_truth").cloned().unwrap_or_else(|| {
            json!({
                "cf_name": cf::CF_ACTION_LOG,
                "row_kind": row_kind,
                "key_hex": hex_encode(key),
                "retention": "24h",
            })
        }),
        record: row,
    }
}

fn audit_row_matches(row: &Value, filters: &CommandAuditQueryFilters) -> bool {
    if filters
        .start_ts_ns
        .is_some_and(|start| audit_row_ts_ns(row) < start)
    {
        return false;
    }
    if filters
        .end_ts_ns
        .is_some_and(|end| audit_row_ts_ns(row) > end)
    {
        return false;
    }
    if filters
        .row_kind
        .as_deref()
        .is_some_and(|row_kind| audit_row_kind(row) != row_kind)
    {
        return false;
    }
    if filters
        .session_id
        .as_deref()
        .is_some_and(|session_id| !audit_row_session_matches(row, session_id))
    {
        return false;
    }
    if filters
        .tool
        .as_deref()
        .is_some_and(|tool| row.get("tool").and_then(Value::as_str) != Some(tool))
    {
        return false;
    }
    if filters
        .status
        .as_deref()
        .is_some_and(|status| !audit_row_status_matches(row, status))
    {
        return false;
    }
    if filters
        .error_code
        .as_deref()
        .is_some_and(|error_code| row.get("error_code").and_then(Value::as_str) != Some(error_code))
    {
        return false;
    }
    true
}

fn audit_row_kind(row: &Value) -> &str {
    match row.get("row_kind").and_then(Value::as_str) {
        Some(COMMAND_AUDIT_ROW_KIND) => COMMAND_AUDIT_ROW_KIND,
        Some(value) => value,
        None => "action_audit",
    }
}

fn audit_row_ts_ns(row: &Value) -> u64 {
    row.get("ts_ns").and_then(Value::as_u64).unwrap_or_default()
}

fn audit_row_session_matches(row: &Value, session_id: &str) -> bool {
    [
        optional_string_field(row, "session_id"),
        actor_session_id(row),
        optional_string_field(row, "target_session_id"),
        audit_context_session_id(row),
        row.get("details")
            .and_then(|details| details.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    ]
    .into_iter()
    .flatten()
    .any(|candidate| candidate == session_id)
}

fn audit_row_status_matches(row: &Value, status: &str) -> bool {
    [
        row.get("status").and_then(Value::as_str),
        row.get("outcome").and_then(Value::as_str),
        row.get("phase").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .any(|candidate| candidate == status)
}

fn actor_session_id(row: &Value) -> Option<String> {
    row.get("actor")
        .and_then(|actor| actor.get("session_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn audit_context_session_id(row: &Value) -> Option<String> {
    row.get("audit_context")
        .and_then(|context| context.get("session_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn string_field(row: &Value, field: &str) -> String {
    row.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn optional_string_field(row: &Value, field: &str) -> Option<String> {
    row.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn audit_query_limit(
    value: Option<usize>,
    default: usize,
    max: usize,
    field: &'static str,
) -> Result<usize, ErrorData> {
    let value = value.unwrap_or(default);
    if value == 0 || value > max {
        return Err(command_audit_params_error(format!(
            "audit query {field} must be between 1 and {max}"
        )));
    }
    Ok(value)
}

fn normalized_filter(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

fn normalize_row_kind_filter(value: Option<&str>) -> Result<Option<String>, ErrorData> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    match value {
        "all" => Ok(None),
        "command" | "command_audit" => Ok(Some(COMMAND_AUDIT_ROW_KIND.to_owned())),
        "action" | "action_audit" => Ok(Some("action_audit".to_owned())),
        _ => Err(command_audit_params_error(
            "audit query row_kind must be all, command_audit, or action_audit",
        )),
    }
}

fn validate_sha256_text(value: &str, field: &'static str) -> Result<(), ErrorData> {
    parse_sha256_text(value, field).map(|_digest| ())
}

fn parse_sha256_text(value: &str, field: &'static str) -> Result<[u8; 32], ErrorData> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(command_audit_params_error(format!(
            "repair_legacy_probe_row {field} must use sha256:<64 lowercase hex>"
        )));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(command_audit_params_error(format!(
            "repair_legacy_probe_row {field} must use sha256:<64 lowercase hex>"
        )));
    }
    let decoded = decode_hex(hex).map_err(command_audit_params_error)?;
    decoded.try_into().map_err(|_error| {
        command_audit_params_error(format!(
            "repair_legacy_probe_row {field} must decode to exactly 32 bytes"
        ))
    })
}

fn revision_sha256_text(revision: &[u8; 32]) -> String {
    format!("sha256:{}", hex_encode(revision))
}

fn validate_issue1540_probe_row(key: &[u8], value: &[u8]) -> Result<String, ErrorData> {
    let key_text = std::str::from_utf8(key).map_err(|_error| {
        mcp_error(
            synapse_core::error_codes::STORAGE_CORRUPTED,
            "COMMAND_AUDIT_LEGACY_REPAIR_NOT_ISSUE1540: noncanonical key is not UTF-8; no mutation occurred",
        )
    })?;
    let (prefix, expected_marker) = if key_text.starts_with("issue1540-final-redaction-") {
        ("issue1540-final-redaction", "ISSUE1540_FINAL_SYNTHETIC")
    } else if key_text.starts_with("issue1540-redaction-") {
        ("issue1540-redaction", "ISSUE1540_SYNTHETIC")
    } else {
        return Err(mcp_error(
            synapse_core::error_codes::STORAGE_CORRUPTED,
            "COMMAND_AUDIT_LEGACY_REPAIR_NOT_ISSUE1540: key does not carry either exact #1540 probe prefix; no mutation occurred",
        ));
    };
    if !key_text.ends_with(":00000000000000000000") {
        return Err(mcp_error(
            synapse_core::error_codes::STORAGE_CORRUPTED,
            "COMMAND_AUDIT_LEGACY_REPAIR_NOT_ISSUE1540: key does not carry the exact one-row prefix_index suffix emitted by storage_put_probe_rows; no mutation occurred",
        ));
    }
    let record = serde_json::from_slice::<Value>(value).map_err(|error| {
        mcp_error(
            synapse_core::error_codes::STORAGE_CORRUPTED,
            format!(
                "COMMAND_AUDIT_LEGACY_REPAIR_NOT_ISSUE1540: value is not JSON (line {} column {}); no mutation occurred",
                error.line(),
                error.column()
            ),
        )
    })?;
    if !record.is_object() {
        return Err(mcp_error(
            synapse_core::error_codes::STORAGE_CORRUPTED,
            "COMMAND_AUDIT_LEGACY_REPAIR_NOT_ISSUE1540: value is not a JSON object; no mutation occurred",
        ));
    }
    let marker = record
        .get("error_code")
        .and_then(Value::as_str)
        .filter(|marker| *marker == expected_marker)
        .ok_or_else(|| {
            mcp_error(
                synapse_core::error_codes::STORAGE_CORRUPTED,
                format!(
                    "COMMAND_AUDIT_LEGACY_REPAIR_NOT_ISSUE1540: error_code does not match the exact #1540 marker {expected_marker} paired with key prefix {prefix}; no mutation occurred"
                ),
            )
        })?;
    // #1540 used the historical generic `storage_put_probe_rows` producer.
    // That producer accepted an arbitrary JSON object and used `or_insert` for
    // `probe_id` and `seq`: it guaranteed that both keys were present, but it
    // deliberately preserved arbitrary caller-supplied values and added
    // `ts_ns` only when the request supplied `ts_ns_start`. Requiring specific
    // values (or requiring `ts_ns` to exist) invents a contract the producer
    // never had and makes this migration reject the artifact it exists to
    // remove. The exact paired #1540 key/marker plus the caller-supplied key and
    // value lengths, SHA-256 identities, and physical revision keep the delete
    // content-addressed and non-generic.
    if !record
        .as_object()
        .is_some_and(|object| object.contains_key("seq") && object.contains_key("probe_id"))
    {
        return Err(mcp_error(
            synapse_core::error_codes::STORAGE_CORRUPTED,
            "COMMAND_AUDIT_LEGACY_REPAIR_NOT_ISSUE1540: JSON lacks the seq/probe_id keys guaranteed by the historical #1540 probe writer; no mutation occurred",
        ));
    }
    Ok(format!("{prefix}/{marker}"))
}

fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = Vec::with_capacity(key.len().saturating_add(1));
    next.extend_from_slice(key);
    next.push(0);
    next
}

pub(crate) fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    let value = value.trim();
    if !value.len().is_multiple_of(2) {
        return Err("audit query cursor must be even-length hex".to_owned());
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = hex_nibble(pair[0])
            .ok_or_else(|| "audit query cursor contains non-hex characters".to_owned())?;
        let low = hex_nibble(pair[1])
            .ok_or_else(|| "audit query cursor contains non-hex characters".to_owned())?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn command_audit_params_error(message: impl ToString) -> ErrorData {
    mcp_error(
        synapse_core::error_codes::TOOL_PARAMS_INVALID,
        message.to_string(),
    )
}

fn command_audit_internal_error(message: impl ToString) -> ErrorData {
    mcp_error(
        synapse_core::error_codes::TOOL_INTERNAL_ERROR,
        message.to_string(),
    )
}

fn next_command_audit_key_parts() -> (u64, u32) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let ts_ns = u64::try_from(nanos).unwrap_or(u64::MAX);
    let seq = COMMAND_AUDIT_SEQ.fetch_add(1, Ordering::Relaxed);
    (ts_ns, seq)
}

fn command_audit_key(ts_ns: u64, seq: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(ACTION_LOG_KEY_LEN);
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_encode(digest.as_ref()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn error_data_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}
