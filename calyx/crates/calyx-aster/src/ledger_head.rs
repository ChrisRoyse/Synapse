use std::fs;
use std::path::{Path, PathBuf};

use calyx_core::{CalyxError, Result};
use calyx_ledger::{CheckpointPayload, EntryKind, LedgerHeadAnchor, LedgerRow, decode};
use serde::{Deserialize, Serialize};

use crate::cf::ColumnFamily;
use crate::ledger_projection::{ProjectionKind, ProjectionRecord, ProjectionSlot};
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

/// Reads one derived projection's payload bytes, or `None` when no anchor is
/// published.
///
/// `None` covers two physically distinct states that are semantically one: the
/// file does not exist, and the file holds a validly-published *vacant* record.
/// Recovery produces the second when the durable Ledger rows carry no anchor,
/// so that publishing an absence never has to unlink a file (#1947).
///
/// A file that predates the fixed-size record format is the bare JSON payload.
/// That is decoded as such, once, and logged — an explicit versioned format
/// transition, not a silent acceptance of unknown bytes. The next publish
/// rewrites it as a record.
fn read_projection_payload(kind: ProjectionKind, path: &Path) -> Result<Option<Vec<u8>>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CalyxError::disk_pressure(format!(
                "read {kind:?} projection {}: {error}",
                path.display()
            )));
        }
    };
    match crate::ledger_projection::classify_projection_bytes(&bytes) {
        crate::ledger_projection::ProjectionShape::Record => {}
        crate::ledger_projection::ProjectionShape::LegacyBareJson => {
            tracing::info!(
                code = "CALYX_ASTER_LEDGER_PROJECTION_LEGACY_FORMAT_READ",
                path = %path.display(),
                bytes = bytes.len(),
                projection = ?kind,
                "read a derived Ledger projection in the pre-record bare-JSON format; the next publish rewrites it as a fixed-size record"
            );
            return Ok(Some(bytes));
        }
        crate::ledger_projection::ProjectionShape::Unrecognized => {
            // Not a record and not the format that preceded it. Say exactly
            // that, with the leading bytes, rather than attempting either
            // decode and reporting whichever one fails first.
            return Err(derived_projection_unreadable(format!(
                "{kind:?} projection {} is in no recognized format: {} bytes beginning {:02x?}; \
                 expected either a fixed-size record magic or a bare JSON object",
                path.display(),
                bytes.len(),
                &bytes[..bytes.len().min(8)]
            )));
        }
    }
    // Not `ledger_corrupt`: this file is published without a durability
    // barrier because it is derived from Ledger rows the WAL already fsync'd
    // (#1946), and in place rather than via rename (#1947). Either shape can
    // leave a torn or partially-applied record after a crash. That says
    // nothing about the ledger — it says this projection must be rebuilt from
    // it, which is exactly what the CRC in the record exists to detect.
    match crate::ledger_projection::decode_record(kind, &bytes).map_err(|reason| {
        derived_projection_unreadable(format!(
            "decode {kind:?} projection record {}: {reason}",
            path.display()
        ))
    })? {
        ProjectionRecord::Vacant => Ok(None),
        ProjectionRecord::Present(payload) => Ok(Some(payload)),
    }
}

pub fn read_head_anchor(vault: &Path) -> Result<Option<LedgerHeadAnchor>> {
    let path = head_anchor_path(vault);
    let Some(payload) = read_projection_payload(ProjectionKind::LedgerHead, &path)? else {
        return Ok(None);
    };
    serde_json::from_slice(&payload).map(Some).map_err(|error| {
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

/// The append-only rule for the head projection, shared by every writer so the
/// cold path and the cached commit path cannot enforce different invariants.
fn guard_head_monotonic(
    current: Option<&LedgerHeadAnchor>,
    anchor: &LedgerHeadAnchor,
) -> Result<()> {
    let Some(current) = current else {
        return Ok(());
    };
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
    Ok(())
}

fn encode_head_anchor(anchor: &LedgerHeadAnchor) -> Result<Vec<u8>> {
    serde_json::to_vec(anchor)
        .map_err(|error| CalyxError::ledger_corrupt(format!("encode Aster ledger head: {error}")))
}

pub(crate) fn write_head_anchor(vault: &Path, anchor: &LedgerHeadAnchor) -> Result<()> {
    guard_head_monotonic(read_head_anchor(vault)?.as_ref(), anchor)?;
    let bytes = encode_head_anchor(anchor)?;
    // Published WITHOUT a durability barrier, on purpose (#1946), and in place
    // into a pre-allocated record rather than via create+rename (#1947).
    //
    // This runs on the commit path immediately after `durable.append_batch`
    // has already fsync'd the Ledger rows this anchor is computed from, so the
    // anchor's content is durable before this call begins. fsync'ing it again
    // cannot make it more recoverable than the rows it is derived from — it
    // can only make the commit slower, and it did: measured on the deployment
    // host this publish was 45.9% of all durable-commit cost (7,598 ms of
    // 16,542 ms across 1,105 commits) while performing no I/O the WAL had not
    // already performed. #1947 then measured that what remained was namespace
    // metadata: create+rename cost 1.344 ms against 0.130 ms in place.
    //
    // The projection can therefore be stale or torn after a crash. Both are
    // handled by `ensure_recovered_ledger_sidecars`, which re-derives the head
    // from physical WAL truth at open — the path that already existed, and
    // whose own comment states that "recovery is the authority".
    ProjectionSlot::new(ProjectionKind::LedgerHead, head_anchor_path(vault))
        .publish(Some(&bytes), false)?;
    Ok(())
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
///
/// A recovered *absence* is published as a vacant record rather than by
/// unlinking the file (#1947). Both read back as `None`, and not unlinking
/// keeps the guarantee that no publish after the first mutates the namespace —
/// which also means this can no longer pull a file out from under a writer's
/// open handle.
pub(crate) fn replace_head_anchor_from_recovery(
    vault: &Path,
    anchor: Option<&LedgerHeadAnchor>,
) -> Result<()> {
    let bytes = anchor.map(encode_head_anchor).transpose()?;
    ProjectionSlot::new(ProjectionKind::LedgerHead, head_anchor_path(vault))
        .publish(bytes.as_deref(), true)?;
    Ok(())
}

pub(crate) fn read_checkpoint_anchor(vault: &Path) -> Result<Option<LedgerCheckpointAnchor>> {
    let path = checkpoint_anchor_path(vault);
    let Some(payload) = read_projection_payload(ProjectionKind::LedgerCheckpoint, &path)? else {
        return Ok(None);
    };
    // Same reasoning as `read_head_anchor`: this is a derived projection, so a
    // decode failure means "rebuild me from the WAL", not "the ledger is
    // damaged" (#1946). `validate()` is routed the same way because a torn
    // write can also produce structurally-decodable but internally
    // inconsistent bytes.
    let anchor = serde_json::from_slice::<LedgerCheckpointAnchor>(&payload).map_err(|error| {
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

/// The append-only rule for the checkpoint projection.
///
/// `Ok(false)` means the requested pointer is already published and the write
/// must be skipped, which is distinct from "publish it" and is why this
/// returns a verdict rather than just validating.
fn guard_checkpoint_monotonic(
    current: Option<&LedgerCheckpointAnchor>,
    anchor: &LedgerCheckpointAnchor,
) -> Result<bool> {
    let Some(current) = current else {
        return Ok(true);
    };
    if anchor.seq < current.seq {
        return Err(CalyxError::ledger_append_only_violation(format!(
            "Aster ledger checkpoint pointer regressed from {} to {}",
            current.seq, anchor.seq
        )));
    }
    if anchor.seq == current.seq {
        if anchor != current {
            return Err(CalyxError::ledger_append_only_violation(
                "Aster ledger checkpoint pointer changed at the same seq",
            ));
        }
        return Ok(false);
    }
    Ok(true)
}

fn encode_checkpoint_anchor(anchor: &LedgerCheckpointAnchor) -> Result<Vec<u8>> {
    serde_json::to_vec(anchor).map_err(|error| {
        CalyxError::ledger_corrupt(format!("encode Aster ledger checkpoint pointer: {error}"))
    })
}

pub(crate) fn write_checkpoint_anchor(vault: &Path, anchor: &LedgerCheckpointAnchor) -> Result<()> {
    anchor.validate()?;
    if !guard_checkpoint_monotonic(read_checkpoint_anchor(vault)?.as_ref(), anchor)? {
        return Ok(());
    }
    let bytes = encode_checkpoint_anchor(anchor)?;
    // Derived projection, same contract as the head anchor above (#1946,
    // #1947).
    ProjectionSlot::new(
        ProjectionKind::LedgerCheckpoint,
        checkpoint_anchor_path(vault),
    )
    .publish(Some(&bytes), false)?;
    Ok(())
}

/// Replaces the derived checkpoint sidecar with exact recovered physical
/// truth, publishing a vacant record when no physical checkpoint exists.
pub(crate) fn replace_checkpoint_anchor_from_recovery(
    vault: &Path,
    anchor: Option<&LedgerCheckpointAnchor>,
) -> Result<()> {
    let bytes = anchor
        .map(|anchor| {
            anchor.validate()?;
            encode_checkpoint_anchor(anchor)
        })
        .transpose()?;
    ProjectionSlot::new(
        ProjectionKind::LedgerCheckpoint,
        checkpoint_anchor_path(vault),
    )
    .publish(bytes.as_deref(), true)?;
    Ok(())
}

/// The commit path's owner of both derived Ledger projections (#1947).
///
/// Two costs the free functions above pay per call are structural rather than
/// incidental, and this type removes both:
///
/// 1. **An open per publish.** The backing file is pre-allocated once and the
///    handle is held, so a steady-state publish is one positional write and
///    nothing else.
/// 2. **A read per publish.** The append-only guards needed the currently
///    published anchor, and read it back off disk every time — an open, a
///    read, and a JSON parse to validate a value this process itself wrote.
///    The last published anchor is cached instead.
///
/// The cache is an accelerator for a guard, never the guard itself. A cached
/// value can only ever *skip* a disk read; it can never be the sole reason a
/// commit is rejected, because any violation judged against the cache is
/// re-judged against physical truth before it is returned.
#[derive(Debug)]
pub(crate) struct LedgerProjections {
    vault: PathBuf,
    head: ProjectionSlot,
    /// `None` = not yet loaded from disk. `Some(None)` = loaded, no anchor
    /// published. The two are different states and collapsing them would make
    /// an unloaded cache read as a published absence.
    head_cache: Option<Option<LedgerHeadAnchor>>,
    checkpoint: ProjectionSlot,
    checkpoint_cache: Option<Option<LedgerCheckpointAnchor>>,
}

impl LedgerProjections {
    pub(crate) fn new(vault: &Path) -> Self {
        Self {
            vault: vault.to_path_buf(),
            head: ProjectionSlot::new(ProjectionKind::LedgerHead, head_anchor_path(vault)),
            head_cache: None,
            checkpoint: ProjectionSlot::new(
                ProjectionKind::LedgerCheckpoint,
                checkpoint_anchor_path(vault),
            ),
            checkpoint_cache: None,
        }
    }

    /// Drops both handles and both caches.
    ///
    /// Runtime Ledger reconciliation republishes these files from recovered
    /// physical truth through the free functions, which do not go through this
    /// type. After that happens the cache describes a superseded state and the
    /// handles may refer to a file that was rewritten underneath them, so both
    /// are discarded at that boundary rather than being trusted across it.
    pub(crate) fn reset(&mut self) {
        self.head.reset();
        self.head_cache = None;
        self.checkpoint.reset();
        self.checkpoint_cache = None;
    }

    pub(crate) fn publish_head(
        &mut self,
        anchor: &LedgerHeadAnchor,
    ) -> Result<crate::ledger_projection::PublishTimings> {
        if self.head_cache.is_none() {
            self.head_cache = Some(read_head_anchor(&self.vault)?);
        }
        let cached = self.head_cache.as_ref().and_then(Option::as_ref);
        if guard_head_monotonic(cached, anchor).is_err() {
            // Judge the violation against the disk, not against the cache. If
            // physical truth accepts the write, the cache was stale and the
            // commit proceeds; if it rejects, the real violation is returned
            // carrying the real published value.
            let physical = read_head_anchor(&self.vault)?;
            guard_head_monotonic(physical.as_ref(), anchor)?;
            tracing::warn!(
                code = "CALYX_ASTER_LEDGER_HEAD_PROJECTION_CACHE_RESYNCED",
                path = %self.head.path().display(),
                cached_height = cached.map(|anchor| anchor.height),
                physical_height = physical.as_ref().map(|anchor| anchor.height),
                requested_height = anchor.height,
                "cached Ledger head projection disagreed with disk; resynced from physical truth before publishing"
            );
            self.head_cache = Some(physical);
        }
        let bytes = encode_head_anchor(anchor)?;
        let timings = self.head.publish(Some(&bytes), false)?;
        self.head_cache = Some(Some(anchor.clone()));
        Ok(timings)
    }

    /// Publishes the checkpoint pointer, or `Ok(None)` when the requested
    /// pointer is already the published one.
    pub(crate) fn publish_checkpoint(
        &mut self,
        anchor: &LedgerCheckpointAnchor,
    ) -> Result<Option<crate::ledger_projection::PublishTimings>> {
        anchor.validate()?;
        if self.checkpoint_cache.is_none() {
            self.checkpoint_cache = Some(read_checkpoint_anchor(&self.vault)?);
        }
        let cached = self.checkpoint_cache.as_ref().and_then(Option::as_ref);
        let publish = match guard_checkpoint_monotonic(cached, anchor) {
            Ok(publish) => publish,
            Err(_) => {
                let physical = read_checkpoint_anchor(&self.vault)?;
                let verdict = guard_checkpoint_monotonic(physical.as_ref(), anchor)?;
                tracing::warn!(
                    code = "CALYX_ASTER_LEDGER_CHECKPOINT_PROJECTION_CACHE_RESYNCED",
                    path = %self.checkpoint.path().display(),
                    cached_seq = cached.map(|anchor| anchor.seq),
                    physical_seq = physical.as_ref().map(|anchor| anchor.seq),
                    requested_seq = anchor.seq,
                    "cached Ledger checkpoint projection disagreed with disk; resynced from physical truth before publishing"
                );
                self.checkpoint_cache = Some(physical);
                verdict
            }
        };
        if !publish {
            return Ok(None);
        }
        let bytes = encode_checkpoint_anchor(anchor)?;
        let timings = self.checkpoint.publish(Some(&bytes), false)?;
        self.checkpoint_cache = Some(Some(anchor.clone()));
        Ok(Some(timings))
    }
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
