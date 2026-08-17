//! Durable, panel-scoped change data capture for association consumers.
//!
//! The process-local MVCC changed-key journal is intentionally disposable at a
//! checkpoint/restart boundary.  Long-lived derived products cannot use it as
//! their only input log: a checkpoint can make their cursor older than the
//! retained floor and permanently strand them.  This module publishes one
//! sequence-qualified KV row in the *same commit* as every Base or quantized
//! slot mutation, so derived consumers can resume after both compaction and
//! process recovery without inferring history from current rows. Bootstrap
//! membership signals occupy a separate ordered prefix: association recovery
//! consumes both lanes, while search consumes only genuine mutations.

use super::*;
use crate::cf::SlotFamilyKind;
use crate::mvcc::tombstone_value;

pub const CALYX_PANEL_CHANGE_LOG_INVALID: &str = "CALYX_ASTER_PANEL_CHANGE_LOG_INVALID";
/// Largest explicit snapshot-signal commit.  It matches Synapse's bounded
/// association interval so one commit can never create an indivisible delta.
pub const PANEL_INPUT_SNAPSHOT_MAX_IDENTITIES: usize = 2_000;

const PANEL_CHANGE_PREFIX: &[u8] = b"\x00calyx-panel-change/v1/";
const PANEL_SNAPSHOT_PREFIX: &[u8] = b"\x00calyx-panel-snapshot/v1/";
const PANEL_CHANGE_FLOOR_PREFIX: &[u8] = b"\x00calyx-panel-change-floor/v1/";
const PANEL_CHANGE_VALUE_VERSION: u8 = 1;
const SOURCE_BASE: u8 = 0b0000_0001;
const SOURCE_SLOT: u8 = 0b0000_0010;
const SOURCE_SNAPSHOT: u8 = 0b0000_0100;
const VALUE_LEN: usize = 3;
const KEY_SUFFIX_LEN: usize = 4 + 8 + 16;

/// The latest event for one identity inside a requested sequence interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelInputChange {
    pub seq: Seq,
    pub panel_version: u32,
    pub cx_id: CxId,
    pub present: bool,
    pub base_changed: bool,
    pub slot_changed: bool,
    /// True only for an authoritative bootstrap membership signal. Snapshot
    /// rows are consumed by association recovery, but are not content
    /// mutations and must never dirty a persisted search generation.
    pub snapshot: bool,
}

/// Bounded, exact change-log readback for one panel and snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelInputChangeBatch {
    pub panel_version: u32,
    pub after_seq: Seq,
    pub through_seq: Seq,
    /// One final state per changed identity, ordered by `CxId`.
    pub changes: Vec<PanelInputChange>,
    /// Physical log rows decoded before identity coalescing.
    pub events_scanned: usize,
    /// At least `max_unique + 1` identities were observed.  The caller must
    /// bisect the sequence interval; no partial identity set is usable.
    pub unique_limit_exceeded: bool,
}

/// Physical result of one commit-atomic snapshot-signal chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelInputSnapshotPublication {
    pub panel_version: u32,
    pub before_seq: Seq,
    pub committed_seq: Seq,
    pub identities: usize,
}

/// Bounded physical deletion of CDC rows already covered by the sole durable
/// panel consumer. Cursor publication must precede this call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelInputChangePrune {
    pub panel_version: u32,
    pub through_seq: Seq,
    pub rows_deleted: usize,
    pub committed_seq: Option<Seq>,
    /// Highest real-mutation sequence durably retired in this panel. Search
    /// consumers fail closed if they ask for an older range.
    pub mutation_floor_seq: Seq,
}

#[derive(Clone, Copy, Debug, Default)]
struct PendingChange {
    present: bool,
    sources: u8,
}

pub(super) fn is_reserved_row(row: &encode::WriteRow) -> bool {
    row.cf == ColumnFamily::Kv
        && (row.key.starts_with(PANEL_CHANGE_PREFIX)
            || row.key.starts_with(PANEL_SNAPSHOT_PREFIX)
            || row.key.starts_with(PANEL_CHANGE_FLOOR_PREFIX))
}

fn invalid(message: impl Into<String>, remediation: &'static str) -> CalyxError {
    CalyxError {
        code: CALYX_PANEL_CHANGE_LOG_INVALID,
        message: message.into(),
        remediation,
    }
}

fn decode_panel_for_base(key: &[u8], value: &[u8]) -> Result<(CxId, u32)> {
    let key_bytes: [u8; 16] = key.try_into().map_err(|_| {
        invalid(
            format!(
                "Base change-log source key has {} bytes, expected 16",
                key.len()
            ),
            "preserve the offending commit and repair the malformed Base identity before retrying",
        )
    })?;
    let projection = encode::decode_constellation_base_projection(value)?;
    let cx_id = CxId::from_bytes(key_bytes);
    if projection.cx_id != cx_id || projection.panel_version == 0 {
        return Err(invalid(
            format!(
                "Base change-log source identity mismatch: key={cx_id} payload={} panel_version={}",
                projection.cx_id, projection.panel_version
            ),
            "repair the mismatched Base projection; no durable change event was published",
        ));
    }
    Ok((cx_id, projection.panel_version))
}

fn panel_prefix_for(prefix: &[u8], panel_version: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + 4);
    key.extend_from_slice(prefix);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key
}

fn panel_prefix(panel_version: u32) -> Vec<u8> {
    panel_prefix_for(PANEL_CHANGE_PREFIX, panel_version)
}

fn floor_key(panel_version: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(PANEL_CHANGE_FLOOR_PREFIX.len() + 4);
    key.extend_from_slice(PANEL_CHANGE_FLOOR_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key
}

fn decode_floor(panel_version: u32, value: &[u8]) -> Result<Seq> {
    let bytes: [u8; 8] = value.try_into().map_err(|_| {
        invalid(
            format!(
                "panel {panel_version} change-log mutation floor has {} bytes, expected 8",
                value.len()
            ),
            "preserve and repair the malformed mutation-retention floor before consuming or pruning changes",
        )
    })?;
    Ok(u64::from_be_bytes(bytes))
}

fn change_key(panel_version: u32, seq: Seq, cx_id: CxId) -> Vec<u8> {
    let mut key = Vec::with_capacity(PANEL_CHANGE_PREFIX.len() + KEY_SUFFIX_LEN);
    key.extend_from_slice(&panel_prefix(panel_version));
    key.extend_from_slice(&seq.to_be_bytes());
    key.extend_from_slice(cx_id.as_bytes());
    key
}

fn snapshot_key(panel_version: u32, seq: Seq, cx_id: CxId) -> Vec<u8> {
    let mut key = Vec::with_capacity(PANEL_SNAPSHOT_PREFIX.len() + KEY_SUFFIX_LEN);
    key.extend_from_slice(&panel_prefix_for(PANEL_SNAPSHOT_PREFIX, panel_version));
    key.extend_from_slice(&seq.to_be_bytes());
    key.extend_from_slice(cx_id.as_bytes());
    key
}

fn encode_change(change: PendingChange) -> Vec<u8> {
    vec![
        PANEL_CHANGE_VALUE_VERSION,
        u8::from(change.present),
        change.sources,
    ]
}

fn decode_change(key: &[u8], value: &[u8]) -> Result<PanelInputChange> {
    let (prefix, prefix_snapshot) = if key.starts_with(PANEL_CHANGE_PREFIX) {
        (PANEL_CHANGE_PREFIX, false)
    } else if key.starts_with(PANEL_SNAPSHOT_PREFIX) {
        (PANEL_SNAPSHOT_PREFIX, true)
    } else {
        (PANEL_CHANGE_PREFIX, false)
    };
    let expected_key_len = prefix.len() + KEY_SUFFIX_LEN;
    if key.len() != expected_key_len
        || !(key.starts_with(PANEL_CHANGE_PREFIX) || key.starts_with(PANEL_SNAPSHOT_PREFIX))
    {
        return Err(invalid(
            format!(
                "panel change-log key has {} bytes/prefix_match={}, expected {} bytes with the reserved prefix",
                key.len(),
                key.starts_with(PANEL_CHANGE_PREFIX),
                expected_key_len
            ),
            "preserve the KV row and repair the malformed panel change-log key",
        ));
    }
    if value.len() != VALUE_LEN || value[0] != PANEL_CHANGE_VALUE_VERSION {
        return Err(invalid(
            format!(
                "panel change-log value has len={} version={:?}, expected len={VALUE_LEN} version={PANEL_CHANGE_VALUE_VERSION}",
                value.len(),
                value.first()
            ),
            "preserve the KV row and migrate or repair the unsupported panel change-log value",
        ));
    }
    if value[1] > 1
        || value[2] & (SOURCE_BASE | SOURCE_SLOT) == 0
        || value[2] & !(SOURCE_BASE | SOURCE_SLOT | SOURCE_SNAPSHOT) != 0
    {
        return Err(invalid(
            format!(
                "panel change-log flags are invalid: present={} sources=0x{:02x}",
                value[1], value[2]
            ),
            "repair the invalid presence/source flags; an ambiguous change event cannot advance a consumer cursor",
        ));
    }
    let mut offset = prefix.len();
    let panel_version =
        u32::from_be_bytes(key[offset..offset + 4].try_into().expect("fixed slice"));
    offset += 4;
    let seq = u64::from_be_bytes(key[offset..offset + 8].try_into().expect("fixed slice"));
    offset += 8;
    let cx_id = CxId::from_bytes(key[offset..offset + 16].try_into().expect("fixed slice"));
    if panel_version == 0 || seq == 0 {
        return Err(invalid(
            format!("panel change-log key declares panel_version={panel_version} seq={seq}"),
            "repair the zero panel/sequence identity; neither is a valid committed change",
        ));
    }
    Ok(PanelInputChange {
        seq,
        panel_version,
        cx_id,
        present: value[1] == 1,
        base_changed: value[2] & SOURCE_BASE != 0,
        slot_changed: value[2] & SOURCE_SLOT != 0,
        snapshot: prefix_snapshot || value[2] & SOURCE_SNAPSHOT != 0,
    })
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Publishes a bounded set of authoritative current panel identities as
    /// CDC `read` events without mutating their Base/slot rows.
    ///
    /// This is the bootstrap/recovery equivalent of Debezium's incremental
    /// snapshot signal: normal Base/slot streaming continues concurrently, and
    /// the consumer starts at `before_seq` so it observes both the snapshot
    /// events and every real mutation that races after that watermark.
    pub fn publish_panel_input_snapshot_chunk(
        &self,
        panel_version: u32,
        ids: &[CxId],
    ) -> Result<PanelInputSnapshotPublication> {
        if panel_version == 0 || ids.is_empty() || ids.len() > PANEL_INPUT_SNAPSHOT_MAX_IDENTITIES {
            return Err(invalid(
                format!(
                    "panel input snapshot chunk is invalid: panel_version={panel_version} identities={} allowed=1..={PANEL_INPUT_SNAPSHOT_MAX_IDENTITIES}",
                    ids.len()
                ),
                "supply a positive panel and a non-empty, bounded identity chunk",
            ));
        }
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() != ids.len() {
            return Err(invalid(
                format!(
                    "panel {panel_version} input snapshot chunk contains {} duplicate identities",
                    ids.len() - unique.len()
                ),
                "deduplicate the snapshot membership chunk before publication",
            ));
        }
        self.with_durable_commit_lock(|| {
            let before_seq = self.latest_seq();
            let predicted_seq = before_seq.saturating_add(1);
            let floor_key = floor_key(panel_version);
            let floor_absent = self.read_cf_latest(ColumnFamily::Kv, &floor_key)?.is_none();
            let mut rows = Vec::with_capacity(unique.len() + usize::from(floor_absent));
            if floor_absent {
                // The first CDC publication seals the exact coverage origin.
                // A consumer whose base predates this sequence must rebase;
                // absence is never interpreted as complete pre-install history.
                rows.push(encode::WriteRow {
                    cf: ColumnFamily::Kv,
                    key: floor_key,
                    value: before_seq.to_be_bytes().to_vec(),
                });
            }
            for cx_id in unique {
                // The membership walk and this commit intentionally do not
                // block normal writers.  Re-read current Base state under the
                // commit lock: a member deleted/moved after the walk snapshot
                // must emit `present=false`, not a late `read` event that would
                // resurrect it after its real tombstone event.
                let present = match self.read_cf_latest(ColumnFamily::Base, cx_id.as_bytes())? {
                    Some(value) => {
                        let (decoded_id, decoded_panel) =
                            decode_panel_for_base(cx_id.as_bytes(), &value)?;
                        if decoded_id != cx_id {
                            return Err(invalid(
                                format!(
                                    "panel {panel_version} snapshot identity {cx_id} decodes as {decoded_id}"
                                ),
                                "repair the mismatched Base projection before publishing recovery events",
                            ));
                        }
                        decoded_panel == panel_version
                    }
                    None => false,
                };
                rows.push(encode::WriteRow {
                    cf: ColumnFamily::Kv,
                    key: snapshot_key(panel_version, predicted_seq, cx_id),
                    value: encode_change(PendingChange {
                        present,
                        sources: SOURCE_BASE | SOURCE_SNAPSHOT,
                    }),
                });
            }
            let committed_seq = self.commit_rows_locked_inner(&rows)?;
            if committed_seq != predicted_seq {
                return Err(invalid(
                    format!(
                        "panel {panel_version} snapshot predicted seq {predicted_seq} but committed {committed_seq}"
                    ),
                    "preserve the WAL and reconcile the sequence allocator before retrying; the commit may already be durable",
                ));
            }
            Ok(PanelInputSnapshotPublication {
                panel_version,
                before_seq,
                committed_seq,
                identities: ids.len(),
            })
        })
    }

    /// Deletes at most `max_rows` latest-visible CDC rows whose event sequence
    /// is at or below a consumer's already-durable acknowledgement.
    ///
    /// The range is panel-scoped and ordered by event sequence. The tombstone
    /// commit is followed by independent latest reads of every target key; a
    /// row still visible is an error, never a successful prune claim.
    pub fn prune_panel_input_changes(
        &self,
        panel_version: u32,
        through_seq: Seq,
        mutation_through_seq: Seq,
        max_rows: usize,
    ) -> Result<PanelInputChangePrune> {
        if panel_version == 0
            || through_seq == 0
            || mutation_through_seq > through_seq
            || max_rows == 0
        {
            return Err(invalid(
                format!(
                    "panel change-log prune is invalid: panel_version={panel_version} through_seq={through_seq} mutation_through_seq={mutation_through_seq} max_rows={max_rows}"
                ),
                "supply a positive panel, a mutation bound no newer than the association acknowledgement, and a positive bounded row count",
            ));
        }
        self.with_durable_commit_lock(|| {
            let pinned = self.snapshot_handle(self.latest_seq())?;
            let mut keys = Vec::new();
            let mut mutation_floor_seq = self
                .read_cf_latest(ColumnFamily::Kv, &floor_key(panel_version))?
                .map(|value| decode_floor(panel_version, &value))
                .transpose()?
                .unwrap_or(0);
            for source_prefix in [PANEL_CHANGE_PREFIX, PANEL_SNAPSHOT_PREFIX] {
                let prefix = panel_prefix_for(source_prefix, panel_version);
                let end = if through_seq == u64::MAX {
                    crate::cf::prefix_range(&prefix).end
                } else {
                    let mut end = prefix.clone();
                    end.extend_from_slice(&through_seq.saturating_add(1).to_be_bytes());
                    Some(end)
                };
                let range = KeyRange { start: prefix, end };
                let rows = self.scan_cf_range_page_snapshot(
                    pinned.snapshot(),
                    ColumnFamily::Kv,
                    &range,
                    None,
                    max_rows.saturating_sub(keys.len()).max(1),
                )?;
                for (key, value) in rows {
                    let event = decode_change(&key, &value)?;
                    if event.snapshot || event.seq <= mutation_through_seq {
                        if !event.snapshot {
                            mutation_floor_seq = mutation_floor_seq.max(event.seq);
                        }
                        keys.push(key);
                        if keys.len() == max_rows {
                            break;
                        }
                    }
                }
                if keys.len() == max_rows {
                    break;
                }
            }
            if keys.is_empty() {
                return Ok(PanelInputChangePrune {
                    panel_version,
                    through_seq,
                    rows_deleted: 0,
                    committed_seq: None,
                    mutation_floor_seq,
                });
            }
            drop(pinned);
            let mut tombstones = keys
                .iter()
                .map(|key| encode::WriteRow {
                    cf: ColumnFamily::Kv,
                    key: key.clone(),
                    value: tombstone_value(),
                })
                .collect::<Vec<_>>();
            if mutation_floor_seq > 0 {
                tombstones.push(encode::WriteRow {
                    cf: ColumnFamily::Kv,
                    key: floor_key(panel_version),
                    value: mutation_floor_seq.to_be_bytes().to_vec(),
                });
            }
            let committed_seq = self.commit_rows_locked_inner(&tombstones)?;
            for key in &keys {
                if self.read_cf_latest(ColumnFamily::Kv, key)?.is_some() {
                    return Err(invalid(
                        format!(
                            "panel {panel_version} acknowledged change-log key remains visible after prune commit {committed_seq} (key_len={})",
                            key.len()
                        ),
                        "preserve the WAL and repair the failed tombstone publication before reporting retention progress",
                    ));
                }
            }
            if mutation_floor_seq > 0 {
                let floor_readback = self
                    .read_cf_latest(ColumnFamily::Kv, &floor_key(panel_version))?
                    .ok_or_else(|| {
                        invalid(
                            format!(
                                "panel {panel_version} mutation-retention floor is absent after prune commit {committed_seq}"
                            ),
                            "preserve the WAL and repair the missing floor before acknowledging retention progress",
                        )
                    })?;
                let actual = decode_floor(panel_version, &floor_readback)?;
                if actual != mutation_floor_seq {
                    return Err(invalid(
                        format!(
                            "panel {panel_version} mutation-retention floor readback is {actual}, expected {mutation_floor_seq} after commit {committed_seq}"
                        ),
                        "preserve the WAL and reconcile the floor row before acknowledging retention progress",
                    ));
                }
            }
            Ok(PanelInputChangePrune {
                panel_version,
                through_seq,
                rows_deleted: keys.len(),
                committed_seq: Some(committed_seq),
                mutation_floor_seq,
            })
        })
    }

    /// Adds durable panel-input events to a prepared commit.
    ///
    /// Called only under the vault-wide durable commit lock, before the
    /// predicted sequence is allocated.  The resulting KV rows share the same
    /// WAL record and MVCC sequence as their Base/slot causes.
    pub(super) fn augment_panel_change_log_rows_locked(
        &self,
        rows: &[encode::WriteRow],
        predicted_seq: Seq,
    ) -> Result<Vec<encode::WriteRow>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut expanded = rows.to_vec();
        let mut staged_bases = BTreeMap::<Vec<u8>, Option<(CxId, u32)>>::new();
        let mut changes = BTreeMap::<(u32, CxId), PendingChange>::new();

        for row in rows.iter().filter(|row| row.cf == ColumnFamily::Base) {
            if staged_bases.contains_key(&row.key) {
                return Err(invalid(
                    format!(
                        "atomic batch contains duplicate Base writes for one panel change-log identity (key_len={})",
                        row.key.len()
                    ),
                    "deduplicate the Base mutation before committing it",
                ));
            }
            let prior = self
                .read_cf_latest(ColumnFamily::Base, &row.key)?
                .map(|value| decode_panel_for_base(&row.key, &value))
                .transpose()?;
            let next = if is_tombstone_value(&row.value) {
                None
            } else {
                Some(decode_panel_for_base(&row.key, &row.value)?)
            };
            if let Some((cx_id, panel_version)) = prior
                && next.map(|(_, panel)| panel) != Some(panel_version)
            {
                changes.insert(
                    (panel_version, cx_id),
                    PendingChange {
                        present: false,
                        sources: SOURCE_BASE,
                    },
                );
            }
            if let Some((cx_id, panel_version)) = next {
                let change = changes.entry((panel_version, cx_id)).or_default();
                change.present = true;
                change.sources |= SOURCE_BASE;
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
            let base = match staged_bases.get(&row.key) {
                Some(base) => *base,
                None => self
                    .read_cf_latest(ColumnFamily::Base, &row.key)?
                    .map(|value| decode_panel_for_base(&row.key, &value))
                    .transpose()?,
            };
            if let Some((cx_id, panel_version)) = base {
                let change = changes.entry((panel_version, cx_id)).or_default();
                change.present = true;
                change.sources |= SOURCE_SLOT;
            }
        }

        for ((panel_version, cx_id), change) in changes {
            expanded.push(encode::WriteRow {
                cf: ColumnFamily::Kv,
                key: change_key(panel_version, predicted_seq, cx_id),
                value: encode_change(change),
            });
        }
        let changed_panels = expanded
            .iter()
            .filter(|row| row.cf == ColumnFamily::Kv && row.key.starts_with(PANEL_CHANGE_PREFIX))
            .filter_map(|row| {
                let offset = PANEL_CHANGE_PREFIX.len();
                row.key
                    .get(offset..offset + 4)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u32::from_be_bytes)
            })
            .collect::<BTreeSet<_>>();
        for panel_version in changed_panels {
            let key = floor_key(panel_version);
            if self.read_cf_latest(ColumnFamily::Kv, &key)?.is_none() {
                expanded.push(encode::WriteRow {
                    cf: ColumnFamily::Kv,
                    key,
                    value: predicted_seq.saturating_sub(1).to_be_bytes().to_vec(),
                });
            }
        }
        Ok(expanded)
    }

    /// Reads durable panel-input changes in `(after_seq, snapshot.seq()]`.
    ///
    /// The result coalesces repeated mutations to the last event per identity.
    /// If more than `max_unique` identities occur it returns only a diagnostic
    /// prefix with `unique_limit_exceeded=true`; callers must bisect and retry,
    /// never publish the partial set.
    pub fn panel_input_changes_snapshot(
        &self,
        snapshot: Snapshot,
        after_seq: Seq,
        through_seq: Seq,
        panel_version: u32,
        max_unique: usize,
    ) -> Result<PanelInputChangeBatch> {
        if panel_version == 0
            || after_seq > through_seq
            || through_seq > snapshot.seq()
            || max_unique == 0
        {
            return Err(invalid(
                format!(
                    "panel change-log range is invalid: panel_version={panel_version} after_seq={after_seq} through_seq={through_seq} snapshot_seq={} max_unique={max_unique}",
                    snapshot.seq()
                ),
                "supply a positive panel, after_seq <= through_seq <= the pinned snapshot, and max_unique >= 1",
            ));
        }
        self.panel_input_changes_snapshot_filtered(
            snapshot,
            after_seq,
            through_seq,
            panel_version,
            max_unique,
            false,
        )
    }

    /// Reads only commit-atomic Base/slot mutations, excluding bootstrap
    /// membership signals. This is the durable search-generation delta source.
    pub fn panel_input_mutations_snapshot(
        &self,
        snapshot: Snapshot,
        after_seq: Seq,
        through_seq: Seq,
        panel_version: u32,
        max_unique: usize,
    ) -> Result<PanelInputChangeBatch> {
        let floor = self
            .read_cf_snapshot(snapshot, ColumnFamily::Kv, &floor_key(panel_version))?
            .map(|value| decode_floor(panel_version, &value))
            .transpose()?
            .unwrap_or(0);
        if after_seq < floor {
            return Err(CalyxError::stale_derived(format!(
                "panel {panel_version} durable mutation history is retained only after seq {floor}, but the requested delta starts after {after_seq}; rebuild the persisted consumer generation at or beyond the reported mutation floor before retrying"
            )));
        }
        self.panel_input_changes_snapshot_filtered(
            snapshot,
            after_seq,
            through_seq,
            panel_version,
            max_unique,
            true,
        )
    }

    fn panel_input_changes_snapshot_filtered(
        &self,
        snapshot: Snapshot,
        after_seq: Seq,
        through_seq: Seq,
        panel_version: u32,
        max_unique: usize,
        mutations_only: bool,
    ) -> Result<PanelInputChangeBatch> {
        if panel_version == 0
            || after_seq > through_seq
            || through_seq > snapshot.seq()
            || max_unique == 0
        {
            return Err(invalid(
                format!(
                    "panel change-log range is invalid: panel_version={panel_version} after_seq={after_seq} through_seq={through_seq} snapshot_seq={} max_unique={max_unique}",
                    snapshot.seq()
                ),
                "supply a positive panel, after_seq <= through_seq <= the pinned snapshot, and max_unique >= 1",
            ));
        }
        if after_seq == through_seq {
            return Ok(PanelInputChangeBatch {
                panel_version,
                after_seq,
                through_seq,
                changes: Vec::new(),
                events_scanned: 0,
                unique_limit_exceeded: false,
            });
        }
        let mut latest = BTreeMap::<CxId, PanelInputChange>::new();
        let mut unique_limit_exceeded = false;
        let mut events_scanned = 0usize;
        let page_rows = max_unique.clamp(1, 1_024);
        let prefixes = if mutations_only {
            vec![PANEL_CHANGE_PREFIX]
        } else {
            vec![PANEL_CHANGE_PREFIX, PANEL_SNAPSHOT_PREFIX]
        };
        for source_prefix in prefixes {
            let prefix = panel_prefix_for(source_prefix, panel_version);
            let mut start = prefix.clone();
            start.extend_from_slice(&after_seq.saturating_add(1).to_be_bytes());
            let end = if through_seq == u64::MAX {
                crate::cf::prefix_range(&prefix).end
            } else {
                let mut end = prefix.clone();
                end.extend_from_slice(&through_seq.saturating_add(1).to_be_bytes());
                Some(end)
            };
            let range = KeyRange { start, end };
            let mut after_key = None::<Vec<u8>>;
            loop {
                let page = self.scan_cf_range_page_snapshot(
                    snapshot,
                    ColumnFamily::Kv,
                    &range,
                    after_key.as_deref(),
                    page_rows,
                )?;
                if page.is_empty() {
                    break;
                }
                after_key = page.last().map(|(key, _)| key.clone());
                for (key, value) in page {
                    let event = decode_change(&key, &value)?;
                    if event.panel_version != panel_version
                        || event.seq <= after_seq
                        || event.seq > through_seq
                    {
                        return Err(invalid(
                            format!(
                                "panel change-log range returned out-of-contract event: requested_panel={panel_version} requested=({after_seq},{}] event_panel={} event_seq={}",
                                through_seq, event.panel_version, event.seq
                            ),
                            "preserve the KV range and repair the key-range/index ordering mismatch",
                        ));
                    }
                    events_scanned = events_scanned.checked_add(1).ok_or_else(|| {
                        invalid(
                            "panel change-log scanned-event count overflowed usize",
                            "inspect the requested sequence interval and repair the impossible row cardinality",
                        )
                    })?;
                    if mutations_only && event.snapshot {
                        continue;
                    }
                    match latest.entry(event.cx_id) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(event);
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry)
                            if event.seq >= entry.get().seq =>
                        {
                            entry.insert(event);
                        }
                        std::collections::btree_map::Entry::Occupied(_) => {}
                    }
                    if latest.len() > max_unique {
                        unique_limit_exceeded = true;
                        break;
                    }
                }
                if unique_limit_exceeded {
                    break;
                }
            }
            if unique_limit_exceeded {
                break;
            }
        }
        // Hydration is intentionally against the current pinned snapshot, not
        // an old interval boundary that MVCC may have legitimately rebased
        // away. Collapse every changed identity to its current authoritative
        // panel presence. A later mutation remains in the durable log and is
        // still consumed at its own sequence; using its current value early is
        // idempotent and never resurrects a row.
        for change in latest.values_mut() {
            change.present = match self.read_cf_snapshot(
                snapshot,
                ColumnFamily::Base,
                change.cx_id.as_bytes(),
            )? {
                Some(value) => {
                    let (decoded_id, decoded_panel) =
                        decode_panel_for_base(change.cx_id.as_bytes(), &value)?;
                    if decoded_id != change.cx_id {
                        return Err(invalid(
                            format!(
                                "panel {panel_version} changed identity {} decodes as {decoded_id}",
                                change.cx_id
                            ),
                            "repair the mismatched Base projection before advancing the consumer",
                        ));
                    }
                    decoded_panel == panel_version
                }
                None => false,
            };
        }
        Ok(PanelInputChangeBatch {
            panel_version,
            after_seq,
            through_seq,
            changes: latest.into_values().collect(),
            events_scanned,
            unique_limit_exceeded,
        })
    }
}
