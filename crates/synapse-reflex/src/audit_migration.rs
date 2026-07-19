use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use synapse_core::StoredReflexAudit;
use synapse_storage::{Db, cf, decode_json, encode_json};

use crate::{ReflexError, ReflexResult};

const MIGRATION_SCHEMA_VERSION: u32 = 1;
const MIGRATION_ID: &str = "reflex_audit_retention_corruption_v1";
const MIGRATION_KEY: &[u8] = b"migration/reflex_audit_retention_corruption/v1";
const RETENTION_MARKER_SOURCE: &str = "storage_gc_once:AUDIT_RETENTION";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MigrationReadback {
    schema_version: u32,
    migration_id: String,
    scanned_rows: u64,
    repaired_rows: u64,
    completed_at_unix_ms: u64,
}

pub fn repair_reflex_audit_retention_corruption(db: &Db) -> ReflexResult<()> {
    if let Some(bytes) = db
        .get_cf(cf::CF_KV, MIGRATION_KEY)
        .map_err(|error| migration_error("read migration sentinel", &error))?
    {
        let sentinel = decode_json::<MigrationReadback>(&bytes)
            .map_err(|error| migration_error("decode migration sentinel", &error))?;
        if sentinel.schema_version != MIGRATION_SCHEMA_VERSION
            || sentinel.migration_id != MIGRATION_ID
        {
            return Err(migration_detail(format!(
                "migration sentinel has unsupported identity: schema_version={} migration_id={}",
                sentinel.schema_version, sentinel.migration_id
            )));
        }
        tracing::info!(
            code = "REFLEX_AUDIT_RETENTION_MIGRATION_ALREADY_APPLIED",
            migration_id = MIGRATION_ID,
            scanned_rows = sentinel.scanned_rows,
            repaired_rows = sentinel.repaired_rows,
            completed_at_unix_ms = sentinel.completed_at_unix_ms,
            "verified schema-owned reflex audit repair sentinel"
        );
        return Ok(());
    }

    let rows = db
        .scan_cf(cf::CF_REFLEX_AUDIT)
        .map_err(|error| migration_error("scan CF_REFLEX_AUDIT", &error))?;
    let scanned_rows = rows.len() as u64;
    let mut repairs = Vec::new();
    for (key, bytes) in rows {
        if decode_json::<StoredReflexAudit>(&bytes).is_ok() {
            continue;
        }
        let repaired = repair_one_row(&key, &bytes)?;
        repairs.push((key, repaired));
    }

    if !repairs.is_empty() {
        db.put_batch_pressure_bypass(cf::CF_REFLEX_AUDIT, repairs.clone())
            .map_err(|error| migration_error("write repaired CF_REFLEX_AUDIT rows", &error))?;
        db.flush()
            .map_err(|error| migration_error("flush repaired CF_REFLEX_AUDIT rows", &error))?;
        for (key, expected) in &repairs {
            let actual = db
                .get_cf(cf::CF_REFLEX_AUDIT, key)
                .map_err(|error| migration_error("read repaired CF_REFLEX_AUDIT row", &error))?
                .ok_or_else(|| {
                    migration_detail(format!(
                        "repaired row is absent on physical readback: key_hex={}",
                        hex_encode(key)
                    ))
                })?;
            if actual != *expected {
                return Err(migration_detail(format!(
                    "repaired row readback differs from committed bytes: key_hex={}",
                    hex_encode(key)
                )));
            }
            decode_json::<StoredReflexAudit>(&actual).map_err(|error| {
                migration_detail(format!(
                    "repaired row still violates StoredReflexAudit: key_hex={} error={error}",
                    hex_encode(key)
                ))
            })?;
        }
    }

    let sentinel = MigrationReadback {
        schema_version: MIGRATION_SCHEMA_VERSION,
        migration_id: MIGRATION_ID.to_owned(),
        scanned_rows,
        repaired_rows: repairs.len() as u64,
        completed_at_unix_ms: current_time_ms()?,
    };
    let sentinel_bytes = encode_json(&sentinel)
        .map_err(|error| migration_error("encode migration sentinel", &error))?;
    db.put_batch_pressure_bypass(
        cf::CF_KV,
        [(MIGRATION_KEY.to_vec(), sentinel_bytes.clone())],
    )
    .map_err(|error| migration_error("write migration sentinel", &error))?;
    db.flush()
        .map_err(|error| migration_error("flush migration sentinel", &error))?;
    let readback = db
        .get_cf(cf::CF_KV, MIGRATION_KEY)
        .map_err(|error| migration_error("read migration sentinel after write", &error))?
        .ok_or_else(|| migration_detail("migration sentinel is absent after write".to_owned()))?;
    if readback != sentinel_bytes {
        return Err(migration_detail(
            "migration sentinel readback differs from committed bytes".to_owned(),
        ));
    }
    tracing::info!(
        code = "REFLEX_AUDIT_RETENTION_MIGRATION_APPLIED",
        migration_id = MIGRATION_ID,
        scanned_rows,
        repaired_rows = repairs.len(),
        completed_at_unix_ms = sentinel.completed_at_unix_ms,
        source_of_truth = "CF_REFLEX_AUDIT strict row bytes + CF_KV migration sentinel",
        "completed schema-owned reflex audit repair with physical readback"
    );
    Ok(())
}

fn repair_one_row(key: &[u8], bytes: &[u8]) -> ReflexResult<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(bytes).map_err(|error| {
        migration_detail(format!(
            "unrecognized invalid reflex audit JSON: key_hex={} error={error}",
            hex_encode(key)
        ))
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        migration_detail(format!(
            "unrecognized non-object reflex audit row: key_hex={}",
            hex_encode(key)
        ))
    })?;
    validate_retention_marker(key, object)?;
    validate_copied_profile_fields(key, object)?;
    object.remove("profile_id");
    object.remove("profile_schema_version");
    object.remove("audit_retention");

    let audit = serde_json::from_value::<StoredReflexAudit>(value).map_err(|error| {
        migration_detail(format!(
            "row does not match the recognized retention corruption after stripping exact injected fields: key_hex={} error={error}",
            hex_encode(key)
        ))
    })?;
    encode_json(&audit).map_err(|error| migration_error("encode repaired reflex audit", &error))
}

fn validate_retention_marker(key: &[u8], object: &Map<String, Value>) -> ReflexResult<()> {
    let marker = object
        .get("audit_retention")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            migration_detail(format!(
                "invalid reflex audit row has no recognized audit_retention marker: key_hex={}",
                hex_encode(key)
            ))
        })?;
    let exact_fields = marker.len() == 3
        && marker.get("schema_version").and_then(Value::as_u64) == Some(1)
        && marker
            .get("backfilled_at_ns")
            .and_then(Value::as_u64)
            .is_some()
        && marker.get("source").and_then(Value::as_str) == Some(RETENTION_MARKER_SOURCE);
    if !exact_fields {
        return Err(migration_detail(format!(
            "invalid reflex audit row has an unrecognized audit_retention marker: key_hex={}",
            hex_encode(key)
        )));
    }
    Ok(())
}

fn validate_copied_profile_fields(key: &[u8], object: &Map<String, Value>) -> ReflexResult<()> {
    let audit_context = object.get("audit_context").and_then(Value::as_object);
    for field in ["profile_id", "profile_schema_version"] {
        if let Some(copied) = object.get(field) {
            let original = audit_context.and_then(|context| context.get(field));
            if original != Some(copied) {
                return Err(migration_detail(format!(
                    "invalid reflex audit row has a non-derived {field}: key_hex={}",
                    hex_encode(key)
                )));
            }
        }
    }
    if !object.contains_key("profile_id") && !object.contains_key("profile_schema_version") {
        return Err(migration_detail(format!(
            "invalid reflex audit row has a retention marker but no copied profile fields: key_hex={}",
            hex_encode(key)
        )));
    }
    Ok(())
}

fn current_time_ms() -> ReflexResult<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| migration_detail(format!("system clock before Unix epoch: {error}")))?;
    Ok(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

fn migration_error(context: &str, error: &impl std::fmt::Display) -> ReflexError {
    migration_detail(format!("{context}: {error}"))
}

fn migration_detail(detail: impl Into<String>) -> ReflexError {
    ReflexError::ParamsInvalid {
        detail: format!("{MIGRATION_ID}: {}", detail.into()),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
