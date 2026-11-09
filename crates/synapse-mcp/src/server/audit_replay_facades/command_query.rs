use rmcp::model::ErrorCode;
use serde_json::json;
use synapse_core::error_codes;

use crate::server::{
    ErrorData,
    command_audit::{CommandAuditQueryParams, CommandAuditQueryResponse},
};

use super::{
    AUDIT_SOT, AUDIT_TOOL,
    types::{AuditCommandQueryParams, AuditCommandQueryResponse, AuditCommandQueryRowSummary},
};
impl From<AuditCommandQueryParams> for CommandAuditQueryParams {
    fn from(value: AuditCommandQueryParams) -> Self {
        Self {
            limit: value.limit,
            scan_limit: value.scan_limit,
            start_key_hex: value.start_key_hex,
            start_ts_ns: value.start_ts_ns,
            end_ts_ns: value.end_ts_ns,
            session_id: value.session_id,
            tool: value.tool,
            status: value.status,
            error_code: value.error_code,
            row_kind: value.row_kind,
        }
    }
}

pub(super) fn summarize_command_query(
    response: CommandAuditQueryResponse,
) -> Result<AuditCommandQueryResponse, ErrorData> {
    if response.partial || response.next_start_key_hex.is_some() {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "audit operation=command_query reached a partial CF_ACTION_LOG page".to_owned(),
            Some(json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "tool": AUDIT_TOOL,
                "operation": "command_query",
                "source_id": response.cf_name,
                "source_of_truth": AUDIT_SOT,
                "page_complete": false,
                "scanned_rows": response.scanned_rows,
                "limit": response.limit,
                "scan_limit": response.scan_limit,
                "has_next_page": response.next_start_key_hex.is_some(),
                // Surface the continuation cursor so a partial page is pageable from the
                // public surface (#1515). This is a resume key, NOT a row-count proof, so it
                // does not weaken the fail-closed "partial pages carry no success counts"
                // contract enforced below (matched_rows/returned_count stay absent).
                "next_start_key_hex": response.next_start_key_hex,
                "remediation": "scope the read with start_ts_ns (it seeks CF_ACTION_LOG by ascending timestamp, so recent-activity queries complete in one page), or page forward by passing this next_start_key_hex back as start_key_hex until page_complete is true; a partial page intentionally omits matched/returned row counts",
            })),
        ));
    }
    if response.corrupt_row_count > 0 {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "audit operation=command_query found corrupt CF_ACTION_LOG rows".to_owned(),
            Some(json!({
                "code": error_codes::STORAGE_READ_FAILED,
                "tool": AUDIT_TOOL,
                "operation": "command_query",
                "source_id": response.cf_name,
                "source_of_truth": AUDIT_SOT,
                "corrupt_row_count": response.corrupt_row_count,
                "remediation": "inspect and repair the corrupt CF_ACTION_LOG rows before trusting audit output",
            })),
        ));
    }
    Ok(AuditCommandQueryResponse {
        source_of_truth: response.source_of_truth.to_owned(),
        cf_name: response.cf_name.to_owned(),
        limit: response.limit,
        scan_limit: response.scan_limit,
        scanned_rows: response.scanned_rows,
        matched_rows: response.matched_rows,
        returned_count: response.returned_count,
        corrupt_row_count: response.corrupt_row_count,
        noncanonical_key_count: response.noncanonical_key_count,
        complete: response.exhausted,
        scan_order: response.scan_order.to_owned(),
        has_older: response.has_older,
        oldest_returned_ts_ns: response.oldest_returned_ts_ns,
        start_key_hex: response.start_key_hex,
        rows: response
            .rows
            .into_iter()
            .map(|row| AuditCommandQueryRowSummary {
                key_hex: row.key_hex,
                value_len_bytes: row.value_len_bytes,
                value_sha256: row.value_sha256,
                row_kind: row.row_kind,
                audit_id: row.audit_id,
                ts_ns: row.ts_ns,
                phase: row.phase,
                status: row.status,
                outcome: row.outcome,
                tool: row.tool,
                verb: row.verb,
                channel: row.channel,
                error_code: row.error_code,
                payload_sha256: row.payload_sha256,
                payload_truncated: row.payload_truncated,
            })
            .collect(),
    })
}
