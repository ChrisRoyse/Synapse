use rmcp::ErrorData;
use serde_json::{Value, json};
use synapse_reflex::ReflexRuntime;
use synapse_storage::{
    CalyxAnchorWriteReport, Db, GroundingAnchor, GroundingAnchorValue, StorageError, constellations,
};

use crate::m1::mcp_error;

pub(crate) const SOURCE_OPERATOR: &str = "operator";
pub(crate) const SOURCE_AGENT_EVENT: &str = "synapse-agent-event";
pub(crate) const SOURCE_EPISODE_SEGMENT: &str = "synapse-episode-segment";
pub(crate) const SOURCE_APPROVAL: &str = "synapse-approval";
pub(crate) const SOURCE_VERIFICATION: &str = "synapse-verification";
pub(crate) const SOURCE_ESCALATION: &str = "synapse-escalation";

const MS_PER_NS: u64 = 1_000_000;

pub(crate) fn observed_at_ms_from_ns(ts_ns: u64) -> u64 {
    ts_ns / MS_PER_NS
}

pub(crate) fn enum_anchor(
    kind_label: impl Into<String>,
    value: impl Into<String>,
    source: impl Into<String>,
    observed_at_ms: u64,
) -> GroundingAnchor {
    GroundingAnchor {
        kind_label: kind_label.into(),
        value: GroundingAnchorValue::Enum(value.into()),
        source: source.into(),
        observed_at_ms,
        confidence: 1.0,
    }
}

pub(crate) fn bool_anchor(
    kind_label: impl Into<String>,
    value: bool,
    source: impl Into<String>,
    observed_at_ms: u64,
) -> GroundingAnchor {
    GroundingAnchor {
        kind_label: kind_label.into(),
        value: GroundingAnchorValue::Bool(value),
        source: source.into(),
        observed_at_ms,
        confidence: 1.0,
    }
}

pub(crate) fn write_outcome_constellation_and_anchor(
    db: &Db,
    source_cf: &'static str,
    source_key: &[u8],
    source_value: &[u8],
    source_record: &Value,
    anchor: GroundingAnchor,
    context: &'static str,
) -> Result<CalyxAnchorWriteReport, ErrorData> {
    db.put_outcome_constellation(source_cf, source_key, source_value, source_record)
        .map_err(|error| storage_error(context, "put outcome constellation", error))?;
    write_anchor_for_existing_constellation(
        db,
        source_cf,
        source_key,
        source_value,
        anchor,
        context,
    )
}

pub(crate) fn write_anchor_for_existing_constellation(
    db: &Db,
    source_cf: &'static str,
    source_key: &[u8],
    source_value: &[u8],
    anchor: GroundingAnchor,
    context: &'static str,
) -> Result<CalyxAnchorWriteReport, ErrorData> {
    let payload = anchor_ledger_payload(source_cf, source_key, source_value, &anchor);
    db.put_grounding_anchor_for_source(source_cf, source_key, source_value, anchor, &payload)
        .map_err(|error| storage_error(context, "put grounded anchor", error))
}

pub(crate) fn write_runtime_anchor_for_existing_constellation(
    runtime: &ReflexRuntime,
    source_cf: &'static str,
    source_key: &[u8],
    source_value: &[u8],
    anchor: GroundingAnchor,
    context: &'static str,
) -> Result<CalyxAnchorWriteReport, ErrorData> {
    let payload = anchor_ledger_payload(source_cf, source_key, source_value, &anchor);
    runtime
        .storage_put_grounding_anchor_for_source(
            source_cf,
            source_key,
            source_value,
            anchor,
            &payload,
        )
        .map_err(|error| storage_error(context, "put grounded anchor", error))
}

pub(crate) fn anchor_ledger_payload(
    source_cf: &'static str,
    source_key: &[u8],
    source_value: &[u8],
    anchor: &GroundingAnchor,
) -> Value {
    json!({
        "schema": "synapse.grounding_anchor.v2",
        "source_cf": source_cf,
        "source_row_sha256": constellations::sha256_hex(source_key),
        "source_value_sha256": constellations::sha256_hex(source_value),
        "anchor_kind_sha256": constellations::sha256_hex(anchor.kind_label.as_bytes()),
        "anchor_value": anchor_value_payload(&anchor.value),
        "anchor_source_sha256": constellations::sha256_hex(anchor.source.as_bytes()),
        "observed_at_ms": anchor.observed_at_ms,
        "confidence": anchor.confidence,
    })
}

fn anchor_value_payload(value: &GroundingAnchorValue) -> Value {
    match value {
        GroundingAnchorValue::Bool(value) => json!({ "type": "bool", "value": value }),
        GroundingAnchorValue::Enum(value) => json!({
            "type": "enum",
            "value_sha256": constellations::sha256_hex(value.as_bytes()),
        }),
        GroundingAnchorValue::Number(value) => json!({ "type": "number", "value": value }),
        GroundingAnchorValue::Text(value) => json!({
            "type": "text",
            "value_sha256": constellations::sha256_hex(value.as_bytes()),
        }),
    }
}

fn storage_error(context: &'static str, action: &'static str, error: StorageError) -> ErrorData {
    mcp_error(
        error.code(),
        format!("{context} failed to {action}: {error}"),
    )
}
