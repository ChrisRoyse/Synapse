//! Exact source-event-time secondary index.
//!
//! Bounded temporal intelligence must be proportional to the requested event
//! window, not to the complete panel population.  The ordered IndexBtree rows
//! here are therefore maintained in the same commit as Base mutations.  A
//! per-panel marker is published only after a full historical backfill has
//! independently reconciled the Base and index key multisets.  Readers compare
//! that marker with the panel-content watermark and fail closed on absence or
//! lag; there is deliberately no population-scan fallback.

use super::{AsterVault, encode};
use crate::cf::{ColumnFamily, KeyRange, SlotFamilyKind, prefix_range};
use crate::mvcc::{Freshness, Snapshot, is_tombstone_value, tombstone_value};
use calyx_core::{CalyxError, Clock, CxId, Result, Seq};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const CALYX_EVENT_TIME_INDEX_INCOMPLETE: &str = "CALYX_EVENT_TIME_INDEX_INCOMPLETE";
pub const CALYX_EVENT_TIME_INDEX_STALE: &str = "CALYX_EVENT_TIME_INDEX_STALE";
pub const CALYX_EVENT_TIME_INDEX_INVALID: &str = "CALYX_EVENT_TIME_INDEX_INVALID";

const INDEX_PREFIX: &[u8] = b"\x12calyx-event-time-v1\0";
const MARKER_PREFIX: &[u8] = b"\x13calyx-event-time-marker-v1\0";
const INDEX_VALUE: &[u8] = b"\x01";
const MARKER_VERSION: u8 = 1;
const CX_ID_BYTES: usize = 16;
const INDEX_BATCH_ROWS: usize = 1_024;
const SCAN_PAGE_ROWS: usize = 1_024;
/// Maximum panel-scoped Base delta admitted while the final marker commit owns
/// the durable writer boundary.  The complete historical proof runs outside
/// the lock; this cap prevents an unexpectedly hot panel from turning the
/// delta catch-up into another unbounded stop-the-world scan.
const PUBLISH_DELTA_MAX_KEYS: usize = 4_096;
const BACKFILL_LEASE_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventTimeIndexStatus {
    pub panel_version: u32,
    pub complete: bool,
    pub state: &'static str,
    pub snapshot_seq: Seq,
    pub panel_content_seq: Seq,
    pub marker_content_seq: Option<Seq>,
    pub indexed_records: Option<u64>,
    pub fingerprint_sha256: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventTimeIndexBackfill {
    pub panel_version: u32,
    pub already_complete: bool,
    pub base_rows_scanned: u64,
    pub panel_rows_scanned: u64,
    pub temporal_rows_indexed: u64,
    pub index_batches_committed: u64,
    pub verification_attempts: u32,
    pub status: EventTimeIndexStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventTimeIndexEntry {
    pub source_event_ns: u64,
    pub cx_id: CxId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventTimeIndexRange {
    pub status: EventTimeIndexStatus,
    pub entries: Vec<EventTimeIndexEntry>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fingerprint {
    xor: [u8; 32],
    sum: [u8; 32],
}

impl Fingerprint {
    fn add(&mut self, key: &[u8]) {
        let digest: [u8; 32] = Sha256::digest(key).into();
        for (left, right) in self.xor.iter_mut().zip(digest) {
            *left ^= right;
        }
        add_be(&mut self.sum, &digest);
    }

    fn remove(&mut self, key: &[u8]) {
        let digest: [u8; 32] = Sha256::digest(key).into();
        for (left, right) in self.xor.iter_mut().zip(digest) {
            *left ^= right;
        }
        sub_be(&mut self.sum, &digest);
    }

    fn seal(self, count: u64) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.xor);
        hasher.update(self.sum);
        hasher.update(count.to_be_bytes());
        hasher.finalize().into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Marker {
    panel_version: u32,
    panel_content_seq: Seq,
    indexed_records: u64,
    fingerprint: Fingerprint,
}

#[derive(Clone, Debug)]
struct BaseEvent {
    panel_version: u32,
    source_event_ns: Option<u64>,
    cx_id: CxId,
}

pub(super) fn is_reserved_row(row: &encode::WriteRow) -> bool {
    row.cf == ColumnFamily::IndexBtree
        && (row.key.starts_with(INDEX_PREFIX) || row.key.starts_with(MARKER_PREFIX))
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    pub fn event_time_index_status(&self, panel_version: u32) -> Result<EventTimeIndexStatus> {
        validate_panel(panel_version)?;
        self.with_scoped_latest_snapshot_for_panel(
            panel_version,
            Freshness::FreshDerived,
            BACKFILL_LEASE_MS,
            |error| error,
            |snapshot| self.event_time_index_status_snapshot(snapshot, panel_version),
        )
    }

    pub fn event_time_index_status_snapshot(
        &self,
        snapshot: Snapshot,
        panel_version: u32,
    ) -> Result<EventTimeIndexStatus> {
        validate_panel(panel_version)?;
        let marker = self
            .read_cf_snapshot(
                snapshot,
                ColumnFamily::IndexBtree,
                &marker_key(panel_version),
            )?
            .map(|bytes| decode_marker(&bytes))
            .transpose()?;
        if let Some(marker) = marker
            && marker.panel_version != panel_version
        {
            return Err(CalyxError {
                code: CALYX_EVENT_TIME_INDEX_INVALID,
                message: format!(
                    "event-time marker key for panel {panel_version} contains marker panel {}",
                    marker.panel_version
                ),
                remediation: "preserve the mismatched marker bytes and rebuild this derived panel index before reading it",
            });
        }
        let panel_content_seq = snapshot.derived_content_seq();
        let complete = marker.is_some_and(|marker| marker.panel_content_seq == panel_content_seq);
        let state = match marker {
            None => "missing",
            Some(marker) if marker.panel_content_seq == panel_content_seq => "complete",
            Some(_) => "stale",
        };
        Ok(EventTimeIndexStatus {
            panel_version,
            complete,
            state,
            snapshot_seq: snapshot.seq(),
            panel_content_seq,
            marker_content_seq: marker.map(|marker| marker.panel_content_seq),
            indexed_records: marker.map(|marker| marker.indexed_records),
            fingerprint_sha256: marker
                .map(|marker| marker.fingerprint.seal(marker.indexed_records)),
        })
    }

    /// Resumable historical population followed by an independent physical
    /// Base-vs-IndexBtree reconciliation.  A crash before marker publication
    /// leaves the index unavailable and a retry resumes idempotently.
    pub fn backfill_event_time_index(&self, panel_version: u32) -> Result<EventTimeIndexBackfill> {
        validate_panel(panel_version)?;
        let before = self.event_time_index_status(panel_version)?;
        if before.complete {
            return Ok(EventTimeIndexBackfill {
                panel_version,
                already_complete: true,
                base_rows_scanned: 0,
                panel_rows_scanned: 0,
                temporal_rows_indexed: 0,
                index_batches_committed: 0,
                verification_attempts: 0,
                status: before,
            });
        }
        tracing::info!(
            code = "CALYX_EVENT_TIME_INDEX_BACKFILL_STARTED",
            panel_version,
            before_state = before.state,
            before_marker_content_seq = ?before.marker_content_seq,
            before_panel_content_seq = before.panel_content_seq,
            "starting resumable historical event-time index population"
        );

        let mut base_rows_scanned = 0_u64;
        let mut panel_rows_scanned = 0_u64;
        let mut temporal_rows_indexed = 0_u64;
        let mut index_batches_committed = 0_u64;
        self.with_scoped_latest_snapshot_for_panel(
            panel_version,
            Freshness::FreshDerived,
            BACKFILL_LEASE_MS,
            |error| error,
            |mut snapshot| {
                let range = KeyRange::all();
                let mut after = None::<Vec<u8>>;
                let mut pending_temporal_keys = Vec::with_capacity(INDEX_BATCH_ROWS);
                loop {
                    let page = self.scan_cf_range_page_snapshot(
                        snapshot,
                        ColumnFamily::Base,
                        &range,
                        after.as_deref(),
                        SCAN_PAGE_ROWS,
                    )?;
                    if page.is_empty() {
                        break;
                    }
                    base_rows_scanned = base_rows_scanned
                        .checked_add(page.len() as u64)
                        .ok_or_else(count_overflow)?;
                    for (key, value) in &page {
                        let projection = decode_base_event(key, value)?;
                        if projection.panel_version == panel_version {
                            panel_rows_scanned = panel_rows_scanned
                                .checked_add(1)
                                .ok_or_else(count_overflow)?;
                            if projection.source_event_ns.is_some() {
                                temporal_rows_indexed = temporal_rows_indexed
                                    .checked_add(1)
                                    .ok_or_else(count_overflow)?;
                                pending_temporal_keys.push(key.clone());
                                if pending_temporal_keys.len() == INDEX_BATCH_ROWS {
                                    if self.backfill_event_time_keys_locked(
                                        panel_version,
                                        &pending_temporal_keys,
                                    )? {
                                        index_batches_committed = index_batches_committed
                                            .checked_add(1)
                                            .ok_or_else(count_overflow)?;
                                    }
                                    pending_temporal_keys.clear();
                                }
                            }
                        }
                    }
                    after = page.last().map(|(key, _)| key.clone());
                    snapshot = self.renew_reader(snapshot)?;
                }
                if !pending_temporal_keys.is_empty()
                    && self
                        .backfill_event_time_keys_locked(panel_version, &pending_temporal_keys)?
                {
                    index_batches_committed = index_batches_committed
                        .checked_add(1)
                        .ok_or_else(count_overflow)?;
                }
                Ok(())
            },
        )?;

        // Verify the complete historical population once, then carry that
        // proof to the current commit boundary by reconciling only the exact
        // panel-scoped Base delta.  A busy panel never has to become quiet for
        // the duration of a second whole-corpus scan.
        self.verify_and_publish_event_time_index(panel_version)?;
        let verification_attempts = 1_u32;
        let status = self.event_time_index_status(panel_version)?;
        if !status.complete {
            return Err(stale_status_error(&status));
        }
        let report = EventTimeIndexBackfill {
            panel_version,
            already_complete: false,
            base_rows_scanned,
            panel_rows_scanned,
            temporal_rows_indexed,
            index_batches_committed,
            verification_attempts,
            status,
        };
        tracing::info!(
            code = "CALYX_EVENT_TIME_INDEX_BACKFILL_COMPLETED",
            panel_version,
            base_rows_scanned = report.base_rows_scanned,
            panel_rows_scanned = report.panel_rows_scanned,
            temporal_rows_indexed = report.temporal_rows_indexed,
            index_batches_committed = report.index_batches_committed,
            verification_attempts = report.verification_attempts,
            panel_content_seq = report.status.panel_content_seq,
            indexed_records = ?report.status.indexed_records,
            fingerprint_sha256 = ?report.status.fingerprint_sha256,
            "published independently reconciled event-time index completeness"
        );
        Ok(report)
    }

    pub fn read_event_time_index_range_snapshot(
        &self,
        snapshot: Snapshot,
        panel_version: u32,
        since_ts_ns: Option<u64>,
        until_ts_ns: Option<u64>,
        max_records: usize,
    ) -> Result<EventTimeIndexRange> {
        validate_panel(panel_version)?;
        if max_records == 0 {
            return Err(CalyxError {
                code: CALYX_EVENT_TIME_INDEX_INVALID,
                message: "event-time index max_records must be positive".to_owned(),
                remediation: "supply a positive max_records bound",
            });
        }
        if let (Some(since), Some(until)) = (since_ts_ns, until_ts_ns)
            && since >= until
        {
            return Err(CalyxError {
                code: CALYX_EVENT_TIME_INDEX_INVALID,
                message: format!(
                    "event-time range is empty or inverted: since_ts_ns={since} until_ts_ns={until}"
                ),
                remediation: "supply an inclusive since_ts_ns strictly below the exclusive until_ts_ns",
            });
        }
        let status = self.event_time_index_status_snapshot(snapshot, panel_version)?;
        if !status.complete {
            return Err(stale_status_error(&status));
        }
        let range = event_range(panel_version, since_ts_ns, until_ts_ns);
        let limit = max_records.checked_add(1).ok_or_else(count_overflow)?;
        let rows = self.scan_cf_range_page_snapshot(
            snapshot,
            ColumnFamily::IndexBtree,
            &range,
            None,
            limit,
        )?;
        if rows.len() > max_records {
            return Err(CalyxError {
                code: "CALYX_EVENT_TIME_INDEX_SCOPE_EXCEEDS_MAX_RECORDS",
                message: format!(
                    "panel {panel_version} event-time window contains more than max_records={max_records}; returning a prefix would bias the measurement"
                ),
                remediation: "narrow the event-time window or raise max_records within the caller's declared hard bound",
            });
        }
        let entries = rows
            .into_iter()
            .map(|(key, value)| {
                validate_index_value(&key, &value)?;
                decode_index_key(&key, panel_version)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(EventTimeIndexRange { status, entries })
    }

    pub(super) fn augment_event_time_index_rows_locked(
        &self,
        rows: &[encode::WriteRow],
        predicted_seq: Seq,
    ) -> Result<Vec<encode::WriteRow>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut expanded = rows.to_vec();
        let current_watermarks = self.rows.panel_content_seqs_snapshot()?;
        let mut staged_bases = BTreeMap::<Vec<u8>, Option<BaseEvent>>::new();
        let mut affected = BTreeSet::new();
        let mut deltas = BTreeMap::<u32, i128>::new();
        let mut removals = BTreeMap::<u32, Vec<Vec<u8>>>::new();
        let mut additions = BTreeMap::<u32, Vec<Vec<u8>>>::new();
        let mut witnesses = BTreeMap::<u32, Vec<Vec<u8>>>::new();

        for row in rows.iter().filter(|row| row.cf == ColumnFamily::Base) {
            if staged_bases.contains_key(&row.key) {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "atomic batch contains duplicate Base writes for one CxId key (key_len={})",
                    row.key.len()
                )));
            }
            let prior = self
                .read_cf_latest(ColumnFamily::Base, &row.key)?
                .map(|value| decode_base_event(&row.key, &value))
                .transpose()?
                .map(BaseEvent::from);
            let next = if is_tombstone_value(&row.value) {
                None
            } else {
                Some(BaseEvent::from(decode_base_event(&row.key, &row.value)?))
            };
            if let Some(prior) = &prior {
                affected.insert(prior.panel_version);
            }
            if let Some(next) = &next {
                affected.insert(next.panel_version);
            }
            let prior_key = prior.as_ref().and_then(BaseEvent::index_key);
            let next_key = next.as_ref().and_then(BaseEvent::index_key);
            if prior_key != next_key {
                if let (Some(prior), Some(key)) = (&prior, prior_key) {
                    expanded.push(encode::WriteRow {
                        cf: ColumnFamily::IndexBtree,
                        key: key.clone(),
                        value: tombstone_value().to_vec(),
                    });
                    *deltas.entry(prior.panel_version).or_default() -= 1;
                    removals.entry(prior.panel_version).or_default().push(key);
                }
                if let (Some(next), Some(key)) = (&next, next_key) {
                    expanded.push(encode::WriteRow {
                        cf: ColumnFamily::IndexBtree,
                        key: key.clone(),
                        value: INDEX_VALUE.to_vec(),
                    });
                    *deltas.entry(next.panel_version).or_default() += 1;
                    additions.entry(next.panel_version).or_default().push(key);
                }
            } else if let (Some(next), Some(key)) = (&next, next_key) {
                witnesses.entry(next.panel_version).or_default().push(key);
            }
            staged_bases.insert(row.key.clone(), next);
        }

        for row in rows.iter().filter(|row| {
            matches!(
                row.cf,
                ColumnFamily::Slot {
                    kind: SlotFamilyKind::Quantized,
                    ..
                }
            )
        }) {
            let panel = match staged_bases.get(&row.key) {
                Some(Some(base)) => Some(base.panel_version),
                Some(None) => None,
                None => self
                    .read_cf_latest(ColumnFamily::Base, &row.key)?
                    .map(|value| decode_base_event(&row.key, &value))
                    .transpose()?
                    .map(|base| base.panel_version),
            };
            if let Some(panel) = panel {
                affected.insert(panel);
            }
        }

        for panel_version in affected {
            let key = marker_key(panel_version);
            let Some(bytes) = self.read_cf_latest(ColumnFamily::IndexBtree, &key)? else {
                continue;
            };
            let mut marker = decode_marker(&bytes)?;
            let current = current_watermarks
                .get(&panel_version)
                .copied()
                .unwrap_or_default();
            if marker.panel_content_seq != current {
                tracing::error!(
                    code = CALYX_EVENT_TIME_INDEX_STALE,
                    panel_version,
                    marker_content_seq = marker.panel_content_seq,
                    panel_content_seq = current,
                    "event-time index marker was already stale before this commit; preserving its stale watermark so bounded readers continue to fail closed"
                );
                continue;
            }
            if let Some(keys) = removals.get(&panel_version) {
                for index_key in keys {
                    let value = self
                        .read_cf_latest(ColumnFamily::IndexBtree, index_key)?
                        .ok_or_else(|| missing_index_row(panel_version, index_key))?;
                    validate_index_value(index_key, &value)?;
                    marker.fingerprint.remove(index_key);
                }
            }
            if let Some(keys) = witnesses.get(&panel_version) {
                for index_key in keys {
                    let value = self
                        .read_cf_latest(ColumnFamily::IndexBtree, index_key)?
                        .ok_or_else(|| missing_index_row(panel_version, index_key))?;
                    validate_index_value(index_key, &value)?;
                }
            }
            if let Some(keys) = additions.get(&panel_version) {
                for index_key in keys {
                    marker.fingerprint.add(index_key);
                }
            }
            let delta = deltas.get(&panel_version).copied().unwrap_or_default();
            marker.indexed_records = apply_count_delta(marker.indexed_records, delta)?;
            marker.panel_content_seq = predicted_seq;
            expanded.push(encode::WriteRow {
                cf: ColumnFamily::IndexBtree,
                key,
                value: encode_marker(marker),
            });
        }
        Ok(expanded)
    }

    fn backfill_event_time_keys_locked(
        &self,
        panel_version: u32,
        keys: &[Vec<u8>],
    ) -> Result<bool> {
        if keys.len() > INDEX_BATCH_ROWS {
            return Err(CalyxError {
                code: CALYX_EVENT_TIME_INDEX_INVALID,
                message: format!(
                    "event-time backfill batch has {} keys, exceeding {INDEX_BATCH_ROWS}",
                    keys.len()
                ),
                remediation: "split the historical backfill into bounded batches",
            });
        }
        self.with_durable_commit_lock(|| {
            let mut rows = Vec::new();
            for key in keys {
                let Some(value) = self.read_cf_latest(ColumnFamily::Base, key)? else {
                    continue;
                };
                let base = BaseEvent::from(decode_base_event(key, &value)?);
                if base.panel_version == panel_version
                    && let Some(key) = base.index_key()
                {
                    if let Some(value) = self.read_cf_latest(ColumnFamily::IndexBtree, &key)? {
                        validate_index_value(&key, &value)?;
                        continue;
                    }
                    rows.push(encode::WriteRow {
                        cf: ColumnFamily::IndexBtree,
                        key,
                        value: INDEX_VALUE.to_vec(),
                    });
                }
            }
            if rows.is_empty() {
                return Ok(false);
            }
            self.commit_rows_locked_inner(&rows)?;
            Ok(true)
        })
    }

    fn verify_and_publish_event_time_index(&self, panel_version: u32) -> Result<()> {
        self.with_scoped_latest_snapshot_for_panel(
            panel_version,
            Freshness::FreshDerived,
            BACKFILL_LEASE_MS,
            |error| error,
            |mut snapshot| {
                let mut expected_count = 0_u64;
                let mut expected = Fingerprint::default();
                let mut after = None::<Vec<u8>>;
                loop {
                    let page = self.scan_cf_range_page_snapshot(
                        snapshot,
                        ColumnFamily::Base,
                        &KeyRange::all(),
                        after.as_deref(),
                        SCAN_PAGE_ROWS,
                    )?;
                    if page.is_empty() {
                        break;
                    }
                    for (key, value) in &page {
                        let base = BaseEvent::from(decode_base_event(key, value)?);
                        if base.panel_version == panel_version
                            && let Some(key) = base.index_key()
                        {
                            expected.add(&key);
                            expected_count = expected_count
                                .checked_add(1)
                                .ok_or_else(count_overflow)?;
                        }
                    }
                    after = page.last().map(|(key, _)| key.clone());
                    snapshot = self.renew_reader(snapshot)?;
                }

                let mut actual_count = 0_u64;
                let mut actual = Fingerprint::default();
                let range = event_range(panel_version, None, None);
                let mut after = None::<Vec<u8>>;
                loop {
                    let page = self.scan_cf_range_page_snapshot(
                        snapshot,
                        ColumnFamily::IndexBtree,
                        &range,
                        after.as_deref(),
                        SCAN_PAGE_ROWS,
                    )?;
                    if page.is_empty() {
                        break;
                    }
                    for (key, value) in &page {
                        validate_index_value(key, value)?;
                        let _ = decode_index_key(key, panel_version)?;
                        actual.add(key);
                        actual_count = actual_count.checked_add(1).ok_or_else(count_overflow)?;
                    }
                    after = page.last().map(|(key, _)| key.clone());
                    snapshot = self.renew_reader(snapshot)?;
                }
                if expected_count != actual_count || expected != actual {
                    return Err(CalyxError {
                        code: CALYX_EVENT_TIME_INDEX_INCOMPLETE,
                        message: format!(
                            "panel {panel_version} event-time index reconciliation failed at snapshot {}: expected_rows={expected_count} actual_rows={actual_count} expected_fingerprint={} actual_fingerprint={}",
                            snapshot.seq(),
                            hex(&expected.seal(expected_count)),
                            hex(&actual.seal(actual_count)),
                        ),
                        remediation: "retry the resumable backfill; if the mismatch persists, inspect the exact Base and IndexBtree rows before allowing bounded temporal reads",
                    });
                }
                self.publish_event_time_marker_with_delta_locked(
                    panel_version,
                    snapshot,
                    actual_count,
                    actual,
                )
            },
        )
    }

    /// Seals a fully verified historical snapshot at the latest panel
    /// watermark without requiring a quiet writer window.
    ///
    /// The caller proved Base/index equality at `verified_snapshot`.  Under the
    /// durable commit lock, this method pins the exact current view, enumerates
    /// every panel Base key changed between the two sequences, validates the
    /// transactionally maintained IndexBtree state for those keys, applies
    /// their exact fingerprint/count delta, and commits the marker.  Unchanged
    /// keys retain the historical proof; changed keys are reproved physically.
    fn publish_event_time_marker_with_delta_locked(
        &self,
        panel_version: u32,
        verified_snapshot: Snapshot,
        indexed_records: u64,
        fingerprint: Fingerprint,
    ) -> Result<()> {
        self.with_durable_commit_lock(|| {
            self.with_scoped_latest_snapshot_for_panel(
                panel_version,
                Freshness::FreshDerived,
                BACKFILL_LEASE_MS,
                |error| error,
                |current_snapshot| {
                    let changed = self.changed_base_keys_after_snapshot_for_panel(
                        current_snapshot,
                        verified_snapshot.seq(),
                        panel_version,
                    )?;
                    if changed.keys.len() > PUBLISH_DELTA_MAX_KEYS {
                        return Err(CalyxError {
                            code: CALYX_EVENT_TIME_INDEX_STALE,
                            message: format!(
                                "panel {panel_version} changed by {} Base keys between verified seq {} and current seq {}, exceeding the bounded marker catch-up limit {PUBLISH_DELTA_MAX_KEYS}",
                                changed.keys.len(),
                                verified_snapshot.seq(),
                                current_snapshot.seq(),
                            ),
                            remediation: "retry the resumable backfill so the full verification snapshot is closer to the current panel watermark; do not publish an unbounded or partial delta",
                        });
                    }

                    let mut current_count = indexed_records;
                    let mut current_fingerprint = fingerprint;
                    for key in changed.keys {
                        let before = self
                            .read_cf_snapshot(verified_snapshot, ColumnFamily::Base, &key)?
                            .map(|value| decode_base_event(&key, &value))
                            .transpose()?
                            .map(BaseEvent::from)
                            .filter(|base| base.panel_version == panel_version)
                            .and_then(|base| base.index_key());
                        let after = self
                            .read_cf_snapshot(current_snapshot, ColumnFamily::Base, &key)?
                            .map(|value| decode_base_event(&key, &value))
                            .transpose()?
                            .map(BaseEvent::from)
                            .filter(|base| base.panel_version == panel_version)
                            .and_then(|base| base.index_key());

                        if before == after {
                            if let Some(index_key) = &after {
                                let value = self
                                    .read_cf_snapshot(
                                        current_snapshot,
                                        ColumnFamily::IndexBtree,
                                        index_key,
                                    )?
                                    .ok_or_else(|| missing_index_row(panel_version, index_key))?;
                                validate_index_value(index_key, &value)?;
                            }
                            continue;
                        }
                        if let Some(index_key) = &before {
                            if self
                                .read_cf_snapshot(
                                    current_snapshot,
                                    ColumnFamily::IndexBtree,
                                    index_key,
                                )?
                                .is_some()
                            {
                                return Err(CalyxError {
                                    code: CALYX_EVENT_TIME_INDEX_INCOMPLETE,
                                    message: format!(
                                        "panel {panel_version} delta catch-up found a superseded event-time index row still live at current seq {}",
                                        current_snapshot.seq()
                                    ),
                                    remediation: "preserve the Base and IndexBtree rows for this key, repair the atomic Base/index writer, and rerun the exact historical backfill",
                                });
                            }
                            current_fingerprint.remove(index_key);
                            current_count = current_count.checked_sub(1).ok_or_else(count_overflow)?;
                        }
                        if let Some(index_key) = &after {
                            let value = self
                                .read_cf_snapshot(
                                    current_snapshot,
                                    ColumnFamily::IndexBtree,
                                    index_key,
                                )?
                                .ok_or_else(|| missing_index_row(panel_version, index_key))?;
                            validate_index_value(index_key, &value)?;
                            current_fingerprint.add(index_key);
                            current_count = current_count.checked_add(1).ok_or_else(count_overflow)?;
                        }
                    }

                    self.commit_rows_locked_inner(&[encode::WriteRow {
                        cf: ColumnFamily::IndexBtree,
                        key: marker_key(panel_version),
                        value: encode_marker(Marker {
                            panel_version,
                            panel_content_seq: current_snapshot.derived_content_seq(),
                            indexed_records: current_count,
                            fingerprint: current_fingerprint,
                        }),
                    }])?;
                    Ok(())
                },
            )
        })
    }
}

#[derive(Clone, Debug)]
struct DecodedBaseEvent {
    panel_version: u32,
    source_event_ns: Option<u64>,
    cx_id: CxId,
}

impl From<DecodedBaseEvent> for BaseEvent {
    fn from(value: DecodedBaseEvent) -> Self {
        Self {
            panel_version: value.panel_version,
            source_event_ns: value.source_event_ns,
            cx_id: value.cx_id,
        }
    }
}

impl BaseEvent {
    fn index_key(&self) -> Option<Vec<u8>> {
        self.source_event_ns
            .map(|source_event_ns| index_key(self.panel_version, source_event_ns, self.cx_id))
    }
}

fn decode_base_event(key: &[u8], value: &[u8]) -> Result<DecodedBaseEvent> {
    let base = encode::decode_constellation_base_projection(value)?;
    if base.cx_id.as_bytes() != key {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "Base row key/header CxId mismatch while deriving event-time index (key_len={})",
            key.len()
        )));
    }
    let inactive = base
        .metadata
        .get(calyx_core::METADATA_TEMPORAL_LANE_STATE)
        .is_some_and(|value| value == calyx_core::TEMPORAL_LANE_INACTIVE);
    let source_event_ns = if inactive {
        None
    } else {
        let raw = base
            .metadata
            .get(calyx_core::METADATA_SOURCE_EVENT_TIME_RAW);
        let seconds = base
            .metadata
            .get(calyx_core::METADATA_SOURCE_EVENT_TIME_SECS);
        match (raw, seconds) {
            (None, None) => None,
            (Some(_), None) | (None, Some(_)) => {
                return Err(CalyxError {
                    code: CALYX_EVENT_TIME_INDEX_INVALID,
                    message: format!(
                        "Base row {} has only one of source_event_time_raw/source_event_time_secs; the canonical writer stores both atomically",
                        base.cx_id
                    ),
                    remediation: "repair the Base temporal metadata through the canonical temporal backfill before rebuilding the event-time index",
                });
            }
            (Some(raw), Some(seconds)) => {
                let nanos = raw.parse::<u64>().map_err(|error| CalyxError {
                    code: CALYX_EVENT_TIME_INDEX_INVALID,
                    message: format!(
                        "Base row {} has invalid integer source_event_time_raw {raw:?}: {error}",
                        base.cx_id
                    ),
                    remediation: "repair the Base temporal metadata through the canonical temporal backfill before rebuilding the event-time index",
                })?;
                let seconds = seconds.parse::<i64>().map_err(|error| CalyxError {
                    code: CALYX_EVENT_TIME_INDEX_INVALID,
                    message: format!(
                        "Base row {} has invalid integer source_event_time_secs {seconds:?}: {error}",
                        base.cx_id
                    ),
                    remediation: "repair the Base temporal metadata through the canonical temporal backfill before rebuilding the event-time index",
                })?;
                let nanos_seconds = i64::try_from(nanos / 1_000_000_000).map_err(|_| {
                    CalyxError {
                        code: CALYX_EVENT_TIME_INDEX_INVALID,
                        message: format!(
                            "Base row {} source_event_time_raw exceeds the signed seconds domain",
                            base.cx_id
                        ),
                        remediation: "repair the out-of-domain source timestamp before rebuilding the event-time index",
                    }
                })?;
                if nanos_seconds != seconds {
                    return Err(CalyxError {
                        code: CALYX_EVENT_TIME_INDEX_INVALID,
                        message: format!(
                            "Base row {} temporal stamps disagree: source_event_time_raw/1e9={nanos_seconds} source_event_time_secs={seconds}",
                            base.cx_id
                        ),
                        remediation: "establish the authoritative source timestamp and repair both Base temporal fields atomically before rebuilding the event-time index",
                    });
                }
                Some(nanos)
            }
        }
    };
    Ok(DecodedBaseEvent {
        panel_version: base.panel_version,
        source_event_ns,
        cx_id: base.cx_id,
    })
}

fn index_key(panel_version: u32, source_event_ns: u64, cx_id: CxId) -> Vec<u8> {
    let mut key = Vec::with_capacity(INDEX_PREFIX.len() + 4 + 8 + CX_ID_BYTES);
    key.extend_from_slice(INDEX_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key.extend_from_slice(&source_event_ns.to_be_bytes());
    key.extend_from_slice(cx_id.as_bytes());
    key
}

fn panel_prefix(panel_version: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(INDEX_PREFIX.len() + 4);
    key.extend_from_slice(INDEX_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key
}

fn event_range(panel_version: u32, since_ts_ns: Option<u64>, until_ts_ns: Option<u64>) -> KeyRange {
    let prefix = panel_prefix(panel_version);
    let mut start = prefix.clone();
    if let Some(since) = since_ts_ns {
        start.extend_from_slice(&since.to_be_bytes());
    }
    let end = until_ts_ns.map_or_else(
        || prefix_range(&prefix).end,
        |until| {
            let mut end = prefix.clone();
            end.extend_from_slice(&until.to_be_bytes());
            Some(end)
        },
    );
    KeyRange { start, end }
}

fn marker_key(panel_version: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(MARKER_PREFIX.len() + 4);
    key.extend_from_slice(MARKER_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key
}

fn encode_marker(marker: Marker) -> Vec<u8> {
    let mut value = Vec::with_capacity(1 + 4 + 8 + 8 + 64);
    value.push(MARKER_VERSION);
    value.extend_from_slice(&marker.panel_version.to_be_bytes());
    value.extend_from_slice(&marker.panel_content_seq.to_be_bytes());
    value.extend_from_slice(&marker.indexed_records.to_be_bytes());
    value.extend_from_slice(&marker.fingerprint.xor);
    value.extend_from_slice(&marker.fingerprint.sum);
    value
}

fn decode_marker(value: &[u8]) -> Result<Marker> {
    const LEN: usize = 1 + 4 + 8 + 8 + 64;
    if value.len() != LEN || value[0] != MARKER_VERSION {
        return Err(CalyxError {
            code: CALYX_EVENT_TIME_INDEX_INVALID,
            message: format!(
                "event-time index marker has invalid schema: version={:?} bytes={} expected_version={MARKER_VERSION} expected_bytes={LEN}",
                value.first(),
                value.len()
            ),
            remediation: "preserve the corrupt marker bytes, remove only this derived index generation, and rerun the exact historical backfill",
        });
    }
    let panel_version = u32::from_be_bytes(value[1..5].try_into().expect("checked marker length"));
    let panel_content_seq =
        u64::from_be_bytes(value[5..13].try_into().expect("checked marker length"));
    let indexed_records =
        u64::from_be_bytes(value[13..21].try_into().expect("checked marker length"));
    let mut xor = [0_u8; 32];
    xor.copy_from_slice(&value[21..53]);
    let mut sum = [0_u8; 32];
    sum.copy_from_slice(&value[53..85]);
    Ok(Marker {
        panel_version,
        panel_content_seq,
        indexed_records,
        fingerprint: Fingerprint { xor, sum },
    })
}

fn decode_index_key(key: &[u8], expected_panel: u32) -> Result<EventTimeIndexEntry> {
    let expected_len = INDEX_PREFIX.len() + 4 + 8 + CX_ID_BYTES;
    if key.len() != expected_len || !key.starts_with(INDEX_PREFIX) {
        return Err(CalyxError {
            code: CALYX_EVENT_TIME_INDEX_INVALID,
            message: format!(
                "event-time IndexBtree key has {} bytes, expected {expected_len}",
                key.len()
            ),
            remediation: "preserve the malformed derived row, rebuild the event-time index, and inspect the writer that admitted it",
        });
    }
    let offset = INDEX_PREFIX.len();
    let panel_version = u32::from_be_bytes(
        key[offset..offset + 4]
            .try_into()
            .expect("checked index key length"),
    );
    if panel_version != expected_panel {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "event-time index range for panel {expected_panel} returned panel {panel_version}"
        )));
    }
    let source_event_ns = u64::from_be_bytes(
        key[offset + 4..offset + 12]
            .try_into()
            .expect("checked index key length"),
    );
    let cx_id = CxId::from_bytes(
        key[offset + 12..]
            .try_into()
            .expect("checked index key length"),
    );
    Ok(EventTimeIndexEntry {
        source_event_ns,
        cx_id,
    })
}

fn validate_index_value(key: &[u8], value: &[u8]) -> Result<()> {
    if value == INDEX_VALUE {
        return Ok(());
    }
    Err(CalyxError {
        code: CALYX_EVENT_TIME_INDEX_INVALID,
        message: format!(
            "event-time IndexBtree row has invalid value: key_prefix={} value_bytes={}",
            hex(&key[..key.len().min(12)]),
            value.len()
        ),
        remediation: "preserve the malformed derived row and rerun the exact event-time index backfill; do not interpret it as membership",
    })
}

fn validate_panel(panel_version: u32) -> Result<()> {
    if panel_version == 0 {
        return Err(CalyxError {
            code: CALYX_EVENT_TIME_INDEX_INVALID,
            message: "event-time index panel_version must be greater than zero".to_owned(),
            remediation: "supply a registered non-zero panel version",
        });
    }
    Ok(())
}

fn stale_status_error(status: &EventTimeIndexStatus) -> CalyxError {
    let code = if status.marker_content_seq.is_none() {
        CALYX_EVENT_TIME_INDEX_INCOMPLETE
    } else {
        CALYX_EVENT_TIME_INDEX_STALE
    };
    CalyxError {
        code,
        message: format!(
            "panel {} event-time index is {} at snapshot {}: marker_content_seq={:?} panel_content_seq={}",
            status.panel_version,
            status.state,
            status.snapshot_seq,
            status.marker_content_seq,
            status.panel_content_seq
        ),
        remediation: "run the explicit resumable event-time index backfill and require its independently reconciled completeness marker before retrying",
    }
}

fn missing_index_row(panel_version: u32, key: &[u8]) -> CalyxError {
    CalyxError {
        code: CALYX_EVENT_TIME_INDEX_INCOMPLETE,
        message: format!(
            "complete panel {panel_version} event-time index is missing the row being replaced: key_prefix={}",
            hex(&key[..key.len().min(16)])
        ),
        remediation: "preserve the Base mutation and derived index evidence, rebuild the event-time index, and retry only after reconciliation succeeds",
    }
}

fn apply_count_delta(count: u64, delta: i128) -> Result<u64> {
    let next = i128::from(count)
        .checked_add(delta)
        .ok_or_else(count_overflow)?;
    u64::try_from(next).map_err(|_| count_overflow())
}

fn count_overflow() -> CalyxError {
    CalyxError::aster_corrupt_shard("event-time index row count overflowed its exact integer bound")
}

fn add_be(left: &mut [u8; 32], right: &[u8; 32]) {
    let mut carry = 0_u16;
    for index in (0..32).rev() {
        let sum = u16::from(left[index]) + u16::from(right[index]) + carry;
        left[index] = sum as u8;
        carry = sum >> 8;
    }
}

fn sub_be(left: &mut [u8; 32], right: &[u8; 32]) {
    let mut borrow = 0_i16;
    for index in (0..32).rev() {
        let difference = i16::from(left[index]) - i16::from(right[index]) - borrow;
        if difference < 0 {
            left[index] = (difference + 256) as u8;
            borrow = 1;
        } else {
            left[index] = difference as u8;
            borrow = 0;
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
