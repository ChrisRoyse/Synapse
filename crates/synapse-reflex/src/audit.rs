use synapse_core::StoredReflexAudit;
use synapse_storage::{Db, StorageResult, cf, encode_json};

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
    let key = audit_key(audit);
    let value = encode_json(audit)?;
    db.put_batch(cf::CF_REFLEX_AUDIT, [(key.into_bytes(), value)])
}

fn audit_key(audit: &StoredReflexAudit) -> String {
    format!("{}:{:020}:{}", audit.reflex_id, audit.ts_ns, audit.audit_id)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use serde_json::json;
    use synapse_core::{ReflexState, StoredReflexAudit, new_reflex_id};
    use synapse_storage::{Db, cf, decode_json};
    use tempfile::tempdir;

    use super::write_audit;

    const TEST_SCHEMA_VERSION: u32 = 7;

    #[test]
    fn write_audit_persists_duplicate_ts_rows_and_restart() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let db_path = temp.path().join("db");
        let db = Db::open(&db_path, TEST_SCHEMA_VERSION)?;
        let reflex_id = new_reflex_id();
        let before = db.scan_cf(cf::CF_REFLEX_AUDIT)?;
        assert!(before.is_empty());

        let first = audit("audit-a", &reflex_id, 42, ReflexState::Active);
        let second = audit("audit-b", &reflex_id, 42, ReflexState::Starved);
        write_audit(&db, &first)?;
        write_audit(&db, &second)?;
        db.flush()?;

        let after = db.scan_cf(cf::CF_REFLEX_AUDIT)?;
        let decoded = after
            .iter()
            .map(|(_key, value)| decode_json::<StoredReflexAudit>(value))
            .collect::<Result<Vec<_>, _>>()?;
        let statuses = decoded.iter().map(|audit| audit.status).collect::<Vec<_>>();

        assert_eq!(after.len(), 2);
        assert!(statuses.contains(&ReflexState::Active));
        assert!(statuses.contains(&ReflexState::Starved));
        drop(db);

        let reopened = Db::open(&db_path, TEST_SCHEMA_VERSION)?;
        let reopened_rows = reopened.scan_cf(cf::CF_REFLEX_AUDIT)?;
        assert_eq!(reopened_rows.len(), 2);
        Ok(())
    }

    fn audit(
        audit_id: &str,
        reflex_id: &str,
        ts_ns: u64,
        status: ReflexState,
    ) -> StoredReflexAudit {
        StoredReflexAudit {
            schema_version: TEST_SCHEMA_VERSION,
            audit_id: audit_id.to_owned(),
            reflex_id: reflex_id.to_owned(),
            ts_ns,
            status,
            event_id: None,
            audit_context: None,
            steps: Vec::new(),
            error_code: None,
            details: json!({ "case": "duplicate_ts" }),
            redacted: false,
            redactions: Vec::new(),
        }
    }
}
