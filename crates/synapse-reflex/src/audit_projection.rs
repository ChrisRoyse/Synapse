//! Exact restart-safe reflex audit projections (#2190).

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{LazyLock, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use synapse_core::{ReflexState, ReflexStatus, StoredReflexAudit, error_codes};
use synapse_storage::{
    CfRevisionGuard, Db, RawRow, RawRowWithExpiry, StorageError, StorageResult, cf, decode_json,
    encode_json,
    ordered_index::{
        OrderedSourcePointer, decode_pointer, encode_pointer, hex_decode, hex_encode,
        reflex_audit_order_key, sha256_hex,
    },
};

use crate::{ReflexError, ReflexResult, listing::AuditStatusAccumulator};

const META_KEY: &[u8] = b"projection/reflex-audit/v1/meta";
const PROGRESS_KEY: &[u8] = b"projection/reflex-audit/v1/progress";
const REGISTRY_KEY: &[u8] = b"projection/reflex-audit/v1/registry";
const CLAMP_KEY: &[u8] = b"projection/reflex-audit/v1/recursion-clamps";
const STATE_PREFIX: &[u8] = b"projection/reflex-audit/v1/state/";
const SCHEMA_VERSION: u32 = 1;
const BACKFILL_ROWS: usize = 512;
const MAX_GLOBAL_HISTORY: usize = 1_000;

static PROJECTION_LOCK: LazyLock<Mutex<ProjectionLockState>> =
    LazyLock::new(|| Mutex::new(ProjectionLockState::default()));

#[derive(Clone, Debug, Eq, PartialEq)]
struct VaultIdentity {
    vault_dir: PathBuf,
    vault_id: String,
}

#[derive(Debug, Default)]
struct ProjectionLockState {
    reconciled_vault: Option<VaultIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectionMeta {
    schema_version: u32,
    readable: bool,
    built_at_unix_ms: u64,
    source_rows_at_build: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BuildStage {
    Order,
    Aggregates,
    Complete,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectionProgress {
    schema_version: u32,
    stage: BuildStage,
    resume_after_source_key_hex: Option<String>,
    resume_after_state_key_hex: Option<String>,
    rows_indexed: u64,
    states_written: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReflexProjectionState {
    schema_version: u32,
    reflex_id: String,
    latest_ts_ns: u64,
    latest_audit_id: String,
    latest_source_value_sha256: String,
    accumulator: AuditStatusAccumulator,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReflexProjectionRegistry {
    schema_version: u32,
    reflex_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ClampProjection {
    schema_version: u32,
    total: u64,
}

pub struct GroundedLifecycleDesiredMutation {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub expected_revision: [u8; 32],
}

pub struct GroundedLifecycleProjectionEntry {
    pub audit: StoredReflexAudit,
    pub source_key: Vec<u8>,
    pub source_value: Vec<u8>,
    pub desired: Option<GroundedLifecycleDesiredMutation>,
}

type DesiredLifecycleMutation<'a> = (&'a [u8], &'a [u8], Option<[u8; 32]>);

pub fn ensure(db: &Db) -> ReflexResult<()> {
    let mut guard = lock_projection()?;
    ensure_locked(db, &mut guard)
}

fn lock_projection() -> ReflexResult<MutexGuard<'static, ProjectionLockState>> {
    PROJECTION_LOCK.lock().map_err(|_error| projection_error(
        "REFLEX_AUDIT_PROJECTION_LOCK_POISONED: restart the daemon and inspect the prior panic before retrying",
    ))
}

fn ensure_locked(db: &Db, lock_state: &mut ProjectionLockState) -> ReflexResult<()> {
    let vault_identity = read_vault_identity(db)?;
    if lock_state.reconciled_vault.as_ref() == Some(&vault_identity) {
        return Ok(());
    }
    match read_meta(db)? {
        Some(meta) => {
            validate_meta(&meta)?;
            reconcile_order_sets(db)?;
            validate_durable_aggregates(db)?;
        }
        None => build_projection(db)?,
    }
    lock_state.reconciled_vault = Some(vault_identity);
    Ok(())
}

fn read_vault_identity(db: &Db) -> ReflexResult<VaultIdentity> {
    let status = db.calyx_vault_status().map_err(|error| projection_error(&format!(
        "REFLEX_AUDIT_PROJECTION_VAULT_IDENTITY_READ_FAILED: {error}; remediation=repair the Calyx vault status path before reading or writing the audit projection"
    )))?;
    if !status.enabled || !status.open {
        return Err(projection_error(&format!(
            "REFLEX_AUDIT_PROJECTION_VAULT_IDENTITY_UNAVAILABLE: enabled={} open={} phase={}; remediation=open the configured Calyx vault before reading or writing the audit projection",
            status.enabled, status.open, status.phase
        )));
    }
    let vault_dir = status.vault_dir.ok_or_else(|| projection_error(
        "REFLEX_AUDIT_PROJECTION_VAULT_IDENTITY_UNAVAILABLE: open vault status has no vault_dir; remediation=repair the Calyx status contract before retrying",
    ))?;
    let vault_id = status.vault_id.ok_or_else(|| projection_error(
        "REFLEX_AUDIT_PROJECTION_VAULT_IDENTITY_UNAVAILABLE: open vault status has no vault_id; remediation=repair the Calyx identity read before retrying",
    ))?;
    Ok(VaultIdentity {
        vault_dir,
        vault_id,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "the single source/index/aggregate commit keeps all guarded identities and exact readbacks visible as one atomic invariant"
)]
pub fn write_projected_audit(
    db: &Db,
    audit: &StoredReflexAudit,
    source_key: &[u8],
    source_value: &[u8],
) -> StorageResult<()> {
    let mut guard = PROJECTION_LOCK.lock().map_err(|_error| storage_write_error(
        "REFLEX_AUDIT_PROJECTION_LOCK_POISONED: restart the daemon and inspect the prior panic before retrying",
    ))?;
    ensure_locked(db, &mut guard).map_err(|error| storage_write_error(&error.to_string()))?;

    let order_key = reflex_audit_order_key(audit.ts_ns, &audit.audit_id, &audit.reflex_id);
    let order_value = encode_pointer(&OrderedSourcePointer::new(source_key, source_value))?;
    let existing_source = db.get_cf_revisioned(cf::CF_REFLEX_AUDIT, source_key)?;
    if let Some(existing) = &existing_source {
        if existing.value.as_deref() != Some(source_value) {
            return Err(storage_write_error(&format!(
                "REFLEX_AUDIT_SOURCE_IDENTITY_CONFLICT: source_key_hex={} expected_sha256={} actual_sha256={}; remediation=preserve both values and repair the duplicate audit identity before retrying",
                hex_encode(source_key),
                sha256_hex(source_value),
                existing
                    .value
                    .as_deref()
                    .map_or_else(|| "expired".to_owned(), sha256_hex)
            )));
        }
        let existing_order = db.get_cf(cf::CF_REFLEX_AUDIT_ORDER, &order_key)?;
        if existing_order.as_deref() != Some(order_value.as_slice()) {
            return Err(storage_write_error(&format!(
                "REFLEX_AUDIT_IDEMPOTENT_ORDER_MISSING: source_key_hex={} order_key_hex={}; remediation=preserve the committed source and inspect the violated atomic projection commit",
                hex_encode(source_key),
                hex_encode(&order_key)
            )));
        }
        return Ok(());
    }

    let state_key = state_key(&audit.reflex_id);
    let state_revision = db.get_cf_revisioned(cf::CF_KV, &state_key)?;
    let prior_state = state_revision
        .as_ref()
        .and_then(|row| row.value.as_deref())
        .map(decode_projection_state)
        .transpose()?;
    let next_state = next_projection_state(db, prior_state, audit, source_value)?;
    let state_value = encode_json(&next_state)?;

    let order_revision = db.get_cf_revisioned(cf::CF_REFLEX_AUDIT_ORDER, &order_key)?;
    if let Some(existing) = &order_revision
        && existing.value.as_deref() != Some(order_value.as_slice())
    {
        return Err(storage_write_error(&format!(
            "REFLEX_AUDIT_ORDER_IDENTITY_CONFLICT: order_key_hex={}; remediation=preserve source/index rows and inspect the collision before repair",
            hex_encode(&order_key)
        )));
    }

    let mut guards = vec![
        CfRevisionGuard::new(cf::CF_REFLEX_AUDIT, source_key.to_vec(), None),
        CfRevisionGuard::new(
            cf::CF_REFLEX_AUDIT_ORDER,
            order_key.clone(),
            order_revision.map(|row| row.revision_sha256),
        ),
        CfRevisionGuard::new(
            cf::CF_KV,
            state_key.clone(),
            state_revision.map(|row| row.revision_sha256),
        ),
    ];
    let mut kv_rows = vec![(state_key.clone(), state_value.clone())];

    if audit.error_code.as_deref() == Some(error_codes::REFLEX_RECURSION_LIMIT) {
        let clamp_revision = db.get_cf_revisioned(cf::CF_KV, CLAMP_KEY)?;
        let mut clamp = clamp_revision
            .as_ref()
            .and_then(|row| row.value.as_deref())
            .map(decode_clamp)
            .transpose()?
            .unwrap_or(ClampProjection {
                schema_version: SCHEMA_VERSION,
                total: 0,
            });
        clamp.total = clamp.total.checked_add(1).ok_or_else(|| storage_write_error(
            "REFLEX_RECURSION_CLAMP_COUNTER_OVERFLOW: u64 exhausted; preserve the counter and widen its representation",
        ))?;
        guards.push(CfRevisionGuard::new(
            cf::CF_KV,
            CLAMP_KEY,
            clamp_revision.map(|row| row.revision_sha256),
        ));
        kv_rows.push((CLAMP_KEY.to_vec(), encode_json(&clamp)?));
    }

    let mut registry =
        read_registry(db).map_err(|error| storage_write_error(&error.to_string()))?;
    if !registry.reflex_ids.iter().any(|id| id == &audit.reflex_id) {
        let registry_revision = db.get_cf_revisioned(cf::CF_KV, REGISTRY_KEY)?
            .ok_or_else(|| storage_write_error(
                "REFLEX_AUDIT_REGISTRY_MISSING_AFTER_PUBLICATION: preserve projection state and inspect CF_KV protection",
            ))?;
        registry.reflex_ids.push(audit.reflex_id.clone());
        registry.reflex_ids.sort();
        registry.reflex_ids.dedup();
        guards.push(CfRevisionGuard::new(
            cf::CF_KV,
            REGISTRY_KEY,
            Some(registry_revision.revision_sha256),
        ));
        kv_rows.push((REGISTRY_KEY.to_vec(), encode_json(&registry)?));
    }

    let outcome = db.put_cf_batches_if_revisions_pressure_bypass(
        guards,
        vec![
            (
                cf::CF_REFLEX_AUDIT,
                vec![(source_key.to_vec(), source_value.to_vec())],
            ),
            (
                cf::CF_REFLEX_AUDIT_ORDER,
                vec![(order_key.clone(), order_value.clone())],
            ),
            (cf::CF_KV, kv_rows),
        ],
    )?;
    if !outcome.applied {
        return Err(storage_write_error(&format!(
            "REFLEX_AUDIT_PROJECTION_REVISION_CONFLICT: conflict={:?}; remediation=reload the exact source, aggregate, and registry revisions before retrying",
            outcome.conflict
        )));
    }
    require_exact_readback(db, cf::CF_REFLEX_AUDIT, source_key, source_value)?;
    require_exact_readback(db, cf::CF_REFLEX_AUDIT_ORDER, &order_key, &order_value)?;
    require_exact_readback(db, cf::CF_KV, &state_key, &state_value)?;
    Ok(())
}

/// Atomically publishes a registration audit, every durable projection row,
/// and its grounded Calyx constellation/anchor/ledger evidence.
///
/// Unlike later lifecycle audits, a registration is the publication boundary
/// for a new runtime identity. The source row, ordered pointer, aggregate, and
/// registry mutation therefore share the same guarded WAL/MVCC commit as the
/// native constellation. The scheduler remains prepared and unable to tick
/// until this function returns success.
#[expect(
    clippy::too_many_lines,
    reason = "the single source/order/aggregate/desired-state/grounding commit keeps every guarded identity and readback visible as one invariant"
)]
pub fn write_projected_grounded_lifecycle_audit(
    db: &Db,
    audit: &StoredReflexAudit,
    source_key: &[u8],
    source_value: &[u8],
    desired: Option<DesiredLifecycleMutation<'_>>,
) -> StorageResult<()> {
    let kind = audit.details.get("kind").and_then(Value::as_str);
    let valid_lifecycle = matches!(
        (audit.status, kind),
        (ReflexState::Active, Some("reflex_registered"))
            | (ReflexState::Cancelled, Some("reflex_cancelled"))
            | (ReflexState::Disabled, Some("reflex_disabled_by_operator"))
            | (ReflexState::Expired | ReflexState::ActionDenied, Some(_))
    );
    if !valid_lifecycle {
        return Err(storage_write_error(&format!(
            "REFLEX_GROUNDED_LIFECYCLE_KIND_INVALID: reflex_id={} status={:?} kind={:?}; remediation=route only a supported registration/cancellation/disable audit through the grounded lifecycle transaction",
            audit.reflex_id,
            audit.status,
            audit.details.get("kind")
        )));
    }
    let mut guard = PROJECTION_LOCK.lock().map_err(|_error| storage_write_error(
        "REFLEX_AUDIT_PROJECTION_LOCK_POISONED: restart the daemon and inspect the prior panic before retrying",
    ))?;
    ensure_locked(db, &mut guard).map_err(|error| storage_write_error(&error.to_string()))?;

    let order_key = reflex_audit_order_key(audit.ts_ns, &audit.audit_id, &audit.reflex_id);
    let order_value = encode_pointer(&OrderedSourcePointer::new(source_key, source_value))?;
    let existing_source = db.get_cf_revisioned(cf::CF_REFLEX_AUDIT, source_key)?;
    if let Some(existing) = &existing_source {
        if existing.value.as_deref() != Some(source_value) {
            return Err(storage_write_error(&format!(
                "REFLEX_REGISTRATION_SOURCE_IDENTITY_CONFLICT: source_key_hex={} expected_sha256={} actual_sha256={}; remediation=preserve both values and repair the duplicate audit identity before retrying",
                hex_encode(source_key),
                sha256_hex(source_value),
                existing
                    .value
                    .as_deref()
                    .map_or_else(|| "expired".to_owned(), sha256_hex)
            )));
        }
        require_exact_readback(db, cf::CF_REFLEX_AUDIT_ORDER, &order_key, &order_value)?;
        if let Some((desired_key, desired_value, _revision)) = desired {
            require_exact_readback(db, cf::CF_KV, desired_key, desired_value)?;
        }
        return Ok(());
    }

    let state_key = state_key(&audit.reflex_id);
    let state_revision = db.get_cf_revisioned(cf::CF_KV, &state_key)?;
    let prior_state = state_revision
        .as_ref()
        .and_then(|row| row.value.as_deref())
        .map(decode_projection_state)
        .transpose()?;
    let next_state = next_projection_state(db, prior_state, audit, source_value)?;
    let state_value = encode_json(&next_state)?;

    let order_revision = db.get_cf_revisioned(cf::CF_REFLEX_AUDIT_ORDER, &order_key)?;
    if let Some(existing) = &order_revision
        && existing.value.as_deref() != Some(order_value.as_slice())
    {
        return Err(storage_write_error(&format!(
            "REFLEX_REGISTRATION_ORDER_IDENTITY_CONFLICT: order_key_hex={}; remediation=preserve source/index rows and inspect the collision before repair",
            hex_encode(&order_key)
        )));
    }

    let mut guards = vec![
        CfRevisionGuard::new(cf::CF_REFLEX_AUDIT, source_key.to_vec(), None),
        CfRevisionGuard::new(
            cf::CF_REFLEX_AUDIT_ORDER,
            order_key.clone(),
            order_revision.map(|row| row.revision_sha256),
        ),
        CfRevisionGuard::new(
            cf::CF_KV,
            state_key.clone(),
            state_revision.map(|row| row.revision_sha256),
        ),
    ];
    let mut kv_rows = vec![(state_key.clone(), state_value.clone())];
    if let Some((desired_key, desired_value, expected_revision)) = desired {
        let existing = db.get_cf_revisioned(cf::CF_KV, desired_key)?;
        let actual_revision = existing.as_ref().map(|row| row.revision_sha256);
        if actual_revision != expected_revision {
            return Err(storage_write_error(&format!(
                "REFLEX_DURABLE_DEFINITION_REVISION_CONFLICT: reflex_id={} desired_key_hex={} expected_revision_sha256={:?} actual_revision_sha256={:?}; remediation=reload the exact desired state and retry the complete lifecycle transition",
                audit.reflex_id,
                hex_encode(desired_key),
                expected_revision.map(|revision| hex_encode(&revision)),
                actual_revision.map(|revision| hex_encode(&revision))
            )));
        }
        guards.push(CfRevisionGuard::new(
            cf::CF_KV,
            desired_key.to_vec(),
            expected_revision,
        ));
        kv_rows.push((desired_key.to_vec(), desired_value.to_vec()));
    }

    let registry_update = registration_registry_update(db, &audit.reflex_id)?;
    if let Some((registry_guard, registry_row)) = registry_update.as_ref() {
        guards.push(registry_guard.clone());
        kv_rows.push(registry_row.clone());
    }

    db.put_reflex_lifecycle_grounded_publication(
        guards,
        vec![
            (
                cf::CF_REFLEX_AUDIT,
                vec![(source_key.to_vec(), source_value.to_vec())],
            ),
            (
                cf::CF_REFLEX_AUDIT_ORDER,
                vec![(order_key.clone(), order_value.clone())],
            ),
            (cf::CF_KV, kv_rows),
        ],
        source_key,
        source_value,
        audit,
    )?;
    require_exact_readback(db, cf::CF_REFLEX_AUDIT, source_key, source_value)?;
    require_exact_readback(db, cf::CF_REFLEX_AUDIT_ORDER, &order_key, &order_value)?;
    require_exact_readback(db, cf::CF_KV, &state_key, &state_value)?;
    if let Some((desired_key, desired_value, _revision)) = desired {
        require_exact_readback(db, cf::CF_KV, desired_key, desired_value)?;
    }
    if let Some((_guard, (_key, registry_value))) = registry_update {
        require_exact_readback(db, cf::CF_KV, REGISTRY_KEY, &registry_value)?;
    }
    Ok(())
}

/// Atomically publishes several terminal lifecycle facts, their complete
/// projections/desired-state revisions, and one grounded constellation per
/// fact under a single Calyx WAL/MVCC sequence.
#[expect(
    clippy::too_many_lines,
    reason = "one multi-reflex command must prepare and verify every guarded projection member before crossing its single WAL boundary"
)]
pub fn write_projected_grounded_lifecycle_batch(
    db: &Db,
    entries: Vec<GroundedLifecycleProjectionEntry>,
) -> StorageResult<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut guard = PROJECTION_LOCK.lock().map_err(|_error| storage_write_error(
        "REFLEX_AUDIT_PROJECTION_LOCK_POISONED: restart the daemon and inspect the prior panic before retrying",
    ))?;
    ensure_locked(db, &mut guard).map_err(|error| storage_write_error(&error.to_string()))?;
    let mut seen_reflex_ids = std::collections::BTreeSet::new();
    let mut guards = Vec::new();
    let mut source_rows = Vec::with_capacity(entries.len());
    let mut order_rows = Vec::with_capacity(entries.len());
    let mut kv_rows = Vec::with_capacity(entries.len() * 2);
    let mut members = Vec::with_capacity(entries.len());
    let mut readbacks = Vec::with_capacity(entries.len());

    for entry in entries {
        let audit = &entry.audit;
        let kind = audit.details.get("kind").and_then(Value::as_str);
        if !matches!(
            (audit.status, kind),
            (ReflexState::Cancelled, Some("reflex_cancelled"))
                | (ReflexState::Disabled, Some("reflex_disabled_by_operator"))
        ) {
            return Err(storage_write_error(&format!(
                "REFLEX_LIFECYCLE_BATCH_KIND_INVALID: reflex_id={} status={:?} kind={kind:?}; remediation=route only terminal cancellation/disable facts through the lifecycle batch",
                audit.reflex_id, audit.status
            )));
        }
        if !seen_reflex_ids.insert(audit.reflex_id.clone()) {
            return Err(storage_write_error(&format!(
                "REFLEX_LIFECYCLE_BATCH_DUPLICATE_REFLEX: reflex_id={}; remediation=collapse the command to one terminal transition per reflex",
                audit.reflex_id
            )));
        }
        if db
            .get_cf_revisioned(cf::CF_REFLEX_AUDIT, &entry.source_key)?
            .is_some()
        {
            return Err(storage_write_error(&format!(
                "REFLEX_LIFECYCLE_BATCH_SOURCE_IDENTITY_CONFLICT: reflex_id={} source_key_hex={}; remediation=allocate a fresh audit identity and rebuild the entire command",
                audit.reflex_id,
                hex_encode(&entry.source_key)
            )));
        }
        let order_key = reflex_audit_order_key(audit.ts_ns, &audit.audit_id, &audit.reflex_id);
        let order_value = encode_pointer(&OrderedSourcePointer::new(
            &entry.source_key,
            &entry.source_value,
        ))?;
        let order_revision = db.get_cf_revisioned(cf::CF_REFLEX_AUDIT_ORDER, &order_key)?;
        if let Some(existing) = &order_revision
            && existing.value.as_deref() != Some(order_value.as_slice())
        {
            return Err(storage_write_error(&format!(
                "REFLEX_LIFECYCLE_BATCH_ORDER_IDENTITY_CONFLICT: reflex_id={} order_key_hex={}; remediation=preserve the existing pointer and rebuild the complete command with a fresh audit identity",
                audit.reflex_id,
                hex_encode(&order_key)
            )));
        }
        let state_key = state_key(&audit.reflex_id);
        let state_revision = db.get_cf_revisioned(cf::CF_KV, &state_key)?;
        let prior_state = state_revision
            .as_ref()
            .and_then(|row| row.value.as_deref())
            .map(decode_projection_state)
            .transpose()?;
        let state_value = encode_json(&next_projection_state(
            db,
            prior_state,
            audit,
            &entry.source_value,
        )?)?;
        guards.push(CfRevisionGuard::new(
            cf::CF_REFLEX_AUDIT,
            entry.source_key.clone(),
            None,
        ));
        guards.push(CfRevisionGuard::new(
            cf::CF_REFLEX_AUDIT_ORDER,
            order_key.clone(),
            order_revision.map(|row| row.revision_sha256),
        ));
        guards.push(CfRevisionGuard::new(
            cf::CF_KV,
            state_key.clone(),
            state_revision.map(|row| row.revision_sha256),
        ));
        source_rows.push((entry.source_key.clone(), entry.source_value.clone()));
        order_rows.push((order_key.clone(), order_value.clone()));
        kv_rows.push((state_key.clone(), state_value.clone()));
        if let Some(desired) = entry.desired {
            let actual = db.get_cf_revisioned(cf::CF_KV, &desired.key)?;
            let actual_revision = actual.as_ref().map(|row| row.revision_sha256);
            if actual_revision != Some(desired.expected_revision) {
                return Err(storage_write_error(&format!(
                    "REFLEX_LIFECYCLE_BATCH_DESIRED_REVISION_CONFLICT: reflex_id={} expected_revision={} actual_revision={:?}; remediation=reload every desired-state member and retry the complete command",
                    audit.reflex_id,
                    hex_encode(&desired.expected_revision),
                    actual_revision.map(|revision| hex_encode(&revision))
                )));
            }
            guards.push(CfRevisionGuard::new(
                cf::CF_KV,
                desired.key.clone(),
                Some(desired.expected_revision),
            ));
            kv_rows.push((desired.key.clone(), desired.value.clone()));
            readbacks.push((cf::CF_KV, desired.key, desired.value));
        }
        members.push(synapse_storage::ReflexGroundedLifecycleMember {
            source_key: entry.source_key.clone(),
            raw_bytes: entry.source_value.clone(),
            record: entry.audit,
        });
        readbacks.push((cf::CF_REFLEX_AUDIT, entry.source_key, entry.source_value));
        readbacks.push((cf::CF_REFLEX_AUDIT_ORDER, order_key, order_value));
        readbacks.push((cf::CF_KV, state_key, state_value));
    }

    let mut registry =
        read_registry(db).map_err(|error| storage_write_error(&error.to_string()))?;
    let missing = seen_reflex_ids
        .iter()
        .filter(|id| !registry.reflex_ids.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        let registry_revision = db
            .get_cf_revisioned(cf::CF_KV, REGISTRY_KEY)?
            .ok_or_else(|| storage_write_error(
                "REFLEX_AUDIT_REGISTRY_MISSING_DURING_LIFECYCLE_BATCH: preserve source/projection rows and rebuild the registry before retrying",
            ))?;
        registry.reflex_ids.extend(missing);
        registry.reflex_ids.sort();
        registry.reflex_ids.dedup();
        let registry_value = encode_json(&registry)?;
        guards.push(CfRevisionGuard::new(
            cf::CF_KV,
            REGISTRY_KEY,
            Some(registry_revision.revision_sha256),
        ));
        kv_rows.push((REGISTRY_KEY.to_vec(), registry_value.clone()));
        readbacks.push((cf::CF_KV, REGISTRY_KEY.to_vec(), registry_value));
    }
    db.put_reflex_lifecycle_grounded_batch_publication(
        guards,
        vec![
            (cf::CF_REFLEX_AUDIT, source_rows),
            (cf::CF_REFLEX_AUDIT_ORDER, order_rows),
            (cf::CF_KV, kv_rows),
        ],
        members,
    )?;
    for (cf_name, key, value) in readbacks {
        require_exact_readback(db, cf_name, &key, &value)?;
    }
    Ok(())
}

fn registration_registry_update(
    db: &Db,
    reflex_id: &str,
) -> StorageResult<Option<(CfRevisionGuard, RawRow)>> {
    let mut registry =
        read_registry(db).map_err(|error| storage_write_error(&error.to_string()))?;
    if registry.reflex_ids.iter().any(|id| id == reflex_id) {
        return Ok(None);
    }
    let registry_revision = db
        .get_cf_revisioned(cf::CF_KV, REGISTRY_KEY)?
        .ok_or_else(|| storage_write_error(
            "REFLEX_AUDIT_REGISTRY_MISSING_AFTER_PUBLICATION: preserve projection state and inspect CF_KV protection",
        ))?;
    registry.reflex_ids.push(reflex_id.to_owned());
    registry.reflex_ids.sort();
    registry.reflex_ids.dedup();
    let value = encode_json(&registry)?;
    Ok(Some((
        CfRevisionGuard::new(
            cf::CF_KV,
            REGISTRY_KEY,
            Some(registry_revision.revision_sha256),
        ),
        (REGISTRY_KEY.to_vec(), value),
    )))
}

pub fn global_history(db: &Db, limit: usize) -> ReflexResult<Vec<StoredReflexAudit>> {
    if limit > MAX_GLOBAL_HISTORY {
        return Err(projection_error(&format!(
            "REFLEX_AUDIT_HISTORY_LIMIT_EXCEEDED: requested={limit} maximum={MAX_GLOBAL_HISTORY}"
        )));
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut guard = lock_projection()?;
    ensure_locked(db, &mut guard)?;
    let candidate_limit = limit.saturating_mul(4);
    let (mut rows, more) = db
        .scan_cf_from(cf::CF_REFLEX_AUDIT_ORDER, &[], candidate_limit)
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_ORDER_SCAN_FAILED: {error}")))?;
    if rows.len() < limit && more {
        return Err(projection_error(&format!(
            "REFLEX_AUDIT_ORDER_FRAGMENTED_PAGE: requested={limit} live_rows={} candidate_limit={candidate_limit} more=true; remediation=run retention GC and inspect the ordered index expiry histogram before retrying",
            rows.len()
        )));
    }
    rows.truncate(limit);
    rows.into_iter()
        .map(|(order_key, pointer_bytes)| {
            let pointer = decode_pointer(cf::CF_REFLEX_AUDIT_ORDER, &pointer_bytes)
                .map_err(|error| projection_error(&error.to_string()))?;
            let source_key = pointer
                .source_key(cf::CF_REFLEX_AUDIT_ORDER)
                .map_err(|error| projection_error(&error.to_string()))?;
            let source_value = db
                .get_cf(cf::CF_REFLEX_AUDIT, &source_key)
                .map_err(|error| projection_error(&format!(
                    "REFLEX_AUDIT_ORDER_SOURCE_READ_FAILED: source_key_hex={}: {error}",
                    hex_encode(&source_key)
                )))?
                .ok_or_else(|| projection_error(&format!(
                    "REFLEX_AUDIT_ORDER_DANGLING_POINTER: order_key_hex={} source_key_hex={}; remediation=preserve the vault and inspect the atomic source/index retention boundary",
                    hex_encode(&order_key), hex_encode(&source_key)
                )))?;
            pointer
                .verify_source_value(cf::CF_REFLEX_AUDIT_ORDER, &source_value)
                .map_err(|error| projection_error(&error.to_string()))?;
            let audit = decode_json::<StoredReflexAudit>(&source_value)
                .map_err(|error| projection_error(&format!(
                    "REFLEX_AUDIT_ORDER_SOURCE_DECODE_FAILED: source_key_hex={}: {error}",
                    hex_encode(&source_key)
                )))?;
            let expected = reflex_audit_order_key(audit.ts_ns, &audit.audit_id, &audit.reflex_id);
            if expected != order_key {
                return Err(projection_error(&format!(
                    "REFLEX_AUDIT_ORDER_KEY_MISMATCH: actual={} expected={} source_key_hex={}; remediation=preserve both rows and repair the writer before rebuilding",
                    hex_encode(&order_key), hex_encode(&expected), hex_encode(&source_key)
                )));
            }
            Ok(audit)
        })
        .collect()
}

pub fn terminal_statuses(db: &Db) -> ReflexResult<Vec<ReflexStatus>> {
    let mut guard = lock_projection()?;
    ensure_locked(db, &mut guard)?;
    let registry = read_registry(db)?;
    let mut output = Vec::new();
    for reflex_id in registry.reflex_ids {
        let state = read_state(db, &reflex_id)?.ok_or_else(|| projection_error(&format!(
            "REFLEX_AUDIT_REGISTERED_STATE_MISSING: reflex_id={reflex_id}; remediation=preserve the registry and inspect the missing aggregate row"
        )))?;
        if let Some(status) = state.accumulator.into_terminal_status() {
            output.push(status);
        }
    }
    Ok(output)
}

pub fn terminal_status(db: &Db, reflex_id: &str) -> ReflexResult<Option<ReflexStatus>> {
    let mut guard = lock_projection()?;
    ensure_locked(db, &mut guard)?;
    Ok(read_state(db, reflex_id)?.and_then(|state| state.accumulator.into_terminal_status()))
}

pub fn recursion_clamps_total(db: &Db) -> ReflexResult<u64> {
    let mut guard = lock_projection()?;
    ensure_locked(db, &mut guard)?;
    let bytes = db
        .get_cf(cf::CF_KV, CLAMP_KEY)
        .map_err(|error| projection_error(&format!("REFLEX_CLAMP_PROJECTION_READ_FAILED: {error}")))?
        .ok_or_else(|| projection_error(
            "REFLEX_CLAMP_PROJECTION_MISSING: published projection lacks its durable aggregate; remediation=preserve the meta row and inspect CF_KV protection",
        ))?;
    Ok(decode_clamp(&bytes)
        .map_err(|error| projection_error(&error.to_string()))?
        .total)
}

fn build_projection(db: &Db) -> ReflexResult<()> {
    let mut progress = if let Some(progress) = read_progress(db)? {
        validate_progress(&progress)?;
        progress
    } else {
        prove_initial_projection_empty(db)?;
        let initial = ProjectionProgress {
            schema_version: SCHEMA_VERSION,
            stage: BuildStage::Order,
            resume_after_source_key_hex: None,
            resume_after_state_key_hex: None,
            rows_indexed: 0,
            states_written: 0,
        };
        write_initial_progress(db, &initial)?;
        initial
    };
    if progress.stage == BuildStage::Complete {
        return Err(projection_error(
            "REFLEX_AUDIT_META_MISSING_AFTER_COMPLETE_PROGRESS: preserve the projection and restore readable meta only after exact reconciliation",
        ));
    }

    if progress.stage == BuildStage::Order {
        backfill_order(db, &mut progress)?;
        progress.stage = BuildStage::Aggregates;
        write_progress_guarded(db, &progress)?;
    }
    let expected_states = fold_source_states(db)?;
    backfill_states(db, &expected_states, &mut progress)?;
    let expected_clamps = count_source_clamps(db)?;
    write_registry_and_clamp(db, &expected_states, expected_clamps)?;
    reconcile_order_sets(db)?;
    reconcile_build_aggregates(db, &expected_states, expected_clamps)?;
    progress.stage = BuildStage::Complete;
    publish_meta(db, &progress)?;
    tracing::info!(
        code = "REFLEX_AUDIT_PROJECTION_BUILT",
        source_rows = progress.rows_indexed,
        states = progress.states_written,
        recursion_clamps_total = expected_clamps,
        source_of_truth =
            "CF_REFLEX_AUDIT exact rows + CF_REFLEX_AUDIT_ORDER pointers + CF_KV projection state",
        "published exact reflex audit projections after full source/index/aggregate reconciliation"
    );
    Ok(())
}

fn backfill_order(db: &Db, progress: &mut ProjectionProgress) -> ReflexResult<()> {
    loop {
        let start = match progress.resume_after_source_key_hex.as_deref() {
            Some(value) => key_after(hex_decode(value).ok_or_else(|| projection_error(
                "REFLEX_AUDIT_PROGRESS_SOURCE_CURSOR_INVALID: preserve the progress row before repair",
            ))?),
            None => Vec::new(),
        };
        let (rows, more) = db
            .scan_cf_from(cf::CF_REFLEX_AUDIT, &start, BACKFILL_ROWS)
            .map_err(|error| {
                projection_error(&format!("REFLEX_AUDIT_BACKFILL_SCAN_FAILED: {error}"))
            })?;
        if rows.is_empty() {
            if more {
                return Err(projection_error("REFLEX_AUDIT_BACKFILL_SCAN_STALLED"));
            }
            return Ok(());
        }
        let progress_revision = required_revision(db, cf::CF_KV, PROGRESS_KEY)?;
        let mut guards = vec![CfRevisionGuard::new(
            cf::CF_KV,
            PROGRESS_KEY,
            Some(progress_revision),
        )];
        let mut index_rows = Vec::with_capacity(rows.len());
        for (source_key, source_value) in &rows {
            let source_revisioned = db
                .get_cf_revisioned(cf::CF_REFLEX_AUDIT, source_key)
                .map_err(|error| {
                    projection_error(&format!(
                        "REFLEX_AUDIT_SOURCE_REVISION_READ_FAILED: key_hex={}: {error}",
                        hex_encode(source_key)
                    ))
                })?
                .ok_or_else(|| projection_error("REFLEX_AUDIT_SOURCE_DISAPPEARED_DURING_BUILD"))?;
            if source_revisioned.value.as_deref() != Some(source_value.as_slice()) {
                return Err(projection_error("REFLEX_AUDIT_SOURCE_CHANGED_DURING_BUILD"));
            }
            let audit = decode_json::<StoredReflexAudit>(source_value).map_err(|error| {
                projection_error(&format!(
                    "REFLEX_AUDIT_SOURCE_DECODE_FAILED: key_hex={}: {error}",
                    hex_encode(source_key)
                ))
            })?;
            let order_key = reflex_audit_order_key(audit.ts_ns, &audit.audit_id, &audit.reflex_id);
            let order_value = encode_pointer(&OrderedSourcePointer::new(source_key, source_value))
                .map_err(|error| projection_error(&error.to_string()))?;
            guards.push(CfRevisionGuard::new(
                cf::CF_REFLEX_AUDIT,
                source_key.clone(),
                Some(source_revisioned.revision_sha256),
            ));
            guards.push(CfRevisionGuard::new(
                cf::CF_REFLEX_AUDIT_ORDER,
                order_key.clone(),
                None,
            ));
            index_rows.push(RawRowWithExpiry::preserving_expiry(
                order_key,
                order_value,
                source_revisioned.expires_at_ms,
            ));
        }
        let last_source_key = rows
            .last()
            .map(|row| row.0.as_slice())
            .ok_or_else(|| projection_error("REFLEX_AUDIT_BACKFILL_LAST_KEY_MISSING"))?;
        progress.resume_after_source_key_hex = Some(hex_encode(last_source_key));
        progress.rows_indexed = progress.rows_indexed.saturating_add(rows.len() as u64);
        let progress_bytes = encode_json(progress).map_err(|error| {
            projection_error(&format!("REFLEX_AUDIT_PROGRESS_ENCODE_FAILED: {error}"))
        })?;
        let outcome = db
            .put_cf_batches_with_expiry_if_revisions_pressure_bypass(
                guards,
                vec![
                    (cf::CF_REFLEX_AUDIT_ORDER, index_rows),
                    (
                        cf::CF_KV,
                        vec![RawRowWithExpiry::retained(PROGRESS_KEY, progress_bytes)],
                    ),
                ],
            )
            .map_err(|error| {
                projection_error(&format!("REFLEX_AUDIT_BACKFILL_COMMIT_FAILED: {error}"))
            })?;
        if !outcome.applied {
            return Err(projection_error(&format!(
                "REFLEX_AUDIT_BACKFILL_REVISION_CONFLICT: {:?}",
                outcome.conflict
            )));
        }
        if !more {
            return Ok(());
        }
    }
}

fn backfill_states(
    db: &Db,
    expected: &BTreeMap<String, ReflexProjectionState>,
    progress: &mut ProjectionProgress,
) -> ReflexResult<()> {
    let after = match progress.resume_after_state_key_hex.as_deref() {
        Some(value) => Some(
            hex_decode(value)
                .ok_or_else(|| projection_error("REFLEX_AUDIT_PROGRESS_STATE_CURSOR_INVALID"))?,
        ),
        None => None,
    };
    let mut pending = expected
        .iter()
        .map(|(id, state)| (state_key(id), encode_json(state)))
        .map(|(key, value)| value.map(|value| (key, value)))
        .collect::<StorageResult<Vec<_>>>()
        .map_err(|error| projection_error(&error.to_string()))?;
    pending.sort_by(|left, right| left.0.cmp(&right.0));
    if let Some(after) = after {
        pending.retain(|(key, _)| key > &after);
    }
    for chunk in pending.chunks(BACKFILL_ROWS) {
        let progress_revision = required_revision(db, cf::CF_KV, PROGRESS_KEY)?;
        let mut guards = vec![CfRevisionGuard::new(
            cf::CF_KV,
            PROGRESS_KEY,
            Some(progress_revision),
        )];
        let mut rows = Vec::with_capacity(chunk.len() + 1);
        for (key, value) in chunk {
            let existing = db.get_cf_revisioned(cf::CF_KV, key).map_err(|error| {
                projection_error(&format!("REFLEX_AUDIT_STATE_REVISION_READ_FAILED: {error}"))
            })?;
            if let Some(existing) = &existing
                && existing.value.as_deref() != Some(value.as_slice())
            {
                return Err(projection_error(&format!(
                    "REFLEX_AUDIT_STATE_IDENTITY_CONFLICT: key_hex={}",
                    hex_encode(key)
                )));
            }
            guards.push(CfRevisionGuard::new(
                cf::CF_KV,
                key.clone(),
                existing.map(|row| row.revision_sha256),
            ));
            rows.push((key.clone(), value.clone()));
        }
        let last = chunk
            .last()
            .map(|row| row.0.clone())
            .ok_or_else(|| projection_error("REFLEX_AUDIT_STATE_CHUNK_EMPTY"))?;
        progress.resume_after_state_key_hex = Some(hex_encode(&last));
        progress.states_written = progress.states_written.saturating_add(chunk.len() as u64);
        rows.push((
            PROGRESS_KEY.to_vec(),
            encode_json(progress).map_err(|error| projection_error(&error.to_string()))?,
        ));
        let outcome = db
            .put_cf_batches_if_revisions_pressure_bypass(guards, vec![(cf::CF_KV, rows)])
            .map_err(|error| {
                projection_error(&format!(
                    "REFLEX_AUDIT_STATE_BACKFILL_COMMIT_FAILED: {error}"
                ))
            })?;
        if !outcome.applied {
            return Err(projection_error(&format!(
                "REFLEX_AUDIT_STATE_BACKFILL_REVISION_CONFLICT: {:?}",
                outcome.conflict
            )));
        }
    }
    Ok(())
}

fn write_registry_and_clamp(
    db: &Db,
    states: &BTreeMap<String, ReflexProjectionState>,
    clamp_total: u64,
) -> ReflexResult<()> {
    let registry = ReflexProjectionRegistry {
        schema_version: SCHEMA_VERSION,
        reflex_ids: states.keys().cloned().collect(),
    };
    let clamp = ClampProjection {
        schema_version: SCHEMA_VERSION,
        total: clamp_total,
    };
    write_idempotent_kv_rows(
        db,
        vec![
            (
                REGISTRY_KEY.to_vec(),
                encode_json(&registry).map_err(|error| projection_error(&error.to_string()))?,
            ),
            (
                CLAMP_KEY.to_vec(),
                encode_json(&clamp).map_err(|error| projection_error(&error.to_string()))?,
            ),
        ],
    )
}

fn reconcile_order_sets(db: &Db) -> ReflexResult<()> {
    let source = scan_all(db, cf::CF_REFLEX_AUDIT)?;
    let actual = scan_all(db, cf::CF_REFLEX_AUDIT_ORDER)?;
    let mut expected = Vec::with_capacity(source.len());
    for (source_key, source_value) in &source {
        let audit = decode_json::<StoredReflexAudit>(source_value).map_err(|error| {
            projection_error(&format!(
                "REFLEX_AUDIT_RECONCILE_SOURCE_DECODE_FAILED: key_hex={}: {error}",
                hex_encode(source_key)
            ))
        })?;
        expected.push((
            reflex_audit_order_key(audit.ts_ns, &audit.audit_id, &audit.reflex_id),
            encode_pointer(&OrderedSourcePointer::new(source_key, source_value))
                .map_err(|error| projection_error(&error.to_string()))?,
        ));
    }
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    if expected != actual {
        return Err(projection_error(&format!(
            "REFLEX_AUDIT_ORDER_RECONCILIATION_FAILED: source_rows={} index_rows={} mismatch={}; remediation=preserve the vault and identify the missing/extra/corrupt projection row before rebuilding",
            expected.len(),
            actual.len(),
            first_mismatch(&expected, &actual)
        )));
    }
    Ok(())
}

fn validate_durable_aggregates(db: &Db) -> ReflexResult<()> {
    let registry = read_registry(db)?;
    let actual_states = db
        .scan_cf_prefix(cf::CF_KV, STATE_PREFIX)
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_STATE_SCAN_FAILED: {error}")))?;
    let actual_ids = actual_states
        .iter()
        .map(|(key, value)| {
            let state = decode_projection_state(value)
                .map_err(|error| projection_error(&error.to_string()))?;
            if *key != state_key(&state.reflex_id) {
                return Err(projection_error(&format!(
                    "REFLEX_AUDIT_STATE_KEY_MISMATCH: key_hex={} reflex_id={}",
                    hex_encode(key),
                    state.reflex_id
                )));
            }
            Ok(state.reflex_id)
        })
        .collect::<ReflexResult<Vec<_>>>()?;
    if actual_ids != registry.reflex_ids {
        return Err(projection_error(&format!(
            "REFLEX_AUDIT_REGISTRY_RECONCILIATION_FAILED: registry_ids={} state_rows={}; remediation=preserve both surfaces and restore their atomic identity set",
            registry.reflex_ids.len(),
            actual_ids.len()
        )));
    }
    let clamp = db
        .get_cf(cf::CF_KV, CLAMP_KEY)
        .map_err(|error| {
            projection_error(&format!("REFLEX_CLAMP_PROJECTION_READ_FAILED: {error}"))
        })?
        .ok_or_else(|| projection_error("REFLEX_CLAMP_PROJECTION_MISSING"))?;
    decode_clamp(&clamp).map_err(|error| projection_error(&error.to_string()))?;
    let retained = scan_all(db, cf::CF_REFLEX_AUDIT)?;
    for (_key, value) in retained {
        let audit = decode_json::<StoredReflexAudit>(&value).map_err(|error| {
            projection_error(&format!("REFLEX_AUDIT_SOURCE_DECODE_FAILED: {error}"))
        })?;
        if !registry.reflex_ids.iter().any(|id| id == &audit.reflex_id) {
            return Err(projection_error(&format!(
                "REFLEX_AUDIT_RETAINED_SOURCE_UNREGISTERED: reflex_id={}; remediation=inspect the missing atomic state/registry mutation",
                audit.reflex_id
            )));
        }
    }
    Ok(())
}

fn reconcile_build_aggregates(
    db: &Db,
    expected_states: &BTreeMap<String, ReflexProjectionState>,
    expected_clamps: u64,
) -> ReflexResult<()> {
    let actual = db
        .scan_cf_prefix(cf::CF_KV, STATE_PREFIX)
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_STATE_SCAN_FAILED: {error}")))?;
    let expected = expected_states
        .iter()
        .map(|(id, state)| Ok((state_key(id), encode_json(state)?)))
        .collect::<StorageResult<Vec<_>>>()
        .map_err(|error| projection_error(&error.to_string()))?;
    if actual != expected {
        return Err(projection_error("REFLEX_AUDIT_STATE_RECONCILIATION_FAILED"));
    }
    let registry = read_registry(db)?;
    if registry.reflex_ids != expected_states.keys().cloned().collect::<Vec<_>>() {
        return Err(projection_error(
            "REFLEX_AUDIT_REGISTRY_BUILD_RECONCILIATION_FAILED",
        ));
    }
    let clamp = db
        .get_cf(cf::CF_KV, CLAMP_KEY)
        .map_err(|error| projection_error(&error.to_string()))?
        .ok_or_else(|| projection_error("REFLEX_CLAMP_PROJECTION_MISSING"))?;
    if decode_clamp(&clamp)
        .map_err(|error| projection_error(&error.to_string()))?
        .total
        != expected_clamps
    {
        return Err(projection_error(
            "REFLEX_CLAMP_PROJECTION_RECONCILIATION_FAILED",
        ));
    }
    Ok(())
}

fn fold_source_states(db: &Db) -> ReflexResult<BTreeMap<String, ReflexProjectionState>> {
    let rows = scan_all(db, cf::CF_REFLEX_AUDIT)?;
    let mut grouped = BTreeMap::<String, Vec<(StoredReflexAudit, Vec<u8>)>>::new();
    for (_key, value) in rows {
        let audit = decode_json::<StoredReflexAudit>(&value).map_err(|error| {
            projection_error(&format!("REFLEX_AUDIT_SOURCE_DECODE_FAILED: {error}"))
        })?;
        grouped
            .entry(audit.reflex_id.clone())
            .or_default()
            .push((audit, value));
    }
    let mut output = BTreeMap::new();
    for (reflex_id, mut audits) in grouped {
        audits.sort_by(|left, right| {
            (left.0.ts_ns, &left.0.audit_id).cmp(&(right.0.ts_ns, &right.0.audit_id))
        });
        let mut accumulator = AuditStatusAccumulator::new(reflex_id.clone());
        for (audit, _value) in &audits {
            accumulator.record(audit.clone());
        }
        let (latest, latest_value) = audits
            .last()
            .ok_or_else(|| projection_error("REFLEX_AUDIT_GROUP_EMPTY_DURING_FOLD"))?;
        output.insert(
            reflex_id.clone(),
            ReflexProjectionState {
                schema_version: SCHEMA_VERSION,
                reflex_id,
                latest_ts_ns: latest.ts_ns,
                latest_audit_id: latest.audit_id.clone(),
                latest_source_value_sha256: sha256_hex(latest_value),
                accumulator,
            },
        );
    }
    Ok(output)
}

fn count_source_clamps(db: &Db) -> ReflexResult<u64> {
    let mut total = 0_u64;
    for (_key, value) in scan_all(db, cf::CF_REFLEX_AUDIT)? {
        let audit = decode_json::<StoredReflexAudit>(&value).map_err(|error| {
            projection_error(&format!("REFLEX_AUDIT_SOURCE_DECODE_FAILED: {error}"))
        })?;
        if audit.error_code.as_deref() == Some(error_codes::REFLEX_RECURSION_LIMIT) {
            total = total.checked_add(1).ok_or_else(|| {
                projection_error("REFLEX_RECURSION_CLAMP_COUNTER_OVERFLOW_DURING_BUILD")
            })?;
        }
    }
    Ok(total)
}

fn next_projection_state(
    db: &Db,
    prior: Option<ReflexProjectionState>,
    audit: &StoredReflexAudit,
    source_value: &[u8],
) -> StorageResult<ReflexProjectionState> {
    if let Some(mut state) = prior {
        validate_state(&state)?;
        let new_order = (audit.ts_ns, audit.audit_id.as_str());
        let prior_order = (state.latest_ts_ns, state.latest_audit_id.as_str());
        if new_order >= prior_order {
            state.accumulator.record(audit.clone());
            state.latest_ts_ns = audit.ts_ns;
            state.latest_audit_id.clone_from(&audit.audit_id);
            state.latest_source_value_sha256 = sha256_hex(source_value);
            return Ok(state);
        }
        let mut audits = db
            .scan_cf_prefix(
                cf::CF_REFLEX_AUDIT,
                audit_key_prefix(&audit.reflex_id).as_bytes(),
            )?
            .into_iter()
            .map(|(_key, value)| decode_json::<StoredReflexAudit>(&value))
            .collect::<StorageResult<Vec<_>>>()?;
        audits.push(audit.clone());
        audits.sort_by(|left, right| {
            (left.ts_ns, &left.audit_id).cmp(&(right.ts_ns, &right.audit_id))
        });
        let mut accumulator = AuditStatusAccumulator::new(audit.reflex_id.clone());
        for item in &audits {
            accumulator.record(item.clone());
        }
        let latest = audits
            .last()
            .ok_or_else(|| storage_write_error("REFLEX_AUDIT_RECOMPUTE_EMPTY_AFTER_INSERT"))?;
        return Ok(ReflexProjectionState {
            schema_version: SCHEMA_VERSION,
            reflex_id: audit.reflex_id.clone(),
            latest_ts_ns: latest.ts_ns,
            latest_audit_id: latest.audit_id.clone(),
            latest_source_value_sha256: if latest.audit_id == audit.audit_id
                && latest.ts_ns == audit.ts_ns
            {
                sha256_hex(source_value)
            } else {
                state.latest_source_value_sha256
            },
            accumulator,
        });
    }
    let mut accumulator = AuditStatusAccumulator::new(audit.reflex_id.clone());
    accumulator.record(audit.clone());
    Ok(ReflexProjectionState {
        schema_version: SCHEMA_VERSION,
        reflex_id: audit.reflex_id.clone(),
        latest_ts_ns: audit.ts_ns,
        latest_audit_id: audit.audit_id.clone(),
        latest_source_value_sha256: sha256_hex(source_value),
        accumulator,
    })
}

fn read_state(db: &Db, reflex_id: &str) -> ReflexResult<Option<ReflexProjectionState>> {
    db.get_cf(cf::CF_KV, &state_key(reflex_id))
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_STATE_READ_FAILED: {error}")))?
        .map(|bytes| {
            decode_projection_state(&bytes).map_err(|error| projection_error(&error.to_string()))
        })
        .transpose()
}

fn decode_projection_state(bytes: &[u8]) -> StorageResult<ReflexProjectionState> {
    let state = decode_json::<ReflexProjectionState>(bytes)?;
    validate_state(&state)?;
    Ok(state)
}

fn validate_state(state: &ReflexProjectionState) -> StorageResult<()> {
    if state.schema_version != SCHEMA_VERSION || state.reflex_id.is_empty() {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_KV.to_owned(),
            detail: format!(
                "REFLEX_AUDIT_STATE_INVALID: schema_version={} expected={SCHEMA_VERSION} reflex_id_empty={}; remediation=preserve the row and inspect its writer",
                state.schema_version,
                state.reflex_id.is_empty()
            ),
        });
    }
    Ok(())
}

fn read_registry(db: &Db) -> ReflexResult<ReflexProjectionRegistry> {
    let bytes = db
        .get_cf(cf::CF_KV, REGISTRY_KEY)
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_REGISTRY_READ_FAILED: {error}")))?
        .ok_or_else(|| projection_error("REFLEX_AUDIT_REGISTRY_MISSING"))?;
    let registry = decode_json::<ReflexProjectionRegistry>(&bytes)
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_REGISTRY_CORRUPT: {error}")))?;
    if registry.schema_version != SCHEMA_VERSION
        || !registry.reflex_ids.windows(2).all(|pair| pair[0] < pair[1])
        || registry.reflex_ids.iter().any(String::is_empty)
    {
        return Err(projection_error(
            "REFLEX_AUDIT_REGISTRY_INVALID: ids must be non-empty, unique, and sorted",
        ));
    }
    Ok(registry)
}

fn decode_clamp(bytes: &[u8]) -> StorageResult<ClampProjection> {
    let clamp = decode_json::<ClampProjection>(bytes)?;
    if clamp.schema_version != SCHEMA_VERSION {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_KV.to_owned(),
            detail: format!(
                "REFLEX_CLAMP_PROJECTION_SCHEMA_UNSUPPORTED: actual={} expected={SCHEMA_VERSION}",
                clamp.schema_version
            ),
        });
    }
    Ok(clamp)
}

fn read_meta(db: &Db) -> ReflexResult<Option<ProjectionMeta>> {
    db.get_cf(cf::CF_KV, META_KEY)
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_META_READ_FAILED: {error}")))?
        .map(|bytes| {
            decode_json::<ProjectionMeta>(&bytes)
                .map_err(|error| projection_error(&format!("REFLEX_AUDIT_META_CORRUPT: {error}")))
        })
        .transpose()
}

fn validate_meta(meta: &ProjectionMeta) -> ReflexResult<()> {
    if meta.schema_version != SCHEMA_VERSION || !meta.readable {
        return Err(projection_error(&format!(
            "REFLEX_AUDIT_META_UNREADABLE: schema_version={} expected={SCHEMA_VERSION} readable={}",
            meta.schema_version, meta.readable
        )));
    }
    Ok(())
}

fn read_progress(db: &Db) -> ReflexResult<Option<ProjectionProgress>> {
    db.get_cf(cf::CF_KV, PROGRESS_KEY)
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_PROGRESS_READ_FAILED: {error}")))?
        .map(|bytes| {
            decode_json::<ProjectionProgress>(&bytes).map_err(|error| {
                projection_error(&format!("REFLEX_AUDIT_PROGRESS_CORRUPT: {error}"))
            })
        })
        .transpose()
}

fn validate_progress(progress: &ProjectionProgress) -> ReflexResult<()> {
    if progress.schema_version != SCHEMA_VERSION {
        return Err(projection_error(&format!(
            "REFLEX_AUDIT_PROGRESS_SCHEMA_UNSUPPORTED: actual={} expected={SCHEMA_VERSION}",
            progress.schema_version
        )));
    }
    Ok(())
}

fn prove_initial_projection_empty(db: &Db) -> ReflexResult<()> {
    let (rows, _more) = db
        .scan_cf_from(cf::CF_REFLEX_AUDIT_ORDER, &[], 1)
        .map_err(|error| {
            projection_error(&format!("REFLEX_AUDIT_INITIAL_PROBE_FAILED: {error}"))
        })?;
    if !rows.is_empty() {
        return Err(projection_error(
            "REFLEX_AUDIT_UNTRACKED_PARTIAL_BUILD: order rows exist without meta/progress",
        ));
    }
    let states = db
        .scan_cf_prefix(cf::CF_KV, STATE_PREFIX)
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_STATE_PROBE_FAILED: {error}")))?;
    if !states.is_empty()
        || db
            .get_cf(cf::CF_KV, REGISTRY_KEY)
            .map_err(|error| projection_error(&error.to_string()))?
            .is_some()
        || db
            .get_cf(cf::CF_KV, CLAMP_KEY)
            .map_err(|error| projection_error(&error.to_string()))?
            .is_some()
    {
        return Err(projection_error(
            "REFLEX_AUDIT_UNTRACKED_AGGREGATES: durable projection rows exist without meta/progress",
        ));
    }
    Ok(())
}

fn write_initial_progress(db: &Db, progress: &ProjectionProgress) -> ReflexResult<()> {
    let bytes = encode_json(progress).map_err(|error| projection_error(&error.to_string()))?;
    let outcome = db
        .put_cf_batches_if_revisions_pressure_bypass(
            vec![CfRevisionGuard::new(cf::CF_KV, PROGRESS_KEY, None)],
            vec![(cf::CF_KV, vec![(PROGRESS_KEY.to_vec(), bytes.clone())])],
        )
        .map_err(|error| {
            projection_error(&format!("REFLEX_AUDIT_PROGRESS_WRITE_FAILED: {error}"))
        })?;
    if !outcome.applied {
        return Err(projection_error("REFLEX_AUDIT_PROGRESS_INIT_CONFLICT"));
    }
    require_exact_readback(db, cf::CF_KV, PROGRESS_KEY, &bytes)
        .map_err(|error| projection_error(&error.to_string()))
}

fn write_progress_guarded(db: &Db, progress: &ProjectionProgress) -> ReflexResult<()> {
    let revision = required_revision(db, cf::CF_KV, PROGRESS_KEY)?;
    let bytes = encode_json(progress).map_err(|error| projection_error(&error.to_string()))?;
    let outcome = db
        .put_cf_batches_if_revisions_pressure_bypass(
            vec![CfRevisionGuard::new(
                cf::CF_KV,
                PROGRESS_KEY,
                Some(revision),
            )],
            vec![(cf::CF_KV, vec![(PROGRESS_KEY.to_vec(), bytes)])],
        )
        .map_err(|error| {
            projection_error(&format!("REFLEX_AUDIT_PROGRESS_WRITE_FAILED: {error}"))
        })?;
    if !outcome.applied {
        return Err(projection_error("REFLEX_AUDIT_PROGRESS_REVISION_CONFLICT"));
    }
    Ok(())
}

fn publish_meta(db: &Db, progress: &ProjectionProgress) -> ReflexResult<()> {
    let progress_revision = required_revision(db, cf::CF_KV, PROGRESS_KEY)?;
    let meta = ProjectionMeta {
        schema_version: SCHEMA_VERSION,
        readable: true,
        built_at_unix_ms: now_ms()?,
        source_rows_at_build: progress.rows_indexed,
    };
    let meta_bytes = encode_json(&meta).map_err(|error| projection_error(&error.to_string()))?;
    let progress_bytes =
        encode_json(progress).map_err(|error| projection_error(&error.to_string()))?;
    let outcome = db
        .put_cf_batches_if_revisions_pressure_bypass(
            vec![
                CfRevisionGuard::new(cf::CF_KV, META_KEY, None),
                CfRevisionGuard::new(cf::CF_KV, PROGRESS_KEY, Some(progress_revision)),
            ],
            vec![(
                cf::CF_KV,
                vec![
                    (META_KEY.to_vec(), meta_bytes.clone()),
                    (PROGRESS_KEY.to_vec(), progress_bytes.clone()),
                ],
            )],
        )
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_META_PUBLISH_FAILED: {error}")))?;
    if !outcome.applied {
        return Err(projection_error("REFLEX_AUDIT_META_PUBLISH_CONFLICT"));
    }
    require_exact_readback(db, cf::CF_KV, META_KEY, &meta_bytes)
        .map_err(|error| projection_error(&error.to_string()))?;
    require_exact_readback(db, cf::CF_KV, PROGRESS_KEY, &progress_bytes)
        .map_err(|error| projection_error(&error.to_string()))
}

fn write_idempotent_kv_rows(db: &Db, rows: Vec<(Vec<u8>, Vec<u8>)>) -> ReflexResult<()> {
    let mut guards = Vec::with_capacity(rows.len());
    for (key, value) in &rows {
        let existing = db
            .get_cf_revisioned(cf::CF_KV, key)
            .map_err(|error| projection_error(&error.to_string()))?;
        if let Some(existing) = &existing
            && existing.value.as_deref() != Some(value.as_slice())
        {
            return Err(projection_error(&format!(
                "REFLEX_AUDIT_IDEMPOTENT_KV_CONFLICT: key_hex={}",
                hex_encode(key)
            )));
        }
        guards.push(CfRevisionGuard::new(
            cf::CF_KV,
            key.clone(),
            existing.map(|row| row.revision_sha256),
        ));
    }
    let outcome = db
        .put_cf_batches_if_revisions_pressure_bypass(guards, vec![(cf::CF_KV, rows)])
        .map_err(|error| {
            projection_error(&format!("REFLEX_AUDIT_IDEMPOTENT_KV_WRITE_FAILED: {error}"))
        })?;
    if !outcome.applied {
        return Err(projection_error(
            "REFLEX_AUDIT_IDEMPOTENT_KV_REVISION_CONFLICT",
        ));
    }
    Ok(())
}

fn required_revision(db: &Db, cf_name: &str, key: &[u8]) -> ReflexResult<[u8; 32]> {
    db.get_cf_revisioned(cf_name, key)
        .map_err(|error| projection_error(&error.to_string()))?
        .map(|row| row.revision_sha256)
        .ok_or_else(|| {
            projection_error(&format!(
                "REFLEX_AUDIT_REQUIRED_ROW_MISSING: cf={cf_name} key_hex={}",
                hex_encode(key)
            ))
        })
}

fn scan_all(db: &Db, cf_name: &str) -> ReflexResult<Vec<RawRow>> {
    let mut output = Vec::new();
    let mut start = Vec::new();
    loop {
        let (rows, more) = db
            .scan_cf_from(cf_name, &start, BACKFILL_ROWS)
            .map_err(|error| {
                projection_error(&format!("REFLEX_AUDIT_SCAN_FAILED: cf={cf_name}: {error}"))
            })?;
        if rows.is_empty() {
            if more {
                return Err(projection_error(&format!(
                    "REFLEX_AUDIT_SCAN_STALLED: cf={cf_name}"
                )));
            }
            break;
        }
        start = key_after(
            rows.last()
                .map(|row| row.0.clone())
                .ok_or_else(|| projection_error("REFLEX_AUDIT_SCAN_LAST_KEY_MISSING"))?,
        );
        output.extend(rows);
        if !more {
            break;
        }
    }
    Ok(output)
}

fn require_exact_readback(
    db: &Db,
    cf_name: &str,
    key: &[u8],
    expected: &[u8],
) -> StorageResult<()> {
    let actual = db.get_cf(cf_name, key)?;
    if actual.as_deref() != Some(expected) {
        return Err(storage_write_error(&format!(
            "REFLEX_AUDIT_PROJECTION_READBACK_MISMATCH: cf={cf_name} key_hex={} expected_sha256={} actual_sha256={}; remediation=preserve the committed sequence and inspect the physical row",
            hex_encode(key),
            sha256_hex(expected),
            actual
                .as_deref()
                .map_or_else(|| "absent".to_owned(), sha256_hex)
        )));
    }
    Ok(())
}

fn state_key(reflex_id: &str) -> Vec<u8> {
    let mut key = STATE_PREFIX.to_vec();
    key.extend_from_slice(reflex_id.as_bytes());
    key
}

fn audit_key_prefix(reflex_id: &str) -> String {
    format!("{reflex_id}:")
}

fn key_after(mut key: Vec<u8>) -> Vec<u8> {
    key.push(0);
    key
}

fn first_mismatch(expected: &[(Vec<u8>, Vec<u8>)], actual: &[(Vec<u8>, Vec<u8>)]) -> String {
    let common = expected.len().min(actual.len());
    for index in 0..common {
        if expected[index] != actual[index] {
            return format!(
                "row={index} expected_key={} actual_key={}",
                hex_encode(&expected[index].0),
                hex_encode(&actual[index].0)
            );
        }
    }
    format!("length_boundary={common}")
}

fn projection_error(detail: &str) -> ReflexError {
    ReflexError::ParamsInvalid {
        detail: detail.to_owned(),
    }
}

fn storage_write_error(detail: &str) -> StorageError {
    StorageError::WriteFailed {
        cf_name: cf::CF_REFLEX_AUDIT.to_owned(),
        detail: detail.to_owned(),
    }
}

fn now_ms() -> ReflexResult<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| projection_error(&format!("REFLEX_AUDIT_CLOCK_BEFORE_EPOCH: {error}")))?
        .as_millis();
    u64::try_from(millis).map_err(|_error| projection_error("REFLEX_AUDIT_CLOCK_OVERFLOW"))
}
