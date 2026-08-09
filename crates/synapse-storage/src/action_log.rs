//! Canonical codec boundary for `CF_ACTION_LOG`.
//!
//! The action log is an append-only audit source of truth. Every producer and
//! consumer must agree on the physical 12-byte timestamp/sequence key and the
//! identity fields duplicated in the JSON value. Keeping that contract here,
//! below all public storage write APIs, prevents diagnostic or future callers
//! from creating rows that authoritative readers cannot safely interpret.

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Exact physical key width: big-endian `u64` nanoseconds plus `u32` sequence.
pub const ACTION_LOG_KEY_LEN: usize = 12;
/// Current action/command audit value schema.
pub const ACTION_LOG_SCHEMA_VERSION: u64 = 1;
/// Explicit discriminator used by command-audit rows.
pub const COMMAND_AUDIT_ROW_KIND: &str = "command_audit";

/// The two row shapes currently admitted to `CF_ACTION_LOG`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionLogRowKind {
    /// Action outcome row. This historical shape has no `row_kind` field.
    ActionAudit,
    /// Command intent/final row with `row_kind = "command_audit"`.
    CommandAudit,
}

/// Decoded row returned only after its physical and logical identities agree.
#[derive(Clone, Debug)]
pub struct ValidatedActionLogRow {
    pub ts_ns: u64,
    pub seq: u32,
    pub row_kind: ActionLogRowKind,
    pub value: Value,
}

/// Stable, safely reportable codec failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionLogCodecError {
    pub code: &'static str,
    pub detail: String,
}

/// Bounded evidence for one rejected row. Raw key/value bytes are never
/// included; hashes allow an operator to identify the exact physical row.
#[derive(Clone, Debug, Serialize)]
pub struct ActionLogRowDiagnostic {
    pub failure_code: &'static str,
    pub failure_detail: String,
    pub key_len_bytes: usize,
    pub key_sha256: String,
    pub value_len_bytes: usize,
    pub value_sha256: String,
}

/// Validates and decodes one physical `CF_ACTION_LOG` row.
///
/// # Errors
///
/// Returns a stable codec failure when the key is noncanonical, the value is
/// not the supported JSON object, duplicated identity fields disagree, or the
/// row-specific required fields are absent/invalid.
pub fn validate_action_log_row(
    key: &[u8],
    value: &[u8],
) -> Result<ValidatedActionLogRow, ActionLogCodecError> {
    let (key_ts_ns, key_seq) = decode_action_log_key(key)?;
    let decoded = serde_json::from_slice::<Value>(value).map_err(|error| {
        codec_error(
            "ACTION_LOG_VALUE_JSON_INVALID",
            format!("value is not valid JSON: {error}"),
        )
    })?;
    let object = decoded.as_object().ok_or_else(|| {
        codec_error(
            "ACTION_LOG_VALUE_OBJECT_REQUIRED",
            "value must be a JSON object",
        )
    })?;

    require_u64(object, "schema_version", ACTION_LOG_SCHEMA_VERSION)?;
    let ts_ns = required_u64(object, "ts_ns")?;
    if ts_ns != key_ts_ns {
        return Err(codec_error(
            "ACTION_LOG_TS_KEY_MISMATCH",
            format!("value ts_ns={ts_ns} does not match key ts_ns={key_ts_ns}"),
        ));
    }
    let seq_u64 = required_u64(object, "seq")?;
    let seq = u32::try_from(seq_u64).map_err(|_error| {
        codec_error(
            "ACTION_LOG_SEQ_RANGE_INVALID",
            format!("value seq={seq_u64} exceeds u32 range"),
        )
    })?;
    if seq != key_seq {
        return Err(codec_error(
            "ACTION_LOG_SEQ_KEY_MISMATCH",
            format!("value seq={seq} does not match key seq={key_seq}"),
        ));
    }

    let expected_audit_id = format!("{ts_ns:020}-{seq:010}");
    let audit_id = required_nonempty_string(object, "audit_id")?;
    if audit_id != expected_audit_id {
        return Err(codec_error(
            "ACTION_LOG_AUDIT_ID_MISMATCH",
            format!("audit_id does not match canonical identity {expected_audit_id}"),
        ));
    }
    required_nonempty_string(object, "tool")?;

    let row_kind = match object.get("row_kind") {
        None => {
            required_nonempty_string(object, "status")?;
            ActionLogRowKind::ActionAudit
        }
        Some(Value::String(kind)) if kind == COMMAND_AUDIT_ROW_KIND => {
            let phase = required_nonempty_string(object, "phase")?;
            if !matches!(phase, "intent" | "final") {
                return Err(codec_error(
                    "ACTION_LOG_COMMAND_PHASE_INVALID",
                    format!("command audit phase must be intent or final, actual={phase:?}"),
                ));
            }
            required_nonempty_string(object, "verb")?;
            required_nonempty_string(object, "channel")?;
            required_nonempty_string(object, "outcome")?;
            if !object.get("actor").is_some_and(Value::is_object) {
                return Err(codec_error(
                    "ACTION_LOG_COMMAND_ACTOR_INVALID",
                    "command audit actor must be a JSON object",
                ));
            }
            ActionLogRowKind::CommandAudit
        }
        Some(Value::String(kind)) => {
            return Err(codec_error(
                "ACTION_LOG_ROW_KIND_UNSUPPORTED",
                format!("unsupported explicit row_kind={kind:?}"),
            ));
        }
        Some(_other) => {
            return Err(codec_error(
                "ACTION_LOG_ROW_KIND_INVALID",
                "row_kind must be a string when present",
            ));
        }
    };

    Ok(ValidatedActionLogRow {
        ts_ns,
        seq,
        row_kind,
        value: decoded,
    })
}

/// Produces safe physical evidence for one codec failure.
#[must_use]
pub fn diagnostic_for_invalid_row(
    key: &[u8],
    value: &[u8],
    error: &ActionLogCodecError,
) -> ActionLogRowDiagnostic {
    ActionLogRowDiagnostic {
        failure_code: error.code,
        failure_detail: error.detail.clone(),
        key_len_bytes: key.len(),
        key_sha256: sha256_hex(key),
        value_len_bytes: value.len(),
        value_sha256: sha256_hex(value),
    }
}

fn decode_action_log_key(key: &[u8]) -> Result<(u64, u32), ActionLogCodecError> {
    if key.len() != ACTION_LOG_KEY_LEN {
        return Err(codec_error(
            "ACTION_LOG_KEY_LENGTH_INVALID",
            format!(
                "key length must be {ACTION_LOG_KEY_LEN} bytes, actual={}",
                key.len()
            ),
        ));
    }
    let mut ts_ns = [0_u8; 8];
    ts_ns.copy_from_slice(&key[..8]);
    let mut seq = [0_u8; 4];
    seq.copy_from_slice(&key[8..]);
    Ok((u64::from_be_bytes(ts_ns), u32::from_be_bytes(seq)))
}

fn required_u64(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<u64, ActionLogCodecError> {
    object.get(field).and_then(Value::as_u64).ok_or_else(|| {
        codec_error(
            "ACTION_LOG_UNSIGNED_FIELD_INVALID",
            format!("field {field:?} must be an unsigned integer"),
        )
    })
}

fn require_u64(
    object: &Map<String, Value>,
    field: &'static str,
    expected: u64,
) -> Result<(), ActionLogCodecError> {
    let actual = required_u64(object, field)?;
    if actual != expected {
        return Err(codec_error(
            "ACTION_LOG_SCHEMA_VERSION_UNSUPPORTED",
            format!("field {field:?} must equal {expected}, actual={actual}"),
        ));
    }
    Ok(())
}

fn required_nonempty_string<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ActionLogCodecError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            codec_error(
                "ACTION_LOG_STRING_FIELD_INVALID",
                format!("field {field:?} must be a non-empty string"),
            )
        })
}

fn codec_error(code: &'static str, detail: impl Into<String>) -> ActionLogCodecError {
    ActionLogCodecError {
        code,
        detail: detail.into(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}
