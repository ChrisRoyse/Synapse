use super::{AsterVault, durable, encode, ledger_hook, reject_raw_ledger_rows};
use crate::cf::ColumnFamily;
use calyx_core::{CalyxError, Clock, CxId, Result, Seq};
use calyx_ledger::{
    ActorId, EntryKind, SubjectId, declare_batch_members, require_base_stamp_declared,
};

/// One auditable ledger event to stage alongside a raw CF batch.
pub struct CfLedgerEntry {
    pub kind: EntryKind,
    pub subject: SubjectId,
    pub payload: Vec<u8>,
    pub actor: ActorId,
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Commits raw CF rows and one ledger entry in a single durable batch.
    ///
    /// # The Base-row contract (#2095)
    ///
    /// Any `Base` row in `rows` has its `provenance` rewritten to point at the
    /// entry this call mints, which makes the caller's `(kind, subject)` pair
    /// the answer a future reader gets when it asks what commits that
    /// constellation. Before #2095 the pair was entirely unconstrained: any
    /// crate could stamp any shape — including one whose subject and payload
    /// named no constellation at all — onto an unbounded set of stored
    /// constellations, and the first symptom was a refused query (#2084).
    ///
    /// A batch carrying `Base` rows is therefore held to the same declared table
    /// the search-side verifier reads
    /// ([`calyx_ledger::base_stamp::coverage_rule`]):
    ///
    /// * an undeclared `(kind, subject)` pair is refused before anything is
    ///   staged, with [`calyx_ledger::CALYX_LEDGER_BASE_STAMP_UNDECLARED`];
    /// * the payload is required to be a JSON object and gains a
    ///   [`calyx_ledger::batch_members`] declaration naming every constellation
    ///   stamped, so the entry answers its own membership question (#2096).
    ///
    /// A batch with no `Base` rows — every in-tree caller today: the
    /// KV/document/relational/timeseries/blob layers and the cross-model
    /// transaction — stamps no provenance and is unaffected by either rule.
    pub fn write_cf_batch_with_ledger_entry(
        &self,
        rows: impl IntoIterator<Item = (ColumnFamily, Vec<u8>, Vec<u8>)>,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<Seq> {
        let mut data_rows = rows
            .into_iter()
            .map(|(cf, key, value)| encode::WriteRow { cf, key, value })
            .collect::<Vec<_>>();
        reject_raw_ledger_rows("write_cf_batch_with_ledger_entry", &data_rows)?;
        if data_rows.is_empty() {
            return Ok(self.latest_seq());
        }
        let stamped = stamped_base_row_ids(&data_rows)?;
        let payload = if stamped.is_empty() {
            payload
        } else {
            require_base_stamp_declared("write_cf_batch_with_ledger_entry", kind, &subject)?;
            declare_batch_members(&payload, &stamped)?
        };

        self.with_durable_commit_lock(|| {
            if let Some(hook) = &self.ledger_hook {
                let hook = ledger_hook::lock_hook(hook)?;
                let mut rows = Vec::with_capacity(data_rows.len() + 2);
                let staged = ledger_hook::stage_entry_payload(
                    &hook, &mut rows, kind, subject, payload, actor,
                )?;
                let ledger_ref = staged_ledger_ref(&staged)?;
                attach_ledger_ref_to_base_rows(&mut data_rows, &ledger_ref)?;
                rows.extend(data_rows);
                let seq = self.commit_rows_locked(&rows)?;
                self.commit_persistent_ledger_staged_locked(
                    hook,
                    &staged,
                    "write_cf_batch_with_ledger_entry",
                )?;
                return Ok(seq);
            }

            let mut transient = self.transient_ledger_hook()?;
            let hook = transient
                .get_mut()
                .map_err(|_| CalyxError::ledger_group_commit_failed("transient hook poisoned"))?;
            let mut rows = Vec::with_capacity(data_rows.len() + 1);
            let staged =
                ledger_hook::stage_entry_payload(hook, &mut rows, kind, subject, payload, actor)?;
            let ledger_ref = staged_ledger_ref(&staged)?;
            attach_ledger_ref_to_base_rows(&mut data_rows, &ledger_ref)?;
            rows.extend(data_rows);
            // A transient hook is reconstructed from the authoritative Ledger
            // CF for every operation and is discarded when this scope exits.
            // Advancing that disposable in-memory copy after the rows commit
            // creates a fallible post-commit mutation with no observable
            // benefit: an error would make an already-applied operation look
            // retryable. Staging above performs all required validation; the
            // committed Ledger rows are the next operation's source of truth.
            self.commit_rows_locked(&rows)
        })
    }

    /// Commits raw rows and an ordered set of per-subject ledger events in one
    /// durable batch. This is crate-internal because callers must decide how
    /// individual data rows map to individual audit events.
    pub(crate) fn write_cf_batch_with_ledger_entries_locked(
        &self,
        mut rows: Vec<encode::WriteRow>,
        entries: Vec<CfLedgerEntry>,
    ) -> Result<Seq> {
        reject_raw_ledger_rows("write_cf_batch_with_ledger_entries_locked", &rows)?;
        if rows.is_empty() && entries.is_empty() {
            return Ok(self.latest_seq());
        }
        let drafts = entries
            .into_iter()
            .map(|entry| (entry.kind, entry.subject, entry.payload, entry.actor))
            .collect::<Vec<_>>();

        if let Some(hook) = &self.ledger_hook {
            let hook = ledger_hook::lock_hook(hook)?;
            let staged = hook.stage_many_with_checkpoints(drafts)?;
            rows.extend(staged.iter().map(|row| encode::WriteRow {
                cf: ColumnFamily::Ledger,
                key: row.key().to_vec(),
                value: row.value().to_vec(),
            }));
            let seq = self.commit_rows_locked(&rows)?;
            self.commit_persistent_ledger_staged_locked(
                hook,
                &staged,
                "write_cf_batch_with_ledger_entries",
            )?;
            return Ok(seq);
        }

        let mut transient = self.transient_ledger_hook()?;
        let hook = transient
            .get_mut()
            .map_err(|_| CalyxError::ledger_group_commit_failed("transient hook poisoned"))?;
        let staged = hook.stage_many_with_checkpoints(drafts)?;
        rows.extend(staged.iter().map(|row| encode::WriteRow {
            cf: ColumnFamily::Ledger,
            key: row.key().to_vec(),
            value: row.value().to_vec(),
        }));
        // See the single-entry path above: this hook is disposable and the
        // just-committed Ledger CF is the only state the next call observes.
        self.commit_rows_locked(&rows)
    }

    fn transient_ledger_hook(&self) -> Result<ledger_hook::AsterLedgerHook> {
        let ledger_rows = self
            .scan_cf_at(self.latest_seq(), ColumnFamily::Ledger)?
            .into_iter()
            .map(|(key, value)| encode::WriteRow {
                cf: ColumnFamily::Ledger,
                key,
                value,
            })
            .collect::<Vec<_>>();
        let batches = if ledger_rows.is_empty() {
            Vec::new()
        } else {
            vec![durable::RecoveredBatch {
                seq: self.latest_seq(),
                rows: ledger_rows,
            }]
        };
        ledger_hook::recover_hook(
            &durable::RecoveredBatches {
                batches,
                last_recovered_seq: self.latest_seq(),
                wal_replay_floor_seq: 0,
                derived_content_floor_seq: 0,
                panel_content_floor_seqs: std::collections::BTreeMap::new(),
                active_panel_version: None,
                migrate_derived_content_model: false,
                torn_tail: None,
                temporal_policy: None,
                dedup_policy: None,
                retention_horizon: crate::timetravel::RetentionHorizon::default(),
                router_latest_readback: false,
                wal_tail_stream_floor: None,
            },
            None,
        )
    }
}

fn staged_ledger_ref(staged: &[calyx_ledger::StagedLedgerRow]) -> Result<calyx_core::LedgerRef> {
    staged
        .first()
        .map(calyx_ledger::StagedLedgerRow::ledger_ref)
        .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))
}

/// The constellations whose provenance this batch is about to rewrite.
///
/// Decoded from the caller's own `Base` rows rather than taken on trust, so the
/// membership declaration names exactly the rows that get stamped and cannot
/// drift from them.
fn stamped_base_row_ids(rows: &[encode::WriteRow]) -> Result<Vec<CxId>> {
    rows.iter()
        .filter(|row| row.cf == ColumnFamily::Base)
        .map(|row| {
            Ok(super::base_rewrite::BaseRowRewrite::decode(&row.value)?
                .constellation()
                .cx_id)
        })
        .collect()
}

fn attach_ledger_ref_to_base_rows(
    rows: &mut [encode::WriteRow],
    ledger_ref: &calyx_core::LedgerRef,
) -> Result<()> {
    for row in rows.iter_mut().filter(|row| row.cf == ColumnFamily::Base) {
        // Carries the stored slot hashes through the rewrite. Re-encoding from
        // a plain decode would substitute placeholder hashes for the real
        // integrity record on a live group-commit path (issue #1888).
        let mut rewrite = super::base_rewrite::BaseRowRewrite::decode(&row.value)?;
        rewrite.constellation_mut().provenance = ledger_ref.clone();
        row.value = rewrite.encode()?;
    }
    Ok(())
}
