//! Fail-closed physical coverage check for full-restore opens (issue #1132).
//!
//! Router memtable-flush SSTs (legacy `{ordinal:020}.sst` and commit-anchored
//! `flush-{watermark:020}-{ordinal:04}.sst`) hold merged latest-state rows
//! whose exact per-row commit seqs are unknowable from the file, so durable
//! readback can never restore them into the MVCC row table. On a healthy vault that is harmless: every committed
//! row also has a commit-domain home (durable-batch/compacted SST or WAL
//! record), so the restored state covers all router content. When that
//! invariant is broken (a manifest advanced past rows whose durable-batch
//! SSTs were never written, or a compaction output landed beyond the manifest
//! floor before its inputs were reclaimed), the affected rows survive only in
//! router SSTs and every snapshot read on a full-restore handle silently
//! misses them. This walk enumerates every Router-class SST row and reports
//! keys the restored MVCC state does not know at any version, so the open can
//! refuse instead of serving partial state.

use super::recovery_readback::tiered_cf_roots;
use super::storage_error;
use crate::cf::ColumnFamily;
use crate::compaction::TieringPolicy;
use crate::sst::shared_reader;
use crate::storage_names::{SstName, classify_sst, parse_cf_dir_name};
use calyx_core::{CalyxError, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const SAMPLE_KEYS_PER_CF: usize = 3;

/// One column family whose Router-class SSTs hold keys the restored MVCC
/// state does not know at any version.
#[derive(Debug)]
pub(in crate::vault) struct RouterOnlyCf {
    pub cf: ColumnFamily,
    pub router_only_rows: u64,
    pub sample_keys: Vec<String>,
}

/// Walks every Router-class SST under the vault's (tiered) CF roots and
/// returns the column families holding keys for which `covered` is false.
/// An empty result proves the full-restore view covers all router content.
pub(in crate::vault) fn router_only_rows(
    root: &Path,
    tiering_policy: Option<&TieringPolicy>,
    covered: impl Fn(ColumnFamily, &[u8]) -> bool,
) -> Result<Vec<RouterOnlyCf>> {
    let mut by_cf = BTreeMap::<ColumnFamily, RouterOnlyCf>::new();
    for cf_root in tiered_cf_roots(root, tiering_policy) {
        if !cf_root.exists() {
            continue;
        }
        for entry in fs::read_dir(&cf_root).map_err(|error| storage_error("read CF root", error))? {
            let cf_dir = entry.map_err(|error| storage_error("read CF entry", error))?;
            if !cf_dir
                .file_type()
                .map_err(|error| storage_error("stat CF entry", error))?
                .is_dir()
            {
                continue;
            }
            let cf_name = cf_dir.file_name().to_string_lossy().to_string();
            let cf = parse_cf_dir_name(&cf_name)?;
            for file in
                fs::read_dir(cf_dir.path()).map_err(|error| storage_error("read CF dir", error))?
            {
                let path = file
                    .map_err(|error| storage_error("read SST entry", error))?
                    .path();
                if !matches!(
                    classify_sst(&path)?,
                    Some(SstName::RouterLegacy { .. } | SstName::Flush { .. })
                ) {
                    continue;
                }
                for row in shared_reader(&path)?.iter()? {
                    if covered(cf, &row.key) {
                        continue;
                    }
                    let violation = by_cf.entry(cf).or_insert_with(|| RouterOnlyCf {
                        cf,
                        router_only_rows: 0,
                        sample_keys: Vec::new(),
                    });
                    violation.router_only_rows += 1;
                    if violation.sample_keys.len() < SAMPLE_KEYS_PER_CF {
                        violation.sample_keys.push(hex_prefix(&row.key));
                    }
                }
            }
        }
    }
    Ok(by_cf.into_values().collect())
}

/// Builds the typed fail-closed error for a full-restore open that would
/// silently miss router-flushed rows.
pub(in crate::vault) fn router_only_rows_error(violations: &[RouterOnlyCf]) -> CalyxError {
    let total: u64 = violations
        .iter()
        .map(|violation| violation.router_only_rows)
        .sum();
    let details = violations
        .iter()
        .map(|violation| {
            format!(
                "{}: {} row(s), e.g. [{}]",
                violation.cf.name(),
                violation.router_only_rows,
                violation.sample_keys.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    CalyxError {
        code: "CALYX_ASTER_ROUTER_ONLY_ROWS",
        message: format!(
            "full-restore open (restore_mvcc_rows=true) would silently miss {total} \
             router-flushed row(s) that have no commit-domain durable home — {details}"
        ),
        remediation: "open latest-only (restore_mvcc_rows=false, read_only=true) for current-state reads, or adopt the rows into the durable domain with the CLI `compact` command before requesting historical/MVCC reads",
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    let mut value = String::new();
    for byte in bytes.iter().take(12) {
        value.push_str(&format!("{byte:02x}"));
    }
    if bytes.len() > 12 {
        value.push_str("...");
    }
    value
}
