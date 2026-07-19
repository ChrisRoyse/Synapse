use super::{AsterVault, encode, ledger_hook};
use crate::cf::{ColumnFamily, anchor_key, base_key, ledger_key};
use crate::ledger_view::parse_aster_ledger_seq;
use calyx_core::{Anchor, CalyxError, Clock, CxId, LedgerRef, Result, SystemClock, VaultStore};
use calyx_ledger::{
    ActorId, EntryKind, ForgeBackend, LedgerAppender, LedgerCfStore, LedgerHeadAnchor, LedgerRow,
    QueryId, ReproduceInputResolver, ReproduceLensRegistry, ReproduceResult, StagedLedgerRow,
    SubjectId, reproduce_payload_bytes, reproduce_verdict_with_input_resolver,
    reproduce_with_input_resolver,
};

struct LedgerEntryInput {
    kind: EntryKind,
    subject: SubjectId,
    payload: Vec<u8>,
    actor: ActorId,
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Advances the persistent in-memory ledger hook after its exact rows have
    /// durably committed. If hook advancement fails, the guard is released and
    /// the hook is rebuilt from the authoritative physical Ledger CF while the
    /// caller still owns the durable commit boundary.
    ///
    /// The method always returns a reconciliation-required error after a hook
    /// failure—even when rebuild succeeds—because the data commit already
    /// happened and blindly retrying would duplicate the logical operation.
    pub(crate) fn commit_persistent_ledger_staged_locked(
        &self,
        mut guard: ledger_hook::AsterLedgerHookGuard<'_>,
        staged: &[StagedLedgerRow],
        operation: &'static str,
    ) -> Result<LedgerRef> {
        match ledger_hook::commit_staged(&mut guard, staged) {
            Ok(ledger_ref) => Ok(ledger_ref),
            Err(hook_error) => {
                drop(guard);
                self.ledger_state_reconciliation_required
                    .store(true, std::sync::atomic::Ordering::Release);
                let reconciliation = self.reconcile_ledger_state_from_durable_locked();
                if reconciliation.is_ok() {
                    self.ledger_state_reconciliation_required
                        .store(false, std::sync::atomic::Ordering::Release);
                }
                tracing::error!(
                    code = super::CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
                    operation,
                    hook_error_code = hook_error.code,
                    hook_error = %hook_error,
                    reconciliation_ok = reconciliation.is_ok(),
                    reconciliation_error = reconciliation.as_ref().err().map(ToString::to_string),
                    "durable Ledger rows committed but persistent hook advancement failed; rebuilt hook from physical truth before releasing commit boundary"
                );
                Err(CalyxError {
                    code: super::CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
                    message: format!(
                        "{operation}: durable Ledger rows committed but hook advancement failed: hook=error[{}]: {}; reconciliation={}",
                        hook_error.code,
                        hook_error.message,
                        reconciliation
                            .as_ref()
                            .map(|()| "ok".to_owned())
                            .unwrap_or_else(|error| {
                                format!("error[{}]: {}", error.code, error.message)
                            })
                    ),
                    remediation: "treat the durable operation as committed; use its idempotency/readback identity before retrying, and inspect the hook-reconciliation diagnostics",
                })
            }
        }
    }

    pub(crate) fn stage_raw_ledger_entry_locked(
        &self,
        rows: &mut Vec<encode::WriteRow>,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<LedgerRef> {
        let store = AsterRawLedgerStore { vault: self };
        let appender = LedgerAppender::open(store, SystemClock)?;
        let prepared = appender.prepare(kind, subject, payload, actor)?;
        let ledger_ref = prepared.ledger_ref();
        rows.push(encode::WriteRow {
            cf: ColumnFamily::Ledger,
            key: ledger_key(prepared.seq()),
            value: prepared.bytes().to_vec(),
        });
        Ok(ledger_ref)
    }

    pub(crate) fn stage_raw_ingest_ledger_locked(
        &self,
        rows: &mut Vec<encode::WriteRow>,
        subject: calyx_core::CxId,
        payload: Vec<u8>,
    ) -> Result<LedgerRef> {
        self.stage_raw_ledger_entry_locked(
            rows,
            EntryKind::Ingest,
            SubjectId::Cx(subject),
            payload,
            ActorId::Service("calyx-aster".to_string()),
        )
    }

    /// Adds an anchor and stamps the stored base row with the same ledger ref.
    pub fn anchor_with_ledger_entry(
        &self,
        id: CxId,
        anchor: Anchor,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<LedgerRef> {
        let entry = LedgerEntryInput {
            kind,
            subject,
            payload,
            actor,
        };
        anchor.validate_schema()?;
        self.with_durable_commit_lock(|| {
            let latest = self.snapshot();
            let mut constellation = self.get(id, latest)?;
            constellation.anchors.push(anchor.clone());
            constellation.flags.ungrounded = constellation.anchors.is_empty();
            let Some(hook) = &self.ledger_hook else {
                return self.anchor_with_raw_ledger_entry(id, &mut constellation, anchor, entry);
            };
            let guard = ledger_hook::lock_hook(hook)?;
            let staged = guard.stage_with_checkpoints(
                entry.kind,
                entry.subject,
                entry.payload,
                entry.actor,
            )?;
            let ledger_ref = staged
                .first()
                .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
                .ledger_ref();
            constellation.provenance = ledger_ref.clone();
            let mut rows = anchor_rows(id, &constellation, &anchor)?;
            rows.extend(staged.iter().map(|row| encode::WriteRow {
                cf: ColumnFamily::Ledger,
                key: row.key().to_vec(),
                value: row.value().to_vec(),
            }));
            self.commit_rows_locked(&rows)?;
            self.commit_persistent_ledger_staged_locked(
                guard,
                &staged,
                "anchor_with_ledger_entry",
            )?;
            Ok(ledger_ref)
        })
    }

    /// Appends a provenance Ledger entry through Aster's durable group-commit path.
    pub fn append_ledger_entry(
        &self,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<LedgerRef> {
        self.with_durable_commit_lock(|| {
            let Some(hook) = &self.ledger_hook else {
                return self.append_ledger_entry_without_hook(kind, subject, payload, actor);
            };
            let guard = ledger_hook::lock_hook(hook)?;
            let staged = guard.stage_with_checkpoints(kind, subject, payload, actor)?;
            let ledger_ref = staged
                .first()
                .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
                .ledger_ref();
            let rows = staged
                .iter()
                .map(|row| encode::WriteRow {
                    cf: ColumnFamily::Ledger,
                    key: row.key().to_vec(),
                    value: row.value().to_vec(),
                })
                .collect::<Vec<_>>();
            self.commit_rows_locked(&rows)?;
            self.commit_persistent_ledger_staged_locked(guard, &staged, "append_ledger_entry")?;
            Ok(ledger_ref)
        })
    }

    /// Records a reproduce verdict as a `reproduce_v1` Ledger Admin row.
    pub fn record_reproduce_with_input_resolver(
        &self,
        registry: &dyn ReproduceLensRegistry,
        forge: &mut dyn ForgeBackend,
        resolver: &dyn ReproduceInputResolver,
        answer_id: &QueryId,
    ) -> Result<ReproduceResult> {
        if self.ledger_hook.is_none() {
            let mut store = AsterRawLedgerStore { vault: self };
            return reproduce_with_input_resolver(&mut store, registry, forge, resolver, answer_id);
        }

        let store = AsterRawLedgerStore { vault: self };
        let result =
            reproduce_verdict_with_input_resolver(&store, registry, forge, resolver, answer_id)?;
        let payload = reproduce_payload_bytes(answer_id, &result, self.clock_now())?;
        self.append_ledger_entry(
            EntryKind::Admin,
            SubjectId::Query(answer_id.clone()),
            payload,
            ActorId::Service("calyx-reproduce".to_string()),
        )?;
        Ok(result)
    }

    fn append_ledger_entry_without_hook(
        &self,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<LedgerRef> {
        let store = AsterRawLedgerStore { vault: self };
        let appender = LedgerAppender::open(store, SystemClock)?;
        let prepared = appender.prepare(kind, subject, payload, actor)?;
        let ledger_ref = prepared.ledger_ref();
        let rows = [encode::WriteRow {
            cf: ColumnFamily::Ledger,
            key: ledger_key(prepared.seq()),
            value: prepared.bytes().to_vec(),
        }];
        self.commit_rows_locked(&rows)?;
        Ok(ledger_ref)
    }

    /// Appends a ledger row prepared by an external adapter while keeping the
    /// vault-owned live ledger hook synchronized with the durable Ledger CF.
    pub fn append_external_ledger_row(&self, seq: u64, bytes: &[u8]) -> Result<()> {
        self.with_durable_commit_lock(|| {
            let key = ledger_key(seq);
            if self
                .read_cf_at(self.latest_seq(), ColumnFamily::Ledger, &key)?
                .is_some()
            {
                return Err(CalyxError::ledger_append_only_violation(format!(
                    "Aster ledger seq {seq} already exists"
                )));
            }
            let rows = [encode::WriteRow {
                cf: ColumnFamily::Ledger,
                key,
                value: bytes.to_vec(),
            }];
            self.commit_rows_locked(&rows)?;
            self.ledger_state_reconciliation_required
                .store(true, std::sync::atomic::Ordering::Release);
            match self.reconcile_ledger_state_from_durable_locked() {
                Ok(()) => {
                    self.ledger_state_reconciliation_required
                        .store(false, std::sync::atomic::Ordering::Release);
                    Ok(())
                }
                Err(error) => Err(CalyxError {
                    code: super::CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
                    message: format!(
                        "append_external_ledger_row: durable Ledger row committed but hook rebuild failed: error[{}]: {}",
                        error.code, error.message
                    ),
                    remediation: "treat the external Ledger row as committed; the next durable operation will retry physical hook reconstruction before admitting new work",
                }),
            }
        })
    }

    pub(crate) fn reconcile_ledger_state_from_durable_locked(&self) -> Result<()> {
        let Some(durable) = &self.durable else {
            return Err(CalyxError::ledger_group_commit_failed(
                "Ledger state reconciliation requires a durable vault",
            ));
        };
        let recovered = durable.recover_current_batches()?;
        self.reconcile_ledger_state_from_recovery_locked(&recovered)
    }

    pub(super) fn reconcile_ledger_state_from_recovery_locked(
        &self,
        recovered: &super::durable::RecoveredBatches,
    ) -> Result<()> {
        let Some(durable) = &self.durable else {
            return Err(CalyxError::ledger_group_commit_failed(
                "Ledger state reconciliation requires a durable vault",
            ));
        };
        // A prior process may have failed after WAL durability but before
        // updating these derived sidecars. Repair them before optimized
        // physical hook hydration trusts the head boundary.
        ledger_hook::ensure_recovered_ledger_sidecars(durable.root(), recovered, !self.read_only)?;
        let Some(hook) = &self.ledger_hook else {
            return Ok(());
        };
        if durable.value_crypto_enabled() {
            ledger_hook::refresh_hook_from_recovery(hook, recovered, durable.ledger_checkpoint())
        } else {
            ledger_hook::refresh_hook(
                hook,
                durable.root(),
                recovered,
                durable.ledger_checkpoint(),
                durable.tiering_policy(),
            )
        }
    }

    fn anchor_with_raw_ledger_entry(
        &self,
        id: CxId,
        constellation: &mut calyx_core::Constellation,
        anchor: Anchor,
        entry: LedgerEntryInput,
    ) -> Result<LedgerRef> {
        let store = AsterRawLedgerStore { vault: self };
        let appender = LedgerAppender::open(store, SystemClock)?;
        let prepared = appender.prepare(entry.kind, entry.subject, entry.payload, entry.actor)?;
        let ledger_ref = prepared.ledger_ref();
        constellation.provenance = ledger_ref.clone();
        let mut rows = anchor_rows(id, constellation, &anchor)?;
        rows.push(encode::WriteRow {
            cf: ColumnFamily::Ledger,
            key: ledger_key(prepared.seq()),
            value: prepared.bytes().to_vec(),
        });
        self.commit_rows_locked(&rows)?;
        Ok(ledger_ref)
    }

    pub(crate) fn has_real_ledger_hook(&self) -> bool {
        self.ledger_hook.is_some()
    }

    pub(crate) fn next_ledger_seq_locked(&self) -> Result<u64> {
        let Some(hook) = &self.ledger_hook else {
            let store = AsterRawLedgerStore { vault: self };
            return Ok(LedgerAppender::open(store, SystemClock)?.next_seq());
        };
        let guard = ledger_hook::lock_hook(hook)?;
        Ok(guard.appender().next_seq())
    }

    pub(crate) fn commit_rows_with_ledger_entry_locked(
        &self,
        rows: Vec<encode::WriteRow>,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<LedgerRef> {
        self.commit_rows_with_ledger_entry_policy_locked(rows, kind, subject, payload, actor, false)
    }

    pub(crate) fn commit_erasure_rows_with_ledger_entry_locked(
        &self,
        rows: Vec<encode::WriteRow>,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<LedgerRef> {
        self.commit_rows_with_ledger_entry_policy_locked(rows, kind, subject, payload, actor, true)
    }

    fn commit_rows_with_ledger_entry_policy_locked(
        &self,
        mut rows: Vec<encode::WriteRow>,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
        erasure: bool,
    ) -> Result<LedgerRef> {
        let Some(hook) = &self.ledger_hook else {
            return self
                .commit_rows_with_raw_ledger_entry(rows, kind, subject, payload, actor, erasure);
        };
        let guard = ledger_hook::lock_hook(hook)?;
        let staged = guard.stage_with_checkpoints(kind, subject, payload, actor)?;
        let ledger_ref = staged
            .first()
            .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
            .ledger_ref();
        rows.extend(staged.iter().map(|row| encode::WriteRow {
            cf: ColumnFamily::Ledger,
            key: row.key().to_vec(),
            value: row.value().to_vec(),
        }));
        if erasure {
            self.commit_erasure_rows_locked(&rows)?;
        } else {
            self.commit_rows_locked(&rows)?;
        }
        self.commit_persistent_ledger_staged_locked(
            guard,
            &staged,
            "commit_rows_with_ledger_entry",
        )?;
        Ok(ledger_ref)
    }

    fn commit_rows_with_raw_ledger_entry(
        &self,
        mut rows: Vec<encode::WriteRow>,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
        erasure: bool,
    ) -> Result<LedgerRef> {
        let store = AsterRawLedgerStore { vault: self };
        let appender = LedgerAppender::open(store, SystemClock)?;
        let prepared = appender.prepare(kind, subject, payload, actor)?;
        let ledger_ref = prepared.ledger_ref();
        rows.push(encode::WriteRow {
            cf: ColumnFamily::Ledger,
            key: ledger_key(prepared.seq()),
            value: prepared.bytes().to_vec(),
        });
        if erasure {
            self.commit_erasure_rows_locked(&rows)?;
        } else {
            self.commit_rows_locked(&rows)?;
        }
        Ok(ledger_ref)
    }
}

fn anchor_rows(
    id: CxId,
    constellation: &calyx_core::Constellation,
    anchor: &Anchor,
) -> Result<Vec<encode::WriteRow>> {
    Ok(vec![
        encode::WriteRow {
            cf: ColumnFamily::Base,
            key: base_key(id),
            value: encode::encode_constellation_base(constellation)?,
        },
        encode::WriteRow {
            cf: ColumnFamily::Anchors,
            key: anchor_key(id, &anchor.kind),
            value: encode::encode_anchor(anchor)?,
        },
    ])
}

struct AsterRawLedgerStore<'a, C> {
    vault: &'a AsterVault<C>,
}

impl<C> LedgerCfStore for AsterRawLedgerStore<'_, C>
where
    C: Clock,
{
    fn scan(&self) -> Result<Vec<LedgerRow>> {
        let mut rows = Vec::new();
        for (key, bytes) in self
            .vault
            .scan_cf_at(self.vault.snapshot(), ColumnFamily::Ledger)?
        {
            rows.push(LedgerRow {
                seq: parse_aster_ledger_seq(&key)?,
                bytes,
            });
        }
        rows.sort_by_key(|row| row.seq);
        Ok(rows)
    }

    fn put_new(&mut self, seq: u64, bytes: &[u8]) -> Result<()> {
        let key = ledger_key(seq);
        if self
            .vault
            .read_cf_at(self.vault.snapshot(), ColumnFamily::Ledger, &key)?
            .is_some()
        {
            return Err(CalyxError::ledger_append_only_violation(format!(
                "ledger seq {seq} already exists"
            )));
        }
        self.vault
            .write_raw_ledger_row_without_hook(key, bytes.to_vec())
            .map(|_| ())
    }

    fn head_anchor(&self) -> Result<Option<LedgerHeadAnchor>> {
        let Some(durable) = &self.vault.durable else {
            return Ok(None);
        };
        let anchor = crate::ledger_head::read_head_anchor(durable.root())?;
        if anchor.is_none() {
            let rows = self.scan()?;
            return crate::ledger_head::require_head_anchor_for_rows(durable.root(), anchor, &rows);
        }
        Ok(anchor)
    }

    fn put_head_anchor(&mut self, anchor: &LedgerHeadAnchor) -> Result<()> {
        if let Some(durable) = &self.vault.durable {
            crate::ledger_head::write_head_anchor(durable.root(), anchor)?;
        }
        Ok(())
    }
}
