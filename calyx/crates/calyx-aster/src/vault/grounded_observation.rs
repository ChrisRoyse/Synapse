use std::{collections::BTreeSet, time::Instant};

use calyx_core::{Anchor, CalyxError, Clock, Constellation, LedgerRef, Result, VaultStore};
use calyx_ledger::{ActorId, EntryKind, SubjectId};

use super::{AsterVault, PutDisposition, encode, ledger_hook, prepared};
use crate::cf::ColumnFamily;

/// A revision changed between projection preparation and the atomic grounded
/// publication. No source, constellation, anchor, ledger, WAL, or MVCC row was
/// committed.
pub const CALYX_ASTER_GROUNDED_OBSERVATION_REVISION_CONFLICT: &str =
    "CALYX_ASTER_GROUNDED_OBSERVATION_REVISION_CONFLICT";

/// Readback from one atomic source-row + grounded-observation publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroundedObservationCommit {
    pub cx_id: calyx_core::CxId,
    pub disposition: PutDisposition,
    pub ledger_ref: LedgerRef,
    pub source_row_count: usize,
    pub committed_seq: u64,
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Atomically publishes append-only source rows and their native grounded
    /// observation. Source, Base/Slot/Scalar/Anchor, and grounding-ledger rows
    /// share one WAL/MVCC commit.
    pub fn put_grounded_observation_with_source_rows(
        &self,
        source_rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)>,
        content_addressed_source_identity: Vec<u8>,
        constellation: Constellation,
        anchor: Anchor,
        ledger_payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<GroundedObservationCommit> {
        self.put_grounded_observation_with_source_rows_inner(
            source_rows,
            None,
            content_addressed_source_identity,
            constellation,
            anchor,
            ledger_payload,
            actor,
        )
    }

    /// Atomically publishes revision-guarded source rows and their native
    /// grounded observation.
    ///
    /// Every source row must have exactly one guard. Comparison, source rows,
    /// Base/Slot/Scalar/Anchor rows, provenance ledger entry, and the WAL/MVCC
    /// sequence share one durable commit lock. A conflict fails before any
    /// mutation with
    /// [`CALYX_ASTER_GROUNDED_OBSERVATION_REVISION_CONFLICT`].
    ///
    /// # Errors
    ///
    /// Returns a structured error for malformed or mismatching guards, invalid
    /// source/constellation/anchor data, or a ledger/WAL/MVCC durability
    /// failure.
    #[allow(clippy::too_many_arguments)]
    pub fn put_guarded_grounded_observation_with_source_rows(
        &self,
        source_rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)>,
        source_guards: Vec<super::CfRevisionGuard>,
        content_addressed_source_identity: Vec<u8>,
        constellation: Constellation,
        anchor: Anchor,
        ledger_payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<GroundedObservationCommit> {
        self.put_grounded_observation_with_source_rows_inner(
            source_rows,
            Some(source_guards),
            content_addressed_source_identity,
            constellation,
            anchor,
            ledger_payload,
            actor,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn put_grounded_observation_with_source_rows_inner(
        &self,
        source_rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)>,
        source_guards: Option<Vec<super::CfRevisionGuard>>,
        content_addressed_source_identity: Vec<u8>,
        mut constellation: Constellation,
        anchor: Anchor,
        ledger_payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<GroundedObservationCommit> {
        if source_rows.is_empty() {
            return Err(CalyxError::aster_corrupt_shard(
                "grounded observation publication requires at least one source row",
            ));
        }
        if constellation.vault_id != self.vault_id() {
            return Err(CalyxError::vault_access_denied(
                "grounded observation belongs to another vault",
            ));
        }
        if !constellation.anchors.is_empty() || !constellation.flags.ungrounded {
            return Err(CalyxError::aster_corrupt_shard(
                "grounded observation input must be ungrounded with no pre-attached anchors",
            ));
        }
        if content_addressed_source_identity.is_empty() {
            return Err(CalyxError::aster_corrupt_shard(
                "grounded observation content-addressed source identity must not be empty",
            ));
        }
        let expected_cx_id = self.cx_id_for_input(
            &content_addressed_source_identity,
            constellation.panel_version,
        );
        if constellation.cx_id != expected_cx_id {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "grounded observation constellation id is not derived from the supplied source identity: expected={} actual={} panel_version={}",
                expected_cx_id, constellation.cx_id, constellation.panel_version
            )));
        }
        anchor.validate_schema()?;
        constellation.anchors.push(anchor);
        constellation.flags.ungrounded = false;
        constellation.validate_schema()?;
        let prepared = prepared::PreparedConstellationEncoding::new(&constellation)?;

        let mut source_keys = BTreeSet::new();
        for (cf, key, _value) in &source_rows {
            if *cf != ColumnFamily::Kv {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "grounded observation source row must use kv CF, got {}",
                    cf.name()
                )));
            }
            if key.is_empty() {
                return Err(CalyxError::aster_corrupt_shard(
                    "grounded observation source row key must not be empty",
                ));
            }
            if !source_keys.insert(key.clone()) {
                return Err(CalyxError::aster_corrupt_shard(
                    "grounded observation source rows contain a duplicate key",
                ));
            }
        }

        if let Some(guards) = source_guards.as_ref() {
            let mut guard_keys = BTreeSet::new();
            for guard in guards {
                if guard.cf != ColumnFamily::Kv || guard.key.is_empty() {
                    return Err(CalyxError::aster_corrupt_shard(
                        "guarded grounded observation guards must name non-empty KV keys",
                    ));
                }
                if !guard_keys.insert(guard.key.clone()) {
                    return Err(CalyxError::aster_corrupt_shard(
                        "guarded grounded observation contains a duplicate guard key",
                    ));
                }
            }
            if guard_keys != source_keys {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "guarded grounded observation requires exactly one guard per source row: source_rows={} guards={} matching_keys={}",
                    source_keys.len(),
                    guard_keys.len(),
                    source_keys.intersection(&guard_keys).count()
                )));
            }
        }

        let publication_started = Instant::now();
        let commit = self.with_durable_commit_lock(move || {
            let latest = self.snapshot();
            let source_precondition_started = Instant::now();
            if let Some(guards) = source_guards.as_ref() {
                for (guard_index, guard) in guards.iter().enumerate() {
                    let actual_revision = self
                        .read_cf_at(latest, guard.cf, &guard.key)?
                        .as_deref()
                        .map(super::value_revision);
                    if actual_revision != guard.expected_revision {
                        return Err(CalyxError {
                            code: CALYX_ASTER_GROUNDED_OBSERVATION_REVISION_CONFLICT,
                            message: format!(
                                "grounded observation source revision changed before publication: guard_index={guard_index} key_len={} expected={:?} actual={actual_revision:?}",
                                guard.key.len(), guard.expected_revision
                            ),
                            remediation: "reread every guarded source revision and rebuild the complete grounded publication; no row from this attempt was committed",
                        });
                    }
                }
            } else {
                for (_cf, key, _value) in &source_rows {
                    if self.read_cf_at(latest, ColumnFamily::Kv, key)?.is_some() {
                        return Err(CalyxError::ledger_append_only_violation(format!(
                            "grounded observation source row already exists: key_len={}",
                            key.len()
                        )));
                    }
                }
            }
            let source_precondition_us = elapsed_us(source_precondition_started);

            let mut rows = source_rows
                .into_iter()
                .map(|(cf, key, value)| encode::WriteRow { cf, key, value })
                .collect::<Vec<_>>();
            let mut hook_guard = match &self.ledger_hook {
                Some(hook) => Some(ledger_hook::lock_hook(hook)?),
                None => None,
            };
            let staged_ledger = if let Some(hook) = hook_guard.as_deref() {
                let staged = ledger_hook::stage_entry_payload(
                    hook,
                    &mut rows,
                    EntryKind::Grounding,
                    SubjectId::Cx(constellation.cx_id),
                    ledger_payload,
                    actor,
                )?;
                constellation.provenance = staged
                    .first()
                    .ok_or_else(|| {
                        CalyxError::ledger_group_commit_failed("no staged grounding ledger rows")
                    })?
                    .ledger_ref();
                Some(staged)
            } else {
                constellation.provenance = self.stage_raw_ledger_entry_locked(
                    &mut rows,
                    EntryKind::Grounding,
                    SubjectId::Cx(constellation.cx_id),
                    ledger_payload,
                    actor,
                )?;
                None
            };
            let ledger_ref = constellation.provenance.clone();
            // Provenance is part of the durable constellation. Capture the
            // expected state only after the ledger reference has been staged,
            // immediately before those exact rows enter the atomic commit.
            let expected_constellation = constellation.clone();
            prepared::stage_validated_constellation_rows(&mut rows, &constellation, prepared)?;
            let commit_started = Instant::now();
            let committed_seq = self.commit_rows_locked(&rows)?;
            let commit_us = elapsed_us(commit_started);
            if let (Some(hook), Some(staged)) = (hook_guard.take(), staged_ledger.as_ref()) {
                self.commit_persistent_ledger_staged_locked(hook, staged, "grounded_observation")?;
            }
            Ok((
                GroundedObservationCommit {
                    cx_id: constellation.cx_id,
                    disposition: PutDisposition::Inserted,
                    ledger_ref,
                    source_row_count: source_keys.len(),
                    committed_seq,
                },
                expected_constellation,
                source_precondition_us,
                commit_us,
            ))
        })?;
        let (commit, expected_constellation, source_precondition_us, commit_us) = commit;
        let readback_started = Instant::now();
        let readback = self.get(commit.cx_id, self.snapshot())?;
        let readback_us = elapsed_us(readback_started);
        if readback != expected_constellation {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "grounded observation post-commit readback mismatch for {} at committed seq {}: expected_flags={:?} actual_flags={:?} expected_provenance={:?} actual_provenance={:?} expected_slots={} actual_slots={} expected_scalars={} actual_scalars={} expected_anchors={} actual_anchors={}",
                commit.cx_id,
                commit.committed_seq,
                expected_constellation.flags,
                readback.flags,
                expected_constellation.provenance,
                readback.provenance,
                expected_constellation.slots.len(),
                readback.slots.len(),
                expected_constellation.scalars.len(),
                readback.scalars.len(),
                expected_constellation.anchors.len(),
                readback.anchors.len()
            )));
        }
        tracing::info!(
            code = "CALYX_ASTER_GROUNDED_OBSERVATION_STAGE_TIMINGS",
            cx_id = %commit.cx_id,
            committed_seq = commit.committed_seq,
            source_precondition_us,
            commit_us,
            constellation_readback_us = readback_us,
            total_us = elapsed_us(publication_started),
            "completed atomic grounded-observation stage timing readback"
        );
        Ok(commit)
    }
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}
