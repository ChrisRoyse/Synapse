//! Byte-level read-back verification of a restored Aster vault directory.
//!
//! This verifier is read-only: it never opens a writable vault handle, creates
//! directories, truncates WAL tails, or replays bytes into the vault. Counts are
//! measured by scanning SST and WAL bytes directly.
//!
//! Every column-family traversal here is **streamed** (#2059/#2243). The first
//! version merged each CF into one in-memory map and then re-hashed the chain
//! from a fully materialized `Vec<LedgerRow>`, so peak retention grew with the
//! vault. The first paged correction still eagerly decoded every SST lookup
//! index and reopened/re-sought the merge cursor for each output page. On a
//! production vault that verifier overlapped scheduled kernel maintenance and
//! raised process-private memory to 2.83 GiB. One cold immutable cursor now
//! advances forward for the complete CF, retaining only one current row per SST
//! source plus one bounded output page. Ledger rows are parsed through borrowed
//! wire slices and hashed incrementally, so the scrubber does not copy the
//! entire logical journal through a second decoded allocation stream.
//!
//! Peak retention is now one page of rows plus the WAL overlay, independent of
//! vault size, so the whole vault is covered on every pass. That is the shape
//! every mature scrubber takes: ZFS's scrub streams the pool under an explicit
//! memory limit and issues bounded sorted ranges rather than accumulating the
//! whole scan (openzfs/zfs#15260), and RocksDB's `VerifyChecksum` /
//! `VerifyFileChecksums` stream file blocks with readahead under a rate limiter
//! instead of materializing a database and then checking it.
//!
//! When a scan genuinely cannot run — a resource budget refuses it rather than
//! any integrity evidence failing — that is recorded as `unverifiable`, never as
//! `error`. "I could not finish looking" and "I looked and it is damaged" are
//! different findings and must not share a remediation.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cf::{ColumnFamily, slot_key};
use crate::ledger_head::read_head_anchor;
use crate::ledger_view::parse_aster_ledger_seq;
use crate::security::value_crypto::{SharedVaultContext, open_rows, open_value, open_value_owned};
use crate::sst::SstEntry;
use crate::sst::level::SstLevel;
use crate::vault::encode::{decode_constellation_base, decode_slot_vector, decode_write_batch};
use crate::wal::replay_dir_read_only;
use calyx_core::{CalyxError, Result};
use calyx_ledger::{AnchorDiscipline, StreamingChainVerifier, StreamingStart, VerifyResult};
use serde::Serialize;

/// Invalid restore target path.
pub const CALYX_ASTER_RESTORE_INVALID: &str = "CALYX_ASTER_RESTORE_INVALID";

const OPTIONAL_REBUILDABLE_DIRS: [&str; 3] = ["ann", "kernel", "guard"];

/// Rows materialized per bounded verification page.
///
/// Peak retention of a scan is this many decoded rows, not the column family.
/// One cursor remains open across pages. Hot families use retained lookup
/// indexes; cold families use the bounded on-disk index cursor. Neither path
/// materializes the immutable corpus or re-seeks its sources per page.
const VERIFY_SCAN_PAGE_ROWS: usize = 4_096;

/// Scan refusals that mean "this check could not run to completion", never
/// "this data is bad".
///
/// Each of these is a defensive ceiling failing closed before an allocator abort
/// or an unbounded read. None of them is evidence about vault integrity, so none
/// of them may reach an operator wearing the corruption remediation.
const RESOURCE_REFUSAL_CODES: [&str; 5] = [
    "CALYX_ASTER_SCAN_MEMORY_BUDGET",
    "CALYX_ASTER_SCAN_ALLOC",
    "CALYX_ASTER_SST_PAGE_SOURCE_LIMIT_EXCEEDED",
    "CALYX_ASTER_SST_SEQUENTIAL_SOURCE_LIMIT_EXCEEDED",
    "CALYX_ASTER_SEQUENTIAL_SNAPSHOT_MEMORY_BUDGET",
];

type WalOverlay = HashMap<ColumnFamily, Vec<(Vec<u8>, Vec<u8>)>>;

/// Byte-level verification report for a restored vault.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyRestoreReport {
    pub vault_path: PathBuf,
    pub constellation_count: u64,
    pub anchor_count: u64,
    pub ledger_entry_count: u64,
    pub ledger_tip_hash: String,
    pub chain_intact: bool,
    pub wal_bytes_present: u64,
    pub first_cx_id: Option<String>,
    /// Integrity evidence that failed, or a read that proved the vault bytes
    /// unusable. Presence here means "this vault is bad".
    pub error: Option<String>,
    /// A resource budget refused the scan before it could reach a verdict.
    ///
    /// Presence here means "this vault is **unverified**", which is a real
    /// deficiency and keeps [`Self::success`] false — but it is not evidence of
    /// damage and must never carry a restore-from-backup remediation (#2059).
    pub unverifiable: Option<String>,
    /// `CALYX_*` code of the refusal recorded in [`Self::unverifiable`].
    pub unverifiable_code: Option<String>,
}

impl VerifyRestoreReport {
    fn empty(vault_path: &Path) -> Self {
        Self {
            vault_path: vault_path.to_path_buf(),
            constellation_count: 0,
            anchor_count: 0,
            ledger_entry_count: 0,
            ledger_tip_hash: String::new(),
            chain_intact: false,
            wal_bytes_present: 0,
            first_cx_id: None,
            error: None,
            unverifiable: None,
            unverifiable_code: None,
        }
    }

    /// Restore-integrity predicate: intact chain and durable state bytes present.
    ///
    /// A newly created vault legitimately has no Base or Anchor rows. Corpus
    /// population is reported by the counts, but is not a storage-integrity
    /// invariant and must not turn scheduled verification into a permanent
    /// false alarm on fresh installations.
    ///
    /// A refused scan is not a pass either: an unverified vault stays non-green.
    /// What changes is only what the non-green verdict *means* — see
    /// [`Self::unverifiable_only`].
    pub fn success(&self) -> bool {
        self.error.is_none()
            && self.unverifiable.is_none()
            && self.chain_intact
            && self.wal_bytes_present > 0
    }

    /// True when nothing proved the vault bad and the scan simply could not run.
    ///
    /// This is the predicate that separates an indeterminate verdict from a
    /// corruption alarm. It is deliberately conjunctive: any integrity evidence
    /// in `error` outranks a refusal, because a vault can be both damaged and
    /// too large to finish scanning, and the damage is what must be reported.
    pub fn unverifiable_only(&self) -> bool {
        self.error.is_none() && self.unverifiable.is_some()
    }

    /// Names every unmet pass criterion.
    pub fn failure_reasons(&self) -> Vec<String> {
        if let Some(error) = &self.error {
            return vec![error.clone()];
        }
        if let Some(reason) = &self.unverifiable {
            return vec![format!("unverifiable: {reason}")];
        }
        let mut reasons = Vec::new();
        if !self.chain_intact {
            reasons.push("ledger chain not verified intact".to_string());
        }
        if self.wal_bytes_present == 0 {
            reasons
                .push("wal_bytes_present=0: no wal/*.wal bytes in the restored vault".to_string());
        }
        reasons
    }
}

/// Verifies a restored vault with zero write side effects.
///
/// Restore discipline: the vault is quiescent, so the anchored head must equal
/// the physical head exactly ([`AnchorDiscipline::ExactHead`]).
pub fn verify_restore(vault_path: &Path) -> Result<VerifyRestoreReport> {
    verify_restore_inner(vault_path, None, AnchorDiscipline::ExactHead)
}

/// Verifies a LIVE vault with zero write side effects (#2059).
///
/// Live discipline: rows append continuously, so the anchor is read *before*
/// the row snapshot and verified mid-stream at its own height
/// ([`AnchorDiscipline::AnchoredPrefix`]); rows the snapshot holds beyond the
/// anchored head are chain-verified as the anchored tip's continuation instead
/// of being declared corruption. Under `ExactHead` a healthy live vault failed
/// 27 of 30 verifications purely on appends that landed between the row
/// snapshot and the anchor read.
pub fn verify_restore_live(vault_path: &Path) -> Result<VerifyRestoreReport> {
    verify_restore_inner(vault_path, None, AnchorDiscipline::AnchoredPrefix)
}

/// Verifies an encrypted restored vault with zero write side effects.
pub fn verify_restore_with_value_crypto(
    vault_path: &Path,
    context: &SharedVaultContext,
) -> Result<VerifyRestoreReport> {
    verify_restore_inner(vault_path, Some(context), AnchorDiscipline::ExactHead)
}

fn verify_restore_inner(
    vault_path: &Path,
    value_crypto: Option<&SharedVaultContext>,
    anchor_discipline: AnchorDiscipline,
) -> Result<VerifyRestoreReport> {
    if !vault_path.is_dir() {
        return Err(restore_invalid(format!(
            "vault path {} does not exist or is not a directory",
            vault_path.display()
        )));
    }
    if !vault_path.join("cf").is_dir() && !vault_path.join("wal").is_dir() {
        return Err(restore_invalid(format!(
            "vault path {} holds no Aster state (neither cf/ nor wal/ exists)",
            vault_path.display()
        )));
    }
    for dir in OPTIONAL_REBUILDABLE_DIRS {
        if !vault_path.join(dir).is_dir() {
            eprintln!(
                "calyx verify-restore: optional dir {dir}/ absent in {} - rebuildable, \
                 excluded from backup; skipping",
                vault_path.display()
            );
        }
    }

    let mut report = VerifyRestoreReport::empty(vault_path);
    // #2059: the head anchor is read BEFORE any row snapshot is taken. Under
    // live appends the anchor can therefore only trail the rows the snapshot
    // holds, never lead them — which is what makes an anchor ahead of the
    // physical head unambiguous corruption in both disciplines, and what lets
    // AnchoredPrefix verify the anchor mid-stream at its own height without
    // racing writers.
    let head_anchor = match read_head_anchor(vault_path) {
        Ok(anchor) => anchor,
        Err(error) => {
            record_scan_error(&mut report, &error);
            return Ok(report);
        }
    };
    report.wal_bytes_present = match wal_total_bytes(vault_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            record_scan_error(&mut report, &error);
            return Ok(report);
        }
    };
    let overlay = match read_wal_overlay(vault_path, value_crypto) {
        Ok(overlay) => overlay,
        Err(error) => {
            record_scan_error(&mut report, &error);
            return Ok(report);
        }
    };

    let base = match scan_cf_rows(vault_path, ColumnFamily::Base, &overlay, value_crypto) {
        Ok(scan) => scan,
        Err(error) => {
            record_scan_error(&mut report, &error);
            return Ok(report);
        }
    };
    report.constellation_count = base.row_count;
    match scan_cf_rows(vault_path, ColumnFamily::Anchors, &overlay, value_crypto) {
        Ok(scan) => report.anchor_count = scan.row_count,
        Err(error) => {
            record_scan_error(&mut report, &error);
            return Ok(report);
        }
    }

    if let Some(first) = &base.first_row {
        match read_back_first_constellation(
            vault_path,
            &overlay,
            value_crypto,
            &first.key,
            &first.value,
        ) {
            Ok(cx_id) => report.first_cx_id = Some(cx_id),
            Err(error) => {
                record_scan_error(&mut report, &error);
                return Ok(report);
            }
        }
    }

    let ledger = match verify_ledger_paged(
        vault_path,
        &overlay,
        value_crypto,
        head_anchor,
        anchor_discipline,
    ) {
        Ok(ledger) => ledger,
        Err(error) => {
            record_scan_error(&mut report, &error);
            return Ok(report);
        }
    };
    report.ledger_entry_count = ledger.entry_count;
    match ledger.result {
        VerifyResult::Intact { .. } => {
            report.chain_intact = true;
            report.ledger_tip_hash = ledger.tip_hash;
        }
        VerifyResult::Broken { at_seq, .. } => {
            report.error = Some(format!("CALYX_LEDGER_CHAIN_BROKEN at seq={at_seq}"));
        }
        VerifyResult::Corrupt { at_seq, reason } => {
            report.error = Some(format!("CALYX_LEDGER_CORRUPT at seq={at_seq}: {reason}"));
        }
    }
    Ok(report)
}

/// Files a scan failure under the finding it actually is.
///
/// A resource refusal names the budget, the need, and its own remediation, and
/// is recorded as indeterminate. Everything else is evidence about the bytes and
/// stays an error.
fn record_scan_error(report: &mut VerifyRestoreReport, error: &CalyxError) {
    if RESOURCE_REFUSAL_CODES.contains(&error.code) {
        report.unverifiable_code = Some(error.code.to_owned());
        report.unverifiable = Some(format!("{error}; remediation={}", error.remediation));
    } else {
        report.error = Some(error.to_string());
    }
}

#[derive(Default)]
struct CfScan {
    row_count: u64,
    /// Lowest live key in the merged view, retained whole so the first
    /// constellation can be decoded without a second traversal.
    first_row: Option<SstEntry>,
}

fn scan_cf_rows(
    vault: &Path,
    cf: ColumnFamily,
    overlay: &WalOverlay,
    value_crypto: Option<&SharedVaultContext>,
) -> Result<CfScan> {
    let mut scan = CfScan::default();
    visit_cf_rows(vault, cf, overlay, value_crypto, |entry| {
        scan.row_count = scan.row_count.saturating_add(1);
        if scan
            .first_row
            .as_ref()
            .is_none_or(|first| entry.key < first.key)
        {
            scan.first_row = Some(entry);
        }
        Ok(())
    })?;
    Ok(scan)
}

/// Streams the merged (SST + WAL) live rows of one column family, one bounded
/// page at a time.
///
/// The WAL overlay wins over SST bytes for the same key, exactly as the previous
/// whole-CF merge did. Tombstoned rows are skipped: the paged reader resolves the
/// latest visible state, so a deleted row no longer inflates the counts — and the
/// lowest live key can no longer be a tombstone whose value fails to decode.
fn visit_cf_rows(
    vault: &Path,
    cf: ColumnFamily,
    overlay: &WalOverlay,
    value_crypto: Option<&SharedVaultContext>,
    mut visit: impl FnMut(SstEntry) -> Result<()>,
) -> Result<()> {
    let wal_rows = overlay_rows(overlay, cf)
        .into_iter()
        .map(|(key, value)| SstEntry { key, value })
        .collect();
    let level = cf_level(vault, cf)?;
    let mut stream = level.open_sequential_page_stream_with_overlay_origins(
        &[],
        None,
        VERIFY_SCAN_PAGE_ROWS,
        wal_rows,
    )?;
    while let Some(page) = stream.next_page()? {
        for winner in page {
            let entry = if winner.from_overlay {
                winner.entry
            } else {
                open_sst_entry(winner.entry, cf, value_crypto)?
            };
            if !crate::mvcc::is_tombstone_value(&entry.value) {
                visit(entry)?;
            }
        }
    }
    Ok(())
}

/// Collapses the WAL overlay for one column family to its last-write-wins view.
///
/// The overlay is bounded by the WAL, not by the vault, so it is the one part of
/// the scan that may be held whole.
fn overlay_rows(overlay: &WalOverlay, cf: ColumnFamily) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut rows = BTreeMap::new();
    if let Some(wal_rows) = overlay.get(&cf) {
        for (key, value) in wal_rows {
            rows.insert(key.clone(), value.clone());
        }
    }
    rows
}

fn read_wal_overlay(vault: &Path, value_crypto: Option<&SharedVaultContext>) -> Result<WalOverlay> {
    let wal_dir = vault.join("wal");
    let mut overlay = WalOverlay::new();
    if !wal_dir.is_dir() {
        return Ok(overlay);
    }
    let replay = replay_dir_read_only(&wal_dir)?;
    if let Some(torn) = replay.torn_tail {
        return Err(torn.error());
    }
    for record in replay.records {
        let rows = open_rows(value_crypto, decode_write_batch(&record.payload)?)?;
        for row in rows {
            overlay
                .entry(row.cf)
                .or_default()
                .push((row.key, row.value));
        }
    }
    Ok(overlay)
}

/// Opens a column family's immutable files with only validated bounds retained.
///
/// The verifier subsequently owns one forward-only stream. Eager lookup
/// metadata would duplicate every immutable key in memory, while reopening a
/// cursor per output page would repeatedly rebuild and seek the same source
/// frontier. Neither is part of a bounded integrity check.
fn cf_level(vault: &Path, cf: ColumnFamily) -> Result<SstLevel> {
    SstLevel::from_oldest_first(cf_sst_paths(vault, cf)?)
}

fn cf_sst_paths(vault: &Path, cf: ColumnFamily) -> Result<Vec<PathBuf>> {
    let dir = vault.join("cf").join(cf.name());
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in
        fs::read_dir(&dir).map_err(|error| read_error(&dir, "read CF dir", &error.to_string()))?
    {
        let path = entry
            .map_err(|error| read_error(&dir, "read CF dir entry", &error.to_string()))?
            .path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("sst") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Reads one key from a column family without materializing the family.
///
/// The slot-column read-back needs a single row per slot; the previous merged
/// view built the entire quantized slot CF to answer it.
fn point_read_cf(
    vault: &Path,
    cf: ColumnFamily,
    overlay: &WalOverlay,
    value_crypto: Option<&SharedVaultContext>,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    if let Some(rows) = overlay.get(&cf)
        && let Some((_, value)) = rows.iter().rev().find(|(row_key, _)| row_key == key)
    {
        return Ok(Some(value.clone()));
    }
    let level = SstLevel::from_oldest_first(cf_sst_paths(vault, cf)?)?;
    let Some(value) = level.get(key)? else {
        return Ok(None);
    };
    let Some(context) = value_crypto else {
        return Ok(Some(value));
    };
    Ok(Some(open_value(context, cf, key, &value)?))
}

fn read_back_first_constellation(
    vault: &Path,
    overlay: &WalOverlay,
    value_crypto: Option<&SharedVaultContext>,
    key: &[u8],
    value: &[u8],
) -> Result<String> {
    if value.is_empty() {
        return Err(CalyxError::aster_corrupt_shard(
            "first base CF row is empty",
        ));
    }
    let constellation = decode_constellation_base(value)?;
    if key != constellation.cx_id.as_bytes() {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "base CF key {} does not match embedded cx_id {}",
            hex(key),
            hex(constellation.cx_id.as_bytes())
        )));
    }
    let wanted = slot_key(constellation.cx_id);
    for slot in constellation.slots.keys() {
        let cf = ColumnFamily::slot(*slot);
        let bytes = point_read_cf(vault, cf, overlay, value_crypto, &wanted)?.ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "slot {slot} column missing for first constellation {}",
                hex(constellation.cx_id.as_bytes())
            ))
        })?;
        decode_slot_vector(&bytes)?;
    }
    Ok(hex(key))
}

fn open_sst_entry(
    entry: SstEntry,
    cf: ColumnFamily,
    value_crypto: Option<&SharedVaultContext>,
) -> Result<SstEntry> {
    let Some(context) = value_crypto else {
        return Ok(entry);
    };
    Ok(SstEntry {
        value: open_value_owned(context, cf, &entry.key, entry.value)?,
        key: entry.key,
    })
}

struct LedgerVerification {
    entry_count: u64,
    result: VerifyResult,
    tip_hash: String,
}

/// Re-hashes the provenance chain from the physical Ledger CF without ever
/// holding it whole.
///
/// The Ledger CF key is a big-endian `u64`, so the paged reader yields rows in
/// ascending sequence — which is exactly the order
/// [`StreamingChainVerifier`] consumes. That verifier has existed since the
/// chain was written and had no caller; it is the bounded counterpart to
/// `verify_chain`, which needs the whole chain in a `Vec` first.
fn verify_ledger_paged(
    vault: &Path,
    overlay: &WalOverlay,
    value_crypto: Option<&SharedVaultContext>,
    anchor: Option<calyx_ledger::LedgerHeadAnchor>,
    discipline: AnchorDiscipline,
) -> Result<LedgerVerification> {
    let wal_rows = ledger_overlay_rows(overlay)?;
    let level = cf_level(vault, ColumnFamily::Ledger)?;
    let head = physical_ledger_head(&level, &wal_rows)?;
    if anchor.is_none() && head > 0 {
        return Err(crate::ledger_head::missing_head_anchor(vault, head));
    }
    let verifier = match StreamingChainVerifier::start(0..head, anchor, None, discipline)? {
        StreamingStart::Complete(result) => {
            return Ok(LedgerVerification {
                entry_count: 0,
                result,
                tip_hash: hex(&[0_u8; 32]),
            });
        }
        StreamingStart::Ready(verifier) => verifier,
    };
    feed_ledger_rows(&level, &wal_rows, value_crypto, verifier, head)
}

/// Highest physically present ledger sequence, plus one.
///
/// Derived from the bytes rather than from the head anchor on purpose: a chain
/// whose rows run past its anchored head is exactly the mismatch
/// [`StreamingChainVerifier::start`] refuses, and taking the head from the
/// anchor would make that check vacuous.
fn physical_ledger_head(level: &SstLevel, wal_rows: &BTreeMap<u64, Vec<u8>>) -> Result<u64> {
    let mut max_seq = wal_rows.keys().next_back().copied();
    if let Some(entry) = level.predecessor(&[], &u64::MAX.to_be_bytes(), true)? {
        let seq = parse_aster_ledger_seq(&entry.key)?;
        max_seq = Some(max_seq.map_or(seq, |current| current.max(seq)));
    }
    Ok(max_seq.map_or(0, |seq| seq.saturating_add(1)))
}

fn ledger_overlay_rows(overlay: &WalOverlay) -> Result<BTreeMap<u64, Vec<u8>>> {
    let mut rows = BTreeMap::new();
    let Some(wal_rows) = overlay.get(&ColumnFamily::Ledger) else {
        return Ok(rows);
    };
    for (key, value) in wal_rows {
        let seq = parse_aster_ledger_seq(key)?;
        if let Some(existing) = rows.get(&seq) {
            if existing == value {
                continue;
            }
            return Err(CalyxError::ledger_corrupt(format!(
                "divergent ledger bytes for seq {seq} between WAL records"
            )));
        }
        rows.insert(seq, value.clone());
    }
    Ok(rows)
}

fn feed_ledger_rows(
    level: &SstLevel,
    wal_rows: &BTreeMap<u64, Vec<u8>>,
    value_crypto: Option<&SharedVaultContext>,
    mut verifier: StreamingChainVerifier,
    head: u64,
) -> Result<LedgerVerification> {
    let mut wal_iter = wal_rows.iter().peekable();
    let mut entry_count = 0_u64;
    let mut last_hash: Option<[u8; 32]> = None;
    let mut verdict: Option<VerifyResult> = None;
    let mut stream = level.open_sequential_page_stream_with_overlay_origins(
        &[],
        None,
        VERIFY_SCAN_PAGE_ROWS,
        Vec::new(),
    )?;
    'pages: while let Some(page) = stream.next_page()? {
        for winner in page {
            let entry = open_sst_entry(winner.entry, ColumnFamily::Ledger, value_crypto)?;
            if crate::mvcc::is_tombstone_value(&entry.value) {
                continue;
            }
            let seq = parse_aster_ledger_seq(&entry.key)?;
            // WAL-only sequences below this SST row come first in chain order.
            while wal_iter.peek().is_some_and(|(wal_seq, _)| **wal_seq < seq) {
                let Some((wal_seq, bytes)) = wal_iter.next() else {
                    break;
                };
                if let Some(result) = feed_ledger_row(
                    &mut verifier,
                    *wal_seq,
                    bytes,
                    &mut entry_count,
                    &mut last_hash,
                )? {
                    verdict = Some(result);
                    break 'pages;
                }
            }
            // The same sequence present in both an SST and the WAL must be
            // byte-identical; a divergence is corruption, not a merge choice.
            if let Some((wal_seq, wal_bytes)) = wal_iter.peek()
                && **wal_seq == seq
            {
                if wal_bytes.as_slice() != entry.value.as_slice() {
                    return Err(CalyxError::ledger_corrupt(format!(
                        "divergent ledger bytes for seq {seq} between SST and WAL"
                    )));
                }
                wal_iter.next();
            }
            if let Some(result) = feed_ledger_row(
                &mut verifier,
                seq,
                &entry.value,
                &mut entry_count,
                &mut last_hash,
            )? {
                verdict = Some(result);
                break 'pages;
            }
        }
    }
    if verdict.is_none() {
        for (wal_seq, bytes) in wal_iter {
            if let Some(result) = feed_ledger_row(
                &mut verifier,
                *wal_seq,
                bytes,
                &mut entry_count,
                &mut last_hash,
            )? {
                verdict = Some(result);
                break;
            }
        }
    }
    let result = match verdict {
        Some(result) => result,
        None => {
            let stalled_at = verifier.next_seq();
            verifier.verify_next(None)?.ok_or_else(|| {
                CalyxError::ledger_chain_broken(format!(
                    "ledger verification exhausted the physical Ledger CF at seq {stalled_at} \
                     before reaching head {head}"
                ))
            })?
        }
    };
    let tip_hash = match &last_hash {
        Some(hash) => hex(hash),
        None => hex(&[0_u8; 32]),
    };
    Ok(LedgerVerification {
        entry_count,
        result,
        tip_hash,
    })
}

/// Feeds one row to the streaming verifier, turning a sequence gap into a named
/// verdict rather than a silently shortened chain.
fn feed_ledger_row(
    verifier: &mut StreamingChainVerifier,
    seq: u64,
    bytes: &[u8],
    entry_count: &mut u64,
    last_hash: &mut Option<[u8; 32]>,
) -> Result<Option<VerifyResult>> {
    if seq != verifier.next_seq() {
        return verifier.verify_next(None);
    }
    *entry_count = entry_count.saturating_add(1);
    let result = verifier.verify_next_bytes(seq, bytes)?;
    *last_hash = Some(verifier.verified_tip_hash());
    Ok(result)
}

fn wal_total_bytes(vault: &Path) -> Result<u64> {
    let wal_dir = vault.join("wal");
    if !wal_dir.is_dir() {
        return Ok(0);
    }
    let mut total = 0;
    for entry in fs::read_dir(&wal_dir)
        .map_err(|error| read_error(&wal_dir, "read WAL dir", &error.to_string()))?
    {
        let path = entry
            .map_err(|error| read_error(&wal_dir, "read WAL dir entry", &error.to_string()))?
            .path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("wal") {
            total += fs::metadata(&path)
                .map_err(|error| read_error(&path, "stat WAL file", &error.to_string()))?
                .len();
        }
    }
    Ok(total)
}

fn read_error(path: &Path, action: &str, detail: &str) -> CalyxError {
    CalyxError::disk_pressure(format!("{action} {}: {detail}", path.display()))
}

fn restore_invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ASTER_RESTORE_INVALID,
        message: message.into(),
        remediation: "choose a restored Aster vault directory containing cf/ or wal/ bytes",
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    out
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + value - 10),
        _ => unreachable!("nibble out of range"),
    }
}
