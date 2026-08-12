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
const REPAIR_KEY: &[u8] = b"projection/agent-transcript-order/v1/repair";
const SCHEMA_VERSION: u32 = 2;
const BACKFILL_ROWS: usize = 512;
pub(super) const MAX_SNAPSHOT_ROWS: usize = 50;
const PROJECTION_SOURCE_OF_TRUTH: &str =
    "CF_AGENT_TRANSCRIPTS + CF_AGENT_TRANSCRIPT_ORDER + CF_KV projection meta/progress/repair rows";

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
    resume_after_physical_hex: Option<String>,
    rows_indexed: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectionRepairMarker {
    schema_version: u32,
    vault_id: String,
    authorized_repair_token_sha256: String,
    started_at_unix_ms: u64,
}

pub(super) type TranscriptOrderStatus = crate::m3::storage::StorageTranscriptOrderStatusResponse;
pub(super) type TranscriptOrderRebuild = crate::m3::storage::StorageTranscriptOrderRebuildResponse;

pub(super) fn projection_status(db: &Db) -> Result<TranscriptOrderStatus, String> {
    let _guard = lock_projection()?;
    projection_status_locked(db)
}

pub(super) fn rebuild_projection(
    db: &Db,
    expected_repair_token: &str,
) -> Result<TranscriptOrderRebuild, String> {
    if !repair_token_is_valid(expected_repair_token) {
        return Err(
            "AGENT_TRANSCRIPT_ORDER_REPAIR_TOKEN_INVALID: expected_repair_token must be exactly 64 lowercase hexadecimal characters copied from a prior status read"
                .to_owned(),
        );
    }

    let mut guard = lock_projection()?;
    let existing_marker = read_repair_marker(db)?;
    let before = projection_status_locked(db)?;
    if let Some(marker) = existing_marker.as_ref()
        && marker.vault_id != before.vault_id
    {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_VAULT_MISMATCH: marker_vault_id={} active_vault_id={}; remediation=preserve the marker and vault, then identify how a repair authorization crossed vault identities",
            marker.vault_id, before.vault_id
        ));
    }
    let authorization_token = existing_marker
        .as_ref()
        .map_or(before.repair_token_sha256.as_str(), |marker| {
            marker.authorized_repair_token_sha256.as_str()
        });
    if expected_repair_token != authorization_token {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_REPAIR_TOKEN_CONFLICT: expected={expected_repair_token} actual={authorization_token}; remediation=re-read storage operation=transcript_order_status and retry with its repair_token_sha256"
        ));
    }
    if before.ready && existing_marker.is_none() {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_REPAIR_NOT_NEEDED: projection is already exact and readable at state_token_sha256={}",
            before.state_token_sha256
        ));
    }

    let resumed_repair = existing_marker.is_some();
    let marker = match existing_marker {
        Some(marker) => marker,
        None => {
            let marker = ProjectionRepairMarker {
                schema_version: SCHEMA_VERSION,
                vault_id: before.vault_id.clone(),
                authorized_repair_token_sha256: expected_repair_token.to_owned(),
                started_at_unix_ms: now_ms()?,
            };
            write_repair_marker(db, &marker)?;
            marker
        }
    };
    let mut deleted_index_rows = 0u64;
    let mut rebuilt_index_rows = 0u64;

    if before.exact_match {
        finalize_repaired_projection(db, &before)?;
    } else {
        deleted_index_rows = clear_projection_rows(db)?;
        clear_publication_rows(db)?;
        build_projection(db)?;
        let repaired = projection_status_locked(db)?;
        if !projection_publication_complete(db, &repaired)? {
            return Err(format!(
                "AGENT_TRANSCRIPT_ORDER_REPAIR_READBACK_MISMATCH: source_rows={} index_rows={} exact_match={} ready={} state_token_sha256={}; remediation=preserve the repair marker and resume only after inspecting the physical projection state",
                repaired.source_rows,
                repaired.index_rows,
                repaired.exact_match,
                repaired.ready,
                repaired.state_token_sha256
            ));
        }
        rebuilt_index_rows = repaired.index_rows;
    }

    delete_exact_row(db, cf::CF_KV, REPAIR_KEY, "REPAIR_MARKER")?;
    guard.reconciled_vault = Some(read_vault_identity(db)?);
    let after = projection_status_locked(db)?;
    if !after.ready || after.repair_in_progress {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_REPAIR_FINAL_READBACK_MISMATCH: exact_match={} ready={} repair_in_progress={} state_token_sha256={}; remediation=preserve the vault and inspect the exact projection rows",
            after.exact_match, after.ready, after.repair_in_progress, after.state_token_sha256
        ));
    }
    tracing::info!(
        code = "AGENT_TRANSCRIPT_ORDER_REPAIRED",
        vault_id = marker.vault_id,
        deleted_index_rows,
        rebuilt_index_rows,
        resumed_repair,
        source_rows = after.source_rows,
        index_rows = after.index_rows,
        state_token_sha256 = after.state_token_sha256,
        source_of_truth = PROJECTION_SOURCE_OF_TRUTH,
        "rebuilt and independently reconciled the transcript timestamp-order projection"
    );
    Ok(TranscriptOrderRebuild {
        finalized_only: rebuilt_index_rows == 0,
        before,
        after,
        deleted_index_rows,
        rebuilt_index_rows,
        resumed_repair,
        authorization_token_sha256: expected_repair_token.to_owned(),
        source_of_truth: PROJECTION_SOURCE_OF_TRUTH,
    })
}

fn projection_publication_complete(
    db: &Db,
    status: &TranscriptOrderStatus,
) -> Result<bool, String> {
    let Some(meta) = read_meta(db)? else {
        return Ok(false);
    };
    let Some(progress) = read_progress(db)? else {
        return Ok(false);
    };
    validate_published_rows(&meta, &progress)?;
    Ok(status.exact_match)
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
    if let Some(marker) = read_repair_marker(db)? {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_REPAIR_IN_PROGRESS: vault_id={} authorization_token_sha256={} started_at_unix_ms={}; remediation=use storage operation=transcript_order_status, then resume storage operation=transcript_order_rebuild with the reported repair_token_sha256",
            marker.vault_id, marker.authorized_repair_token_sha256, marker.started_at_unix_ms
        ));
    }
    let vault_identity = read_vault_identity(db)?;
    if lock_state.reconciled_vault.as_ref() == Some(&vault_identity) {
        return Ok(());
    }
    match read_meta(db)? {
        Some(meta) => {
            let progress = read_progress(db)?.ok_or_else(|| {
                "AGENT_TRANSCRIPT_ORDER_PROGRESS_MISSING: readable projection metadata exists without its publication progress row; remediation=preserve the metadata and run the revision-guarded projection repair"
                    .to_owned()
            })?;
            validate_published_rows(&meta, &progress)?;
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
            let existing = db
                .scan_cf_physical_page(cf::CF_AGENT_TRANSCRIPT_ORDER, None, 1)
                .map_err(|error| {
                    format!("AGENT_TRANSCRIPT_ORDER_INITIAL_INDEX_PROBE_FAILED: {error}")
                })?;
            if !existing.rows.is_empty() || existing.expired_rows_skipped > 0 {
                return Err(
                    "AGENT_TRANSCRIPT_ORDER_UNTRACKED_PARTIAL_BUILD: order rows exist without meta or progress; remediation=preserve the rows and inspect the interrupted/non-authoritative writer before exact repair"
                        .to_owned(),
                );
            }
            let initial = ProjectionProgress {
                schema_version: SCHEMA_VERSION,
                complete: false,
                resume_after_physical_hex: None,
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
        let after_physical = progress
            .resume_after_physical_hex
            .as_deref()
            .map(|encoded| {
                hex_decode(encoded).ok_or_else(|| {
                    "AGENT_TRANSCRIPT_ORDER_PROGRESS_CURSOR_INVALID: remediation=preserve the progress row and inspect its opaque physical cursor before repair".to_owned()
                })
            })
            .transpose()?;
        let page = db
            .scan_cf_physical_page(
                cf::CF_AGENT_TRANSCRIPTS,
                after_physical.as_deref(),
                BACKFILL_ROWS,
            )
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_BACKFILL_SCAN_FAILED: {error}"))?;
        if page.rows.is_empty() && !page.more {
            break;
        }
        let next_cursor = if page.more {
            Some(page.resume_after_physical.clone().ok_or_else(|| {
                "AGENT_TRANSCRIPT_ORDER_PHYSICAL_CURSOR_ABSENT: scan reported more=true without an opaque resume cursor".to_owned()
            })?)
        } else {
            page.resume_after_physical.clone()
        };
        let source_rows = page.rows;

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
        progress.resume_after_physical_hex = next_cursor.as_deref().map(hex_encode);
        progress.rows_indexed = progress
            .rows_indexed
            .saturating_add(source_rows.len() as u64);
        let progress_bytes = encode_json(&progress)
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_ENCODE_FAILED: {error}"))?;
        let mut batches = Vec::with_capacity(2);
        if !index_rows.is_empty() {
            batches.push((cf::CF_AGENT_TRANSCRIPT_ORDER, index_rows));
        }
        batches.push((
            cf::CF_KV,
            vec![RawRowWithExpiry::retained(PROGRESS_KEY, progress_bytes)],
        ));
        let outcome = db
            .put_cf_batches_with_expiry_if_revisions_pressure_bypass(guards, batches)
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_BACKFILL_COMMIT_FAILED: {error}"))?;
        if !outcome.applied {
            return Err(format!(
                "AGENT_TRANSCRIPT_ORDER_BACKFILL_REVISION_CONFLICT: conflict={:?}; remediation=retry the unpublished build after the exact competing source/progress mutation is understood",
                outcome.conflict
            ));
        }
        if !page.more {
            break;
        }
    }

    let (source_rows, digest) = reconcile_complete_sets(db)?;
    progress.complete = true;
    progress.resume_after_physical_hex = None;
    progress.rows_indexed = source_rows;
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
    let mut index_rows = scan_all(db, cf::CF_AGENT_TRANSCRIPT_ORDER)?;
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
    index_rows.sort_by(|left, right| left.0.cmp(&right.0));
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
    let mut after_physical = None;
    loop {
        let page = db
            .scan_cf_physical_page(cf_name, after_physical.as_deref(), BACKFILL_ROWS)
            .map_err(|error| {
                format!("AGENT_TRANSCRIPT_ORDER_RECONCILE_SCAN_FAILED: cf={cf_name}: {error}")
            })?;
        if page.more && page.resume_after_physical.is_none() {
            return Err(format!(
                "AGENT_TRANSCRIPT_ORDER_RECONCILE_CURSOR_ABSENT: cf={cf_name}"
            ));
        }
        output.extend(page.rows);
        if !page.more {
            break;
        }
        after_physical = page.resume_after_physical;
    }
    Ok(output)
}

fn projection_status_locked(db: &Db) -> Result<TranscriptOrderStatus, String> {
    let vault_identity = read_vault_identity(db)?;
    let source_rows = scan_all(db, cf::CF_AGENT_TRANSCRIPTS)?;
    let mut index_rows = scan_all(db, cf::CF_AGENT_TRANSCRIPT_ORDER)?;
    let mut expected = Vec::with_capacity(source_rows.len());
    for (source_key, source_value) in &source_rows {
        let record = decode_json::<AgentTranscriptRecord>(source_value).map_err(|error| {
            format!(
                "AGENT_TRANSCRIPT_ORDER_STATUS_SOURCE_DECODE_FAILED: key_sha256={}: {error}",
                sha256_bytes(source_key)
            )
        })?;
        expected.push(order_row_for_record(source_key, source_value, &record)?);
    }
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    index_rows.sort_by(|left, right| left.0.cmp(&right.0));

    let meta_bytes = db
        .get_cf(cf::CF_KV, META_KEY)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_META_READ_FAILED: {error}"))?;
    let (
        meta,
        meta_schema_version,
        meta_readable,
        meta_decode_error,
        meta_source_rows_at_build,
        meta_source_digest_at_build,
    ) = match meta_bytes.as_deref() {
        Some(bytes) => match decode_json::<ProjectionMeta>(bytes) {
            Ok(meta) => {
                let schema_version = meta.schema_version;
                let readable = meta.readable;
                let source_rows_at_build = meta.source_rows_at_build;
                let source_digest_at_build = meta.source_digest_at_build.clone();
                (
                    Some(meta),
                    Some(schema_version),
                    Some(readable),
                    None,
                    Some(source_rows_at_build),
                    Some(source_digest_at_build),
                )
            }
            Err(error) => (None, None, None, Some(error.to_string()), None, None),
        },
        None => (None, None, None, None, None, None),
    };
    let progress_bytes = db
        .get_cf(cf::CF_KV, PROGRESS_KEY)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_PROGRESS_READ_FAILED: {error}"))?;
    let (
        progress,
        progress_complete,
        progress_rows_indexed,
        progress_decode_error,
        progress_resume_cursor_present,
    ) = match progress_bytes.as_deref() {
        Some(bytes) => match decode_json::<ProjectionProgress>(bytes) {
            Ok(progress) => {
                let complete = progress.complete;
                let rows_indexed = progress.rows_indexed;
                let resume_cursor_present = progress.resume_after_physical_hex.is_some();
                (
                    Some(progress),
                    Some(complete),
                    Some(rows_indexed),
                    None,
                    Some(resume_cursor_present),
                )
            }
            Err(error) => (None, None, None, Some(error.to_string()), None),
        },
        None => (None, None, None, None, None),
    };
    let marker = read_repair_marker(db)?;
    let exact_match = expected == index_rows;
    let publication_validation = match (meta.as_ref(), progress.as_ref()) {
        (Some(meta), Some(progress)) => validate_published_rows(meta, progress),
        (None, _) if meta_decode_error.is_some() => Err(format!(
            "AGENT_TRANSCRIPT_ORDER_META_CORRUPT: {}",
            meta_decode_error.as_deref().unwrap_or("<unknown>")
        )),
        (None, _) => Err(
            "AGENT_TRANSCRIPT_ORDER_META_MISSING: no durable publication metadata row".to_owned(),
        ),
        (_, None) if progress_decode_error.is_some() => Err(format!(
            "AGENT_TRANSCRIPT_ORDER_PROGRESS_CORRUPT: {}",
            progress_decode_error.as_deref().unwrap_or("<unknown>")
        )),
        (_, None) => Err(
            "AGENT_TRANSCRIPT_ORDER_PROGRESS_MISSING: no durable publication progress row"
                .to_owned(),
        ),
    };
    let publication_consistent = publication_validation.is_ok();
    let publication_error = publication_validation.err();
    let ready = exact_match && publication_consistent && marker.is_none();
    let source_digest_sha256 = rows_digest(&source_rows);
    let expected_index_digest_sha256 = rows_digest(&expected);
    let actual_index_digest_sha256 = rows_digest(&index_rows);
    let first_mismatch = (!exact_match).then(|| first_mismatch(&expected, &index_rows));

    let mut state_hasher = Sha256::new();
    for component in [
        vault_identity.vault_id.as_bytes(),
        source_digest_sha256.as_bytes(),
        expected_index_digest_sha256.as_bytes(),
        actual_index_digest_sha256.as_bytes(),
        meta_bytes.as_deref().unwrap_or_default(),
        progress_bytes.as_deref().unwrap_or_default(),
    ] {
        state_hasher.update((component.len() as u64).to_be_bytes());
        state_hasher.update(component);
    }
    if let Some(marker) = marker.as_ref() {
        state_hasher.update(marker.schema_version.to_be_bytes());
        state_hasher.update((marker.vault_id.len() as u64).to_be_bytes());
        state_hasher.update(marker.vault_id.as_bytes());
        state_hasher.update((marker.authorized_repair_token_sha256.len() as u64).to_be_bytes());
        state_hasher.update(marker.authorized_repair_token_sha256.as_bytes());
        state_hasher.update(marker.started_at_unix_ms.to_be_bytes());
    }
    let state_token_sha256 = hex_encode(&state_hasher.finalize());
    let repair_token_sha256 = marker.as_ref().map_or_else(
        || {
            repair_authorization_token(
                &vault_identity.vault_id,
                meta_bytes.as_deref(),
                progress_bytes.as_deref(),
                exact_match,
            )
        },
        |marker| marker.authorized_repair_token_sha256.clone(),
    );

    Ok(TranscriptOrderStatus {
        vault_id: vault_identity.vault_id,
        source_rows: source_rows.len() as u64,
        index_rows: index_rows.len() as u64,
        source_digest_sha256,
        expected_index_digest_sha256,
        actual_index_digest_sha256,
        exact_match,
        ready,
        first_mismatch,
        meta_present: meta_bytes.is_some(),
        meta_schema_version,
        meta_readable,
        meta_decode_error,
        meta_source_rows_at_build,
        meta_source_digest_at_build,
        progress_present: progress_bytes.is_some(),
        progress_complete,
        progress_rows_indexed,
        progress_decode_error,
        progress_resume_cursor_present,
        publication_consistent,
        publication_error,
        repair_in_progress: marker.is_some(),
        state_token_sha256,
        repair_token_sha256,
        source_of_truth: PROJECTION_SOURCE_OF_TRUTH,
    })
}

/// Application-managed optimistic-concurrency token for the repair decision.
///
/// The full `state_token_sha256` deliberately changes with every physical
/// source/index mutation and remains the audit/readback token. Repair does not
/// mutate source rows, however, and re-censuses both row sets after it acquires
/// their one-writer lock. Including the live row digests in its authorization
/// token made the required MCP status -> rebuild sequence self-invalidating:
/// ordinary transcript ingest for the status/tool calls atomically advanced
/// both otherwise-correct row sets before rebuild could acquire that lock.
///
/// Scope this token to exactly the state the repair decision depends on. A
/// different vault, changed publication row, or transition between exact and
/// divergent row sets still conflicts. Mirrored source/index inserts and
/// retention deletes do not, because rebuild verifies their latest complete
/// sets under the lock before choosing finalize-only versus reconstruction.
fn repair_authorization_token(
    vault_id: &str,
    meta_bytes: Option<&[u8]>,
    progress_bytes: Option<&[u8]>,
    exact_match: bool,
) -> String {
    let mut hasher = Sha256::new();
    for component in [
        b"agent-transcript-order-repair/v2".as_slice(),
        vault_id.as_bytes(),
        meta_bytes.unwrap_or_default(),
        progress_bytes.unwrap_or_default(),
    ] {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component);
    }
    hasher.update([u8::from(exact_match)]);
    hex_encode(&hasher.finalize())
}

fn read_repair_marker(db: &Db) -> Result<Option<ProjectionRepairMarker>, String> {
    db.get_cf(cf::CF_KV, REPAIR_KEY)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_READ_FAILED: {error}"))?
        .map(|bytes| {
            let marker = decode_json::<ProjectionRepairMarker>(&bytes).map_err(|error| {
                format!(
                    "AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_CORRUPT: {error}; remediation=preserve the exact CF_KV repair row and inspect the interrupted repair before retrying"
                )
            })?;
            if marker.schema_version != SCHEMA_VERSION {
                return Err(format!(
                    "AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_SCHEMA_UNSUPPORTED: actual={} expected={SCHEMA_VERSION}",
                    marker.schema_version
                ));
            }
            if marker.vault_id.is_empty()
                || !repair_token_is_valid(&marker.authorized_repair_token_sha256)
            {
                return Err(format!(
                    "AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_FIELDS_INVALID: vault_id_empty={} authorization_token_valid={}; remediation=preserve the exact CF_KV repair row and inspect the interrupted writer",
                    marker.vault_id.is_empty(),
                    repair_token_is_valid(&marker.authorized_repair_token_sha256)
                ));
            }
            Ok(marker)
        })
        .transpose()
}

fn repair_token_is_valid(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn write_repair_marker(db: &Db, marker: &ProjectionRepairMarker) -> Result<(), String> {
    let bytes = encode_json(marker)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_ENCODE_FAILED: {error}"))?;
    let outcome = db
        .put_batch_if_revision_pressure_bypass(
            cf::CF_KV,
            REPAIR_KEY,
            None,
            [(REPAIR_KEY.to_vec(), bytes.clone())],
        )
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_WRITE_FAILED: {error}"))?;
    if !outcome.applied {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_CONFLICT: actual_revision={}; remediation=re-read projection status before retrying",
            outcome
                .previous_revision_sha256
                .map_or_else(|| "absent".to_owned(), |value| hex_encode(&value))
        ));
    }
    let readback = db.get_cf(cf::CF_KV, REPAIR_KEY).map_err(|error| {
        format!("AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_READBACK_FAILED: {error}")
    })?;
    if readback.as_deref() != Some(bytes.as_slice()) {
        return Err(
            "AGENT_TRANSCRIPT_ORDER_REPAIR_MARKER_READBACK_MISMATCH: remediation=inspect the exact CF_KV row before retrying"
                .to_owned(),
        );
    }
    Ok(())
}

fn clear_projection_rows(db: &Db) -> Result<u64, String> {
    let mut deleted = 0u64;
    let mut after_physical = None;
    loop {
        let page = db
            .scan_cf_physical_page(
                cf::CF_AGENT_TRANSCRIPT_ORDER,
                after_physical.as_deref(),
                BACKFILL_ROWS,
            )
            .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_REPAIR_SCAN_FAILED: {error}"))?;
        if page.more && page.resume_after_physical.is_none() {
            return Err(
                "AGENT_TRANSCRIPT_ORDER_REPAIR_CURSOR_ABSENT: scan reported more=true without an opaque physical cursor"
                    .to_owned(),
            );
        }
        let keys = page.rows.into_iter().map(|row| row.0).collect::<Vec<_>>();
        if !keys.is_empty() {
            db.delete_batch(cf::CF_AGENT_TRANSCRIPT_ORDER, keys.iter().cloned())
                .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_REPAIR_DELETE_FAILED: {error}"))?;
            for key in &keys {
                if db
                    .get_cf(cf::CF_AGENT_TRANSCRIPT_ORDER, key)
                    .map_err(|error| {
                        format!("AGENT_TRANSCRIPT_ORDER_REPAIR_DELETE_READBACK_FAILED: {error}")
                    })?
                    .is_some()
                {
                    return Err(format!(
                        "AGENT_TRANSCRIPT_ORDER_REPAIR_DELETE_READBACK_MISMATCH: key_sha256={}; remediation=preserve the repair marker and inspect the physical tombstone",
                        sha256_bytes(key)
                    ));
                }
            }
            deleted = deleted.saturating_add(keys.len() as u64);
        }
        if !page.more {
            break;
        }
        after_physical = page.resume_after_physical;
    }
    Ok(deleted)
}

fn clear_publication_rows(db: &Db) -> Result<(), String> {
    delete_exact_row(db, cf::CF_KV, META_KEY, "META")?;
    delete_exact_row(db, cf::CF_KV, PROGRESS_KEY, "PROGRESS")
}

fn delete_exact_row(db: &Db, cf_name: &str, key: &[u8], label: &str) -> Result<(), String> {
    if db
        .get_cf(cf_name, key)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_{label}_PREDELETE_READ_FAILED: {error}"))?
        .is_none()
    {
        return Ok(());
    }
    db.delete_batch(cf_name, [key.to_vec()])
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_{label}_DELETE_FAILED: {error}"))?;
    if db
        .get_cf(cf_name, key)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_{label}_DELETE_READBACK_FAILED: {error}"))?
        .is_some()
    {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_{label}_DELETE_READBACK_MISMATCH: remediation=inspect the physical tombstone before retrying"
        ));
    }
    Ok(())
}

fn finalize_repaired_projection(db: &Db, status: &TranscriptOrderStatus) -> Result<(), String> {
    clear_publication_rows(db)?;
    let progress = ProjectionProgress {
        schema_version: SCHEMA_VERSION,
        complete: true,
        resume_after_physical_hex: None,
        rows_indexed: status.source_rows,
    };
    write_initial_progress(db, &progress)?;
    let meta = ProjectionMeta {
        schema_version: SCHEMA_VERSION,
        readable: true,
        built_at_unix_ms: now_ms()?,
        source_rows_at_build: status.source_rows,
        source_digest_at_build: status.expected_index_digest_sha256.clone(),
    };
    publish_meta(db, &meta, &progress)
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

fn validate_published_rows(
    meta: &ProjectionMeta,
    progress: &ProjectionProgress,
) -> Result<(), String> {
    validate_meta(meta)?;
    validate_progress(progress)?;
    if !progress.complete || progress.resume_after_physical_hex.is_some() {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_PUBLICATION_INCOMPLETE: complete={} resume_cursor_present={}; remediation=resume the revision-guarded projection repair before serving reads",
            progress.complete,
            progress.resume_after_physical_hex.is_some()
        ));
    }
    if progress.rows_indexed != meta.source_rows_at_build {
        return Err(format!(
            "AGENT_TRANSCRIPT_ORDER_PUBLICATION_COUNT_MISMATCH: progress_rows_indexed={} meta_source_rows_at_build={}; remediation=preserve both CF_KV publication rows and run the revision-guarded projection repair",
            progress.rows_indexed, meta.source_rows_at_build
        ));
    }
    if !repair_token_is_valid(&meta.source_digest_at_build) {
        return Err(
            "AGENT_TRANSCRIPT_ORDER_PUBLICATION_DIGEST_INVALID: source_digest_at_build must be exactly 64 lowercase hexadecimal characters; remediation=preserve the metadata row and run the revision-guarded projection repair"
                .to_owned(),
        );
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
                "row={index} expected_key_sha256={} actual_key_sha256={} expected_value_sha256={} actual_value_sha256={}",
                sha256_bytes(&expected[index].0),
                sha256_bytes(&actual[index].0),
                sha256_bytes(&expected[index].1),
                sha256_bytes(&actual[index].1)
            );
        }
    }
    format!("length_boundary={common}")
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
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

fn now_ms() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("AGENT_TRANSCRIPT_ORDER_CLOCK_BEFORE_EPOCH: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_error| {
        "AGENT_TRANSCRIPT_ORDER_CLOCK_OVERFLOW: unix milliseconds exceed u64".to_owned()
    })
}
