//! Exact retained timestamp-order projection for transcript health (#2189).

use std::{
    path::PathBuf,
    sync::{LazyLock, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synapse_core::AgentTranscriptRecord;
use synapse_storage::{
    CfRevisionGuard, Db, RawRow, RawRowWithExpiry, cf, decode_json, encode_json,
    ordered_index::{
        OrderedSourcePointer, decode_pointer, encode_pointer, hex_decode, hex_encode,
        transcript_order_key,
    },
};

const META_KEY: &[u8] = b"projection/agent-transcript-order/v1/meta";
const PROGRESS_KEY: &[u8] = b"projection/agent-transcript-order/v1/progress";
const SCHEMA_VERSION: u32 = 1;
const BACKFILL_ROWS: usize = 512;
pub(super) const MAX_SNAPSHOT_ROWS: usize = 50;

static PROJECTION_LOCK: LazyLock<Mutex<ProjectionLockState>> =
    LazyLock::new(|| Mutex::new(ProjectionLockState::default()));

#[derive(Clone, Debug, Eq, PartialEq)]
struct VaultIdentity {
    vault_dir: PathBuf,
    vault_id: String,
}

#[derive(Debug, Default)]
pub(super) struct ProjectionLockState {
    reconciled_vault: Option<VaultIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectionMeta {
    schema_version: u32,
    readable: bool,
    built_at_unix_ms: u64,
    source_rows_at_build: u64,
    source_digest_at_build: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectionProgress {
    schema_version: u32,
    complete: bool,
    resume_after_source_key_hex: Option<String>,
    rows_indexed: u64,
}

pub(super) fn lock_projection() -> Result<MutexGuard<'static, ProjectionLockState>, String> {
    PROJECTION_LOCK.lock().map_err(|_error| {
        "AGENT_TRANSCRIPT_ORDER_LOCK_POISONED: the projection serialization lock is poisoned; remediation=restart the daemon and inspect the prior panic before retrying".to_owned()
    })
}

pub(super) fn ensure_projection_locked(
    db: &Db,
    lock_state: &mut ProjectionLockState,
) -> Result<(), String> {
    let vault_identity = read_vault_identity(db)?;
    if lock_state.reconciled_vault.as_ref() == Some(&vault_identity) {
        return Ok(());
    }
    match read_meta(db)? {
        Some(meta) => {
            validate_meta(&meta)?;
            reconcile_complete_sets(db)?;
        }
        None => build_projection(db)?,
    }
    lock_state.reconciled_vault = Some(vault_identity);
    Ok(())
}

fn read_vault_identity(db: &Db) -> Result<VaultIdentity, String> {
    let status = db.calyx_vault_status().map_err(|error| {
        format!(
            "AGENT_TRANSCRIPT_ORDER_VAULT_IDENTITY_READ_FAILED: {error}; remediation=repair the Calyx vault status path before reading or writing the ordered projection"
        )
    })?;
    if !status.enabled || !status.open {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_VAULT_IDENTITY_UNAVAILABLE: enabled={} open={} phase={}; remediation=open the configured Calyx vault before reading or writing the ordered projection",
            status.enabled, status.open, status.phase
        ));
    }
    let vault_dir = status.vault_dir.ok_or_else(|| {
        "AGENT_TRANSCRIPT_ORDER_VAULT_IDENTITY_UNAVAILABLE: open vault status has no vault_dir; remediation=repair the Calyx status contract before retrying".to_owned()
    })?;
    let vault_id = status.vault_id.ok_or_else(|| {
        "AGENT_TRANSCRIPT_ORDER_VAULT_IDENTITY_UNAVAILABLE: open vault status has no vault_id; remediation=repair the Calyx identity read before retrying".to_owned()
    })?;
    Ok(VaultIdentity {
        vault_dir,
        vault_id,
    })
}

pub(super) fn order_row_for_record(
    source_key: &[u8],
    source_value: &[u8],
    record: &AgentTranscriptRecord,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let key = transcript_order_key(record.ts_ns, &record.spawn_id, record.line_no);
    let value = encode_pointer(&OrderedSourcePointer::new(source_key, source_value))
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_POINTER_ENCODE_FAILED: {error}"))?;
    Ok((key, value))
}

pub(super) fn newest_rows(
    db: &Db,
    limit: usize,
) -> Result<Vec<(Vec<u8>, AgentTranscriptRecord)>, String> {
    if limit > MAX_SNAPSHOT_ROWS {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_LIMIT_EXCEEDED: requested={limit} maximum={MAX_SNAPSHOT_ROWS}; remediation=use the bounded health/dashboard contract or add a separately designed paged API"
        ));
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut guard = lock_projection()?;
    ensure_projection_locked(db, &mut guard)?;
    let candidate_limit = limit.saturating_mul(4);
    let (mut rows, more) = db
        .scan_cf_from(cf::CF_AGENT_TRANSCRIPT_ORDER, &[], candidate_limit)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_SCAN_FAILED: {error}"))?;
    if rows.len() < limit && more {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_FRAGMENTED_PAGE: requested={limit} live_rows={} candidate_limit={candidate_limit} more=true; remediation=run retention GC and inspect the ordered index expiry histogram before retrying",
            rows.len()
        ));
    }
    rows.truncate(limit);
    let mut output = Vec::with_capacity(rows.len());
    for (order_key, pointer_bytes) in rows {
        let pointer = decode_pointer(cf::CF_AGENT_TRANSCRIPT_ORDER, &pointer_bytes)
            .map_err(|error| error.to_string())?;
        let source_key = pointer
            .source_key(cf::CF_AGENT_TRANSCRIPT_ORDER)
            .map_err(|error| error.to_string())?;
        let source_value = db
            .get_cf(cf::CF_AGENT_TRANSCRIPTS, &source_key)
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_SOURCE_READ_FAILED: key_hex={}: {error}", hex_encode(&source_key)))?
            .ok_or_else(|| format!(
                "AGENT_TRANSCRIPT_ORDER_DANGLING_POINTER: order_key_hex={} source_key_hex={}; remediation=preserve the vault, inspect the atomic source/index commit and retention envelopes, then rebuild only after the cause is known",
                hex_encode(&order_key), hex_encode(&source_key)
            ))?;
        pointer
            .verify_source_value(cf::CF_AGENT_TRANSCRIPT_ORDER, &source_value)
            .map_err(|error| error.to_string())?;
        let record = decode_json::<AgentTranscriptRecord>(&source_value).map_err(|error| {
            format!(
                "AGENT_TRANSCRIPT_ORDER_SOURCE_DECODE_FAILED: source_key_hex={}: {error}",
                hex_encode(&source_key)
            )
        })?;
        let expected_order = transcript_order_key(record.ts_ns, &record.spawn_id, record.line_no);
        if order_key != expected_order {
            return Err(format!(
                "AGENT_TRANSCRIPT_ORDER_KEY_MISMATCH: actual={} expected={} source_key_hex={}; remediation=preserve both rows and repair the projection writer before rebuilding",
                hex_encode(&order_key),
                hex_encode(&expected_order),
                hex_encode(&source_key)
            ));
        }
        output.push((source_key, record));
    }
    Ok(output)
}

fn build_projection(db: &Db) -> Result<(), String> {
    let mut progress = match read_progress(db)? {
        Some(progress) => {
            validate_progress(&progress)?;
            progress
        }
        None => {
            let (existing, _more) = db
                .scan_cf_from(cf::CF_AGENT_TRANSCRIPT_ORDER, &[], 1)
                .map_err(|error| {
                    format!("AGENT_TRANSCRIPT_ORDER_INITIAL_INDEX_PROBE_FAILED: {error}")
                })?;
            if !existing.is_empty() {
                return Err(
                    "AGENT_TRANSCRIPT_ORDER_UNTRACKED_PARTIAL_BUILD: order rows exist without meta or progress; remediation=preserve the rows and inspect the interrupted/non-authoritative writer before exact repair"
                        .to_owned(),
                );
            }
            let initial = ProjectionProgress {
                schema_version: SCHEMA_VERSION,
                complete: false,
                resume_after_source_key_hex: None,
                rows_indexed: 0,
            };
            write_initial_progress(db, &initial)?;
            initial
        }
    };
    if progress.complete {
        return Err(
            "AGENT_TRANSCRIPT_ORDER_META_MISSING_AFTER_COMPLETE_PROGRESS: remediation=preserve the projection rows and progress marker and restore the missing readable meta atomically after exact reconciliation"
                .to_owned(),
        );
    }

    loop {
        let start = match progress.resume_after_source_key_hex.as_deref() {
            Some(encoded) => key_after(hex_decode(encoded).ok_or_else(|| {
                "AGENT_TRANSCRIPT_ORDER_PROGRESS_CURSOR_INVALID: remediation=preserve the progress row and inspect its source before repair".to_owned()
            })?),
            None => Vec::new(),
        };
        let (source_rows, more) = db
            .scan_cf_from(cf::CF_AGENT_TRANSCRIPTS, &start, BACKFILL_ROWS)
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_BACKFILL_SCAN_FAILED: {error}"))?;
        if source_rows.is_empty() {
            if more {
                return Err(
                    "AGENT_TRANSCRIPT_ORDER_SCAN_STALLED: storage reported more rows without a resumable live row; remediation=inspect expired physical candidates and the bounded scan contract"
                        .to_owned(),
                );
            }
            break;
        }

        let prior_progress = db
            .get_cf_revisioned(cf::CF_KV, PROGRESS_KEY)
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_REVISION_READ_FAILED: {error}"))?
            .ok_or_else(|| "AGENT_TRANSCRIPT_ORDER_PROGRESS_DISAPPEARED: remediation=inspect CF_KV protection and restore the exact progress row".to_owned())?;
        let mut guards = vec![CfRevisionGuard::new(
            cf::CF_KV,
            PROGRESS_KEY,
            Some(prior_progress.revision_sha256),
        )];
        let mut index_rows = Vec::with_capacity(source_rows.len());
        for (source_key, source_value) in &source_rows {
            let revisioned = db
                .get_cf_revisioned(cf::CF_AGENT_TRANSCRIPTS, source_key)
                .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_SOURCE_REVISION_READ_FAILED: key_hex={}: {error}", hex_encode(source_key)))?
                .ok_or_else(|| format!("AGENT_TRANSCRIPT_ORDER_SOURCE_DISAPPEARED: key_hex={}; remediation=retry only after retention/GC completes", hex_encode(source_key)))?;
            if revisioned.value.as_deref() != Some(source_value.as_slice()) {
                return Err(format!(
                    "AGENT_TRANSCRIPT_ORDER_SOURCE_CHANGED_DURING_BUILD: key_hex={}; remediation=retry the unpublished build after the source writer/retention transition completes",
                    hex_encode(source_key)
                ));
            }
            let record = decode_json::<AgentTranscriptRecord>(source_value).map_err(|error| {
                format!(
                    "AGENT_TRANSCRIPT_ORDER_SOURCE_DECODE_FAILED: key_hex={}: {error}",
                    hex_encode(source_key)
                )
            })?;
            let (order_key, order_value) = order_row_for_record(source_key, source_value, &record)?;
            guards.push(CfRevisionGuard::new(
                cf::CF_AGENT_TRANSCRIPTS,
                source_key.clone(),
                Some(revisioned.revision_sha256),
            ));
            guards.push(CfRevisionGuard::new(
                cf::CF_AGENT_TRANSCRIPT_ORDER,
                order_key.clone(),
                None,
            ));
            index_rows.push(RawRowWithExpiry::preserving_expiry(
                order_key,
                order_value,
                revisioned.expires_at_ms,
            ));
        }
        let last_key = source_rows
            .last()
            .map(|row| row.0.clone())
            .ok_or_else(|| "AGENT_TRANSCRIPT_ORDER_BACKFILL_LAST_KEY_MISSING".to_owned())?;
        progress.resume_after_source_key_hex = Some(hex_encode(&last_key));
        progress.rows_indexed = progress
            .rows_indexed
            .saturating_add(source_rows.len() as u64);
        let progress_bytes = encode_json(&progress)
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_ENCODE_FAILED: {error}"))?;
        let outcome = db
            .put_cf_batches_with_expiry_if_revisions_pressure_bypass(
                guards,
                vec![
                    (cf::CF_AGENT_TRANSCRIPT_ORDER, index_rows),
                    (
                        cf::CF_KV,
                        vec![RawRowWithExpiry::retained(PROGRESS_KEY, progress_bytes)],
                    ),
                ],
            )
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_BACKFILL_COMMIT_FAILED: {error}"))?;
        if !outcome.applied {
            return Err(format!(
                "AGENT_TRANSCRIPT_ORDER_BACKFILL_REVISION_CONFLICT: conflict={:?}; remediation=retry the unpublished build after the exact competing source/progress mutation is understood",
                outcome.conflict
            ));
        }
        if !more {
            break;
        }
    }

    let (source_rows, digest) = reconcile_complete_sets(db)?;
    progress.complete = true;
    let meta = ProjectionMeta {
        schema_version: SCHEMA_VERSION,
        readable: true,
        built_at_unix_ms: now_ms()?,
        source_rows_at_build: source_rows,
        source_digest_at_build: digest,
    };
    publish_meta(db, &meta, &progress)?;
    tracing::info!(
        code = "AGENT_TRANSCRIPT_ORDER_PROJECTION_BUILT",
        source_rows,
        source_of_truth = "CF_AGENT_TRANSCRIPTS exact rows + CF_AGENT_TRANSCRIPT_ORDER pointers + CF_KV readable meta",
        "published exact transcript timestamp-order projection after full reconciliation"
    );
    Ok(())
}

fn reconcile_complete_sets(db: &Db) -> Result<(u64, String), String> {
    let source_rows = scan_all(db, cf::CF_AGENT_TRANSCRIPTS)?;
    let index_rows = scan_all(db, cf::CF_AGENT_TRANSCRIPT_ORDER)?;
    let mut expected = Vec::with_capacity(source_rows.len());
    for (source_key, source_value) in &source_rows {
        let record = decode_json::<AgentTranscriptRecord>(source_value).map_err(|error| {
            format!(
                "AGENT_TRANSCRIPT_ORDER_RECONCILE_SOURCE_DECODE_FAILED: key_hex={}: {error}",
                hex_encode(source_key)
            )
        })?;
        expected.push(order_row_for_record(source_key, source_value, &record)?);
    }
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    if expected != index_rows {
        let mismatch = first_mismatch(&expected, &index_rows);
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_RECONCILIATION_FAILED: source_rows={} index_rows={} mismatch={mismatch}; remediation=preserve the vault, identify the missing/extra/corrupt atomic projection row, and rebuild only after fixing its cause",
            expected.len(),
            index_rows.len()
        ));
    }
    Ok((expected.len() as u64, rows_digest(&expected)))
}

fn scan_all(db: &Db, cf_name: &str) -> Result<Vec<RawRow>, String> {
    let mut output = Vec::new();
    let mut start = Vec::new();
    loop {
        let (rows, more) = db
            .scan_cf_from(cf_name, &start, BACKFILL_ROWS)
            .map_err(|error| {
                format!("AGENT_TRANSCRIPT_ORDER_RECONCILE_SCAN_FAILED: cf={cf_name}: {error}")
            })?;
        if rows.is_empty() {
            if more {
                return Err(format!(
                    "AGENT_TRANSCRIPT_ORDER_RECONCILE_SCAN_STALLED: cf={cf_name}"
                ));
            }
            break;
        }
        start = key_after(
            rows.last()
                .map(|row| row.0.clone())
                .ok_or_else(|| "AGENT_TRANSCRIPT_ORDER_SCAN_LAST_KEY_MISSING".to_owned())?,
        );
        output.extend(rows);
        if !more {
            break;
        }
    }
    Ok(output)
}

fn read_meta(db: &Db) -> Result<Option<ProjectionMeta>, String> {
    db.get_cf(cf::CF_KV, META_KEY)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_META_READ_FAILED: {error}"))?
        .map(|bytes| {
            decode_json::<ProjectionMeta>(&bytes)
                .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_META_CORRUPT: {error}; remediation=preserve the exact CF_KV row and inspect the publisher"))
        })
        .transpose()
}

fn read_progress(db: &Db) -> Result<Option<ProjectionProgress>, String> {
    db.get_cf(cf::CF_KV, PROGRESS_KEY)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_READ_FAILED: {error}"))?
        .map(|bytes| {
            decode_json::<ProjectionProgress>(&bytes)
                .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_CORRUPT: {error}; remediation=preserve the exact CF_KV row and inspect the interrupted publisher"))
        })
        .transpose()
}

fn validate_meta(meta: &ProjectionMeta) -> Result<(), String> {
    if meta.schema_version != SCHEMA_VERSION || !meta.readable {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_META_UNREADABLE: schema_version={} expected={SCHEMA_VERSION} readable={}; remediation=complete the persisted backfill before enabling reads",
            meta.schema_version, meta.readable
        ));
    }
    Ok(())
}

fn validate_progress(progress: &ProjectionProgress) -> Result<(), String> {
    if progress.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_PROGRESS_SCHEMA_UNSUPPORTED: actual={} expected={SCHEMA_VERSION}; remediation=preserve the row and migrate it explicitly",
            progress.schema_version
        ));
    }
    Ok(())
}

fn write_initial_progress(db: &Db, progress: &ProjectionProgress) -> Result<(), String> {
    let bytes = encode_json(progress)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_ENCODE_FAILED: {error}"))?;
    let outcome = db
        .put_cf_batches_if_revisions_pressure_bypass(
            vec![CfRevisionGuard::new(cf::CF_KV, PROGRESS_KEY, None)],
            vec![(cf::CF_KV, vec![(PROGRESS_KEY.to_vec(), bytes.clone())])],
        )
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_WRITE_FAILED: {error}"))?;
    if !outcome.applied {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_PROGRESS_INIT_CONFLICT: {:?}",
            outcome.conflict
        ));
    }
    let readback = db
        .get_cf(cf::CF_KV, PROGRESS_KEY)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_READBACK_FAILED: {error}"))?;
    if readback.as_deref() != Some(bytes.as_slice()) {
        return Err("AGENT_TRANSCRIPT_ORDER_PROGRESS_READBACK_MISMATCH: remediation=inspect the committed CF_KV row and Calyx read path".to_owned());
    }
    Ok(())
}

fn publish_meta(
    db: &Db,
    meta: &ProjectionMeta,
    progress: &ProjectionProgress,
) -> Result<(), String> {
    let progress_revision = db
        .get_cf_revisioned(cf::CF_KV, PROGRESS_KEY)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_REVISION_READ_FAILED: {error}"))?
        .ok_or_else(|| "AGENT_TRANSCRIPT_ORDER_PROGRESS_MISSING_AT_PUBLISH".to_owned())?;
    let meta_bytes = encode_json(meta)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_META_ENCODE_FAILED: {error}"))?;
    let progress_bytes = encode_json(progress)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_ENCODE_FAILED: {error}"))?;
    let outcome = db
        .put_cf_batches_if_revisions_pressure_bypass(
            vec![
                CfRevisionGuard::new(cf::CF_KV, META_KEY, None),
                CfRevisionGuard::new(
                    cf::CF_KV,
                    PROGRESS_KEY,
                    Some(progress_revision.revision_sha256),
                ),
            ],
            vec![(
                cf::CF_KV,
                vec![
                    (META_KEY.to_vec(), meta_bytes.clone()),
                    (PROGRESS_KEY.to_vec(), progress_bytes.clone()),
                ],
            )],
        )
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_META_PUBLISH_FAILED: {error}"))?;
    if !outcome.applied {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_META_PUBLISH_CONFLICT: {:?}",
            outcome.conflict
        ));
    }
    if db
        .get_cf(cf::CF_KV, META_KEY)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_META_READBACK_FAILED: {error}"))?
        .as_deref()
        != Some(meta_bytes.as_slice())
        || db
            .get_cf(cf::CF_KV, PROGRESS_KEY)
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_READBACK_FAILED: {error}"))?
            .as_deref()
            != Some(progress_bytes.as_slice())
    {
        return Err("AGENT_TRANSCRIPT_ORDER_PUBLICATION_READBACK_MISMATCH: remediation=inspect both exact CF_KV rows and the atomic commit sequence".to_owned());
    }
    Ok(())
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

fn rows_digest(rows: &[(Vec<u8>, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    for (key, value) in rows {
        hasher.update((key.len() as u64).to_be_bytes());
        hasher.update(key);
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    hex_encode(&hasher.finalize())
}

fn key_after(mut key: Vec<u8>) -> Vec<u8> {
    key.push(0);
    key
}

fn now_ms() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_CLOCK_BEFORE_EPOCH: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_error| {
        "AGENT_TRANSCRIPT_ORDER_CLOCK_OVERFLOW: unix milliseconds exceed u64".to_owned()
    })
}
