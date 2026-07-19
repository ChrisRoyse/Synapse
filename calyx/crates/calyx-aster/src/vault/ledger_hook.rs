use super::durable::RecoveredBatches;
use super::encode::WriteRow;
use crate::cf::ColumnFamily;
use crate::compaction::TieringPolicy;
use crate::ledger_view::{
    AsterLedgerCfStore, LedgerPointReadTrace, read_ledger_seqs_unlocked_traced,
};
use calyx_core::{
    CalyxError, Constellation, LedgerRef, METADATA_CHUNK_ID, METADATA_DATABASE_NAME, Result,
    SystemClock,
};
use calyx_ledger::{
    ActorId, CheckpointConfig, CheckpointPayload, DefaultLedgerHook, EntryKind, LedgerAppender,
    LedgerCfStore, LedgerHeadAnchor, MemoryLedgerStore, PayloadBuilder, StagedLedgerRow, SubjectId,
    decode,
};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

pub(super) type AsterLedgerHook = Mutex<DefaultLedgerHook<MemoryLedgerStore, SystemClock>>;
pub(super) type AsterLedgerHookGuard<'a> =
    MutexGuard<'a, DefaultLedgerHook<MemoryLedgerStore, SystemClock>>;

const CHECKPOINT_RECOVERY_BATCH_ROWS: u64 = 128;

pub(super) fn recover_hook(
    recovery: &RecoveredBatches,
    checkpoint: Option<CheckpointConfig>,
) -> Result<AsterLedgerHook> {
    recover_hook_from_store(recovered_ledger_store(recovery)?, checkpoint)
}

pub(super) fn recover_hook_from_vault_dir(
    vault_dir: &Path,
    recovery: &RecoveredBatches,
    checkpoint: Option<CheckpointConfig>,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<AsterLedgerHook> {
    let started_at = Instant::now();
    let recovery_batch_count = recovery.batches.len();
    let recovery_ledger_rows = recovery
        .batches
        .iter()
        .flat_map(|batch| batch.rows.iter())
        .filter(|row| row.cf == ColumnFamily::Ledger)
        .count();
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_RECOVERY_START",
        vault_dir = %vault_dir.display(),
        checkpoint_enabled = checkpoint.is_some(),
        checkpoint_interval_entries = checkpoint.as_ref().map(|config| config.interval_entries),
        recovery_batch_count,
        recovery_ledger_rows,
        "recovering Calyx ledger hook"
    );
    let store = match physical_ledger_store(
        vault_dir,
        LedgerViewLock::Acquire,
        checkpoint.as_ref(),
        tiering_policy,
    )? {
        Some(store) => store,
        None => recovered_ledger_store(recovery)?,
    };
    let hook = recover_hook_from_store(store, checkpoint)?;
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_RECOVERY_DONE",
        vault_dir = %vault_dir.display(),
        elapsed_ms = started_at.elapsed().as_millis(),
        "recovered Calyx ledger hook"
    );
    Ok(hook)
}

fn recover_hook_from_store(
    store: MemoryLedgerStore,
    checkpoint: Option<CheckpointConfig>,
) -> Result<AsterLedgerHook> {
    let appender = LedgerAppender::open(store, SystemClock)?;
    let hook = match checkpoint {
        Some(config) => DefaultLedgerHook::with_checkpoint_config(appender, config)?,
        None => DefaultLedgerHook::new(appender),
    };
    Ok(Mutex::new(hook))
}

fn recovered_ledger_store(recovery: &RecoveredBatches) -> Result<MemoryLedgerStore> {
    let mut store = MemoryLedgerStore::default();
    for batch in &recovery.batches {
        for row in &batch.rows {
            if row.cf == ColumnFamily::Ledger {
                store.insert_raw(parse_ledger_seq(&row.key)?, row.value.clone());
            }
        }
    }
    Ok(store)
}

#[derive(Clone, Copy, Debug)]
enum LedgerViewLock {
    Acquire,
    AlreadyHeld,
}

fn physical_ledger_store(
    vault_dir: &Path,
    lock: LedgerViewLock,
    checkpoint: Option<&CheckpointConfig>,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<Option<MemoryLedgerStore>> {
    let started_at = Instant::now();
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_PHYSICAL_STORE_START",
        vault_dir = %vault_dir.display(),
        lock = ?lock,
        checkpoint_enabled = checkpoint.is_some(),
        checkpoint_interval_entries = checkpoint.map(|config| config.interval_entries),
        "opening physical ledger store for hook recovery"
    );
    let lock_started_at = Instant::now();
    let _commit_guard = match lock {
        LedgerViewLock::Acquire => Some(crate::file_lock::FileLockGuard::acquire(
            &durable_commit_lock_path(vault_dir),
        )?),
        LedgerViewLock::AlreadyHeld => None,
    };
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_COMMIT_LOCK_READY",
        vault_dir = %vault_dir.display(),
        lock = ?lock,
        elapsed_ms = lock_started_at.elapsed().as_millis(),
        "ledger hook recovery commit lock ready"
    );
    if let Some(anchor) = crate::ledger_head::read_head_anchor(vault_dir)? {
        tracing::info!(
            code = "CALYX_ASTER_LEDGER_HOOK_HEAD_ANCHOR_FOUND",
            vault_dir = %vault_dir.display(),
            head_height = anchor.height,
            "using durable ledger head anchor for hook recovery"
        );
        let store = anchored_physical_ledger_store(vault_dir, &anchor, checkpoint, tiering_policy)?;
        tracing::info!(
            code = "CALYX_ASTER_LEDGER_HOOK_PHYSICAL_STORE_DONE",
            vault_dir = %vault_dir.display(),
            source = "head_anchor",
            elapsed_ms = started_at.elapsed().as_millis(),
            "opened physical ledger store for hook recovery"
        );
        return Ok(store);
    }

    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_NO_HEAD_ANCHOR_SCAN_START",
        vault_dir = %vault_dir.display(),
        "ledger hook recovery has no durable head anchor; probing physical ledger rows"
    );
    let view_started_at = Instant::now();
    let view = match AsterLedgerCfStore::open_unlocked_with_tiering(vault_dir, tiering_policy) {
        Ok(view) => view,
        Err(error)
            if error.code == "CALYX_LEDGER_CORRUPT"
                && error.message.contains("requires real Aster ledger state") =>
        {
            tracing::info!(
                code = "CALYX_ASTER_LEDGER_HOOK_NO_PHYSICAL_STORE",
                vault_dir = %vault_dir.display(),
                elapsed_ms = started_at.elapsed().as_millis(),
                "no physical ledger store available for hook recovery"
            );
            return Ok(None);
        }
        Err(error) => {
            tracing::error!(
                code = "CALYX_ASTER_LEDGER_HOOK_NO_HEAD_ANCHOR_SCAN_FAILED",
                vault_dir = %vault_dir.display(),
                elapsed_ms = view_started_at.elapsed().as_millis(),
                error = %error,
                "physical ledger scan failed during hook recovery"
            );
            return Err(error);
        }
    };
    let rows = view.scan()?;
    if rows.is_empty() {
        tracing::info!(
            code = "CALYX_ASTER_LEDGER_HOOK_NO_PHYSICAL_ROWS",
            vault_dir = %vault_dir.display(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "physical ledger store is empty for hook recovery"
        );
        return Ok(None);
    }
    let mut store = MemoryLedgerStore::default();
    let row_count = rows.len();
    for row in rows {
        store.insert_raw(row.seq, row.bytes);
    }
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_PHYSICAL_STORE_DONE",
        vault_dir = %vault_dir.display(),
        source = "scan_without_head_anchor",
        row_count,
        elapsed_ms = started_at.elapsed().as_millis(),
        "opened physical ledger store for hook recovery"
    );
    Ok(Some(store))
}

fn anchored_physical_ledger_store(
    vault_dir: &Path,
    anchor: &LedgerHeadAnchor,
    checkpoint: Option<&CheckpointConfig>,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<Option<MemoryLedgerStore>> {
    let started_at = Instant::now();
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_ANCHORED_STORE_START",
        vault_dir = %vault_dir.display(),
        head_height = anchor.height,
        checkpoint_enabled = checkpoint.is_some(),
        checkpoint_interval_entries = checkpoint.map(|config| config.interval_entries),
        "opening anchored physical ledger store for hook recovery"
    );
    let mut store = MemoryLedgerStore::default();
    store.put_head_anchor(anchor)?;
    if anchor.height == 0 {
        tracing::info!(
            code = "CALYX_ASTER_LEDGER_HOOK_ANCHORED_STORE_DONE",
            vault_dir = %vault_dir.display(),
            head_height = anchor.height,
            hydration_start_seq = 0_u64,
            hydrated_rows = 0_u64,
            elapsed_ms = started_at.elapsed().as_millis(),
            "opened empty anchored physical ledger store for hook recovery"
        );
        return Ok(Some(store));
    }
    let start = match checkpoint {
        Some(config) => {
            checkpoint_hydration_start(vault_dir, anchor.height, config, tiering_policy)?
        }
        None => anchor.height - 1,
    };
    let hydrated_rows =
        hydrate_physical_ledger_rows(vault_dir, start, anchor.height, &mut store, tiering_policy)?;
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_ANCHORED_STORE_DONE",
        vault_dir = %vault_dir.display(),
        head_height = anchor.height,
        hydration_start_seq = start,
        hydrated_rows,
        elapsed_ms = started_at.elapsed().as_millis(),
        "opened anchored physical ledger store for hook recovery"
    );
    Ok(Some(store))
}

fn checkpoint_hydration_start(
    vault_dir: &Path,
    head_height: u64,
    config: &CheckpointConfig,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<u64> {
    if config.interval_entries == 0 {
        return Err(CalyxError::ledger_corrupt(
            "checkpoint interval_entries must be greater than zero",
        ));
    }
    if head_height == 0 {
        return Ok(0);
    }
    let scan_limit = config
        .interval_entries
        .saturating_mul(2)
        .saturating_add(16)
        .min(head_height);
    let floor = head_height - scan_limit;
    let batch_rows = scan_limit.clamp(1, CHECKPOINT_RECOVERY_BATCH_ROWS);
    let started_at = Instant::now();
    if let Some(anchor) =
        checkpoint_hydration_start_from_pointer(vault_dir, head_height, scan_limit, tiering_policy)?
    {
        tracing::info!(
            code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_DONE",
            vault_dir = %vault_dir.display(),
            head_height,
            hydration_start_seq = anchor,
            elapsed_ms = started_at.elapsed().as_millis(),
            "selected checkpoint recovery start from durable pointer"
        );
        return Ok(anchor);
    }
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_SCAN_START",
        vault_dir = %vault_dir.display(),
        head_height,
        scan_floor_seq = floor,
        scan_limit,
        batch_rows,
        checkpoint_interval_entries = config.interval_entries,
        "searching ledger tail for newest checkpoint row"
    );
    let mut end = head_height;
    let mut batches_read = 0_u64;
    let mut rows_scanned = 0_u64;
    while end > floor {
        let start = end.saturating_sub(batch_rows).max(floor);
        let batch_started_at = Instant::now();
        let (rows, trace) =
            read_physical_ledger_rows_traced(vault_dir, start, end, tiering_policy)?;
        batches_read = batches_read.saturating_add(1);
        rows_scanned = rows_scanned.saturating_add(rows.len() as u64);
        log_point_read_trace("checkpoint_scan", start, end, rows.len(), &trace);
        tracing::info!(
            code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_SCAN_BATCH",
            vault_dir = %vault_dir.display(),
            range_start_seq = start,
            range_end_seq = end,
            rows_read = rows.len(),
            batches_read,
            rows_scanned,
            elapsed_ms = batch_started_at.elapsed().as_millis(),
            "read checkpoint recovery scan batch"
        );
        for row in rows.iter().rev() {
            let entry = decode(&row.bytes).map_err(|error| {
                CalyxError::ledger_corrupt(format!(
                    "decode ledger row {} during checkpoint recovery: {error}",
                    row.seq
                ))
            })?;
            if entry.kind != EntryKind::Admin {
                continue;
            }
            if let Some(payload) = CheckpointPayload::decode_optional(&entry.payload)? {
                let pointer = crate::ledger_head::LedgerCheckpointAnchor::from_checkpoint_payload(
                    entry.seq,
                    entry.entry_hash,
                    &payload,
                )?;
                crate::ledger_head::write_checkpoint_anchor(vault_dir, &pointer)?;
                tracing::info!(
                    code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_FOUND",
                    vault_dir = %vault_dir.display(),
                    checkpoint_seq = row.seq,
                    checkpoint_range_start = payload.range_start,
                    checkpoint_range_end = payload.range_end,
                    head_height,
                    batches_read,
                    rows_scanned,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "found newest checkpoint row for ledger hook recovery"
                );
                tracing::info!(
                    code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_REPAIRED",
                    vault_dir = %vault_dir.display(),
                    checkpoint_seq = pointer.seq,
                    checkpoint_range_start = pointer.range_start,
                    checkpoint_range_end = pointer.range_end,
                    head_height,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "persisted durable checkpoint pointer from bounded ledger scan"
                );
                return Ok(row.seq);
            }
        }
        end = start;
    }
    if head_height <= config.interval_entries {
        tracing::info!(
            code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_SCAN_GENESIS_WINDOW",
            vault_dir = %vault_dir.display(),
            head_height,
            checkpoint_interval_entries = config.interval_entries,
            batches_read,
            rows_scanned,
            elapsed_ms = started_at.elapsed().as_millis(),
            "ledger head has not crossed checkpoint interval; hydrating from genesis"
        );
        return Ok(0);
    }
    tracing::error!(
        code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_SCAN_UNBOUNDED",
        vault_dir = %vault_dir.display(),
        head_height,
        scan_limit,
        batches_read,
        rows_scanned,
        elapsed_ms = started_at.elapsed().as_millis(),
        "ledger hook recovery refused unbounded checkpoint scan"
    );
    Err(CalyxError {
        code: "CALYX_LEDGER_CHECKPOINT_RECOVERY_UNBOUNDED",
        message: format!(
            "anchored ledger head {head_height} has no checkpoint row in the last {scan_limit} rows; refusing full ledger hook scan during vault open"
        ),
        remediation: "rebuild or persist a ledger checkpoint pointer, then reopen the vault; do not bypass by disabling checkpoint recovery",
    })
}

fn checkpoint_hydration_start_from_pointer(
    vault_dir: &Path,
    head_height: u64,
    scan_limit: u64,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<Option<u64>> {
    let started_at = Instant::now();
    let Some(anchor) = crate::ledger_head::read_checkpoint_anchor(vault_dir)? else {
        tracing::info!(
            code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_MISSING",
            vault_dir = %vault_dir.display(),
            head_height,
            scan_limit,
            "durable checkpoint pointer is absent; bounded legacy scan will repair it"
        );
        return Ok(None);
    };
    if anchor.seq >= head_height {
        checkpoint_pointer_error(
            vault_dir,
            head_height,
            scan_limit,
            &anchor,
            "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_OUT_OF_RANGE",
            format!(
                "checkpoint pointer seq {} is outside anchored ledger head {}",
                anchor.seq, head_height
            ),
        )?;
    }
    let rows_after_pointer = head_height.saturating_sub(anchor.seq);
    if rows_after_pointer > scan_limit {
        tracing::error!(
            code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_TOO_OLD",
            vault_dir = %vault_dir.display(),
            checkpoint_seq = anchor.seq,
            checkpoint_range_start = anchor.range_start,
            checkpoint_range_end = anchor.range_end,
            head_height,
            scan_limit,
            rows_after_pointer,
            "durable checkpoint pointer is older than bounded recovery window"
        );
        return Err(CalyxError {
            code: "CALYX_LEDGER_CHECKPOINT_RECOVERY_UNBOUNDED",
            message: format!(
                "anchored ledger head {head_height} has checkpoint pointer seq {} older than the bounded {scan_limit}-row recovery window",
                anchor.seq
            ),
            remediation: "repair the persisted checkpoint pointer before reopening the vault; do not bypass by disabling checkpoint recovery",
        });
    }
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_VALIDATE_START",
        vault_dir = %vault_dir.display(),
        checkpoint_seq = anchor.seq,
        checkpoint_range_start = anchor.range_start,
        checkpoint_range_end = anchor.range_end,
        head_height,
        scan_limit,
        "validating durable checkpoint pointer against physical ledger row"
    );
    let (rows, trace) =
        read_physical_ledger_rows_traced(vault_dir, anchor.seq, anchor.seq + 1, tiering_policy)?;
    log_point_read_trace(
        "checkpoint_pointer",
        anchor.seq,
        anchor.seq + 1,
        rows.len(),
        &trace,
    );
    let Some(row) = rows.first() else {
        checkpoint_pointer_error(
            vault_dir,
            head_height,
            scan_limit,
            &anchor,
            "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_ROW_MISSING",
            format!(
                "checkpoint pointer seq {} is missing from the physical ledger",
                anchor.seq
            ),
        )?;
        unreachable!("checkpoint_pointer_error always returns Err");
    };
    let Some(actual) = crate::ledger_head::checkpoint_anchor_from_row(row)? else {
        checkpoint_pointer_error(
            vault_dir,
            head_height,
            scan_limit,
            &anchor,
            "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_NOT_CHECKPOINT",
            format!(
                "checkpoint pointer seq {} does not reference an admin checkpoint row",
                anchor.seq
            ),
        )?;
        unreachable!("checkpoint_pointer_error always returns Err");
    };
    if actual != anchor {
        checkpoint_pointer_error(
            vault_dir,
            head_height,
            scan_limit,
            &anchor,
            "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_MISMATCH",
            format!(
                "checkpoint pointer seq {} does not match physical checkpoint row seq {}",
                anchor.seq, actual.seq
            ),
        )?;
    }
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_CHECKPOINT_POINTER_VALIDATED",
        vault_dir = %vault_dir.display(),
        checkpoint_seq = anchor.seq,
        checkpoint_entry_hash = %hex(&anchor.entry_hash),
        checkpoint_range_start = anchor.range_start,
        checkpoint_range_end = anchor.range_end,
        head_height,
        scan_limit,
        elapsed_ms = started_at.elapsed().as_millis(),
        "validated durable checkpoint pointer for ledger hook recovery"
    );
    Ok(Some(anchor.seq))
}

fn checkpoint_pointer_error(
    vault_dir: &Path,
    head_height: u64,
    scan_limit: u64,
    anchor: &crate::ledger_head::LedgerCheckpointAnchor,
    code: &'static str,
    message: String,
) -> Result<()> {
    tracing::error!(
        code,
        vault_dir = %vault_dir.display(),
        checkpoint_seq = anchor.seq,
        checkpoint_entry_hash = %hex(&anchor.entry_hash),
        checkpoint_range_start = anchor.range_start,
        checkpoint_range_end = anchor.range_end,
        head_height,
        scan_limit,
        "durable checkpoint pointer is invalid"
    );
    Err(CalyxError::ledger_corrupt(message))
}

fn hydrate_physical_ledger_rows(
    vault_dir: &Path,
    start: u64,
    end: u64,
    store: &mut MemoryLedgerStore,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<u64> {
    let started_at = Instant::now();
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_HYDRATE_START",
        vault_dir = %vault_dir.display(),
        range_start_seq = start,
        range_end_seq = end,
        requested_rows = end.saturating_sub(start),
        "hydrating physical ledger rows for hook recovery"
    );
    let (rows, trace) = read_physical_ledger_rows_traced(vault_dir, start, end, tiering_policy)?;
    let hydrated_rows = rows.len() as u64;
    log_point_read_trace("hydrate", start, end, rows.len(), &trace);
    for row in rows {
        store.insert_raw(row.seq, row.bytes);
    }
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_HOOK_HYDRATE_DONE",
        vault_dir = %vault_dir.display(),
        range_start_seq = start,
        range_end_seq = end,
        hydrated_rows,
        elapsed_ms = started_at.elapsed().as_millis(),
        "hydrated physical ledger rows for hook recovery"
    );
    Ok(hydrated_rows)
}

fn read_physical_ledger_rows_traced(
    vault_dir: &Path,
    start: u64,
    end: u64,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<(Vec<calyx_ledger::LedgerRow>, LedgerPointReadTrace)> {
    if start > end {
        tracing::error!(
            code = "CALYX_ASTER_LEDGER_HOOK_PHYSICAL_READ_INVALID_RANGE",
            vault_dir = %vault_dir.display(),
            range_start_seq = start,
            range_end_seq = end,
            "invalid physical ledger hydration range"
        );
        return Err(CalyxError::ledger_corrupt(format!(
            "invalid physical ledger hydration range {start}..{end}"
        )));
    }
    let wanted = (start..end).collect::<BTreeSet<_>>();
    let (rows, trace) = read_ledger_seqs_unlocked_traced(vault_dir, &wanted, tiering_policy)?;
    let missing = wanted
        .iter()
        .filter(|seq| !rows.contains_key(seq))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        let samples = missing
            .iter()
            .take(3)
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CalyxError::ledger_chain_broken(format!(
            "anchored physical ledger hydration missing {} of {} requested seqs in range {start}..{end}; sample missing seqs: {samples}",
            missing.len(),
            wanted.len(),
        )));
    }
    let mut out = Vec::with_capacity(wanted.len());
    for seq in wanted {
        let row = rows.get(&seq).expect("missing rows checked above");
        out.push(row.clone());
    }
    Ok((out, trace))
}

fn durable_commit_lock_path(vault_dir: &Path) -> std::path::PathBuf {
    vault_dir.join("locks").join("durable.commit.lock")
}

fn log_point_read_trace(
    phase: &'static str,
    start: u64,
    end: u64,
    rows_read: usize,
    trace: &LedgerPointReadTrace,
) {
    for tier in &trace.tiers {
        tracing::info!(
            code = "CALYX_ASTER_LEDGER_HOOK_POINT_READ_TIER",
            phase,
            range_start_seq = start,
            range_end_seq = end,
            rows_read,
            tier = tier.tier,
            tier_wanted = tier.wanted,
            tier_resolved = tier.resolved,
            tier_files_opened = tier.files_opened,
            tier_elapsed_ms = tier.elapsed_ms,
            "ledger hook point-read tier"
        );
    }
}

pub(super) fn lock_hook(hook: &AsterLedgerHook) -> Result<AsterLedgerHookGuard<'_>> {
    hook.lock()
        .map_err(|_| CalyxError::ledger_group_commit_failed("ledger hook lock poisoned"))
}

pub(super) fn ensure_recovered_ledger_sidecars(
    vault_dir: &Path,
    recovery: &RecoveredBatches,
    repair: bool,
) -> Result<()> {
    if recovery.router_latest_readback {
        // restore_mvcc_rows=false intentionally omits manifested batches and
        // carries only the WAL tail. That partial set cannot prove an exact
        // Ledger head/checkpoint, so comparing it with a real sidecar would
        // falsely classify older manifested truth as stale-ahead. Sidecars are
        // irrelevant to this router-latest read-only handle; writer/full
        // recovery and runtime reconciliation always use complete coverage.
        tracing::debug!(
            code = "CALYX_ASTER_LEDGER_SIDECAR_CHECK_SKIPPED_PARTIAL_RECOVERY",
            vault_dir = %vault_dir.display(),
            "skipped exact Ledger sidecar comparison because recovery intentionally omitted manifested batches"
        );
        return Ok(());
    }
    let mut newest_head = None;
    let mut newest_checkpoint = None;
    for batch in &recovery.batches {
        if let Some(anchor) = crate::ledger_head::newest_anchor_from_rows(&batch.rows)?
            && newest_head
                .as_ref()
                .is_none_or(|current: &LedgerHeadAnchor| anchor.height > current.height)
        {
            newest_head = Some(anchor);
        }
        if let Some(anchor) = crate::ledger_head::newest_checkpoint_from_rows(&batch.rows)?
            && newest_checkpoint.as_ref().is_none_or(
                |current: &crate::ledger_head::LedgerCheckpointAnchor| anchor.seq > current.seq,
            )
        {
            newest_checkpoint = Some(anchor);
        }
    }
    let current_head = crate::ledger_head::read_head_anchor(vault_dir)?;
    if current_head != newest_head {
        let current_height = current_head
            .as_ref()
            .map_or_else(|| "absent".to_owned(), |anchor| anchor.height.to_string());
        let recovered_height = newest_head
            .as_ref()
            .map_or_else(|| "absent".to_owned(), |anchor| anchor.height.to_string());
        if !repair {
            return Err(CalyxError {
                code: super::CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
                message: format!(
                    "read-only open found Ledger head sidecar divergent from recovered physical truth: current_height={current_height} recovered_height={recovered_height}"
                ),
                remediation: "open the vault with a write-capable owner so it can replace the derived Ledger head with exact recovered WAL truth",
            });
        }
        crate::ledger_head::replace_head_anchor_from_recovery(vault_dir, newest_head.as_ref())?;
        tracing::warn!(
            code = "CALYX_ASTER_LEDGER_HEAD_SIDECAR_REPAIRED",
            current_height,
            recovered_height,
            "replaced divergent Ledger head sidecar with exact recovered physical truth"
        );
    }
    let current_checkpoint = crate::ledger_head::read_checkpoint_anchor(vault_dir)?;
    if current_checkpoint != newest_checkpoint {
        let current_seq = current_checkpoint
            .as_ref()
            .map_or_else(|| "absent".to_owned(), |anchor| anchor.seq.to_string());
        let recovered_seq = newest_checkpoint
            .as_ref()
            .map_or_else(|| "absent".to_owned(), |anchor| anchor.seq.to_string());
        if !repair {
            return Err(CalyxError {
                code: super::CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
                message: format!(
                    "read-only open found Ledger checkpoint sidecar divergent from recovered physical truth: current_seq={current_seq} recovered_seq={recovered_seq}"
                ),
                remediation: "open the vault with a write-capable owner so it can replace the derived Ledger checkpoint with exact recovered WAL truth",
            });
        }
        crate::ledger_head::replace_checkpoint_anchor_from_recovery(
            vault_dir,
            newest_checkpoint.as_ref(),
        )?;
        tracing::warn!(
            code = "CALYX_ASTER_LEDGER_CHECKPOINT_SIDECAR_REPAIRED",
            current_seq,
            recovered_seq,
            "replaced divergent Ledger checkpoint sidecar with exact recovered physical truth"
        );
    }
    Ok(())
}

pub(super) fn ensure_recovered_ledger_sidecars_with_commit_lock(
    vault_dir: &Path,
    recovery: &RecoveredBatches,
    repair: bool,
) -> Result<()> {
    let _commit_guard =
        crate::file_lock::FileLockGuard::acquire(&durable_commit_lock_path(vault_dir))?;
    ensure_recovered_ledger_sidecars(vault_dir, recovery, repair)
}

pub(super) fn refresh_hook(
    hook: &AsterLedgerHook,
    vault_dir: &Path,
    recovery: &RecoveredBatches,
    checkpoint: Option<CheckpointConfig>,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<()> {
    let store = match physical_ledger_store(
        vault_dir,
        LedgerViewLock::AlreadyHeld,
        checkpoint.as_ref(),
        tiering_policy,
    )? {
        Some(store) => store,
        None => recovered_ledger_store(recovery)?,
    };
    let replacement = recover_hook_from_store(store, checkpoint)?
        .into_inner()
        .map_err(|_| CalyxError::ledger_group_commit_failed("new ledger hook lock poisoned"))?;
    let mut guard = lock_hook(hook)?;
    *guard = replacement;
    Ok(())
}

pub(super) fn refresh_hook_from_recovery(
    hook: &AsterLedgerHook,
    recovery: &RecoveredBatches,
    checkpoint: Option<CheckpointConfig>,
) -> Result<()> {
    let replacement = recover_hook_from_store(recovered_ledger_store(recovery)?, checkpoint)?
        .into_inner()
        .map_err(|_| CalyxError::ledger_group_commit_failed("new ledger hook lock poisoned"))?;
    let mut guard = lock_hook(hook)?;
    *guard = replacement;
    Ok(())
}

pub(super) fn stage_ingest(
    hook: &DefaultLedgerHook<MemoryLedgerStore, SystemClock>,
    rows: &mut Vec<WriteRow>,
    constellation: &Constellation,
) -> Result<Vec<StagedLedgerRow>> {
    stage_ingest_payload(
        hook,
        rows,
        constellation.cx_id,
        ingest_payload(constellation),
    )
}

pub(super) fn stage_ingest_payload(
    hook: &DefaultLedgerHook<MemoryLedgerStore, SystemClock>,
    rows: &mut Vec<WriteRow>,
    subject: calyx_core::CxId,
    payload: Vec<u8>,
) -> Result<Vec<StagedLedgerRow>> {
    stage_entry_payload(
        hook,
        rows,
        EntryKind::Ingest,
        SubjectId::Cx(subject),
        payload,
        ActorId::Service("calyx-aster".to_string()),
    )
}

pub(super) fn stage_entry_payload(
    hook: &DefaultLedgerHook<MemoryLedgerStore, SystemClock>,
    rows: &mut Vec<WriteRow>,
    kind: EntryKind,
    subject: SubjectId,
    payload: Vec<u8>,
    actor: ActorId,
) -> Result<Vec<StagedLedgerRow>> {
    let staged = hook.stage_with_checkpoints(kind, subject, payload, actor)?;
    for row in &staged {
        rows.push(WriteRow {
            cf: ColumnFamily::Ledger,
            key: row.key().to_vec(),
            value: row.value().to_vec(),
        });
    }
    Ok(staged)
}

pub(super) fn commit_staged(
    hook: &mut DefaultLedgerHook<MemoryLedgerStore, SystemClock>,
    staged: &[StagedLedgerRow],
) -> Result<LedgerRef> {
    let data_ref = staged
        .first()
        .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
        .ledger_ref();
    for row in staged {
        hook.commit_staged(row)?;
    }
    Ok(data_ref)
}

pub(super) fn ingest_payload(constellation: &Constellation) -> Vec<u8> {
    let mut payload = PayloadBuilder::default();
    let mut metadata = serde_json::Map::new();
    for key in [METADATA_CHUNK_ID, METADATA_DATABASE_NAME] {
        if let Some(value) = constellation.metadata.get(key) {
            metadata.insert(key.to_string(), json!(value));
        }
    }
    payload
        .insert_str("cx_id", constellation.cx_id.to_string())
        .insert_str("input_hash", hex(&constellation.input_ref.hash))
        .insert_value(
            "input_ref",
            json!({
                "hash": constellation.input_ref.hash,
                "redacted": true,
            }),
        )
        .insert_u64("ts", constellation.created_at);
    if !metadata.is_empty() {
        payload.insert_value("metadata", serde_json::Value::Object(metadata));
    }
    calyx_ledger::RedactionPolicy::default().apply_to_payload(&payload)
}

fn parse_ledger_seq(key: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = key
        .try_into()
        .map_err(|_| CalyxError::ledger_corrupt(format!("ledger key length {} != 8", key.len())))?;
    Ok(u64::from_be_bytes(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
