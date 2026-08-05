use super::{AsterVault, encode, ledger_hook, raw_commitment};
use crate::cf::{ColumnFamily, anchor_key, base_key, ledger_key};
use crate::ledger_view::parse_aster_ledger_seq;
use calyx_core::{Anchor, CalyxError, Clock, CxId, LedgerRef, Result, SystemClock, VaultStore};
use calyx_ledger::{
    ActorId, EntryKind, ForgeBackend, LedgerAppender, LedgerCfStore, LedgerEntry, LedgerHeadAnchor,
    LedgerRow, LedgerSnapshot, QueryId, RedactionPolicy, ReproduceInputResolver,
    ReproduceLensRegistry, ReproduceResult, StagedLedgerRow, SubjectId, VerifyResult,
    decode as decode_ledger_entry, reproduce_payload_bytes, reproduce_verdict_with_input_resolver,
    reproduce_with_input_resolver, verify_snapshot,
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
        let store = AsterRawLedgerStore { vault: self };
        let (snapshot_seq, snapshot) = store.coherent_snapshot()?;
        let head = snapshot.head_anchor().cloned();
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
        let result = verify_snapshot(&snapshot, verified_range.clone())?;
        let raw_commitments = self.verify_raw_commitments(snapshot_seq, snapshot.rows())?;
        Ok(AsterLedgerChainVerification {
            result,
            head_height,
            tip_hash: head.map(|anchor| anchor.tip_hash),
            verified_range,
            raw_commitments,
        })
    }

    fn verify_raw_commitments(
        &self,
        snapshot_seq: u64,
        ledger_rows: &[LedgerRow],
    ) -> Result<AsterRawCommitmentVerification> {
        let rows = self.scan_cf_at(snapshot_seq, ColumnFamily::RawCommitment)?;
        let mut commitments = Vec::with_capacity(rows.len());
        for (key, value) in rows {
            match raw_commitment::decode_commitment(&key, &value) {
                Ok(commitment) => commitments.push(commitment),
                Err(error) => {
                    return Ok(raw_commitment_failure(
                        &commitments,
                        0,
                        0,
                        format!(
                            "raw commitment CF decode failed with error[{}]: {}",
                            error.code, error.message
                        ),
                    ));
                }
            }
        }
        commitments.sort_by_key(|commitment| commitment.seq);
        if let Some(window) = commitments
            .windows(2)
            .find(|window| window[0].seq >= window[1].seq)
        {
            return Ok(raw_commitment_failure(
                &commitments,
                0,
                0,
                format!(
                    "raw commitment CF sequence is not strictly ordered: {} then {}",
                    window[0].seq, window[1].seq
                ),
            ));
        }

        let mut cursor = 0_usize;
        let mut seal_count = 0_u64;
        let mut sealed_through_seq = None;
        for ledger_row in ledger_rows {
            let entry = match decode_ledger_entry(&ledger_row.bytes) {
                Ok(entry) => entry,
                // The primary Ledger verifier already classifies malformed
                // entry bytes. Avoid inventing a second, less precise error.
                Err(_) => continue,
            };
            let seal = match raw_commitment::ledger_seal(&entry) {
                Ok(Some(seal)) => seal,
                Ok(None) => continue,
                Err(error) => {
                    return Ok(raw_commitment_failure(
                        &commitments,
                        seal_count,
                        cursor,
                        format!(
                            "raw commitment Ledger seal {} failed decode with error[{}]: {}",
                            entry.seq, error.code, error.message
                        ),
                    ));
                }
            };
            let cohort_len = match usize::try_from(seal.commitment_count) {
                Ok(count) => count,
                Err(_) => {
                    return Ok(raw_commitment_failure(
                        &commitments,
                        seal_count,
                        cursor,
                        format!(
                            "raw commitment Ledger seal {} count {} does not fit this host",
                            entry.seq, seal.commitment_count
                        ),
                    ));
                }
            };
            let Some(end) = cursor.checked_add(cohort_len) else {
                return Ok(raw_commitment_failure(
                    &commitments,
                    seal_count,
                    cursor,
                    format!(
                        "raw commitment Ledger seal {} count overflows the verification cursor",
                        entry.seq
                    ),
                ));
            };
            let Some(cohort) = commitments.get(cursor..end) else {
                return Ok(raw_commitment_failure(
                    &commitments,
                    seal_count,
                    cursor,
                    // Issue #1876: how many rows vanished is not where they
                    // vanished. Pin the position against the sealed ladder.
                    format!(
                        "raw commitment Ledger seal {} claims {} rows but only {} remain in the physical commitment CF: {}",
                        entry.seq,
                        seal.commitment_count,
                        commitments.len().saturating_sub(cursor),
                        raw_commitment::describe_truncated_cohort(
                            &seal,
                            commitments.get(cursor..).unwrap_or(&[])
                        )
                    ),
                ));
            };
            if !raw_commitment::seal_matches(&seal, cohort)? {
                return Ok(raw_commitment_failure(
                    &commitments,
                    seal_count,
                    cursor,
                    // Issue #1876: the cohort range alone gave the operator a
                    // span to hand-search. Decompose the divergence and name the
                    // offending sequence(s). Failure path only, so the extra
                    // hashing costs nothing on an intact vault.
                    format!(
                        "raw commitment Ledger seal {} does not match physical commitment rows {}..={} count={}: {}",
                        entry.seq,
                        seal.first_seq,
                        seal.last_seq,
                        seal.commitment_count,
                        raw_commitment::describe_mismatch(&seal, cohort)
                    ),
                ));
            }
            cursor = end;
            seal_count = seal_count.saturating_add(1);
            sealed_through_seq = Some(seal.last_seq);
        }

        Ok(AsterRawCommitmentVerification {
            intact: true,
            seal_count,
            commitment_count: usize_to_u64(commitments.len()),
            sealed_commitment_count: usize_to_u64(cursor),
            pending_commitment_count: usize_to_u64(commitments.len().saturating_sub(cursor)),
            coverage_from_seq: commitments.first().map(|commitment| commitment.seq),
            sealed_through_seq,
            first_pending_seq: commitments.get(cursor).map(|commitment| commitment.seq),
            failure: None,
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

fn raw_commitment_failure(
    commitments: &[raw_commitment::RawCommitment],
    seal_count: u64,
    sealed_count: usize,
    failure: String,
) -> AsterRawCommitmentVerification {
    AsterRawCommitmentVerification {
        intact: false,
        seal_count,
        commitment_count: usize_to_u64(commitments.len()),
        sealed_commitment_count: usize_to_u64(sealed_count),
        pending_commitment_count: usize_to_u64(commitments.len().saturating_sub(sealed_count)),
        coverage_from_seq: commitments.first().map(|commitment| commitment.seq),
        sealed_through_seq: sealed_count
            .checked_sub(1)
            .and_then(|index| commitments.get(index))
            .map(|commitment| commitment.seq),
        first_pending_seq: commitments
            .get(sealed_count)
            .map(|commitment| commitment.seq),
        failure: Some(failure),
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
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

impl<C> AsterRawLedgerStore<'_, C>
where
    C: Clock,
{
    fn coherent_snapshot(&self) -> Result<(u64, LedgerSnapshot<'static>)> {
        let durable = self.vault.durable.as_ref().ok_or_else(|| CalyxError {
            code: "CALYX_ASTER_LEDGER_VERIFY_UNAVAILABLE",
            message: "ledger snapshot requires a durable vault handle".to_owned(),
            remediation: "verify through a write-capable handle or use verify_restore",
        })?;
        let _commit_guard = crate::file_lock::FileLockGuard::acquire(
            &durable.root().join("locks").join("durable.commit.lock"),
        )?;
        let snapshot_seq = self.vault.snapshot();
        let mut rows = Vec::new();
        for (key, bytes) in self.vault.scan_cf_at(snapshot_seq, ColumnFamily::Ledger)? {
            rows.push(LedgerRow {
                seq: parse_aster_ledger_seq(&key)?,
                bytes,
            });
        }
        rows.sort_by_key(|row| row.seq);
        let anchor = crate::ledger_head::read_head_anchor(durable.root())?;
        let anchor =
            crate::ledger_head::require_head_anchor_for_rows(durable.root(), anchor, &rows)?;
        Ok((snapshot_seq, LedgerSnapshot::owned(rows, anchor)))
    }
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
