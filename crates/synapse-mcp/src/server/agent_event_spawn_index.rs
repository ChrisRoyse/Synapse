//! Exact spawn-scoped projection over `CF_AGENT_EVENTS` (#2140).
//!
//! The journal's primary key is chronological and `spawn_id` lives in its JSON
//! value. This projection makes that join seekable without weakening the
//! journal's append-only identity. It is unpublished until a complete guarded
//! backfill reconciles source and index sets, then every new source/index pair
//! shares one writer lock and one physical Calyx transaction.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{LazyLock, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use synapse_core::AgentEventRecord;
use synapse_storage::{
    CfRevisionGuard, Db, RawRow, RawRowWithExpiry, StorageError, StorageResult,
    agent_events::{
        agent_event_spawn_index_key, agent_event_spawn_index_prefix, decode_agent_event_key,
        decode_agent_event_spawn_index_key,
    },
    cf, decode_json, encode_json,
    ordered_index::{OrderedSourcePointer, decode_pointer, encode_pointer, hex_decode, hex_encode},
};

const META_KEY: &[u8] = b"projection/agent-event-spawn-index/v1/meta";
const PROGRESS_KEY: &[u8] = b"projection/agent-event-spawn-index/v1/progress";
const SCHEMA_VERSION: u32 = 1;
const BACKFILL_ROWS: usize = 256;

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
    source_rows_examined_at_build: u64,
    index_rows_at_build: u64,
    index_digest_at_build: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectionProgress {
    schema_version: u32,
    complete: bool,
    resume_after_source_key_hex: Option<String>,
    source_rows_examined: u64,
    index_rows_written: u64,
}

pub(super) struct IndexedAgentEventRow {
    pub source_key: Vec<u8>,
    pub source_value: Vec<u8>,
    pub record: AgentEventRecord,
}

pub(super) struct IndexedAgentEventRows {
    pub rows_scanned: usize,
    pub by_spawn: BTreeMap<String, Vec<IndexedAgentEventRow>>,
}

pub(super) fn lock_projection() -> StorageResult<MutexGuard<'static, ProjectionLockState>> {
    PROJECTION_LOCK.lock().map_err(|_error| {
        index_read_failed(
            "AGENT_EVENT_SPAWN_INDEX_LOCK_POISONED: projection serialization lock is poisoned; remediation=restart the daemon and inspect the prior panic before retrying",
        )
    })
}

pub(super) fn ensure_projection_locked(
    db: &Db,
    lock_state: &mut ProjectionLockState,
) -> StorageResult<()> {
    let vault_identity = read_vault_identity(db)?;
    if lock_state.reconciled_vault.as_ref() == Some(&vault_identity) {
        return Ok(());
    }
    match read_meta(db)? {
        Some(meta) => {
            validate_meta(&meta)?;
            let progress = read_progress(db)?.ok_or_else(|| {
                index_read_failed(
                    "AGENT_EVENT_SPAWN_INDEX_PROGRESS_MISSING: readable meta exists without its completion record; remediation=preserve the exact marker rows and restore only after source/index reconciliation",
                )
            })?;
            validate_progress(&progress)?;
            if !progress.complete {
                return Err(index_read_failed(format!(
                    "AGENT_EVENT_SPAWN_INDEX_PUBLICATION_INVALID: readable meta exists while progress.complete=false resume_after={:?}; remediation=preserve both marker rows and reconcile the interrupted publisher",
                    progress.resume_after_source_key_hex
                )));
            }
            let (source_rows, index_rows, digest) = reconcile_complete_sets(db)?;
            tracing::info!(
                code = "AGENT_EVENT_SPAWN_INDEX_RECONCILED",
                source_rows,
                index_rows,
                digest,
                built_source_rows = meta.source_rows_examined_at_build,
                built_index_rows = meta.index_rows_at_build,
                built_index_digest = meta.index_digest_at_build,
                "proved the complete retained source/index sets before enabling spawn-scoped reads"
            );
        }
        None => build_projection(db)?,
    }
    lock_state.reconciled_vault = Some(vault_identity);
    Ok(())
}

fn read_vault_identity(db: &Db) -> StorageResult<VaultIdentity> {
    let status = db.calyx_vault_status().map_err(|error| {
        index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_VAULT_IDENTITY_READ_FAILED: {error}; remediation=repair the Calyx vault status path before reading or writing the projection"
        ))
    })?;
    if !status.enabled || !status.open {
        return Err(index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_VAULT_IDENTITY_UNAVAILABLE: enabled={} open={} phase={}; remediation=open the configured Calyx vault before reading or writing the projection",
            status.enabled, status.open, status.phase
        )));
    }
    let vault_dir = status.vault_dir.ok_or_else(|| {
        index_read_failed(
            "AGENT_EVENT_SPAWN_INDEX_VAULT_IDENTITY_UNAVAILABLE: open vault status has no vault_dir; remediation=repair the Calyx status contract before retrying",
        )
    })?;
    let vault_id = status.vault_id.ok_or_else(|| {
        index_read_failed(
            "AGENT_EVENT_SPAWN_INDEX_VAULT_IDENTITY_UNAVAILABLE: open vault status has no vault_id; remediation=repair the Calyx identity read before retrying",
        )
    })?;
    Ok(VaultIdentity {
        vault_dir,
        vault_id,
    })
}

pub(super) fn index_row_for_record(
    source_key: &[u8],
    source_value: &[u8],
    record: &AgentEventRecord,
) -> StorageResult<Option<(Vec<u8>, Vec<u8>)>> {
    validate_source_record(source_key, record)?;
    let Some(spawn_id) = record.spawn_id.as_deref() else {
        return Ok(None);
    };
    let key = agent_event_spawn_index_key(spawn_id, source_key)?;
    let value = encode_pointer(&OrderedSourcePointer::new(source_key, source_value))?;
    Ok(Some((key, value)))
}

pub(super) fn read_for_spawns(
    db: &Db,
    spawns: &BTreeSet<String>,
) -> StorageResult<IndexedAgentEventRows> {
    if spawns.is_empty() {
        return Ok(IndexedAgentEventRows {
            rows_scanned: 0,
            by_spawn: BTreeMap::new(),
        });
    }
    let mut projection_guard = lock_projection()?;
    ensure_projection_locked(db, &mut projection_guard)?;

    let mut rows_scanned = 0_usize;
    let mut by_spawn = BTreeMap::new();
    for requested_spawn in spawns {
        let prefix = agent_event_spawn_index_prefix(requested_spawn)?;
        let index_rows = db.scan_cf_prefix(cf::CF_AGENT_EVENT_SPAWN_INDEX, &prefix)?;
        rows_scanned = rows_scanned.saturating_add(index_rows.len());
        for (index_key, pointer_bytes) in index_rows {
            let (indexed_spawn, indexed_source_key) =
                decode_agent_event_spawn_index_key(&index_key)?;
            if indexed_spawn != *requested_spawn {
                return Err(index_read_failed(format!(
                    "AGENT_EVENT_SPAWN_INDEX_PREFIX_ESCAPE: requested_spawn={requested_spawn:?} indexed_spawn={indexed_spawn:?} index_key={}; remediation=preserve the row and repair the prefix scanner or corrupt key before rebuilding",
                    hex_encode(&index_key)
                )));
            }
            let pointer = decode_pointer(cf::CF_AGENT_EVENT_SPAWN_INDEX, &pointer_bytes)?;
            let pointer_source_key = pointer.source_key(cf::CF_AGENT_EVENT_SPAWN_INDEX)?;
            if pointer_source_key != indexed_source_key {
                return Err(index_read_failed(format!(
                    "AGENT_EVENT_SPAWN_INDEX_SOURCE_KEY_MISMATCH: index_key={} suffix_source_key={} pointer_source_key={}; remediation=preserve the row and identify the divergent writer before repair",
                    hex_encode(&index_key),
                    hex_encode(&indexed_source_key),
                    hex_encode(&pointer_source_key)
                )));
            }
            let source_value = db
                .get_cf(cf::CF_AGENT_EVENTS, &indexed_source_key)?
                .ok_or_else(|| {
                    index_read_failed(format!(
                        "AGENT_EVENT_SPAWN_INDEX_DANGLING_POINTER: spawn_id={requested_spawn:?} index_key={} source_key={}; remediation=preserve the vault and inspect the atomic source/index commit plus retention envelopes before rebuilding",
                        hex_encode(&index_key),
                        hex_encode(&indexed_source_key)
                    ))
                })?;
            pointer.verify_source_value(cf::CF_AGENT_EVENT_SPAWN_INDEX, &source_value)?;
            let record: AgentEventRecord = decode_json(&source_value)?;
            validate_source_record(&indexed_source_key, &record)?;
            if record.spawn_id.as_deref() != Some(requested_spawn.as_str()) {
                return Err(index_read_failed(format!(
                    "AGENT_EVENT_SPAWN_INDEX_RECORD_ID_MISMATCH: requested_spawn={requested_spawn:?} source_spawn={:?} source_key={}; remediation=preserve source and index rows and repair the writer before rebuilding",
                    record.spawn_id,
                    hex_encode(&indexed_source_key)
                )));
            }
            let expected_index_key =
                agent_event_spawn_index_key(requested_spawn, &indexed_source_key)?;
            if index_key != expected_index_key {
                return Err(index_read_failed(format!(
                    "AGENT_EVENT_SPAWN_INDEX_KEY_MISMATCH: actual={} expected={} source_key={}; remediation=preserve the row and repair the codec or writer before rebuilding",
                    hex_encode(&index_key),
                    hex_encode(&expected_index_key),
                    hex_encode(&indexed_source_key)
                )));
            }
            by_spawn
                .entry(requested_spawn.clone())
                .or_insert_with(Vec::new)
                .push(IndexedAgentEventRow {
                    source_key: indexed_source_key,
                    source_value,
                    record,
                });
        }
    }
    Ok(IndexedAgentEventRows {
        rows_scanned,
        by_spawn,
    })
}

fn build_projection(db: &Db) -> StorageResult<()> {
    let mut progress = match read_progress(db)? {
        Some(progress) => {
            validate_progress(&progress)?;
            progress
        }
        None => {
            let (existing, _more) = db.scan_cf_from(cf::CF_AGENT_EVENT_SPAWN_INDEX, &[], 1)?;
            if !existing.is_empty() {
                return Err(index_read_failed(
                    "AGENT_EVENT_SPAWN_INDEX_UNTRACKED_PARTIAL_BUILD: index rows exist without meta or progress; remediation=preserve the rows and inspect the interrupted or non-authoritative writer before exact repair",
                ));
            }
            let initial = ProjectionProgress {
                schema_version: SCHEMA_VERSION,
                complete: false,
                resume_after_source_key_hex: None,
                source_rows_examined: 0,
                index_rows_written: 0,
            };
            write_initial_progress(db, &initial)?;
            initial
        }
    };
    if progress.complete {
        return Err(index_read_failed(
            "AGENT_EVENT_SPAWN_INDEX_META_MISSING_AFTER_COMPLETE_PROGRESS: remediation=preserve the projection rows and progress marker and restore the missing readable meta only after exact reconciliation",
        ));
    }

    loop {
        let start = match progress.resume_after_source_key_hex.as_deref() {
            Some(encoded) => key_after(hex_decode(encoded).ok_or_else(|| {
                index_read_failed(
                    "AGENT_EVENT_SPAWN_INDEX_PROGRESS_CURSOR_INVALID: remediation=preserve the progress row and inspect its source before repair",
                )
            })?),
            None => Vec::new(),
        };
        let (source_rows, more) = db.scan_cf_from(cf::CF_AGENT_EVENTS, &start, BACKFILL_ROWS)?;
        if source_rows.is_empty() {
            if more {
                return Err(index_read_failed(
                    "AGENT_EVENT_SPAWN_INDEX_SCAN_STALLED: storage reported more rows without a resumable live row; remediation=inspect expired physical candidates and the bounded scan contract",
                ));
            }
            break;
        }

        let prior_progress = db
            .get_cf_revisioned(cf::CF_KV, PROGRESS_KEY)?
            .ok_or_else(|| {
                index_read_failed(
                    "AGENT_EVENT_SPAWN_INDEX_PROGRESS_DISAPPEARED: remediation=inspect CF_KV protection and restore the exact progress row",
                )
            })?;
        let mut guards = vec![CfRevisionGuard::new(
            cf::CF_KV,
            PROGRESS_KEY,
            Some(prior_progress.revision_sha256),
        )];
        let mut index_rows = Vec::new();
        for (source_key, source_value) in &source_rows {
            let revisioned = db
                .get_cf_revisioned(cf::CF_AGENT_EVENTS, source_key)?
                .ok_or_else(|| {
                    index_read_failed(format!(
                        "AGENT_EVENT_SPAWN_INDEX_SOURCE_DISAPPEARED: source_key={}; remediation=retry only after retention or GC completes",
                        hex_encode(source_key)
                    ))
                })?;
            if revisioned.value.as_deref() != Some(source_value.as_slice()) {
                return Err(index_read_failed(format!(
                    "AGENT_EVENT_SPAWN_INDEX_SOURCE_CHANGED_DURING_BUILD: source_key={}; remediation=retry the unpublished build after the source writer or retention transition completes",
                    hex_encode(source_key)
                )));
            }
            let record: AgentEventRecord = decode_json(source_value)?;
            validate_source_record(source_key, &record)?;
            guards.push(CfRevisionGuard::new(
                cf::CF_AGENT_EVENTS,
                source_key.clone(),
                Some(revisioned.revision_sha256),
            ));
            if let Some((index_key, index_value)) =
                index_row_for_record(source_key, source_value, &record)?
            {
                guards.push(CfRevisionGuard::new(
                    cf::CF_AGENT_EVENT_SPAWN_INDEX,
                    index_key.clone(),
                    None,
                ));
                index_rows.push(RawRowWithExpiry::preserving_expiry(
                    index_key,
                    index_value,
                    revisioned.expires_at_ms,
                ));
            }
        }
        let last_key = source_rows
            .last()
            .map(|row| row.0.clone())
            .ok_or_else(|| index_read_failed("AGENT_EVENT_SPAWN_INDEX_LAST_KEY_MISSING"))?;
        progress.resume_after_source_key_hex = Some(hex_encode(&last_key));
        progress.source_rows_examined = progress
            .source_rows_examined
            .saturating_add(source_rows.len() as u64);
        progress.index_rows_written = progress
            .index_rows_written
            .saturating_add(index_rows.len() as u64);
        let progress_bytes = encode_json(&progress)?;
        let mut batches = Vec::with_capacity(2);
        if !index_rows.is_empty() {
            batches.push((cf::CF_AGENT_EVENT_SPAWN_INDEX, index_rows.clone()));
        }
        batches.push((
            cf::CF_KV,
            vec![RawRowWithExpiry::retained(
                PROGRESS_KEY,
                progress_bytes.clone(),
            )],
        ));
        let outcome =
            db.put_cf_batches_with_expiry_if_revisions_pressure_bypass(guards, batches)?;
        if !outcome.applied {
            return Err(index_read_failed(format!(
                "AGENT_EVENT_SPAWN_INDEX_BACKFILL_REVISION_CONFLICT: conflict={:?}; remediation=retry the unpublished build after the exact competing source or progress mutation is understood",
                outcome.conflict
            )));
        }
        exact_value_readback(
            db,
            cf::CF_KV,
            PROGRESS_KEY,
            &progress_bytes,
            "backfill progress",
        )?;
        for row in &index_rows {
            exact_value_readback(
                db,
                cf::CF_AGENT_EVENT_SPAWN_INDEX,
                &row.key,
                &row.value,
                "backfilled spawn index row",
            )?;
        }
        if !more {
            break;
        }
    }

    let (source_rows, index_rows, digest) = reconcile_complete_sets(db)?;
    progress.complete = true;
    let meta = ProjectionMeta {
        schema_version: SCHEMA_VERSION,
        readable: true,
        built_at_unix_ms: now_ms()?,
        source_rows_examined_at_build: source_rows,
        index_rows_at_build: index_rows,
        index_digest_at_build: digest.clone(),
    };
    publish_meta(db, &meta, &progress)?;
    tracing::info!(
        code = "AGENT_EVENT_SPAWN_INDEX_BUILT",
        source_rows,
        index_rows,
        digest,
        source_of_truth = "CF_AGENT_EVENTS exact rows + CF_AGENT_EVENT_SPAWN_INDEX digest pointers + CF_KV readable meta",
        "published exact spawn-scoped projection after full reconciliation"
    );
    Ok(())
}

fn reconcile_complete_sets(db: &Db) -> StorageResult<(u64, u64, String)> {
    let source_rows = scan_all(db, cf::CF_AGENT_EVENTS)?;
    let index_rows = scan_all(db, cf::CF_AGENT_EVENT_SPAWN_INDEX)?;
    let mut expected = Vec::new();
    for (source_key, source_value) in &source_rows {
        let record: AgentEventRecord = decode_json(source_value)?;
        validate_source_record(source_key, &record)?;
        if let Some(index_row) = index_row_for_record(source_key, source_value, &record)? {
            expected.push(index_row);
        }
    }
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    if expected != index_rows {
        let mismatch = first_mismatch(&expected, &index_rows);
        return Err(index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_RECONCILIATION_FAILED: source_rows={} expected_index_rows={} actual_index_rows={} mismatch={mismatch}; remediation=preserve the vault, identify the missing, extra, or corrupt atomic projection row, and rebuild only after fixing its cause",
            source_rows.len(),
            expected.len(),
            index_rows.len()
        )));
    }
    Ok((
        source_rows.len() as u64,
        expected.len() as u64,
        rows_digest(&expected),
    ))
}

fn scan_all(db: &Db, cf_name: &str) -> StorageResult<Vec<RawRow>> {
    let mut output = Vec::new();
    let mut start = Vec::new();
    loop {
        let (rows, more) = db.scan_cf_from(cf_name, &start, BACKFILL_ROWS)?;
        if rows.is_empty() {
            if more {
                return Err(index_read_failed(format!(
                    "AGENT_EVENT_SPAWN_INDEX_RECONCILE_SCAN_STALLED: cf={cf_name}"
                )));
            }
            break;
        }
        start =
            key_after(rows.last().map(|row| row.0.clone()).ok_or_else(|| {
                index_read_failed("AGENT_EVENT_SPAWN_INDEX_SCAN_LAST_KEY_MISSING")
            })?);
        output.extend(rows);
        if !more {
            break;
        }
    }
    Ok(output)
}

fn validate_source_record(source_key: &[u8], record: &AgentEventRecord) -> StorageResult<()> {
    record.validate().map_err(|detail| {
        index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_SOURCE_RECORD_INVALID: source_key={} detail={detail}; remediation=preserve and repair the authoritative journal row before rebuilding",
            hex_encode(source_key)
        ))
    })?;
    let (key_ts_ns, _seq) = decode_agent_event_key(source_key)?;
    if key_ts_ns != record.ts_ns {
        return Err(index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_SOURCE_TIMESTAMP_MISMATCH: source_key={} key_ts_ns={key_ts_ns} record_ts_ns={}; remediation=preserve and repair the authoritative journal row before rebuilding",
            hex_encode(source_key),
            record.ts_ns
        )));
    }
    Ok(())
}

fn read_meta(db: &Db) -> StorageResult<Option<ProjectionMeta>> {
    db.get_cf(cf::CF_KV, META_KEY)?
        .map(|bytes| {
            decode_json::<ProjectionMeta>(&bytes).map_err(|error| {
                index_read_failed(format!(
                    "AGENT_EVENT_SPAWN_INDEX_META_CORRUPT: {error}; remediation=preserve the exact CF_KV row and inspect the publisher"
                ))
            })
        })
        .transpose()
}

fn read_progress(db: &Db) -> StorageResult<Option<ProjectionProgress>> {
    db.get_cf(cf::CF_KV, PROGRESS_KEY)?
        .map(|bytes| {
            decode_json::<ProjectionProgress>(&bytes).map_err(|error| {
                index_read_failed(format!(
                    "AGENT_EVENT_SPAWN_INDEX_PROGRESS_CORRUPT: {error}; remediation=preserve the exact CF_KV row and inspect the interrupted publisher"
                ))
            })
        })
        .transpose()
}

fn validate_meta(meta: &ProjectionMeta) -> StorageResult<()> {
    if meta.schema_version != SCHEMA_VERSION || !meta.readable {
        return Err(index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_META_UNREADABLE: schema_version={} expected={SCHEMA_VERSION} readable={}; remediation=complete the persisted backfill before enabling reads",
            meta.schema_version, meta.readable
        )));
    }
    Ok(())
}

fn validate_progress(progress: &ProjectionProgress) -> StorageResult<()> {
    if progress.schema_version != SCHEMA_VERSION {
        return Err(index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_PROGRESS_SCHEMA_UNSUPPORTED: actual={} expected={SCHEMA_VERSION}; remediation=preserve the row and migrate it explicitly",
            progress.schema_version
        )));
    }
    Ok(())
}

fn write_initial_progress(db: &Db, progress: &ProjectionProgress) -> StorageResult<()> {
    let bytes = encode_json(progress)?;
    let outcome = db.put_cf_batches_if_revisions_pressure_bypass(
        vec![CfRevisionGuard::new(cf::CF_KV, PROGRESS_KEY, None)],
        vec![(cf::CF_KV, vec![(PROGRESS_KEY.to_vec(), bytes.clone())])],
    )?;
    if !outcome.applied {
        return Err(index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_PROGRESS_INIT_CONFLICT: {:?}",
            outcome.conflict
        )));
    }
    exact_value_readback(db, cf::CF_KV, PROGRESS_KEY, &bytes, "initial progress")
}

fn publish_meta(
    db: &Db,
    meta: &ProjectionMeta,
    progress: &ProjectionProgress,
) -> StorageResult<()> {
    let progress_revision = db
        .get_cf_revisioned(cf::CF_KV, PROGRESS_KEY)?
        .ok_or_else(|| index_read_failed("AGENT_EVENT_SPAWN_INDEX_PROGRESS_MISSING_AT_PUBLISH"))?;
    let meta_bytes = encode_json(meta)?;
    let progress_bytes = encode_json(progress)?;
    let outcome = db.put_cf_batches_if_revisions_pressure_bypass(
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
    )?;
    if !outcome.applied {
        return Err(index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_META_PUBLISH_CONFLICT: {:?}",
            outcome.conflict
        )));
    }
    exact_value_readback(db, cf::CF_KV, META_KEY, &meta_bytes, "readable meta")?;
    exact_value_readback(
        db,
        cf::CF_KV,
        PROGRESS_KEY,
        &progress_bytes,
        "complete progress",
    )
}

fn exact_value_readback(
    db: &Db,
    cf_name: &str,
    key: &[u8],
    expected: &[u8],
    identity: &str,
) -> StorageResult<()> {
    let actual = db.get_cf(cf_name, key)?;
    if actual.as_deref() != Some(expected) {
        return Err(index_read_failed(format!(
            "AGENT_EVENT_SPAWN_INDEX_READBACK_MISMATCH: identity={identity} cf={cf_name} key={} expected_len={} actual_len={}; remediation=preserve the vault and inspect the exact guarded commit before retrying",
            hex_encode(key),
            expected.len(),
            actual.as_ref().map_or(0, Vec::len)
        )));
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
    use sha2::{Digest, Sha256};
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

fn now_ms() -> StorageResult<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            index_read_failed(format!(
                "AGENT_EVENT_SPAWN_INDEX_CLOCK_BEFORE_EPOCH: {error}"
            ))
        })?
        .as_millis();
    u64::try_from(millis).map_err(|_error| {
        index_read_failed("AGENT_EVENT_SPAWN_INDEX_CLOCK_OVERFLOW: unix milliseconds exceed u64")
    })
}

fn index_read_failed(detail: impl Into<String>) -> StorageError {
    StorageError::ReadFailed {
        cf_name: cf::CF_AGENT_EVENT_SPAWN_INDEX.to_owned(),
        detail: detail.into(),
    }
}
