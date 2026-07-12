use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use std::cell::Cell;

use rmcp::ErrorData;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use synapse_storage::cf;

use super::SynapseService;
use crate::m1::mcp_error;

const COMMAND_AUDIT_ROW_KIND: &str = "command_audit";
const COMMAND_AUDIT_SCHEMA_VERSION: u32 = 1;
const COMMAND_AUDIT_PAYLOAD_MAX_BYTES: usize = 8192;
const COMMAND_AUDIT_SNAPSHOT_SCAN_LIMIT: usize = 1000;
const COMMAND_AUDIT_SNAPSHOT_ROW_LIMIT: usize = 100;
const COMMAND_AUDIT_QUERY_DEFAULT_LIMIT: usize = 100;
const COMMAND_AUDIT_QUERY_MAX_LIMIT: usize = 250;
const COMMAND_AUDIT_QUERY_DEFAULT_SCAN_LIMIT: usize = 1000;
const COMMAND_AUDIT_QUERY_MAX_SCAN_LIMIT: usize = 5000;
const COMMAND_AUDIT_QUERY_BATCH_ROWS: usize = 256;

static COMMAND_AUDIT_SEQ: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
thread_local! {
    static COMMAND_AUDIT_FORCE_FAIL: Cell<bool> = const { Cell::new(false) };
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
        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            command_audit_internal_error("reflex runtime lock poisoned while reading command audit")
        })?;
        let rows = runtime
            .storage_cf_tail_rows(cf::CF_ACTION_LOG, COMMAND_AUDIT_SNAPSHOT_SCAN_LIMIT)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let scanned_rows = rows.len();
        let mut parsed = Vec::new();
        for (key, value) in rows.into_iter().rev() {
            let Ok(row) = synapse_storage::decode_json::<Value>(&value) else {
                continue;
            };
            if row.get("row_kind").and_then(Value::as_str) != Some(COMMAND_AUDIT_ROW_KIND) {
                continue;
            }
            parsed.push(command_audit_snapshot_row(&key, &row));
            if parsed.len() >= COMMAND_AUDIT_SNAPSHOT_ROW_LIMIT {
                break;
            }
        }
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

        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            command_audit_internal_error("reflex runtime lock poisoned while querying action audit")
        })?;

        let mut cursor = start_key;
        let mut scanned_rows = 0_usize;
        let mut matched_rows = 0_usize;
        let mut corrupt_row_count = 0_usize;
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
                let row = match synapse_storage::decode_json::<Value>(&value) {
                    Ok(row) => row,
                    Err(_error) => {
                        corrupt_row_count = corrupt_row_count.saturating_add(1);
                        continue;
                    }
                };
                let ts_ns = audit_row_ts_ns(&row);
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
            corrupt_row_count,
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
        let mut corrupt_row_count = 0_usize;
        let mut returned = Vec::new();
        let mut has_older = false;
        let mut newest_start_key_hex = None;
        let mut oldest_returned_ts_ns = None;

        for (key, value) in tail.into_iter().rev() {
            scanned_rows = scanned_rows.saturating_add(1);
            let row = match synapse_storage::decode_json::<Value>(&value) {
                Ok(row) => row,
                Err(_error) => {
                    corrupt_row_count = corrupt_row_count.saturating_add(1);
                    continue;
                }
            };
            let ts_ns = audit_row_ts_ns(&row);
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
                break;
            }
            if newest_start_key_hex.is_none() {
                newest_start_key_hex = Some(hex_encode(&key));
            }
            matched_rows = matched_rows.saturating_add(1);
            oldest_returned_ts_ns = Some(ts_ns);
            returned.push(command_audit_query_row(&key, &value, row));
        }

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
            corrupt_row_count,
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
        #[cfg(test)]
        if COMMAND_AUDIT_FORCE_FAIL.with(Cell::get) {
            return Err(command_audit_internal_error(
                "command audit forced failure for test",
            ));
        }

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
        runtime
            .storage_put_action_log_rows(vec![(key.clone(), encoded.clone())])
            .map_err(|error| {
                command_audit_internal_error(format!("command audit write failed: {error}"))
            })?;
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

#[cfg(test)]
pub(crate) fn set_command_audit_force_fail_for_tests(enabled: bool) {
    COMMAND_AUDIT_FORCE_FAIL.with(|force_fail| force_fail.set(enabled));
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

fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = Vec::with_capacity(key.len().saturating_add(1));
    next.extend_from_slice(key);
    next.push(0);
    next
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
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
    let mut key = Vec::with_capacity(12);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{num::NonZeroUsize, path::Path};

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};

    fn service_with_db(path: &Path) -> SynapseService {
        SynapseService::try_with_m2_shutdown_reason_and_m3_config(
            CancellationToken::new(),
            "test",
            CancellationToken::new(),
            &M2ServiceConfig::default(),
            M3ServiceConfig::from_cli_parts(
                Some(path.join("db")),
                Some(path.to_path_buf()),
                false,
                "127.0.0.1:0".to_owned(),
                NonZeroUsize::new(4).expect("nonzero"),
                false,
                true,
                None,
                false,
                None,
            ),
            M4ServiceConfig::default(),
        )
        .expect("construct service")
    }

    fn seed_command_rows(service: &SynapseService, tool: &'static str, rows: usize) {
        for index in 0..rows {
            service
                .command_audit_final(CommandAuditInput::mcp(
                    tool,
                    "list",
                    Some("issue1487-session".to_owned()),
                    None,
                    json!({ "index": index }),
                    json!({}),
                    json!({ "ok": true }),
                    "ok",
                ))
                .expect("write command audit row");
        }
    }

    #[test]
    fn command_audit_redacts_sensitive_payload_fields() {
        let payload = json!({
            "token": "raw-token",
            "nested": {
                "api_key": "raw-key",
                "safe": "kept",
            },
            "items": [
                {"cookie": "raw-cookie"},
                {"name": "kept"}
            ]
        });
        let bounded = bounded_redacted_payload(&payload);
        assert!(bounded.redacted);
        assert!(bounded.redactions.contains(&"$.token".to_owned()));
        assert!(bounded.redactions.contains(&"$.nested.api_key".to_owned()));
        assert!(bounded.redactions.contains(&"$.items[0].cookie".to_owned()));
        assert_eq!(bounded.value["token"], "[REDACTED]");
        assert_eq!(bounded.value["nested"]["safe"], "kept");
        assert!(bounded.sha256.starts_with("sha256:"));
    }

    #[test]
    fn command_audit_bounds_large_payload() {
        let payload = json!({"body": "A".repeat(COMMAND_AUDIT_PAYLOAD_MAX_BYTES + 128)});
        let bounded = bounded_redacted_payload(&payload);
        assert!(bounded.truncated);
        assert!(bounded.bytes > COMMAND_AUDIT_PAYLOAD_MAX_BYTES);
        assert!(
            bounded
                .value
                .get("omitted_bytes")
                .and_then(Value::as_u64)
                .is_some_and(|omitted| omitted > 0)
        );
    }

    #[test]
    fn command_audit_query_unwindowed_newest_first_exact_complete_extra_reports_has_older() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let tool = "issue1487_browser_tabs";
        seed_command_rows(&service, tool, 3);

        let exact = service
            .command_audit_query(CommandAuditQueryParams {
                limit: Some(3),
                scan_limit: Some(16),
                tool: Some(tool.to_owned()),
                ..Default::default()
            })
            .expect("exact-limit page should query");
        assert_eq!(exact.returned_count, 3);
        assert_eq!(exact.matched_rows, 3);
        assert!(!exact.partial);
        assert!(exact.exhausted);
        assert!(exact.next_start_key_hex.is_none());

        let extra_match = service
            .command_audit_query(CommandAuditQueryParams {
                limit: Some(2),
                scan_limit: Some(16),
                tool: Some(tool.to_owned()),
                ..Default::default()
            })
            .expect("limit-plus-one page should query");
        assert_eq!(extra_match.returned_count, 2);
        assert_eq!(extra_match.matched_rows, 2);
        // #1550: an unwindowed default query now scans newest-first and returns the
        // most-recent rows as a COMPLETE success page. `partial` is the fail-closed
        // signal reserved for explicit start_key_hex paging, so it stays false here;
        // "more matching rows exist" is reported via `has_older`, and the caller pages
        // further back by windowing with end_ts_ns = oldest_returned_ts_ns.
        assert!(!extra_match.partial);
        assert!(extra_match.has_older);
        assert!(!extra_match.exhausted);
        assert_eq!(extra_match.scan_order, "newest_first");
        assert!(extra_match.oldest_returned_ts_ns.is_some());
        assert!(extra_match.next_start_key_hex.is_none());
    }

    #[test]
    fn command_audit_query_unwindowed_newest_first_scan_capped_no_match_reports_has_older() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        seed_command_rows(&service, "issue1487_other_tool", 3);

        let response = service
            .command_audit_query(CommandAuditQueryParams {
                limit: Some(20),
                scan_limit: Some(2),
                tool: Some("issue1487_no_match".to_owned()),
                ..Default::default()
            })
            .expect("scan-limited no-match page should query");

        assert_eq!(response.returned_count, 0);
        assert_eq!(response.matched_rows, 0);
        assert_eq!(response.scanned_rows, 2);
        // #1550: the unwindowed default scans newest-first. A scan_limit-capped page
        // that matched nothing reports `has_older` (rows remain beyond the scanned
        // tail) as a SUCCESS, not a fail-closed partial. To search older history for
        // a specific tool, window with start_ts_ns/end_ts_ns — the newest-first
        // default intentionally surfaces recent activity first.
        assert!(!response.partial);
        assert!(response.has_older);
        assert!(!response.exhausted);
        assert_eq!(response.scan_order, "newest_first");
    }
}
