use super::*;
use crate::mvcc::tombstone_value;
use crate::vault::CfLedgerEntry;
use crate::vault::encode::WriteRow;

impl<C> OrphanGcTarget for VaultOrphanGcTarget<'_, C>
where
    C: Clock,
{
    fn base_entries(&self) -> Result<Vec<OrphanBaseEntry>> {
        let mut entries = Vec::new();
        for (key, bytes) in self
            .vault
            .scan_cf_at(self.vault.latest_seq(), ColumnFamily::Base)?
        {
            let cx_id = key_to_cx(&key)?;
            let cx = decode_constellation_base(&bytes)?;
            entries.push(OrphanBaseEntry {
                cx_id,
                expected_slots: cx.slots.keys().copied().collect(),
                repair_queued: cx.flags.degraded
                    && cx
                        .metadata
                        .get(REBUILD_METADATA_KEY)
                        .is_some_and(|state| state == REBUILD_METADATA_VALUE),
            });
        }
        entries.sort_by_key(|entry| entry.cx_id);
        Ok(entries)
    }

    fn slot_index_entries(&self) -> Result<Vec<OrphanIndexEntry>> {
        let mut entries = Vec::new();
        for slot in &self.slots {
            for (key, _) in self
                .vault
                .scan_cf_at(self.vault.latest_seq(), ColumnFamily::slot(*slot))?
            {
                entries.push(OrphanIndexEntry {
                    cx_id: key_to_cx(&key)?,
                    slot: *slot,
                });
            }
        }
        entries.sort_unstable();
        Ok(entries)
    }

    fn purge_orphan_index(&self, cx_id: CxId, slots: &[SlotId]) -> Result<usize> {
        self.purge_orphan_indexes(&[OrphanIndexRepair {
            cx_id,
            slots: slots.to_vec(),
        }])?
        .into_iter()
        .next()
        .map(|outcome| outcome.purged_rows)
        .ok_or_else(|| orphan_error("single orphan-index repair returned no outcome"))
    }

    fn flag_orphan_base(&self, cx_id: CxId) -> Result<()> {
        let outcome = self
            .flag_orphan_bases(&[cx_id])?
            .into_iter()
            .next()
            .ok_or_else(|| orphan_error("single orphan-Base repair returned no outcome"))?;
        if outcome.degraded {
            Ok(())
        } else {
            Err(orphan_error("base row disappeared before repair"))
        }
    }

    fn purge_orphan_indexes(
        &self,
        repairs: &[OrphanIndexRepair],
    ) -> Result<Vec<OrphanIndexRepairOutcome>> {
        let (outcomes, affected) = self.vault.with_durable_commit_lock(|| {
            self.vault
                .with_scoped_snapshot(self.vault.latest_seq(), |snapshot| {
                    let mut rows = Vec::<WriteRow>::new();
                    let mut ledger = Vec::new();
                    let mut affected = BTreeSet::new();
                    let mut outcomes = Vec::with_capacity(repairs.len());
                    for repair in repairs {
                        if self
                            .vault
                            .read_cf_snapshot(
                                snapshot,
                                ColumnFamily::Base,
                                &base_key(repair.cx_id),
                            )?
                            .is_some()
                        {
                            outcomes.push(OrphanIndexRepairOutcome {
                                cx_id: repair.cx_id,
                                purged_rows: 0,
                            });
                            continue;
                        }
                        let slots = if repair.slots.is_empty() {
                            self.slots.as_slice()
                        } else {
                            repair.slots.as_slice()
                        };
                        let mut unique_slots = BTreeSet::new();
                        let before = rows.len();
                        for slot in slots
                            .iter()
                            .copied()
                            .filter(|slot| unique_slots.insert(*slot))
                        {
                            let cf = ColumnFamily::slot(slot);
                            let key = slot_key(repair.cx_id);
                            if self.vault.read_cf_snapshot(snapshot, cf, &key)?.is_some() {
                                rows.push(WriteRow {
                                    cf,
                                    key,
                                    value: tombstone_value(),
                                });
                                affected.insert(cf);
                            }
                        }
                        let purged_rows = rows.len() - before;
                        if purged_rows > 0 {
                            ledger.push(CfLedgerEntry {
                                kind: EntryKind::Admin,
                                subject: SubjectId::Cx(repair.cx_id),
                                payload: orphan_payload("orphan_index_purged", purged_rows)?,
                                actor: ActorId::System,
                            });
                        }
                        outcomes.push(OrphanIndexRepairOutcome {
                            cx_id: repair.cx_id,
                            purged_rows,
                        });
                    }
                    self.vault
                        .write_cf_batch_with_ledger_entries_locked(rows, ledger)?;
                    Ok((outcomes, affected.into_iter().collect::<Vec<_>>()))
                })
        })?;
        if self.compact_after_tombstone && !affected.is_empty() {
            self.pending_compaction_cfs
                .lock()
                .map_err(|_| orphan_error("orphan compaction state lock poisoned"))?
                .extend(affected);
        }
        Ok(outcomes)
    }

    fn flag_orphan_bases(&self, cx_ids: &[CxId]) -> Result<Vec<OrphanBaseRepairOutcome>> {
        self.vault.with_durable_commit_lock(|| {
            self.vault
                .with_scoped_snapshot(self.vault.latest_seq(), |snapshot| {
                    let mut rows = Vec::<WriteRow>::new();
                    let mut ledger = Vec::new();
                    let mut outcomes = Vec::with_capacity(cx_ids.len());
                    for cx_id in cx_ids {
                        let key = base_key(*cx_id);
                        let Some(bytes) =
                            self.vault
                                .read_cf_snapshot(snapshot, ColumnFamily::Base, &key)?
                        else {
                            return Err(orphan_error(format!(
                                "Base row for orphan repair {cx_id} disappeared before the atomic repair batch"
                            )));
                        };
                        // Carries the stored slot hashes through the rewrite (#1888).
                        let mut rewrite = BaseRowRewrite::decode(&bytes)?;
                        if rewrite.constellation().flags.degraded
                            && rewrite
                                .constellation()
                                .metadata
                                .get(REBUILD_METADATA_KEY)
                                .is_some_and(|state| state == REBUILD_METADATA_VALUE)
                        {
                            outcomes.push(OrphanBaseRepairOutcome {
                                cx_id: *cx_id,
                                degraded: false,
                            });
                            continue;
                        }
                        let cx = rewrite.constellation_mut();
                        cx.flags.degraded = true;
                        cx.metadata.insert(
                            REBUILD_METADATA_KEY.to_string(),
                            REBUILD_METADATA_VALUE.to_string(),
                        );
                        let mut rebuild_key = REBUILD_PREFIX.to_vec();
                        rebuild_key.extend_from_slice(cx_id.as_bytes());
                        let slot_count = rewrite.constellation().slots.len();
                        rows.push(WriteRow {
                            cf: ColumnFamily::Base,
                            key,
                            value: rewrite.encode()?,
                        });
                        rows.push(WriteRow {
                            cf: ColumnFamily::AnnealReplay,
                            key: rebuild_key,
                            value: orphan_payload("orphan_base_rebuild_requested", slot_count)?,
                        });
                        ledger.push(CfLedgerEntry {
                            kind: EntryKind::Admin,
                            subject: SubjectId::Cx(*cx_id),
                            payload: orphan_payload("orphan_base_degraded", slot_count)?,
                            actor: ActorId::System,
                        });
                        outcomes.push(OrphanBaseRepairOutcome {
                            cx_id: *cx_id,
                            degraded: true,
                        });
                    }
                    self.vault
                        .write_cf_batch_with_ledger_entries_locked(rows, ledger)?;
                    Ok(outcomes)
                })
        })
    }

    fn finish_orphan_index_repairs(&self) -> Result<()> {
        if !self.compact_after_tombstone {
            return Ok(());
        }
        let affected = self
            .pending_compaction_cfs
            .lock()
            .map_err(|_| orphan_error("orphan compaction state lock poisoned"))?
            .iter()
            .copied()
            .collect::<Vec<_>>();
        if affected.is_empty() {
            return Ok(());
        }
        // Keep the set intact until every CF compaction succeeds. If this
        // fails, tombstones and audit rows are already durable and a retry on
        // this target will attempt the same deduplicated finalization again.
        self.vault.purge_tombstoned_cfs(&affected)?;
        let mut pending = self
            .pending_compaction_cfs
            .lock()
            .map_err(|_| orphan_error("orphan compaction state lock poisoned"))?;
        for cf in affected {
            pending.remove(&cf);
        }
        Ok(())
    }
}
