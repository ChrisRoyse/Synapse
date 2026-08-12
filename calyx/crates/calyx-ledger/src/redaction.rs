//! Ledger payload redaction and secret guardrails.

use calyx_core::{CalyxError, InputRef, METADATA_CHUNK_ID, METADATA_DATABASE_NAME, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::entry::ActorId;

/// Largest unclassified, non-whitespace identifier that can be persisted
/// without entering the Ledger's secret-token classification boundary.
///
/// Callers that persist operator-supplied identifiers in a Ledger payload
/// must use this bound during their own input validation so they fail before
/// creating related durable state.
pub const MAX_UNCLASSIFIED_TOKEN_LEN: usize = 39;
const SECRET_TOKEN_MIN: usize = MAX_UNCLASSIFIED_TOKEN_LEN + 1;
const MAX_HASH_OR_ID_LEN: usize = 64;
const MAX_DISCOVERY_MANIFEST_TOKEN_LEN: usize = 160;
const MAX_QUANT_SLOT_METADATA_LEN: usize = 4096;
const MAX_SOURCE_METADATA_LEN: usize = 128;
const MAX_STABLE_CODE_LEN: usize = 128;

/// Per-vault ledger redaction policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RedactionPolicy {
    pub store_raw_input: bool,
    pub redact_actor_name: bool,
}

impl RedactionPolicy {
    pub const fn new(store_raw_input: bool, redact_actor_name: bool) -> Self {
        Self {
            store_raw_input,
            redact_actor_name,
        }
    }

    /// Rejects payloads that contain secret-like fields or token material.
    pub fn check_payload(payload: &[u8]) -> Result<()> {
        Self::default().check_payload_with_policy(payload)
    }

    /// Rejects payloads using this policy's scanner settings.
    pub fn check_payload_with_policy(&self, payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return Ok(());
        }
        match serde_json::from_slice::<Value>(payload) {
            Ok(value) => check_json_value(&value, None),
            Err(_) => check_text_tokens(&String::from_utf8_lossy(payload), None),
        }
    }

    /// Applies the same field-aware token policy used for a JSON Ledger
    /// payload to one caller-supplied identifier.
    ///
    /// This lets producers validate identifiers before they create adjacent
    /// durable state, while keeping the classification policy in one place.
    pub fn check_public_identifier(field: &str, value: &str) -> Result<()> {
        if is_secret_field(field) {
            return Err(secret_error(format!(
                "ledger payload field `{field}` is secret-like"
            )));
        }
        check_text_tokens(value, Some(field))
    }

    /// Redacts the raw input pointer while preserving the stable content hash.
    pub const fn redact_input_ref(&self, input_ref: &InputRef) -> RedactedInput {
        RedactedInput {
            hash: input_ref.hash,
            redacted: true,
            pointer: None,
        }
    }

    /// Builds a hash/id-only payload from a richer payload builder.
    pub fn apply_to_payload(&self, raw: &PayloadBuilder) -> Vec<u8> {
        let filtered = filter_payload_value(raw.value(), self.store_raw_input);
        serde_json::to_vec(&filtered).expect("serde_json::Value serializes")
    }

    pub fn apply_to_actor(&self, actor: ActorId) -> ActorId {
        if !self.redact_actor_name {
            return actor;
        }
        match actor {
            ActorId::Agent(_) => ActorId::Agent("redacted".to_string()),
            ActorId::Service(_) => ActorId::Service("redacted".to_string()),
            ActorId::System => ActorId::System,
        }
    }
}

/// Hash-only input reference safe for ledger payloads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedInput {
    pub hash: [u8; 32],
    pub redacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

/// Small JSON payload builder for redaction before append.
#[derive(Clone, Debug, PartialEq)]
pub struct PayloadBuilder {
    value: Value,
}

impl Default for PayloadBuilder {
    fn default() -> Self {
        Self::object()
    }
}

impl PayloadBuilder {
    pub fn object() -> Self {
        Self {
            value: Value::Object(Map::new()),
        }
    }

    pub fn from_value(value: Value) -> Self {
        Self { value }
    }

    pub fn insert_value(&mut self, key: impl Into<String>, value: Value) -> &mut Self {
        if !self.value.is_object() {
            self.value = Value::Object(Map::new());
        }
        self.value
            .as_object_mut()
            .expect("value was just normalized to object")
            .insert(key.into(), value);
        self
    }

    pub fn insert_str(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.insert_value(key, Value::String(value.into()))
    }

    pub fn insert_u64(&mut self, key: impl Into<String>, value: u64) -> &mut Self {
        self.insert_value(key, Value::Number(value.into()))
    }

    pub const fn value(&self) -> &Value {
        &self.value
    }
}

fn check_json_value(value: &Value, field: Option<&str>) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if is_secret_field(key) {
                    return Err(secret_error(format!(
                        "ledger payload field `{key}` is secret-like"
                    )));
                }
                check_json_value(child, Some(key))?;
            }
            Ok(())
        }
        Value::Array(values) => {
            for child in values {
                check_json_value(child, field)?;
            }
            Ok(())
        }
        Value::String(text) => check_text_tokens(text, field),
        _ => Ok(()),
    }
}

fn check_text_tokens(text: &str, field: Option<&str>) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    if text_has_no_space_printable_run(text) && !allowed_stable_identifier(text, field) {
        return Err(secret_token_error(
            "long_non_whitespace_token",
            field,
            text.len(),
        ));
    }
    for token in token_candidates(text) {
        if token.len() >= SECRET_TOKEN_MIN && !allowed_stable_identifier(token, field) {
            return Err(secret_token_error("token_like_secret", field, token.len()));
        }
    }
    Ok(())
}

/// Describe a rejected token without echoing any part of the token or an
/// attacker-controlled JSON field name. The field is normalized with the same
/// classifier used by the allowlist and bounded before it enters an error/log.
/// This keeps redaction failures actionable without turning the diagnostic
/// itself into a secret or log-injection surface.
fn secret_token_error(kind: &str, field: Option<&str>, token_len: usize) -> CalyxError {
    let field = diagnostic_field(field);
    secret_error(format!(
        "ledger payload contains a rejected token: kind={kind} field={field} token_len={token_len} classification_threshold={SECRET_TOKEN_MIN}; remediation=store a SHA-256 binding under a *_sha256 field or use a supported stable identifier encoding"
    ))
}

fn diagnostic_field(field: Option<&str>) -> String {
    const MAX_DIAGNOSTIC_FIELD_CHARS: usize = 64;
    let Some(field) = field else {
        return "<root>".to_owned();
    };
    let mut normalized = String::with_capacity(field.len().min(MAX_DIAGNOSTIC_FIELD_CHARS));
    let mut chars = field.chars();
    for ch in chars.by_ref().take(MAX_DIAGNOSTIC_FIELD_CHARS) {
        normalized.push(if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '_'
        });
    }
    if normalized.is_empty() {
        normalized.push_str("<empty>");
    }
    if chars.next().is_some() {
        normalized.push_str("_truncated");
    }
    normalized
}

fn token_candidates(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if is_token_char(ch) {
            start.get_or_insert(index);
            continue;
        }
        if let Some(begin) = start.take() {
            out.push(&text[begin..index]);
        }
    }
    if let Some(begin) = start {
        out.push(&text[begin..]);
    }
    out
}

fn text_has_no_space_printable_run(text: &str) -> bool {
    text.chars().count() >= SECRET_TOKEN_MIN
        && text.chars().all(|ch| ch.is_ascii_graphic())
        && text.chars().all(|ch| !ch.is_whitespace())
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '_' | '-' | '.')
}

fn allowed_stable_identifier(token: &str, field: Option<&str>) -> bool {
    let Some(field) = field else {
        return false;
    };
    let field = normalized_field(field);
    if is_source_metadata_field(&field) {
        return allowed_source_metadata_value(token);
    }
    if field == "code" {
        return allowed_stable_code(token);
    }
    if field == "signature" {
        return token.len() == 128 && is_hex(token);
    }
    if field == "git_sha" {
        return matches!(token.len(), 7..=40) && is_hex(token);
    }
    if field == "blake3" || field.ends_with("_blake3") {
        return token.len() == MAX_HASH_OR_ID_LEN && is_hex(token);
    }
    if field_allows_manifest_slug(&field) && is_manifest_slug(token) {
        return true;
    }
    if is_public_key_field(&field) {
        return token.len() == MAX_HASH_OR_ID_LEN && is_hex(token);
    }
    if field.starts_with("quant_slot_") {
        return token.len() <= MAX_QUANT_SLOT_METADATA_LEN && is_hex(token);
    }
    if !field_allows_stable_identifier(&field) || token.len() > MAX_HASH_OR_ID_LEN {
        return false;
    }
    is_hex(token) || is_base58(token) || is_uuid(token)
}

fn allowed_stable_code(token: &str) -> bool {
    token.starts_with("CALYX_")
        && token.len() <= MAX_STABLE_CODE_LEN
        && token
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn field_allows_stable_identifier(field: &str) -> bool {
    let field = normalized_field(field);
    field == "hash"
        || field == "metadata"
        || is_source_metadata_field(&field)
        || field == "input_hash"
        || field == "root"
        || field == "signature"
        || field == "git_sha"
        || field == "weights_sha256"
        || is_public_key_field(&field)
        || field.ends_with("_hash")
        || field.ends_with("_id")
        || field.ends_with("_sha256")
        || field.ends_with("_digest")
}

fn field_allows_manifest_slug(field: &str) -> bool {
    matches!(
        field,
        "run_id" | "corpus_vault_id" | "stage_id" | "upstream_stage_id" | "command",
    )
}

fn is_manifest_slug(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_DISCOVERY_MANIFEST_TOKEN_LEN
        && token
            .chars()
            .any(|ch| matches!(ch, '-' | '_' | ':' | '/' | '.'))
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '/' | '.'))
}

fn is_source_metadata_field(field: &str) -> bool {
    matches!(field, METADATA_CHUNK_ID | METADATA_DATABASE_NAME)
}

fn allowed_source_metadata_value(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_SOURCE_METADATA_LEN
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':' | '/'))
}

fn is_secret_field(field: &str) -> bool {
    let field = normalized_field(field);
    if is_public_key_field(&field) {
        return false;
    }
    matches!(
        field.as_str(),
        "password" | "passwd" | "token" | "secret" | "key"
    ) || field.ends_with("_password")
        || field.ends_with("_passwd")
        || field.ends_with("_token")
        || field.ends_with("_secret")
        || field.ends_with("_key")
}

fn is_public_key_field(field: &str) -> bool {
    matches!(field, "signer_pubkey" | "public_key" | "verifying_key")
}

fn normalized_field(field: &str) -> String {
    field
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn is_hex(token: &str) -> bool {
    token.len().is_multiple_of(2) && token.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn is_base58(token: &str) -> bool {
    token
        .chars()
        .all(|ch| "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".contains(ch))
}

fn is_uuid(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 36
        && matches!(bytes[8], b'-')
        && matches!(bytes[13], b'-')
        && matches!(bytes[18], b'-')
        && matches!(bytes[23], b'-')
        && token
            .chars()
            .filter(|ch| *ch != '-')
            .all(|ch| ch.is_ascii_hexdigit())
}

fn filter_payload_value(value: &Value, store_raw_input: bool) -> Value {
    match value {
        Value::Object(map) => {
            let mut filtered = Map::new();
            for (key, child) in map {
                if key == "input_ref" {
                    filtered.insert(key.clone(), filter_input_ref(child));
                    continue;
                }
                if keep_payload_field(key, store_raw_input) {
                    filtered.insert(key.clone(), filter_payload_value(child, store_raw_input));
                }
            }
            Value::Object(filtered)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|child| filter_payload_value(child, store_raw_input))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn filter_input_ref(value: &Value) -> Value {
    let mut filtered = Map::new();
    if let Some(hash) = value.get("hash") {
        filtered.insert("hash".to_string(), hash.clone());
    }
    filtered.insert("redacted".to_string(), Value::Bool(true));
    Value::Object(filtered)
}

fn keep_payload_field(field: &str, store_raw_input: bool) -> bool {
    let field = normalized_field(field);
    if is_secret_field(&field) {
        return false;
    }
    if is_raw_field(&field) {
        return store_raw_input;
    }
    field == "ts" || field == "redacted" || field_allows_stable_identifier(&field)
}

fn is_raw_field(field: &str) -> bool {
    matches!(
        field,
        "raw" | "raw_bytes" | "raw_input" | "input_bytes" | "plaintext"
    ) || field.ends_with("_raw")
        || field.ends_with("_bytes")
}

fn secret_error(message: impl Into<String>) -> CalyxError {
    CalyxError::ledger_secret_in_payload(message)
}
