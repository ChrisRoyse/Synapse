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

fn audit_key(audit: &StoredReflexAudit) -> String {
    format!("{}:{:020}:{}", audit.reflex_id, audit.ts_ns, audit.audit_id)
}
