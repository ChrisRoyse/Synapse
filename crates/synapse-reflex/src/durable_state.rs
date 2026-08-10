//! Private restart-safe desired state for executable reflex definitions.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use synapse_core::{ReflexState, ReflexStatus, SCHEMA_VERSION, StoredReflexAudit, error_codes};
use synapse_storage::{Db, RevisionedRawValue, cf, decode_json, encode_json};

use crate::{ReflexError, ReflexResult, ScheduledReflex, scheduler};
use uuid::Uuid;

pub const DESIRED_STATE_PREFIX: &[u8] = b"reflex/desired/v1/";
const DESIRED_STATE_SCHEMA_VERSION: u32 = 1;

/// Activation work which lives outside the scheduler thread but is required
/// for the stored reflex to receive its trigger events.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "activation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReflexActivation {
    FileJsonlTail {
        host: String,
        path: String,
        json_path: String,
        json_pointer: String,
        event_data_pointer: String,
        equals: Value,
        min_lines: u64,
        poll_interval_ms: u64,
        local_host: bool,
        stop_after_first_match: bool,
    },
}

impl ReflexActivation {
    fn validate(&self, reflex_id: &str) -> ReflexResult<()> {
        match self {
            Self::FileJsonlTail {
                host,
                path,
                json_path,
                json_pointer,
                event_data_pointer,
                min_lines,
                poll_interval_ms,
                ..
            } => {
                if host.is_empty()
                    || path.is_empty()
                    || json_path.is_empty()
                    || json_pointer.is_empty()
                    || event_data_pointer.is_empty()
                    || *min_lines == 0
                    || !(50..=600_000).contains(poll_interval_ms)
                {
                    return Err(durable_error(format!(
                        "REFLEX_DURABLE_ACTIVATION_INVALID: reflex_id={reflex_id} activation=file_jsonl_tail host_empty={} path_empty={} json_path_empty={} json_pointer_empty={} event_data_pointer_empty={} min_lines={min_lines} poll_interval_ms={poll_interval_ms}; remediation=preserve the row, inspect the registration source, and repair the exact desired-state record before restarting",
                        host.is_empty(),
                        path.is_empty(),
                        json_path.is_empty(),
                        json_pointer.is_empty(),
                        event_data_pointer.is_empty(),
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableReflexRecord {
    schema_version: u32,
    pub definition: ScheduledReflex,
    pub status: ReflexStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<ReflexActivation>,
}

impl DurableReflexRecord {
    pub fn active(
        definition: ScheduledReflex,
        status: ReflexStatus,
        activation: Option<ReflexActivation>,
    ) -> ReflexResult<Self> {
        let record = Self {
            schema_version: DESIRED_STATE_SCHEMA_VERSION,
            definition,
            status,
            activation,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn with_status(&self, status: ReflexStatus) -> ReflexResult<Self> {
        let record = Self {
            schema_version: self.schema_version,
            definition: self.definition.clone(),
            status,
            activation: self.activation.clone(),
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> ReflexResult<()> {
        let reflex_id = self.definition.reflex_id.as_str();
        if self.schema_version != DESIRED_STATE_SCHEMA_VERSION {
            return Err(durable_error(format!(
                "REFLEX_DURABLE_SCHEMA_UNSUPPORTED: reflex_id={reflex_id} expected_version={DESIRED_STATE_SCHEMA_VERSION} actual_version={}; remediation=run the explicit desired-state schema migration before starting reflex",
                self.schema_version
            )));
        }
        if self.status.id != self.definition.reflex_id {
            return Err(durable_error(format!(
                "REFLEX_DURABLE_ID_MISMATCH: key_reflex_id={reflex_id} status_reflex_id={}; remediation=preserve the row and repair the mismatched desired-state identity",
                self.status.id
            )));
        }
        if !matches!(
            self.status.state,
            ReflexState::Active
                | ReflexState::Disabled
                | ReflexState::Cancelled
                | ReflexState::Expired
                | ReflexState::ActionDenied
        ) {
            return Err(durable_error(format!(
                "REFLEX_DURABLE_STATE_INVALID: reflex_id={reflex_id} state={:?}; remediation=preserve the row and publish an explicit supported lifecycle transition",
                self.status.state
            )));
        }
        scheduler::validate_reflexes(std::slice::from_ref(&self.definition))?;
        if let Some(activation) = &self.activation {
            activation.validate(reflex_id)?;
        }
        Ok(())
    }
}

pub fn desired_state_key(reflex_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(DESIRED_STATE_PREFIX.len() + reflex_id.len());
    key.extend_from_slice(DESIRED_STATE_PREFIX);
    key.extend_from_slice(reflex_id.as_bytes());
    key
}

pub fn encode_record(record: &DurableReflexRecord) -> ReflexResult<Vec<u8>> {
    encode_json(record).map_err(|error| durable_error(format!(
        "REFLEX_DURABLE_ENCODE_FAILED: reflex_id={} detail={error}; remediation=repair the private desired-state serializer before retrying",
        record.definition.reflex_id
    )))
}

pub fn load_records(db: &Db) -> ReflexResult<Vec<DurableReflexRecord>> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, DESIRED_STATE_PREFIX)
        .map_err(|error| durable_error(format!(
            "REFLEX_DURABLE_SCAN_FAILED: detail={error}; remediation=repair the Calyx CF_KV prefix scan before starting reflex"
        )))?;
    let mut records = Vec::with_capacity(rows.len());
    for (key, _value) in rows {
        let reflex_id = std::str::from_utf8(&key[DESIRED_STATE_PREFIX.len()..]).map_err(|error| {
            durable_error(format!(
                "REFLEX_DURABLE_KEY_INVALID: key_hex={} detail={error}; remediation=preserve and repair the non-UTF-8 desired-state key",
                synapse_storage::ordered_index::hex_encode(&key)
            ))
        })?;
        let RevisionedRawValue { value, .. } = db
            .get_cf_revisioned(cf::CF_KV, &key)
            .map_err(|error| durable_error(format!(
                "REFLEX_DURABLE_READ_FAILED: reflex_id={reflex_id} detail={error}; remediation=repair the exact CF_KV row before starting reflex"
            )))?
            .ok_or_else(|| durable_error(format!(
                "REFLEX_DURABLE_READ_RACED: reflex_id={reflex_id}; remediation=stop concurrent lifecycle writers and restart reflex reconciliation"
            )))?;
        let value = value.ok_or_else(|| durable_error(format!(
            "REFLEX_DURABLE_ROW_EXPIRED: reflex_id={reflex_id}; remediation=restore the non-expiring desired-state row from a verified backup"
        )))?;
        let record = decode_json::<DurableReflexRecord>(&value).map_err(|error| durable_error(format!(
            "REFLEX_DURABLE_DECODE_FAILED: reflex_id={reflex_id} value_sha256={} detail={error}; remediation=preserve and repair or explicitly migrate the exact desired-state row",
            synapse_storage::ordered_index::sha256_hex(&value)
        )))?;
        record.validate()?;
        if record.definition.reflex_id != reflex_id {
            return Err(durable_error(format!(
                "REFLEX_DURABLE_KEY_ID_MISMATCH: key_reflex_id={reflex_id} record_reflex_id={}; remediation=preserve the row and repair the exact key/record identity",
                record.definition.reflex_id
            )));
        }
        records.push(record);
    }
    Ok(records)
}

pub fn load_record(db: &Db, reflex_id: &str) -> ReflexResult<DurableReflexRecord> {
    let key = desired_state_key(reflex_id);
    let value = db
        .get_cf(cf::CF_KV, &key)
        .map_err(|error| durable_error(format!(
            "REFLEX_DURABLE_READ_FAILED: reflex_id={reflex_id} detail={error}; remediation=repair the exact desired-state row before publishing terminal scheduler state"
        )))?
        .ok_or_else(|| durable_error(format!(
            "REFLEX_DURABLE_DEFINITION_MISSING: reflex_id={reflex_id}; remediation=stop reflex execution and run explicit orphan reconciliation"
        )))?;
    let record = decode_json::<DurableReflexRecord>(&value).map_err(|error| durable_error(format!(
        "REFLEX_DURABLE_DECODE_FAILED: reflex_id={reflex_id} detail={error}; remediation=preserve and repair the exact desired-state row"
    )))?;
    record.validate()?;
    Ok(record)
}

pub fn reconcile_legacy_active_orphans(db: &Db) -> ReflexResult<()> {
    let desired_ids = load_records(db)?
        .into_iter()
        .map(|record| record.definition.reflex_id)
        .collect::<std::collections::HashSet<_>>();
    let orphans = crate::listing::latest_active_statuses(db)?
        .into_iter()
        .filter(|status| !desired_ids.contains(&status.id))
        .collect::<Vec<_>>();
    if orphans.is_empty() {
        return Ok(());
    }
    let mut entries = Vec::with_capacity(orphans.len());
    for status in &orphans {
        let audit = StoredReflexAudit {
            schema_version: SCHEMA_VERSION,
            audit_id: Uuid::now_v7().to_string(),
            reflex_id: status.id.clone(),
            ts_ns: crate::audit_timestamp::now_unix_ns(crate::REFLEX_DISABLED_KIND)?,
            status: ReflexState::Disabled,
            event_id: None,
            audit_context: None,
            steps: Vec::new(),
            error_code: Some(error_codes::REFLEX_DURABLE_DEFINITION_MISSING.to_owned()),
            details: serde_json::json!({
                "kind": crate::REFLEX_DISABLED_KIND,
                "kind_summary": status.kind_summary,
                "priority": status.priority,
                "lifetime": status.lifetime,
                "exclusive": status.exclusive,
                "reason": "legacy_active_definition_missing",
                "migration": "synapse.reflex.desired.v1",
            }),
            redacted: false,
            redactions: Vec::new(),
        };
        let source_key =
            format!("{}:{:020}:{}", audit.reflex_id, audit.ts_ns, audit.audit_id).into_bytes();
        let source_value = encode_json(&audit).map_err(|error| durable_error(format!(
            "REFLEX_DURABLE_ORPHAN_AUDIT_ENCODE_FAILED: reflex_id={} detail={error}; remediation=repair the migration serializer before reflex startup",
            audit.reflex_id
        )))?;
        entries.push(crate::audit_projection::GroundedLifecycleProjectionEntry {
            audit,
            source_key,
            source_value,
            desired: None,
        });
    }
    crate::audit_projection::write_projected_grounded_lifecycle_batch(db, entries).map_err(
        |error| durable_error(format!(
            "REFLEX_DURABLE_ORPHAN_MIGRATION_FAILED: orphan_count={} detail={error}; remediation=preserve the vault, repair the named Calyx failure, and retry; no executable definition was invented",
            orphans.len()
        )),
    )?;
    let remaining = crate::listing::latest_active_statuses(db)?
        .into_iter()
        .filter(|status| !desired_ids.contains(&status.id))
        .map(|status| status.id)
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        return Err(durable_error(format!(
            "REFLEX_DURABLE_ORPHAN_MIGRATION_READBACK_FAILED: remaining_active_ids={remaining:?}; remediation=preserve the vault and inspect the exact source/order/aggregate lifecycle rows"
        )));
    }
    tracing::warn!(
        code = "REFLEX_DURABLE_ORPHANS_DISABLED",
        orphan_count = orphans.len(),
        orphan_ids = ?orphans.iter().map(|status| &status.id).collect::<Vec<_>>(),
        remediation = "re-register each reflex from its original complete definition if it is still required; no definition was inferred from audit summaries",
        "explicitly disabled legacy active reflex audits that had no executable desired-state definition"
    );
    Ok(())
}

const fn durable_error(detail: String) -> ReflexError {
    ReflexError::ParamsInvalid { detail }
}
