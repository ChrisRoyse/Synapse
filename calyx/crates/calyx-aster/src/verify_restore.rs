//! Byte-level read-back verification of a restored Aster vault directory.
//!
//! This verifier is read-only: it never opens a writable vault handle, creates
//! directories, truncates WAL tails, or replays bytes into the vault. Counts are
//! measured by scanning SST and WAL bytes directly.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cf::{ColumnFamily, slot_key};
use crate::ledger_head::read_head_anchor;
use crate::ledger_view::parse_aster_ledger_seq;
use crate::security::value_crypto::{SharedVaultContext, open_rows, open_value};
use crate::sst::SstEntry;
use crate::sst::level::SstLevel;
use crate::vault::encode::{decode_constellation_base, decode_slot_vector, decode_write_batch};
use crate::wal::replay_dir_read_only;
use calyx_core::{CalyxError, Result};
use calyx_ledger::{
    LedgerCfStore, LedgerHeadAnchor, LedgerRow, VerifyResult, decode as decode_ledger_entry,
    verify_chain,
};
use serde::Serialize;

/// Invalid restore target path.
pub const CALYX_ASTER_RESTORE_INVALID: &str = "CALYX_ASTER_RESTORE_INVALID";

const OPTIONAL_REBUILDABLE_DIRS: [&str; 3] = ["ann", "kernel", "guard"];

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
    pub error: Option<String>,
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
        }
    }

    /// Strict DR-drill predicate: intact chain and real data bytes present.
    pub fn success(&self) -> bool {
        self.error.is_none()
            && self.chain_intact
            && self.constellation_count > 0
            && self.anchor_count > 0
            && self.wal_bytes_present > 0
    }

    /// Names every unmet pass criterion.
    pub fn failure_reasons(&self) -> Vec<String> {
        if let Some(error) = &self.error {
            return vec![error.clone()];
        }
        let mut reasons = Vec::new();
        if !self.chain_intact {
            reasons.push("ledger chain not verified intact".to_string());
        }
        if self.constellation_count == 0 {
            reasons.push(
                "constellation_count=0: no constellation readable from the base CF".to_string(),
            );
        }
        if self.anchor_count == 0 {
            reasons.push("anchor_count=0: no anchor readable from the anchors CF".to_string());
        }
        if self.wal_bytes_present == 0 {
            reasons
                .push("wal_bytes_present=0: no wal/*.wal bytes in the restored vault".to_string());
        }
        reasons
    }
}

/// Verifies a restored vault with zero write side effects.
pub fn verify_restore(vault_path: &Path) -> Result<VerifyRestoreReport> {
    verify_restore_inner(vault_path, None)
}

/// Verifies an encrypted restored vault with zero write side effects.
pub fn verify_restore_with_value_crypto(
    vault_path: &Path,
    context: &SharedVaultContext,
) -> Result<VerifyRestoreReport> {
    verify_restore_inner(vault_path, Some(context))
}

fn verify_restore_inner(
    vault_path: &Path,
    value_crypto: Option<&SharedVaultContext>,
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
    report.wal_bytes_present = match wal_total_bytes(vault_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            report.error = Some(error.to_string());
            return Ok(report);
        }
    };
    let scan = match scan_vault(vault_path, value_crypto) {
        Ok(scan) => scan,
        Err(error) => {
            report.error = Some(error.to_string());
            return Ok(report);
        }
    };
    report.constellation_count = scan.constellation_count;
    report.anchor_count = scan.anchor_count;
    report.ledger_entry_count = scan.ledger_rows.len() as u64;
    report.first_cx_id = scan.first_cx_id;
    if scan.ledger_anchor.is_none()
        && let Some(head) = scan.ledger_rows.last().map(|row| row.seq.saturating_add(1))
    {
        report.error = Some(crate::ledger_head::missing_head_anchor(vault_path, head).to_string());
        return Ok(report);
    }

    let store = RestoredLedgerRows {
        rows: scan.ledger_rows,
        anchor: scan.ledger_anchor,
    };
    let head = store.rows.last().map_or(0, |row| row.seq.saturating_add(1));
    match verify_chain(&store, 0..head) {
        Ok(VerifyResult::Intact { .. }) => match tip_hash(&store.rows) {
            Ok(hash) => {
                report.chain_intact = true;
                report.ledger_tip_hash = hash;
            }
            Err(error) => report.error = Some(error.to_string()),
        },
        Ok(VerifyResult::Broken { at_seq, .. }) => {
            report.error = Some(format!("CALYX_LEDGER_CHAIN_BROKEN at seq={at_seq}"));
        }
        Ok(VerifyResult::Corrupt { at_seq, reason }) => {
            report.error = Some(format!("CALYX_LEDGER_CORRUPT at seq={at_seq}: {reason}"));
        }
        Err(error) => report.error = Some(error.to_string()),
    }
    Ok(report)
}

struct VaultScan {
    constellation_count: u64,
    anchor_count: u64,
    first_cx_id: Option<String>,
    ledger_rows: Vec<LedgerRow>,
    ledger_anchor: Option<LedgerHeadAnchor>,
}

fn scan_vault(vault: &Path, value_crypto: Option<&SharedVaultContext>) -> Result<VaultScan> {
    let overlay = read_wal_overlay(vault, value_crypto)?;
    let base = merged_cf(vault, ColumnFamily::Base, &overlay, value_crypto)?;
    let anchors = merged_cf(vault, ColumnFamily::Anchors, &overlay, value_crypto)?;
    let ledger_rows = merged_ledger_rows(vault, &overlay, value_crypto)?;
    let ledger_anchor = read_head_anchor(vault)?;
    let first_cx_id = match base.iter().next() {
        Some((key, value)) => Some(read_back_first_constellation(
            vault,
            &overlay,
            value_crypto,
            key,
            value,
        )?),
        None => None,
    };
    Ok(VaultScan {
        constellation_count: base.len() as u64,
        anchor_count: anchors.len() as u64,
        first_cx_id,
        ledger_rows,
        ledger_anchor,
    })
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

fn merged_cf(
    vault: &Path,
    cf: ColumnFamily,
    overlay: &WalOverlay,
    value_crypto: Option<&SharedVaultContext>,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut rows = BTreeMap::new();
    for entry in read_cf_ssts(vault, cf, value_crypto)? {
        rows.insert(entry.key, entry.value);
    }
    if let Some(wal_rows) = overlay.get(&cf) {
        for (key, value) in wal_rows {
            rows.insert(key.clone(), value.clone());
        }
    }
    Ok(rows)
}

fn read_cf_ssts(
    vault: &Path,
    cf: ColumnFamily,
    value_crypto: Option<&SharedVaultContext>,
) -> Result<Vec<SstEntry>> {
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
    let entries = SstLevel::from_oldest_first(files).iter()?;
    entries
        .into_iter()
        .map(|entry| open_sst_entry(entry, cf, value_crypto))
        .collect()
}

fn merged_ledger_rows(
    vault: &Path,
    overlay: &WalOverlay,
    value_crypto: Option<&SharedVaultContext>,
) -> Result<Vec<LedgerRow>> {
    let mut rows: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for entry in read_cf_ssts(vault, ColumnFamily::Ledger, value_crypto)? {
        let seq = parse_aster_ledger_seq(&entry.key)?;
        insert_ledger_bytes(&mut rows, seq, entry.value)?;
    }
    if let Some(wal_rows) = overlay.get(&ColumnFamily::Ledger) {
        for (key, value) in wal_rows {
            let seq = parse_aster_ledger_seq(key)?;
            insert_ledger_bytes(&mut rows, seq, value.clone())?;
        }
    }
    Ok(rows
        .into_iter()
        .map(|(seq, bytes)| LedgerRow { seq, bytes })
        .collect())
}

fn insert_ledger_bytes(rows: &mut BTreeMap<u64, Vec<u8>>, seq: u64, bytes: Vec<u8>) -> Result<()> {
    if let Some(existing) = rows.get(&seq) {
        if existing == &bytes {
            return Ok(());
        }
        return Err(CalyxError::ledger_corrupt(format!(
            "divergent ledger bytes for seq {seq} between SST and WAL"
        )));
    }
    rows.insert(seq, bytes);
    Ok(())
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
    for slot in constellation.slots.keys() {
        let slot_rows = merged_cf(vault, ColumnFamily::slot(*slot), overlay, value_crypto)?;
        let bytes = slot_rows
            .get(&slot_key(constellation.cx_id))
            .ok_or_else(|| {
                CalyxError::aster_corrupt_shard(format!(
                    "slot {slot} column missing for first constellation {}",
                    hex(constellation.cx_id.as_bytes())
                ))
            })?;
        decode_slot_vector(bytes)?;
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
        value: open_value(context, cf, &entry.key, &entry.value)?,
        key: entry.key,
    })
}

fn tip_hash(rows: &[LedgerRow]) -> Result<String> {
    match rows.last() {
        Some(row) => Ok(hex(&decode_ledger_entry(&row.bytes)?.entry_hash)),
        None => Ok(hex(&[0u8; 32])),
    }
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

struct RestoredLedgerRows {
    rows: Vec<LedgerRow>,
    anchor: Option<LedgerHeadAnchor>,
}

impl LedgerCfStore for RestoredLedgerRows {
    fn scan(&self) -> Result<Vec<LedgerRow>> {
        Ok(self.rows.clone())
    }

    fn put_new(&mut self, seq: u64, _bytes: &[u8]) -> Result<()> {
        Err(CalyxError::ledger_append_only_violation(format!(
            "verify-restore is read-only; rejected append for seq {seq}"
        )))
    }

    fn head_anchor(&self) -> Result<Option<LedgerHeadAnchor>> {
        Ok(self.anchor.clone())
    }
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
