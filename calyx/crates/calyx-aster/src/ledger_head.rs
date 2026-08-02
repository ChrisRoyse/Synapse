use std::fs;
use std::path::{Path, PathBuf};

use calyx_core::{CalyxError, Result};
use calyx_ledger::{CheckpointPayload, EntryKind, LedgerHeadAnchor, LedgerRow, decode};
use serde::{Deserialize, Serialize};

use crate::cf::ColumnFamily;
use crate::ledger_view::parse_aster_ledger_seq;
use crate::vault::encode::WriteRow;

const LEDGER_HEAD_DIR: &str = "ledger_head";
const LEDGER_HEAD_FILE: &str = "current.json";
const LEDGER_CHECKPOINT_FILE: &str = "latest_checkpoint.json";

/// A derived Ledger projection on disk could not be decoded (#1946).
///
/// Distinct from `CALYX_LEDGER_CORRUPT` on purpose. These two files are
/// published without a durability barrier because their content is derived
/// from Ledger rows the WAL has already fsync'd, so a crash can leave the
/// rename applied over unflushed blocks. That is a statement about a
/// regenerable projection, never about the ledger it projects — conflating the
/// two would report a repairable accelerator as ledger damage and send an
/// operator to restore a vault that is intact.
pub const CALYX_LEDGER_DERIVED_PROJECTION_UNREADABLE: &str =
    "CALYX_LEDGER_DERIVED_PROJECTION_UNREADABLE";

const DERIVED_PROJECTION_REMEDIATION: &str = "this file is a regenerable projection of the durable Ledger rows, not an authority: open \
     the vault write-capable so recovery rebuilds it from the WAL. It is not evidence that the \
     ledger is damaged — run verify_chain to assess that";

fn derived_projection_unreadable(message: String) -> CalyxError {
    CalyxError {
        code: CALYX_LEDGER_DERIVED_PROJECTION_UNREADABLE,
        message,
        remediation: DERIVED_PROJECTION_REMEDIATION,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerCheckpointAnchor {
    pub seq: u64,
    pub entry_hash: [u8; 32],
    pub range_start: u64,
    pub range_end: u64,
}

impl LedgerCheckpointAnchor {
    pub(crate) fn from_checkpoint_payload(
        seq: u64,
        entry_hash: [u8; 32],
        payload: &CheckpointPayload,
    ) -> Result<Self> {
        let anchor = Self {
            seq,
            entry_hash,
            range_start: payload.range_start,
            range_end: payload.range_end,
        };
        anchor.validate()?;
        Ok(anchor)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.range_start > self.range_end {
            return Err(CalyxError::ledger_corrupt(format!(
                "Aster ledger checkpoint pointer range {}..{} is invalid",
                self.range_start, self.range_end
            )));
        }
        if self.seq != self.range_end {
            return Err(CalyxError::ledger_corrupt(format!(
                "Aster ledger checkpoint pointer seq {} does not match checkpoint range_end {}",
                self.seq, self.range_end
            )));
        }
        Ok(())
    }
}

pub fn head_anchor_path(vault: &Path) -> PathBuf {
    vault.join(LEDGER_HEAD_DIR).join(LEDGER_HEAD_FILE)
}

pub(crate) fn checkpoint_anchor_path(vault: &Path) -> PathBuf {
    vault.join(LEDGER_HEAD_DIR).join(LEDGER_CHECKPOINT_FILE)
}

pub fn read_head_anchor(vault: &Path) -> Result<Option<LedgerHeadAnchor>> {
    let path = head_anchor_path(vault);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)
        .map_err(|error| CalyxError::disk_pressure(format!("read Aster ledger head: {error}")))?;
    // Not `ledger_corrupt`: this file is published without a durability
    // barrier because it is derived from Ledger rows the WAL already fsync'd
    // (#1946). A crash can therefore leave the rename applied over unflushed
    // blocks, i.e. a torn or zero-filled projection. That says nothing about
    // the ledger — it says this projection must be rebuilt from it.
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        derived_projection_unreadable(format!(
            "decode Aster ledger head projection {}: {error}",
            path.display()
        ))
    })
}

/// Reads the head projection for a caller that can rebuild it from the durable
/// Ledger rows, mapping an unreadable projection to `None`.
///
/// This is not a fallback that hides a failure: every caller of this function
/// reaches a rebuild-from-WAL path on `None`, and the discarded projection is
/// reported with its exact decode error before it is discarded.
pub(crate) fn read_head_anchor_rebuildable(vault: &Path) -> Result<Option<LedgerHeadAnchor>> {
    match read_head_anchor(vault) {
        Err(error) if error.code == CALYX_LEDGER_DERIVED_PROJECTION_UNREADABLE => {
            tracing::warn!(
                code = "CALYX_ASTER_LEDGER_HEAD_PROJECTION_UNREADABLE",
                vault_dir = %vault.display(),
                path = %head_anchor_path(vault).display(),
                decode_error = %error.message,
                "derived Ledger head projection is unreadable; rebuilding it from durable Ledger rows"
            );
            Ok(None)
        }
        other => other,
    }
}

/// Checkpoint-projection counterpart of [`read_head_anchor_rebuildable`].
pub(crate) fn read_checkpoint_anchor_rebuildable(
    vault: &Path,
) -> Result<Option<LedgerCheckpointAnchor>> {
    match read_checkpoint_anchor(vault) {
        Err(error) if error.code == CALYX_LEDGER_DERIVED_PROJECTION_UNREADABLE => {
            tracing::warn!(
                code = "CALYX_ASTER_LEDGER_CHECKPOINT_PROJECTION_UNREADABLE",
                vault_dir = %vault.display(),
                path = %checkpoint_anchor_path(vault).display(),
                decode_error = %error.message,
                "derived Ledger checkpoint projection is unreadable; rebuilding it from durable Ledger rows"
            );
            Ok(None)
        }
        other => other,
    }
}

pub(crate) fn write_head_anchor(vault: &Path, anchor: &LedgerHeadAnchor) -> Result<()> {
    if let Some(current) = read_head_anchor(vault)? {
        if anchor.height < current.height {
            return Err(CalyxError::ledger_append_only_violation(format!(
                "Aster ledger head regressed from {} to {}",
                current.height, anchor.height
            )));
        }
        if anchor.height == current.height && anchor.tip_hash != current.tip_hash {
            return Err(CalyxError::ledger_append_only_violation(
                "Aster ledger head changed hash at the same height",
            ));
        }
    }
    let path = head_anchor_path(vault);
    let bytes = serde_json::to_vec(anchor).map_err(|error| {
        CalyxError::ledger_corrupt(format!("encode Aster ledger head: {error}"))
    })?;
    // Published WITHOUT a durability barrier, on purpose (#1946).
    //
    // This runs on the commit path immediately after `durable.append_batch`
    // has already fsync'd the Ledger rows this anchor is computed from, so the
    // anchor's content is durable before this call begins. fsync'ing it again
    // cannot make it more recoverable than the rows it is derived from — it
    // can only make the commit slower, and it did: measured on the deployment
    // host this publish was 45.9% of all durable-commit cost (7,598 ms of
    // 16,542 ms across 1,105 commits) while performing no I/O the WAL had not
    // already performed.
    //
    // The projection can therefore be stale or torn after a crash. Both are
    // handled by `ensure_recovered_ledger_sidecars`, which re-derives the head
    // from physical WAL truth at open — the path that already existed, and
    // whose own comment states that "recovery is the authority".
    crate::fsync::write_atomic_replace_derived(&path, &bytes, "Aster ledger head")
}

/// Replaces the derived head sidecar with exact recovered physical truth.
///
/// This deliberately bypasses append-only sidecar checks: recovery is the
/// authority used to repair a stale-ahead or otherwise divergent accelerator.
///
/// Unlike the commit-path publish, this one stays fully durable. It runs once
/// per open rather than once per commit, so it costs nothing measurable, and
/// leaving the repaired projection durable means the next crash starts from a
/// projection that is known-good on disk rather than one that has yet to be
/// flushed (#1946).
pub(crate) fn replace_head_anchor_from_recovery(
    vault: &Path,
    anchor: Option<&LedgerHeadAnchor>,
) -> Result<()> {
    let path = head_anchor_path(vault);
    let Some(anchor) = anchor else {
        return crate::fsync::remove_file_durable(&path, "Aster recovered ledger head");
    };
    let bytes = serde_json::to_vec(anchor).map_err(|error| {
        CalyxError::ledger_corrupt(format!("encode recovered Aster ledger head: {error}"))
    })?;
    crate::fsync::write_atomic_replace(&path, &bytes, "Aster recovered ledger head")
}

pub(crate) fn read_checkpoint_anchor(vault: &Path) -> Result<Option<LedgerCheckpointAnchor>> {
    let path = checkpoint_anchor_path(vault);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|error| {
        CalyxError::disk_pressure(format!("read Aster ledger checkpoint pointer: {error}"))
    })?;
    // Same reasoning as `read_head_anchor`: this is a derived projection, so a
    // decode failure means "rebuild me from the WAL", not "the ledger is
    // damaged" (#1946). `validate()` is routed the same way because a torn
    // write can also produce structurally-decodable but internally
    // inconsistent bytes.
    let anchor = serde_json::from_slice::<LedgerCheckpointAnchor>(&bytes).map_err(|error| {
        derived_projection_unreadable(format!(
            "decode Aster ledger checkpoint projection {}: {error}",
            path.display()
        ))
    })?;
    anchor.validate().map_err(|error| {
        derived_projection_unreadable(format!(
            "validate Aster ledger checkpoint projection {}: {}",
            path.display(),
            error.message
        ))
    })?;
    Ok(Some(anchor))
}

pub(crate) fn write_checkpoint_anchor(vault: &Path, anchor: &LedgerCheckpointAnchor) -> Result<()> {
    anchor.validate()?;
    if let Some(current) = read_checkpoint_anchor(vault)? {
        if anchor.seq < current.seq {
            return Err(CalyxError::ledger_append_only_violation(format!(
                "Aster ledger checkpoint pointer regressed from {} to {}",
                current.seq, anchor.seq
            )));
        }
        if anchor.seq == current.seq {
            if anchor != &current {
                return Err(CalyxError::ledger_append_only_violation(
                    "Aster ledger checkpoint pointer changed at the same seq",
                ));
            }
            return Ok(());
        }
    }
    let path = checkpoint_anchor_path(vault);
    let bytes = serde_json::to_vec(anchor).map_err(|error| {
        CalyxError::ledger_corrupt(format!("encode Aster ledger checkpoint pointer: {error}"))
    })?;
    // Derived projection, same contract as the head anchor above (#1946).
    crate::fsync::write_atomic_replace_derived(&path, &bytes, "Aster ledger checkpoint pointer")
}

/// Replaces the derived checkpoint sidecar with exact recovered physical
/// truth, including removal when no physical checkpoint exists.
pub(crate) fn replace_checkpoint_anchor_from_recovery(
    vault: &Path,
    anchor: Option<&LedgerCheckpointAnchor>,
) -> Result<()> {
    let path = checkpoint_anchor_path(vault);
    let Some(anchor) = anchor else {
        return crate::fsync::remove_file_durable(
            &path,
            "Aster recovered ledger checkpoint pointer",
        );
    };
    anchor.validate()?;
    let bytes = serde_json::to_vec(anchor).map_err(|error| {
        CalyxError::ledger_corrupt(format!(
            "encode recovered Aster ledger checkpoint pointer: {error}"
        ))
    })?;
    crate::fsync::write_atomic_replace(&path, &bytes, "Aster recovered ledger checkpoint pointer")
}

pub(crate) fn newest_anchor_from_rows(rows: &[WriteRow]) -> Result<Option<LedgerHeadAnchor>> {
    let mut newest = None;
    for row in rows.iter().filter(|row| row.cf == ColumnFamily::Ledger) {
        let key_seq = parse_aster_ledger_seq(&row.key)?;
        let entry = decode(&row.value)?;
        if key_seq != entry.seq {
            return Err(CalyxError::ledger_corrupt(format!(
                "Aster ledger key seq {key_seq} does not match entry seq {}",
                entry.seq
            )));
        }
        if newest.as_ref().is_none_or(|(seq, _hash)| entry.seq > *seq) {
            newest = Some((entry.seq, entry.entry_hash));
        }
    }
    newest
        .map(|(seq, hash)| {
            let height = seq
                .checked_add(1)
                .ok_or_else(|| CalyxError::ledger_corrupt("Aster ledger head overflow"))?;
            LedgerHeadAnchor::new(height, hash)
        })
        .transpose()
}

pub(crate) fn newest_checkpoint_from_rows(
    rows: &[WriteRow],
) -> Result<Option<LedgerCheckpointAnchor>> {
    let mut newest = None;
    for row in rows.iter().filter(|row| row.cf == ColumnFamily::Ledger) {
        let key_seq = parse_aster_ledger_seq(&row.key)?;
        let entry = decode(&row.value)?;
        if key_seq != entry.seq {
            return Err(CalyxError::ledger_corrupt(format!(
                "Aster ledger key seq {key_seq} does not match entry seq {}",
                entry.seq
            )));
        }
        if entry.kind != EntryKind::Admin {
            continue;
        }
        let Some(payload) = CheckpointPayload::decode_optional(&entry.payload)? else {
            continue;
        };
        let checkpoint =
            LedgerCheckpointAnchor::from_checkpoint_payload(entry.seq, entry.entry_hash, &payload)?;
        if newest
            .as_ref()
            .is_none_or(|current: &LedgerCheckpointAnchor| checkpoint.seq > current.seq)
        {
            newest = Some(checkpoint);
        }
    }
    Ok(newest)
}

pub(crate) fn checkpoint_anchor_from_row(
    row: &LedgerRow,
) -> Result<Option<LedgerCheckpointAnchor>> {
    let entry = decode(&row.bytes)?;
    if row.seq != entry.seq {
        return Err(CalyxError::ledger_corrupt(format!(
            "Aster ledger checkpoint row key seq {} does not match entry seq {}",
            row.seq, entry.seq
        )));
    }
    if entry.kind != EntryKind::Admin {
        return Ok(None);
    }
    let Some(payload) = CheckpointPayload::decode_optional(&entry.payload)? else {
        return Ok(None);
    };
    LedgerCheckpointAnchor::from_checkpoint_payload(entry.seq, entry.entry_hash, &payload).map(Some)
}

pub(crate) fn require_head_anchor_for_rows(
    vault: &Path,
    anchor: Option<LedgerHeadAnchor>,
    rows: &[LedgerRow],
) -> Result<Option<LedgerHeadAnchor>> {
    if anchor.is_none()
        && let Some(head) = rows.last().map(|row| row.seq.saturating_add(1))
    {
        return Err(missing_head_anchor(vault, head));
    }
    Ok(anchor)
}

pub(crate) fn missing_head_anchor(vault: &Path, head: u64) -> CalyxError {
    CalyxError::ledger_chain_broken(format!(
        "Aster ledger head anchor missing for non-empty durable ledger at head {head} in {}",
        vault.display()
    ))
}
