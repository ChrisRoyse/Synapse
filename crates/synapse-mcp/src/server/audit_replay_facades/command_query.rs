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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::command_audit::CommandAuditQueryFilters;

    fn response(partial: bool, next_start_key_hex: Option<&str>) -> CommandAuditQueryResponse {
        CommandAuditQueryResponse {
            source_of_truth: "CF_ACTION_LOG bounded scan",
            cf_name: "CF_ACTION_LOG",
            filters: CommandAuditQueryFilters {
                start_ts_ns: None,
                end_ts_ns: None,
                session_id: None,
                tool: Some("browser_tabs".to_owned()),
                status: None,
                error_code: None,
                row_kind: None,
            },
            limit: 20,
            scan_limit: 200,
            scanned_rows: 34,
            matched_rows: 20,
            returned_count: 20,
            corrupt_row_count: 0,
            noncanonical_key_count: 0,
            partial,
            exhausted: !partial,
            start_key_hex: Some("18be7a58134fc24000000000".to_owned()),
            next_start_key_hex: next_start_key_hex.map(str::to_owned),
            scan_order: crate::server::command_audit::AUDIT_SCAN_ORDER_OLDEST_FIRST,
            has_older: partial,
            oldest_returned_ts_ns: None,
            rows: Vec::new(),
        }
    }

    #[test]
    fn command_query_partial_page_fails_closed_without_success_counts() {
        let error = summarize_command_query(response(true, Some("18be7a763bd0462c0000005500")))
            .expect_err("partial command audit pages should not look successful");

        assert!(error.message.contains("partial CF_ACTION_LOG page"));
        let data = error
            .data
            .expect("partial-page error should carry remediation data");
        assert_eq!(
            data.get("page_complete").and_then(|value| value.as_bool()),
            Some(false)
        );
        assert_eq!(
            data.get("has_next_page").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert!(data.get("matched_rows").is_none());
        assert!(data.get("returned_count").is_none());
        // The continuation cursor IS surfaced so the partial page is pageable (#1515);
        // it is a resume key, not a success/row-count proof.
        assert_eq!(
            data.get("next_start_key_hex")
                .and_then(|value| value.as_str()),
            Some("18be7a763bd0462c0000005500")
        );
    }

    #[test]
    fn command_query_corrupt_rows_still_fail_closed() {
        let mut response = response(false, None);
        response.corrupt_row_count = 1;

        let error = summarize_command_query(response).expect_err("corrupt rows are not trusted");

        assert!(error.message.contains("corrupt CF_ACTION_LOG rows"));
    }

    #[test]
    fn command_query_newest_first_has_older_is_success_not_error() {
        // #1550: the unwindowed default scans newest-first and returns the most
        // recent matches as a COMPLETE page. A full page with older history
        // remaining must be a success with has_older=true, never the forward
        // partial-page hard error.
        let mut response = response(false, None);
        response.scan_order = crate::server::command_audit::AUDIT_SCAN_ORDER_NEWEST_FIRST;
        response.exhausted = false;
        response.has_older = true;
        response.oldest_returned_ts_ns = Some(1_783_000_000_000_000_000);

        let summary = summarize_command_query(response)
            .expect("newest-first has_older page is a complete answer, not a failure");

        assert_eq!(summary.scan_order, "newest_first");
        assert!(summary.has_older);
        assert!(!summary.complete);
        assert_eq!(
            summary.oldest_returned_ts_ns,
            Some(1_783_000_000_000_000_000)
        );
    }
}
