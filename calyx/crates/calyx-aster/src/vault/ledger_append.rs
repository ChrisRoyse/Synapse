use super::{AsterVault, encode, ledger_hook, raw_commitment};
use crate::cf::{ColumnFamily, KeyRange, anchor_key, base_key, ledger_key};
use crate::ledger_view::parse_aster_ledger_seq;
use crate::mvcc::SnapshotCfRowStream;
use calyx_core::{Anchor, CalyxError, Clock, CxId, LedgerRef, Result, SystemClock, VaultStore};
use calyx_ledger::{
    ActorId, AnchorDiscipline, EntryKind, ForgeBackend, LedgerAppender, LedgerCfStore, LedgerEntry,
    LedgerHeadAnchor, LedgerRow, QueryId, RedactionPolicy, ReproduceInputResolver,
    ReproduceLensRegistry, ReproduceResult, StagedLedgerRow, StreamingChainVerifier,
    StreamingStart, SubjectId, VerifyResult, decode as decode_ledger_entry,
    decode_ref as decode_ledger_entry_ref, reproduce_payload_bytes,
    reproduce_verdict_with_input_resolver, reproduce_with_input_resolver,
};
use std::ops::Range;

/// Result of verifying the live physical Ledger hash chain against the exact
/// stored bytes in [`ColumnFamily::Ledger`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsterLedgerChainVerification {
    /// The fail-closed verification verdict for the requested range.
    pub result: VerifyResult,
    /// The durable Ledger head height (entry count) at verification time.
    pub head_height: u64,
    /// The durable Ledger tip hash, when a head anchor exists.
    pub tip_hash: Option<[u8; 32]>,
    /// The exact half-open sequence range that was walked and re-hashed.
    pub verified_range: Range<u64>,
    /// Independent readback of the raw-write commitment CF and every
    /// checkpoint-cohort seal carried by this Ledger.
    pub raw_commitments: AsterRawCommitmentVerification,
}

/// Fail-closed verification of compact raw-write provenance commitments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsterRawCommitmentVerification {
    pub intact: bool,
    pub seal_count: u64,
    pub commitment_count: u64,
    pub sealed_commitment_count: u64,
    /// Atomically committed rows newer than the last periodic checkpoint seal.
    pub pending_commitment_count: u64,
    pub coverage_from_seq: Option<u64>,
    pub sealed_through_seq: Option<u64>,
    pub first_pending_seq: Option<u64>,
    pub failure: Option<String>,
}

/// Re-derivation verdict for one record's recorded provenance binding.
///
/// Reproduce reads the record, follows its recorded provenance pointer into the
/// Ledger, and re-derives the sealed entry hash from the entry's own fields —
/// proving the record still points at a genuine, self-consistent chain entry
/// whose hash matches both the record's provenance ref and the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsterProvenanceReproduction {
    pub cx_id: CxId,
    /// The Ledger sequence recorded in the record's provenance ref.
    pub recorded_seq: u64,
    /// The entry hash recorded in the record's provenance ref.
    pub recorded_hash: [u8; 32],
    /// The record's canonical input hash (evidence pointer).
    pub input_hash: [u8; 32],
    /// Whether the referenced Ledger entry physically exists.
    pub entry_present: bool,
    /// The re-decoded Ledger entry hash, when present.
    pub entry_hash: Option<[u8; 32]>,
    /// Whether the entry re-hashes to its stored hash (self-consistent bytes).
    pub entry_self_verifies: bool,
    /// Whether the entry's subject binds back to this record.
    pub subject_matches: bool,
    /// Overall verdict: the record re-derives to a genuine, matching entry.
    pub reproduced: bool,
}

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

    /// Verifies the live physical Ledger hash chain against the exact stored
    /// bytes, fail-closed.
    ///
    /// Pass `range = None` to verify the full chain (`0..head_height`); pass an
    /// explicit sub-range for an incremental re-walk. The verifier re-hashes
    /// every entry from its sealed fields and checks each `prev_hash` link, so a
    /// single flipped byte anywhere in the requested window yields a `Broken` or
    /// `Corrupt` verdict rather than a silent pass.
    ///
    /// # Errors
    ///
    /// Returns a structured error only when the physical Ledger CF cannot be
    /// scanned or the head anchor cannot be read; a detected tamper is a normal
    /// `Ok(Broken | Corrupt)` verdict, never an `Err`.
    pub fn verify_ledger_chain(
        &self,
        range: Option<Range<u64>>,
    ) -> Result<AsterLedgerChainVerification> {
        // Fail closed rather than verify nothing (#1956).
        //
        // `open.rs` sets `durable: None` for a read-only handle, and
        // `head_anchor` returns `None` when durable is None. Without this
        // guard the head height is 0, the verified range is `0..0`, and this
        // returns `Intact { count: 0 }` -- a pass over an empty chain, on a
        // vault that may hold any number of entries. Measured: the same vault
        // reported `Intact { count: 0 }` read-only and
        // `Intact { count: 125735 }` writable.
        //
        // A read-only handle is exactly what one reaches for to verify a
        // restored backup or a vault one does not trust, so that is the single
        // case where a false "intact" costs the most, and it was the only case
        // where it fired.
        //
        // `AsterVault` does not retain the vault root when `durable` is None,
        // so this cannot be answered by reading the head projection directly;
        // refusing is the honest outcome. Callers that need offline chain
        // verification should use `verify_restore`, which scans the vault path
        // itself and derives its head from the ledger rows present.
        if self.durable.is_none() {
            return Err(CalyxError {
                code: "CALYX_ASTER_LEDGER_VERIFY_UNAVAILABLE",
                message: format!(
                    "ledger chain verification needs a durable handle; this vault was opened read_only={} and cannot read its head anchor, so any verdict would cover zero entries",
                    self.read_only
                ),
                remediation: "verify through a write-capable handle, or use verify_restore, which reads the vault directory directly and needs no durable handle",
            });
        }
        let durable = self.durable.as_ref().ok_or_else(|| CalyxError {
            code: "CALYX_ASTER_LEDGER_VERIFY_UNAVAILABLE",
            message: "ledger snapshot requires a durable vault handle".to_owned(),
            remediation: "verify through a write-capable handle or use verify_restore",
        })?;
        let commit_guard = crate::file_lock::FileLockGuard::acquire(
            &durable.root().join("locks").join("durable.commit.lock"),
        )?;
        let snapshot_seq = self.snapshot();
        let snapshot_handle = self.snapshot_handle(snapshot_seq);
        let snapshot = snapshot_handle.snapshot();
        let head = crate::ledger_head::read_head_anchor(durable.root())?;
        drop(commit_guard);

        if head.is_none() {
            let mut probe = self.rows.open_cf_range_row_stream_at(
                snapshot,
                ColumnFamily::Ledger,
                &KeyRange::all(),
                &self.clock,
            )?;
            let mut first_seq = None;
            let present = probe.next_with(|key, _| {
                first_seq = Some(parse_aster_ledger_seq(key)?);
                Ok(())
            })?;
            if present {
                return Err(crate::ledger_head::missing_head_anchor(
                    durable.root(),
                    first_seq.unwrap_or(0).saturating_add(1),
                ));
            }
        }
        let head_height = head.as_ref().map_or(0, |anchor| anchor.height);
        let verified_range = range.unwrap_or(0..head_height);
        if verified_range.start > verified_range.end || verified_range.end > head_height {
            return Err(CalyxError {
                code: "CALYX_ASTER_LEDGER_VERIFY_RANGE_INVALID",
                message: format!(
                    "ledger verification range [{}..{}) is outside the pinned durable head [0..{})",
                    verified_range.start, verified_range.end, head_height
                ),
                remediation: "use a half-open range with start <= end <= the reported pinned durable head; omit the range to verify the complete pinned chain",
            });
        }

        let previous = if verified_range.start == 0 {
            None
        } else {
            let seq = verified_range.start - 1;
            self.read_cf_snapshot(snapshot, ColumnFamily::Ledger, &ledger_key(seq))?
                .map(|bytes| LedgerRow { seq, bytes })
        };
        let (mut chain, mut result) = match StreamingChainVerifier::start(
            verified_range.clone(),
            head.clone(),
            previous.as_ref(),
            AnchorDiscipline::ExactHead,
        )? {
            StreamingStart::Ready(verifier) => (Some(verifier), None),
            StreamingStart::Complete(result) => (None, Some(result)),
        };

        let raw_stream = self.rows.open_cf_range_row_stream_at(
            snapshot,
            ColumnFamily::RawCommitment,
            &KeyRange::all(),
            &self.clock,
        )?;
        let mut raw_cursor = RawCommitmentCursor::new(raw_stream);
        let mut raw_state = RawCommitmentVerificationState::default();
        let mut ledger_stream = self.rows.open_cf_range_row_stream_at(
            snapshot,
            ColumnFamily::Ledger,
            &KeyRange::all(),
            &self.clock,
        )?;
        while ledger_stream.next_with(|key, bytes| {
            let seq = parse_aster_ledger_seq(key)?;
            raw_state.observe_ledger_row(seq, bytes, &mut raw_cursor)?;
            let terminal = if seq >= verified_range.start && seq < verified_range.end {
                match chain.as_mut() {
                    Some(verifier) => verifier.verify_next_bytes(seq, bytes)?,
                    None => None,
                }
            } else {
                None
            };
            if let Some(terminal) = terminal {
                result = Some(terminal);
                chain = None;
            }
            Ok(())
        })? {}

        if result.is_none() {
            let verifier = chain.as_mut().ok_or_else(|| {
                CalyxError::ledger_corrupt(
                    "ledger streaming verifier ended without a verdict or active cursor",
                )
            })?;
            result = verifier.verify_next(None)?;
        }
        let result = result.ok_or_else(|| {
            CalyxError::ledger_corrupt(
                "ledger streaming verifier reached end of input without a terminal verdict",
            )
        })?;
        let raw_commitments = raw_state.finish(&mut raw_cursor);
        Ok(AsterLedgerChainVerification {
            result,
            head_height,
            tip_hash: head.map(|anchor| anchor.tip_hash),
            verified_range,
            raw_commitments,
        })
    }

    /// Reads and decodes one physical Ledger entry by sequence for provenance
    /// readback. Returns `Ok(None)` when no row exists at `seq`.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the physical row cannot be read or its
    /// bytes cannot be decoded.
    pub fn read_ledger_entry(&self, seq: u64) -> Result<Option<LedgerEntry>> {
        let key = ledger_key(seq);
        let Some(bytes) = self.read_cf_at(self.latest_seq(), ColumnFamily::Ledger, &key)? else {
            return Ok(None);
        };
        Ok(Some(decode_ledger_entry(&bytes)?))
    }

    /// Re-derives a record's recorded provenance binding from the bytes.
    ///
    /// Reads the constellation, follows its recorded provenance pointer into the
    /// physical Ledger, re-decodes that entry, and re-hashes it from its own
    /// sealed fields. The record reproduces only when the referenced entry
    /// exists, self-verifies, binds back to this record's subject, and its hash
    /// matches the record's stored provenance ref.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the record is absent or its bytes cannot
    /// be read; a provenance mismatch is a normal `reproduced == false` verdict.
    pub fn reproduce_record_provenance(&self, id: CxId) -> Result<AsterProvenanceReproduction> {
        let constellation = self.get(id, self.snapshot())?;
        let recorded_seq = constellation.provenance.seq;
        let recorded_hash = constellation.provenance.hash;
        let input_hash = constellation.input_ref.hash;
        let entry = self.read_ledger_entry(recorded_seq)?;
        let (entry_present, entry_hash, entry_self_verifies, subject_matches) = match &entry {
            Some(entry) => (
                true,
                Some(entry.entry_hash),
                entry.verify(),
                entry.subject == SubjectId::Cx(id),
            ),
            None => (false, None, false, false),
        };
        let reproduced = entry_present
            && entry_self_verifies
            && subject_matches
            && entry_hash == Some(recorded_hash);
        Ok(AsterProvenanceReproduction {
            cx_id: id,
            recorded_seq,
            recorded_hash,
            input_hash,
            entry_present,
            entry_hash,
            entry_self_verifies,
            subject_matches,
            reproduced,
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
        self.append_ledger_entry_with_rows(kind, subject, payload, actor, |_ledger_ref| {
            Ok(Vec::new())
        })
    }

    /// Appends a provenance entry and caller-owned rows in one durable commit.
    /// The builder receives the staged ledger reference while the durable
    /// commit lock is held, so an outbox row can carry the exact sequence/hash
    /// without a second-write crash gap.
    pub fn append_ledger_entry_with_rows<F>(
        &self,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
        build_rows: F,
    ) -> Result<LedgerRef>
    where
        F: FnOnce(LedgerRef) -> Result<Vec<(ColumnFamily, Vec<u8>, Vec<u8>)>>,
    {
        self.with_durable_commit_lock(|| {
            let Some(hook) = &self.ledger_hook else {
                let store = AsterRawLedgerStore { vault: self };
                let appender = LedgerAppender::open(store, SystemClock)?;
                let prepared = appender.prepare(kind, subject, payload, actor)?;
                let ledger_ref = prepared.ledger_ref();
                let mut rows = vec![encode::WriteRow {
                    cf: ColumnFamily::Ledger,
                    key: ledger_key(prepared.seq()),
                    value: prepared.bytes().to_vec(),
                }];
                rows.extend(
                    build_rows(ledger_ref.clone())?
                        .into_iter()
                        .map(|(cf, key, value)| encode::WriteRow { cf, key, value }),
                );
                self.commit_rows_locked(&rows)?;
                return Ok(ledger_ref);
            };
            let guard = ledger_hook::lock_hook(hook)?;
            let staged = guard.stage_with_checkpoints(kind, subject, payload, actor)?;
            let ledger_ref = staged
                .first()
                .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
                .ledger_ref();
            let mut rows = staged
                .iter()
                .map(|row| encode::WriteRow {
                    cf: ColumnFamily::Ledger,
                    key: row.key().to_vec(),
                    value: row.value().to_vec(),
                })
                .collect::<Vec<_>>();
            rows.extend(
                build_rows(ledger_ref.clone())?
                    .into_iter()
                    .map(|(cf, key, value)| encode::WriteRow { cf, key, value }),
            );
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

    /// Decodes and admits one already-framed Ledger row against the exact
    /// append boundary currently protected by the durable commit lock.
    ///
    /// Both external adapters and the low-level `LedgerCfStore::put_new` seam
    /// use this validator so pre-encoded rows cannot bypass the semantic
    /// actor/payload checks performed by `LedgerAppender::prepare`, nor race a
    /// stale recovered tip between preparation and physical publication.
    pub(super) fn validate_decoded_ledger_append_locked(
        &self,
        operation: &'static str,
        requested_seq: u64,
        bytes: &[u8],
    ) -> Result<LedgerEntry> {
        let entry = calyx_ledger::decode(bytes)?;
        if entry.seq != requested_seq {
            return Err(CalyxError::ledger_append_only_violation(format!(
                "{operation}: Ledger row identity mismatch: requested_seq={requested_seq} encoded_seq={}",
                entry.seq
            )));
        }
        requested_seq.checked_add(1).ok_or_else(|| {
            CalyxError::ledger_chain_broken(format!(
                "{operation}: Ledger sequence {requested_seq} cannot be represented by the required post-commit head height"
            ))
        })?;

        RedactionPolicy::check_payload(&entry.payload)?;
        let appender = LedgerAppender::open(AsterRawLedgerStore { vault: self }, SystemClock)?;
        entry.actor.validate()?;

        let key = ledger_key(requested_seq);
        if self
            .read_cf_at(self.latest_seq(), ColumnFamily::Ledger, &key)?
            .is_some()
        {
            return Err(CalyxError::ledger_append_only_violation(format!(
                "{operation}: Ledger seq {requested_seq} already exists at the durable append boundary"
            )));
        }
        if requested_seq != appender.next_seq() || entry.prev_hash != appender.prev_hash() {
            return Err(CalyxError::ledger_chain_broken(format!(
                "{operation}: Ledger row is not the exact next chain member: requested_seq={requested_seq} authoritative_next_seq={} expected_prev={:02x?} actual_prev={:02x?}",
                appender.next_seq(),
                appender.prev_hash(),
                entry.prev_hash
            )));
        }
        if entry.ts <= appender.last_ts() {
            return Err(CalyxError::ledger_chain_broken(format!(
                "{operation}: Ledger timestamp must advance monotonically: seq={requested_seq} previous_ts={} incoming_ts={}",
                appender.last_ts(),
                entry.ts
            )));
        }
        Ok(entry)
    }

    /// Appends a ledger row prepared by an external adapter while keeping the
    /// vault-owned live ledger hook synchronized with the durable Ledger CF.
    pub fn append_external_ledger_row(&self, seq: u64, bytes: &[u8]) -> Result<()> {
        self.with_durable_commit_lock(|| {
            let entry = self.validate_decoded_ledger_append_locked(
                "append_external_ledger_row",
                seq,
                bytes,
            )?;
            let committed_head = if self.durable.is_none() {
                Some(LedgerHeadAnchor::new(
                    seq.checked_add(1).ok_or_else(|| {
                        CalyxError::ledger_chain_broken("ledger sequence exhausted")
                    })?,
                    entry.entry_hash,
                )?)
            } else {
                None
            };
            let key = ledger_key(seq);
            let rows = [encode::WriteRow {
                cf: ColumnFamily::Ledger,
                key,
                value: bytes.to_vec(),
            }];
            self.commit_rows_locked(&rows)?;
            if let Some(committed_head) = committed_head {
                self.validate_committed_ledger_head_anchor_locked(&committed_head)?;
                return Ok(());
            }
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
        let recovered = durable.recover_current_batches_under_commit_lock()?;
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
        // The repair below republishes both derived projections from recovered
        // physical truth through the free functions, which do not go through
        // the commit path's cached writers. Drop those handles and cached
        // anchors first, so the next commit reloads what recovery actually
        // wrote instead of trusting what it remembered across a boundary that
        // exists precisely because memory and disk may have diverged (#1947).
        if let Some(projections) = self
            .ledger_projections
            .lock()
            .map_err(|_| CalyxError::backpressure("Ledger projection writer mutex poisoned"))?
            .as_mut()
        {
            projections.reset();
        }
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

struct RawCommitmentCursor<'a> {
    stream: SnapshotCfRowStream<'a>,
    previous_seq: Option<u64>,
    count: u64,
    coverage_from_seq: Option<u64>,
}

impl<'a> RawCommitmentCursor<'a> {
    fn new(stream: SnapshotCfRowStream<'a>) -> Self {
        Self {
            stream,
            previous_seq: None,
            count: 0,
            coverage_from_seq: None,
        }
    }

    fn next(&mut self) -> Result<Option<raw_commitment::RawCommitment>> {
        let mut decoded = None;
        let present = self.stream.next_with(|key, value| {
            decoded = Some(raw_commitment::decode_commitment(key, value)?);
            Ok(())
        })?;
        if !present {
            return Ok(None);
        }
        let commitment = decoded.ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "raw commitment row stream reported a row without invoking its decoder",
            )
        })?;
        if let Some(previous) = self.previous_seq
            && previous >= commitment.seq
        {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "raw commitment CF sequence is not strictly ordered: {previous} then {}",
                commitment.seq
            )));
        }
        self.count = self.count.checked_add(1).ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "raw commitment physical row count exceeds the durable u64 limit",
            )
        })?;
        self.coverage_from_seq.get_or_insert(commitment.seq);
        self.previous_seq = Some(commitment.seq);
        Ok(Some(commitment))
    }
}

#[derive(Default)]
struct RawCommitmentVerificationState {
    seal_count: u64,
    sealed_commitment_count: u64,
    sealed_through_seq: Option<u64>,
    first_pending_seq: Option<u64>,
    failure: Option<String>,
    cursor_unreadable: bool,
}

impl RawCommitmentVerificationState {
    fn observe_ledger_row(
        &mut self,
        ledger_seq: u64,
        bytes: &[u8],
        cursor: &mut RawCommitmentCursor<'_>,
    ) -> Result<()> {
        if self.failure.is_some() {
            return Ok(());
        }
        let entry = match decode_ledger_entry_ref(bytes) {
            Ok(entry) => entry,
            Err(error) => {
                self.failure = Some(format!(
                    "raw commitment Ledger scan could not decode row {ledger_seq} with error[{}]: {}; the verifier cannot prove that the unreadable row is not a cohort seal",
                    error.code, error.message
                ));
                return Ok(());
            }
        };
        if entry.seq() != ledger_seq {
            self.failure = Some(format!(
                "raw commitment Ledger scan key seq {ledger_seq} does not match encoded seq {}",
                entry.seq()
            ));
            return Ok(());
        }
        let seal = match raw_commitment::ledger_seal_ref(&entry) {
            Ok(Some(seal)) => seal,
            Ok(None) => return Ok(()),
            Err(error) => {
                self.failure = Some(format!(
                    "raw commitment Ledger seal {ledger_seq} failed decode with error[{}]: {}",
                    error.code, error.message
                ));
                return Ok(());
            }
        };
        self.verify_seal(ledger_seq, seal, cursor);
        Ok(())
    }

    fn verify_seal(
        &mut self,
        ledger_seq: u64,
        seal: raw_commitment::RawCommitmentSeal,
        cursor: &mut RawCommitmentCursor<'_>,
    ) {
        let mut verifier = raw_commitment::StreamingSealVerifier::new(seal);
        let mut first_candidate_seq = None;
        for _ in 0..verifier.expected_count() {
            let commitment = match cursor.next() {
                Ok(Some(commitment)) => commitment,
                Ok(None) => break,
                Err(error) => {
                    self.failure = Some(format!(
                        "raw commitment CF decode/order failed while matching Ledger seal {ledger_seq} with error[{}]: {}",
                        error.code, error.message
                    ));
                    self.cursor_unreadable = true;
                    return;
                }
            };
            first_candidate_seq.get_or_insert(commitment.seq);
            if let Err(error) = verifier.push(&commitment) {
                self.failure = Some(format!(
                    "raw commitment Merkle stream failed while matching Ledger seal {ledger_seq} with error[{}]: {}",
                    error.code, error.message
                ));
                return;
            }
        }
        let verdict = match verifier.finish() {
            Ok(verdict) => verdict,
            Err(error) => {
                self.failure = Some(format!(
                    "raw commitment Merkle verdict failed for Ledger seal {ledger_seq} with error[{}]: {}",
                    error.code, error.message
                ));
                return;
            }
        };
        if !verdict.intact {
            self.first_pending_seq = first_candidate_seq;
            self.failure = Some(format!(
                "raw commitment Ledger seal {ledger_seq} does not match the physical commitment stream: {}",
                verdict
                    .failure
                    .unwrap_or_else(|| "streaming seal mismatch had no diagnostic".to_owned())
            ));
            return;
        }
        self.seal_count = match self.seal_count.checked_add(1) {
            Some(count) => count,
            None => {
                self.failure = Some(
                    "raw commitment Ledger seal count exceeds the durable u64 limit".to_owned(),
                );
                return;
            }
        };
        // An intact verdict consumed exactly the next sealed cohort. The raw
        // commit sequence is sparse, so the cursor's physical row count is the
        // only valid cumulative count; sequence distance is not a substitute.
        self.sealed_commitment_count = cursor.count;
        self.sealed_through_seq = verdict.last_physical_seq;
    }

    fn finish(mut self, cursor: &mut RawCommitmentCursor<'_>) -> AsterRawCommitmentVerification {
        if !self.cursor_unreadable {
            loop {
                match cursor.next() {
                    Ok(Some(commitment)) => {
                        self.first_pending_seq.get_or_insert(commitment.seq);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        if self.failure.is_none() {
                            self.failure = Some(format!(
                                "raw commitment CF trailing decode/order failed with error[{}]: {}",
                                error.code, error.message
                            ));
                        }
                        self.cursor_unreadable = true;
                        break;
                    }
                }
            }
        }
        AsterRawCommitmentVerification {
            intact: self.failure.is_none(),
            seal_count: self.seal_count,
            commitment_count: cursor.count,
            sealed_commitment_count: self.sealed_commitment_count,
            pending_commitment_count: cursor.count.saturating_sub(self.sealed_commitment_count),
            coverage_from_seq: cursor.coverage_from_seq,
            sealed_through_seq: self.sealed_through_seq,
            first_pending_seq: self.first_pending_seq,
            failure: self.failure,
        }
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
        self.vault
            .write_raw_ledger_row_without_hook(seq, bytes)
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
        self.vault.validate_committed_ledger_head_anchor(anchor)
    }
}
