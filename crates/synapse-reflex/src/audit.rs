use synapse_core::StoredReflexAudit;
use synapse_storage::{Db, StorageResult, encode_json};

/// Writes one reflex audit row to `CF_REFLEX_AUDIT`.
///
/// # Errors
///
/// Returns a storage error when JSON encoding fails or the storage batcher
/// rejects the write.
#[tracing::instrument(
    skip_all,
    fields(
        reflex_id = %audit.reflex_id,
        audit_id = %audit.audit_id,
        ts_ns = audit.ts_ns
    )
)]
pub fn write_audit(db: &Db, audit: &StoredReflexAudit) -> StorageResult<()> {
    // Hot-path boundary (#1686 / #1802). This function performs a batched vault
    // put *and* a native Calyx constellation measurement — real I/O plus lens
    // math. It is the single most expensive thing the reflex crate can do, and
    // it must never run on the tagged scheduler thread. Detection is always-on:
    // in release the guard counts the violation and emits
    // `SYNAPSE_CALYX_HOT_PATH_BOUNDARY_VIOLATION`, which `health` surfaces; in
    // debug it also trips the hard assertion.
    crate::hot_path::guard_cold("reflex_write_audit");
    if matches!(
        audit.status,
        synapse_core::ReflexState::Expired | synapse_core::ReflexState::ActionDenied
    ) {
        let prior_record =
            crate::durable_state::load_record(db, &audit.reflex_id).map_err(|error| {
                synapse_storage::StorageError::WriteFailed {
                    cf_name: synapse_storage::cf::CF_KV.to_owned(),
                    detail: error.to_string(),
                }
            })?;
        let mut status = prior_record.status.clone();
        status.state = audit.status;
        status.last_error_code.clone_from(&audit.error_code);
        let next_record = prior_record.with_status(status).map_err(|error| {
            synapse_storage::StorageError::WriteFailed {
                cf_name: synapse_storage::cf::CF_KV.to_owned(),
                detail: error.to_string(),
            }
        })?;
        return write_terminal_lifecycle_audit(db, audit, &prior_record, &next_record);
    }
    let key = audit_key(audit).into_bytes();
    let value = encode_json(audit)?;
    crate::audit_projection::write_projected_audit(db, audit, &key, &value)?;
    db.put_reflex_audit_constellation(&key, &value, audit)
        .inspect_err(|error| {
            tracing::error!(
                code = "CALYX_REFLEX_CONSTELLATION_MEASUREMENT_FAILED",
                reflex_id = %audit.reflex_id,
                audit_id = %audit.audit_id,
                ts_ns = audit.ts_ns,
                detail = %format!("{error:#}"),
                "reflex audit row was written but native Calyx constellation measurement failed"
            );
        })?;
    Ok(())
}

/// Atomically publishes a reflex registration and every durable derived row.
///
/// The runtime scheduler candidate is still start-gated while this executes.
/// Source, ordered projection, aggregate/registry state, constellation,
/// grounding anchor, provenance ledger row, and WAL/MVCC sequence share one
/// Calyx commit; a failure cannot expose a durable active registration.
pub(crate) fn write_registration_audit(
    db: &Db,
    audit: &StoredReflexAudit,
    durable_record: &crate::durable_state::DurableReflexRecord,
) -> StorageResult<()> {
    crate::hot_path::guard_cold("reflex_write_registration_audit");
    let key = audit_key(audit).into_bytes();
    let value = encode_json(audit)?;
    let desired_key = crate::durable_state::desired_state_key(&audit.reflex_id);
    let desired_value = crate::durable_state::encode_record(durable_record).map_err(|error| {
        synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: error.to_string(),
        }
    })?;
    crate::audit_projection::write_projected_grounded_lifecycle_audit(
        db,
        audit,
        &key,
        &value,
        Some((&desired_key, &desired_value, None)),
    )
}

/// Atomically publishes one terminal lifecycle audit, every projection and
/// grounding row, and the guarded desired-state revision.
pub(crate) fn write_terminal_lifecycle_audit(
    db: &Db,
    audit: &StoredReflexAudit,
    prior_record: &crate::durable_state::DurableReflexRecord,
    next_record: &crate::durable_state::DurableReflexRecord,
) -> StorageResult<()> {
    crate::hot_path::guard_cold("reflex_write_terminal_lifecycle_audit");
    let key = audit_key(audit).into_bytes();
    let value = encode_json(audit)?;
    let desired_key = crate::durable_state::desired_state_key(&audit.reflex_id);
    let prior_value = crate::durable_state::encode_record(prior_record).map_err(|error| {
        synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: error.to_string(),
        }
    })?;
    let next_value = crate::durable_state::encode_record(next_record).map_err(|error| {
        synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: error.to_string(),
        }
    })?;
    let revisioned = db
        .get_cf_revisioned(synapse_storage::cf::CF_KV, &desired_key)?
        .ok_or_else(|| synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: format!(
                "REFLEX_DURABLE_DEFINITION_MISSING: reflex_id={}; remediation=do not mutate runtime state; reconcile the orphaned active audit through the explicit legacy migration",
                audit.reflex_id
            ),
        })?;
    if revisioned.value.as_deref() != Some(prior_value.as_slice()) {
        return Err(synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: format!(
                "REFLEX_DURABLE_DEFINITION_READBACK_MISMATCH: reflex_id={} expected_sha256={} actual_sha256={}; remediation=reload the exact desired-state record and retry the lifecycle command",
                audit.reflex_id,
                synapse_storage::ordered_index::sha256_hex(&prior_value),
                revisioned.value.as_deref().map_or_else(
                    || "expired".to_owned(),
                    synapse_storage::ordered_index::sha256_hex,
                )
            ),
        });
    }
    crate::audit_projection::write_projected_grounded_lifecycle_audit(
        db,
        audit,
        &key,
        &value,
        Some((&desired_key, &next_value, Some(revisioned.revision_sha256))),
    )
}

/// Atomically records a prepared terminal intent while keeping the durable
/// executable status active. The scheduler remains non-dispatchable in memory;
/// only the writer acknowledgement allows the completion phase to be queued.
pub(crate) fn write_terminal_lifecycle_intent(
    db: &Db,
    audit: &StoredReflexAudit,
    prior_record: &crate::durable_state::DurableReflexRecord,
    prepared_record: &crate::durable_state::DurableReflexRecord,
) -> StorageResult<()> {
    crate::hot_path::guard_cold("reflex_write_terminal_lifecycle_intent");
    write_guarded_lifecycle_mutation(db, audit, prior_record, prepared_record)
}

fn write_guarded_lifecycle_mutation(
    db: &Db,
    audit: &StoredReflexAudit,
    prior_record: &crate::durable_state::DurableReflexRecord,
    next_record: &crate::durable_state::DurableReflexRecord,
) -> StorageResult<()> {
    let key = audit_key(audit).into_bytes();
    let value = encode_json(audit)?;
    let desired_key = crate::durable_state::desired_state_key(&audit.reflex_id);
    let prior_value = crate::durable_state::encode_record(prior_record).map_err(|error| {
        synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: error.to_string(),
        }
    })?;
    let next_value = crate::durable_state::encode_record(next_record).map_err(|error| {
        synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: error.to_string(),
        }
    })?;
    let revisioned = db
        .get_cf_revisioned(synapse_storage::cf::CF_KV, &desired_key)?
        .ok_or_else(|| synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: format!(
                "REFLEX_DURABLE_DEFINITION_MISSING: reflex_id={}; remediation=do not publish or execute the pending terminal transition; run explicit orphan reconciliation",
                audit.reflex_id
            ),
        })?;
    if revisioned.value.as_deref() != Some(prior_value.as_slice()) {
        return Err(synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: format!(
                "REFLEX_TERMINAL_INTENT_DESIRED_READBACK_MISMATCH: reflex_id={} expected_sha256={} actual_sha256={}; remediation=keep the reflex non-dispatchable, inspect the exact desired-state revision, and reconcile the competing lifecycle writer",
                audit.reflex_id,
                synapse_storage::ordered_index::sha256_hex(&prior_value),
                revisioned.value.as_deref().map_or_else(
                    || "expired".to_owned(),
                    synapse_storage::ordered_index::sha256_hex,
                )
            ),
        });
    }
    crate::audit_projection::write_projected_grounded_lifecycle_audit(
        db,
        audit,
        &key,
        &value,
        Some((&desired_key, &next_value, Some(revisioned.revision_sha256))),
    )
}

/// Independently reopens the desired-state row and source audit after a flush.
/// Equality of both exact byte strings is the terminal commit acknowledgement.
pub(crate) fn verify_terminal_lifecycle_readback(
    db: &Db,
    audit: &StoredReflexAudit,
    expected_record: &crate::durable_state::DurableReflexRecord,
    phase: &str,
) -> StorageResult<()> {
    let desired_key = crate::durable_state::desired_state_key(&audit.reflex_id);
    let expected_desired =
        crate::durable_state::encode_record(expected_record).map_err(|error| {
            synapse_storage::StorageError::WriteFailed {
                cf_name: synapse_storage::cf::CF_KV.to_owned(),
                detail: error.to_string(),
            }
        })?;
    let actual_desired = db.get_cf(synapse_storage::cf::CF_KV, &desired_key)?;
    if actual_desired.as_deref() != Some(expected_desired.as_slice()) {
        return Err(synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_KV.to_owned(),
            detail: format!(
                "REFLEX_TERMINAL_LIFECYCLE_READBACK_FAILED: phase={phase} reflex_id={} row=desired_state expected_sha256={} actual_sha256={}; remediation=keep the reflex non-dispatchable, preserve the vault, and repair the exact Calyx read/write disagreement",
                audit.reflex_id,
                synapse_storage::ordered_index::sha256_hex(&expected_desired),
                actual_desired.as_deref().map_or_else(
                    || "missing".to_owned(),
                    synapse_storage::ordered_index::sha256_hex,
                )
            ),
        });
    }
    let source_key = audit_key(audit).into_bytes();
    let expected_audit = encode_json(audit)?;
    let actual_audit = db.get_cf(synapse_storage::cf::CF_REFLEX_AUDIT, &source_key)?;
    if actual_audit.as_deref() != Some(expected_audit.as_slice()) {
        return Err(synapse_storage::StorageError::WriteFailed {
            cf_name: synapse_storage::cf::CF_REFLEX_AUDIT.to_owned(),
            detail: format!(
                "REFLEX_TERMINAL_LIFECYCLE_READBACK_FAILED: phase={phase} reflex_id={} row=audit audit_id={} expected_sha256={} actual_sha256={}; remediation=keep the reflex non-dispatchable, preserve the vault, and repair the exact Calyx source-row disagreement",
                audit.reflex_id,
                audit.audit_id,
                synapse_storage::ordered_index::sha256_hex(&expected_audit),
                actual_audit.as_deref().map_or_else(
                    || "missing".to_owned(),
                    synapse_storage::ordered_index::sha256_hex,
                )
            ),
        });
    }
    Ok(())
}

pub(crate) struct TerminalLifecycleTransition {
    pub(crate) audit: StoredReflexAudit,
    pub(crate) prior_record: crate::durable_state::DurableReflexRecord,
    pub(crate) next_record: crate::durable_state::DurableReflexRecord,
}

pub(crate) fn write_terminal_lifecycle_batch(
    db: &Db,
    transitions: Vec<TerminalLifecycleTransition>,
) -> StorageResult<()> {
    crate::hot_path::guard_cold("reflex_write_terminal_lifecycle_batch");
    let mut entries = Vec::with_capacity(transitions.len());
    for transition in transitions {
        let desired_key = crate::durable_state::desired_state_key(&transition.audit.reflex_id);
        let prior_value =
            crate::durable_state::encode_record(&transition.prior_record).map_err(|error| {
                synapse_storage::StorageError::WriteFailed {
                    cf_name: synapse_storage::cf::CF_KV.to_owned(),
                    detail: error.to_string(),
                }
            })?;
        let next_value =
            crate::durable_state::encode_record(&transition.next_record).map_err(|error| {
                synapse_storage::StorageError::WriteFailed {
                    cf_name: synapse_storage::cf::CF_KV.to_owned(),
                    detail: error.to_string(),
                }
            })?;
        let revisioned = db
            .get_cf_revisioned(synapse_storage::cf::CF_KV, &desired_key)?
            .ok_or_else(|| synapse_storage::StorageError::WriteFailed {
                cf_name: synapse_storage::cf::CF_KV.to_owned(),
                detail: format!(
                    "REFLEX_DURABLE_DEFINITION_MISSING: reflex_id={}; remediation=abort the whole lifecycle batch and run explicit orphan reconciliation",
                    transition.audit.reflex_id
                ),
            })?;
        if revisioned.value.as_deref() != Some(prior_value.as_slice()) {
            return Err(synapse_storage::StorageError::WriteFailed {
                cf_name: synapse_storage::cf::CF_KV.to_owned(),
                detail: format!(
                    "REFLEX_LIFECYCLE_BATCH_DESIRED_READBACK_MISMATCH: reflex_id={} expected_sha256={} actual_sha256={}; remediation=reload every exact desired-state member and retry the whole command",
                    transition.audit.reflex_id,
                    synapse_storage::ordered_index::sha256_hex(&prior_value),
                    revisioned.value.as_deref().map_or_else(
                        || "expired".to_owned(),
                        synapse_storage::ordered_index::sha256_hex,
                    )
                ),
            });
        }
        let source_key = audit_key(&transition.audit).into_bytes();
        let source_value = encode_json(&transition.audit)?;
        entries.push(crate::audit_projection::GroundedLifecycleProjectionEntry {
            audit: transition.audit,
            source_key,
            source_value,
            desired: Some(crate::audit_projection::GroundedLifecycleDesiredMutation {
                key: desired_key,
                value: next_value,
                expected_revision: revisioned.revision_sha256,
            }),
        });
    }
    crate::audit_projection::write_projected_grounded_lifecycle_batch(db, entries)
}

pub(crate) fn audit_key(audit: &StoredReflexAudit) -> String {
    format!("{}:{:020}:{}", audit.reflex_id, audit.ts_ns, audit.audit_id)
}
