//! Canonical on-disk file-name contract for Aster-owned directories.
//!
//! Aster's `cf/<family>/` directories are shared by writers with disjoint
//! canonical name shapes, and the WAL directory has one:
//!
//! - LSM router memtable flush (commit-anchored, issue #1138):
//!   `flush-{watermark:020}-{ordinal:04}.sst`
//! - LSM router memtable flush (legacy shape, written before #1138):
//!   `{ordinal:020}.sst`
//! - durable group-commit batch (and the reserved `9000..=9999` index range
//!   used by compaction adoption slots): `{seq:020}-{index:04}.sst`
//! - compaction output: `compacted-{seq:020}.sst`
//! - WAL segment: `{index:020}.wal`
//!
//! Recovery and scan paths previously claimed files by "parse failure means
//! the file belongs to another subsystem", which silently dropped corrupt or
//! foreign names from replay and durable readback. This module is the single
//! fail-closed authority: every `*.sst` / `*.wal` name must classify into a
//! canonical shape, otherwise the caller receives a typed
//! `CALYX_ASTER_CORRUPT_SHARD` error instead of silent data loss.
//!
//! # Sequence domains and ordering (issues #1132/#1137/#1138)
//!
//! The numbers in these names live in two incomparable domains. Durable
//! batches, compaction outputs, and the `watermark` of commit-anchored flush
//! files carry the vault-wide *commit seq*. Legacy flush files carry only a
//! per-CF *flush ordinal*, which says nothing about commit time. Ordering is
//! therefore epoch-based ([`SstOrderKey`]): legacy flush files form epoch 0,
//! ordered among themselves by ordinal (sound because the ordinal is monotone
//! per CF and a later flush of the same memtable chain always holds
//! newer-or-equal state for every key it shares with an earlier flush), and
//! all commit-domain files form epoch 1, ordered by commit seq. Epoch 0 sorts
//! first, which is sound only while every committed row also has a
//! commit-domain durable home — the invariant enforced fail-closed since
//! #1132/#1139. When a legacy ordinal numerically exceeds a commit-domain seq
//! in the same directory, the pre-#1138 single-domain sort interleaved the
//! two domains arbitrarily (newer durable batches could be shadowed by older
//! flushes); [`ensure_unambiguous_sst_order`] rejects that layout with
//! `CALYX_ASTER_SST_ORDER_AMBIGUOUS` instead of guessing.

use crate::cf::ColumnFamily;
use calyx_core::{CalyxError, Result};
use std::path::Path;

/// Canonical SST file-name classes; each variant names the subsystem that
/// owns files of that shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SstName {
    /// Legacy LSM router memtable flush: `{ordinal:020}.sst`. The number is a
    /// per-CF flush ordinal, NOT a commit seq (issue #1138); only written by
    /// pre-#1138 binaries and by raw `CfRouter` users with no commit domain.
    RouterLegacy { ordinal: u64 },
    /// Commit-anchored LSM router memtable flush:
    /// `flush-{watermark:020}-{ordinal:04}.sst`. `watermark` is the highest
    /// commit seq whose rows can be present in the file (recorded by the
    /// writer at flush time); `ordinal` continues the per-CF flush chain.
    Flush { watermark: u64, ordinal: usize },
    /// Durable group-commit batch (and compaction adoption slots in the
    /// `9000..=9999` index range): `{seq:020}-{index:04}.sst`.
    DurableBatch { seq: u64, index: usize },
    /// Compaction output: `compacted-{seq:020}.sst`.
    Compacted { seq: u64 },
}

/// Canonical chronological order for SST files inside one CF.
///
/// `epoch` separates the two sequence domains: 0 for legacy flush files
/// (ordered by flush ordinal), 1 for commit-domain files (ordered by commit
/// seq). See the module docs for why epoch 0 sorts first and when that is
/// sound. Callers that merge rows newest-wins across a CF's files must gate
/// the file set through [`ensure_unambiguous_sst_order`] first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SstOrderKey {
    pub epoch: u8,
    pub seq: u64,
    pub class_rank: u8,
    pub index: usize,
}

impl SstOrderKey {
    fn from_name(name: SstName) -> Self {
        match name {
            SstName::RouterLegacy { ordinal } => Self {
                epoch: 0,
                seq: ordinal,
                class_rank: 0,
                index: 0,
            },
            SstName::Flush { watermark, ordinal } => Self {
                epoch: 1,
                seq: watermark,
                class_rank: 1,
                index: ordinal,
            },
            SstName::DurableBatch { seq, index } => Self {
                epoch: 1,
                seq,
                class_rank: 2,
                index,
            },
            SstName::Compacted { seq } => Self {
                epoch: 1,
                seq,
                class_rank: 3,
                index: usize::MAX,
            },
        }
    }
}

/// Classifies an SST path. Returns `Ok(None)` for paths without an `sst`
/// extension (foreign files such as locks and dot-temp files are not Aster's
/// to judge), `Ok(Some(_))` for canonical names, and a typed error for any
/// `*.sst` name that matches no canonical writer shape.
pub fn classify_sst(path: &Path) -> Result<Option<SstName>> {
    if path.extension().and_then(|value| value.to_str()) != Some("sst") {
        return Ok(None);
    }
    let stem = path.file_stem().and_then(|value| value.to_str());
    stem.and_then(classify_sst_stem)
        .map(Some)
        .ok_or_else(|| unrecognized_name(path, "SST"))
}

/// Returns the chronological sort key for a canonical SST file.
pub fn sst_order_key(path: &Path) -> Result<Option<SstOrderKey>> {
    Ok(classify_sst(path)?.map(SstOrderKey::from_name))
}

/// Canonical file name for a commit-anchored router flush.
pub fn flush_sst_file_name(watermark: u64, ordinal: usize) -> String {
    format!("flush-{watermark:020}-{ordinal:04}.sst")
}

/// Fails closed when one CF directory holds a legacy flush file whose ordinal
/// numerically exceeds a commit-domain seq in the same set (issue #1138).
///
/// In that layout the legacy file's true commit watermark is unknowable and
/// the pre-#1138 single-domain sort could shadow a newer durable batch behind
/// an older flush, so every ordering consumer must refuse instead of serving
/// a potentially stale merge. Legacy files whose ordinals all sit below every
/// commit-domain seq keep the (unchanged) epoch-0-first order, which is sound
/// under the durable-coverage invariant enforced since #1132/#1139.
/// Commit-anchored `Flush` files never trip this gate: against legacy files
/// they share the per-CF ordinal chain (their ordinals are strictly larger),
/// and against durable batches their watermark is an exact commit seq.
pub fn ensure_unambiguous_sst_order<'a, I>(files: I) -> Result<()>
where
    I: IntoIterator<Item = &'a Path>,
{
    let mut max_legacy: Option<(u64, &Path)> = None;
    let mut min_commit: Option<(u64, &Path)> = None;
    for path in files {
        match classify_sst(path)? {
            Some(SstName::RouterLegacy { ordinal }) => {
                if max_legacy.is_none_or(|(max, _)| ordinal > max) {
                    max_legacy = Some((ordinal, path));
                }
            }
            Some(SstName::DurableBatch { seq, .. } | SstName::Compacted { seq }) => {
                if min_commit.is_none_or(|(min, _)| seq < min) {
                    min_commit = Some((seq, path));
                }
            }
            Some(SstName::Flush { .. }) | None => {}
        }
    }
    let (Some((ordinal, legacy_path)), Some((seq, commit_path))) = (max_legacy, min_commit) else {
        return Ok(());
    };
    if ordinal <= seq {
        return Ok(());
    }
    Err(CalyxError {
        code: "CALYX_ASTER_SST_ORDER_AMBIGUOUS",
        message: format!(
            "legacy router flush {} (per-CF flush ordinal {ordinal}) and commit-domain SST {} \
             (commit seq {seq}) overlap numerically; the flush ordinal is not a commit seq, so \
             newest-wins ordering across these files is undefined and reads could silently \
             return stale rows (issue #1138)",
            legacy_path.display(),
            commit_path.display()
        ),
        remediation: "run the CLI `compact` command on this vault/CF to adopt the legacy flush \
                      files into the commit domain, then retry; do not reorder or delete the \
                      files by hand",
    })
}

/// Returns the WAL segment index for canonical `{index:020}.wal` names,
/// `Ok(None)` for non-`.wal` files, and a typed error for any `*.wal` name
/// that is not canonical (such files would otherwise be silently excluded
/// from replay, losing committed writes).
pub fn wal_segment_index(path: &Path) -> Result<Option<u64>> {
    if path.extension().and_then(|value| value.to_str()) != Some("wal") {
        return Ok(None);
    }
    path.file_stem()
        .and_then(|value| value.to_str())
        .and_then(canonical_seq)
        .map(Some)
        .ok_or_else(|| unrecognized_name(path, "WAL"))
}

/// Parses a `cf/<name>` directory name into its column family, failing closed
/// on unknown or non-canonical names. The parse round-trips through
/// [`ColumnFamily::name`] so a miszero-padded slot directory (which writers
/// would never create) is rejected instead of silently aliasing another CF.
pub fn parse_cf_dir_name(value: &str) -> Result<ColumnFamily> {
    // ColumnFamily owns the durable directory registry. Keeping a second
    // hand-written list here caused newly added, valid CFs to be created by a
    // commit and then rejected by the next router open (#1804). The canonical
    // round-trip below still rejects aliases and malformed slot padding.
    let cf = ColumnFamily::from_name(value).ok_or_else(|| {
        // Not corruption: the directory is well formed, this build simply has no
        // registration for it. Saying "corrupt shard / restore from snapshot"
        // here is what pointed an operator at deleting an intact vault (#1875).
        CalyxError::aster_vault_schema_ahead(format!(
            "durable CF directory {value} is not registered in this build's ColumnFamily registry; \
             the vault was written by a build that knows this column family"
        ))
    })?;
    if cf.name() != value {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "non-canonical CF directory {value} (canonical form is {})",
            cf.name()
        )));
    }
    Ok(cf)
}

fn classify_sst_stem(stem: &str) -> Option<SstName> {
    if let Some(seq_text) = stem.strip_prefix("compacted-") {
        return Some(SstName::Compacted {
            seq: canonical_seq(seq_text)?,
        });
    }
    if let Some(rest) = stem.strip_prefix("flush-") {
        let (watermark_text, ordinal_text) = rest.split_once('-')?;
        return Some(SstName::Flush {
            watermark: canonical_seq(watermark_text)?,
            ordinal: canonical_index(ordinal_text)?,
        });
    }
    if let Some((seq_text, index_text)) = stem.split_once('-') {
        return Some(SstName::DurableBatch {
            seq: canonical_seq(seq_text)?,
            index: canonical_index(index_text)?,
        });
    }
    Some(SstName::RouterLegacy {
        ordinal: canonical_seq(stem)?,
    })
}

/// Accepts exactly the output of `format!("{seq:020}")` for a `u64`.
fn canonical_seq(text: &str) -> Option<u64> {
    if text.len() != 20 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // A 20-digit string can still exceed u64::MAX; parse failure rejects it.
    text.parse().ok()
}

/// Accepts exactly the output of `format!("{index:04}")` for a `usize`.
fn canonical_index(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let index: usize = text.parse().ok()?;
    if format!("{index:04}") != text {
        return None;
    }
    Some(index)
}

fn unrecognized_name(path: &Path, kind: &str) -> CalyxError {
    CalyxError::aster_corrupt_shard(format!(
        "unrecognized {kind} file name {}: not a canonical Aster storage name; \
         refusing to silently skip it during recovery/scan",
        path.display()
    ))
}
