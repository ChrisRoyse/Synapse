//! Recurrence-series rows stored in Aster's dedicated recurrence CF.

use crate::cf::{ColumnFamily, base_key, recurrence_key, recurrence_prefix_range};
use crate::dedup::{EpochSecs, OccurrenceId};
use crate::vault::AsterVault;
use crate::vault::base_rewrite::BaseRowRewrite;
use calyx_core::{CalyxError, Clock, Constellation, CxId, Result, Seq, VaultStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const CALYX_RECURRENCE_CONTEXT_TOO_LARGE: &str = "CALYX_RECURRENCE_CONTEXT_TOO_LARGE";
pub const CALYX_RECURRENCE_INVALID_RETENTION: &str = "CALYX_RECURRENCE_INVALID_RETENTION";
pub const CALYX_RECURRENCE_OCCURRENCE_CONFLICT: &str = "CALYX_RECURRENCE_OCCURRENCE_CONFLICT";
pub const MAX_CONTEXT_BYTES: usize = 256;
pub const DEFAULT_MAX_OCCURRENCES: usize = 10_000;
pub const DEFAULT_MAX_AGE_SECS: u64 = 365 * 86_400;
pub const FREQUENCY_SCALAR: &str = "recurrence.frequency";

const SUMMARY_OCCURRENCE_ID: u64 = u64::MAX;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OccurrenceContext {
    pub bytes: Vec<u8>,
}

impl OccurrenceContext {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.len() > MAX_CONTEXT_BYTES {
            return Err(recurrence_error(
                CALYX_RECURRENCE_CONTEXT_TOO_LARGE,
                format!(
                    "context blob is {} bytes; max is {MAX_CONTEXT_BYTES}",
                    bytes.len()
                ),
            ));
        }
        Ok(Self { bytes })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub id: OccurrenceId,
    pub t_k: EpochSecs,
    pub context: OccurrenceContext,
    /// Caller-stable identity for idempotent event projection. `None` is
    /// reserved for legacy/unkeyed Calyx callers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_key_sha256: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecurrenceAppendDisposition {
    Inserted,
    ExistingIdentical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceAppendOutcome {
    pub occurrence_id: OccurrenceId,
    pub disposition: RecurrenceAppendDisposition,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RollupSummary {
    pub oldest_t: EpochSecs,
    pub count_rolled: u64,
    pub period_estimate_secs: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecurrenceSeries {
    pub cx_id: CxId,
    pub occurrences: Vec<Occurrence>,
    pub frequency: u64,
    pub cadence_secs: Option<f64>,
    pub rollup_summary: Option<RollupSummary>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceReadStats {
    pub range_scan_rows: usize,
    pub decoded_rows: usize,
    pub occurrence_rows: usize,
    pub rollup_summary_rows: usize,
    pub rolled_occurrence_rows: usize,
    pub tombstone_rows: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecurrenceSeriesReadback {
    pub series: RecurrenceSeries,
    pub stats: RecurrenceReadStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub max_occurrences: usize,
    pub max_age_secs: u64,
}

impl RetentionPolicy {
    pub fn new(max_occurrences: usize, max_age_secs: u64) -> Result<Self> {
        let policy = Self {
            max_occurrences,
            max_age_secs,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(self) -> Result<()> {
        if self.max_occurrences == 0 {
            return Err(recurrence_error(
                CALYX_RECURRENCE_INVALID_RETENTION,
                "max_occurrences must be positive",
            ));
        }
        Ok(())
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_occurrences: DEFAULT_MAX_OCCURRENCES,
            max_age_secs: DEFAULT_MAX_AGE_SECS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "row", rename_all = "snake_case")]
pub enum StoredRecurrenceRow {
    Occurrence(Occurrence),
    RollupSummary(RollupSummary),
    RolledOccurrence {
        id: OccurrenceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dedup_key_sha256: Option<[u8; 32]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t_k: Option<EpochSecs>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_sha256: Option<[u8; 32]>,
    },
    Tombstone {
        id: OccurrenceId,
    },
}

#[derive(Clone, Debug)]
pub struct RecurrenceAppend {
    /// The subject's Base row, rewritten in place.
    ///
    /// A `Constellation` here would be wrong: every occurrence append rewrites
    /// this row, and re-encoding a decoded constellation replaces its real slot
    /// hashes with hashes of the `Absent` placeholders the decode produced
    /// (issue #1888). This is the panel that lazily backfills its slots, so
    /// that clobber landed on the one panel it could actually damage — and it
    /// did, silently, on every occurrence.
    pub updated_base: BaseRowRewrite,
    pub recurrence_rows: Vec<(Vec<u8>, Vec<u8>)>,
    pub occurrence_id: OccurrenceId,
}

pub fn append_occurrence<C>(
    vault: &AsterVault<C>,
    cx_id: CxId,
    t_k: EpochSecs,
    context: OccurrenceContext,
    observed_at: EpochSecs,
    retention: RetentionPolicy,
) -> Result<OccurrenceId>
where
    C: Clock,
{
    vault.with_recurrence_write_lock(|| {
        let base = read_base(vault, cx_id)?.ok_or_else(|| {
            CalyxError::stale_derived("recurrence append requires an existing constellation")
        })?;
        let append = build_append(vault, base, t_k, context, observed_at, retention, None)?;
        let occurrence_id = append.occurrence_id;
        vault.commit_recurrence_batch_locked(
            append.recurrence_rows,
            Some(append.updated_base),
            Vec::new(),
        )?;
        Ok(occurrence_id)
    })
}

/// Appends one occurrence exactly once for a caller-stable identity.
///
/// A retry with the same identity, timestamp, and context is an idempotent
/// read. Reusing an identity for different evidence fails closed before any
/// row is committed. The identity survives active-row rollup, so replaying old
/// source events cannot inflate frequency after retention compaction.
pub fn append_occurrence_once<C>(
    vault: &AsterVault<C>,
    cx_id: CxId,
    t_k: EpochSecs,
    context: OccurrenceContext,
    observed_at: EpochSecs,
    retention: RetentionPolicy,
    dedup_key_sha256: [u8; 32],
) -> Result<RecurrenceAppendOutcome>
where
    C: Clock,
{
    append_occurrence_once_with_rows(
        vault,
        RecurrenceAppendOnceRequest {
            cx_id,
            t_k,
            context,
            observed_at,
            retention,
            dedup_key_sha256,
        },
        |_occurrence_id, _frequency, _commit_seq| Ok(Vec::new()),
    )
}

pub struct RecurrenceAppendOnceRequest {
    pub cx_id: CxId,
    pub t_k: EpochSecs,
    pub context: OccurrenceContext,
    pub observed_at: EpochSecs,
    pub retention: RetentionPolicy,
    pub dedup_key_sha256: [u8; 32],
}

/// Appends one idempotent occurrence and caller-owned rows in the same commit.
/// The row builder runs only for a genuinely new occurrence while the durable
/// recurrence lock is held.
pub fn append_occurrence_once_with_rows<C, F>(
    vault: &AsterVault<C>,
    request: RecurrenceAppendOnceRequest,
    build_rows: F,
) -> Result<RecurrenceAppendOutcome>
where
    C: Clock,
    F: FnOnce(OccurrenceId, u64, Seq) -> Result<Vec<(ColumnFamily, Vec<u8>, Vec<u8>)>>,
{
    let RecurrenceAppendOnceRequest {
        cx_id,
        t_k,
        context,
        observed_at,
        retention,
        dedup_key_sha256,
    } = request;
    vault.with_recurrence_write_lock(|| {
        let base = read_base(vault, cx_id)?.ok_or_else(|| {
            CalyxError::stale_derived("recurrence append requires an existing constellation")
        })?;
        let existing = read_rows(vault, cx_id)?;
        if let Some(existing) = existing.dedup_evidence(dedup_key_sha256) {
            if existing.matches(t_k, &context) {
                return Ok(RecurrenceAppendOutcome {
                    occurrence_id: existing.id,
                    disposition: RecurrenceAppendDisposition::ExistingIdentical,
                });
            }
            return Err(recurrence_error(
                CALYX_RECURRENCE_OCCURRENCE_CONFLICT,
                format!(
                    "recurrence occurrence identity {} for {cx_id} was reused with different timestamp or context",
                    hex32(dedup_key_sha256)
                ),
            ));
        }
        let append = build_append(
            vault,
            base,
            t_k,
            context,
            observed_at,
            retention,
            Some(dedup_key_sha256),
        )?;
        let occurrence_id = append.occurrence_id;
        let frequency = occurrence_id.0.checked_add(1).ok_or_else(|| {
            CalyxError::aster_corrupt_shard("recurrence frequency overflow before atomic extension")
        })?;
        let commit_seq = vault.latest_seq().saturating_add(1);
        let additional_rows = build_rows(occurrence_id, frequency, commit_seq)?;
        vault.commit_recurrence_batch_locked(
            append.recurrence_rows,
            Some(append.updated_base),
            additional_rows,
        )?;
        Ok(RecurrenceAppendOutcome {
            occurrence_id,
            disposition: RecurrenceAppendDisposition::Inserted,
        })
    })
}

pub(crate) fn build_append<C>(
    vault: &AsterVault<C>,
    mut base: BaseRowRewrite,
    t_k: EpochSecs,
    context: OccurrenceContext,
    observed_at: EpochSecs,
    retention: RetentionPolicy,
    dedup_key_sha256: Option<[u8; 32]>,
) -> Result<RecurrenceAppend>
where
    C: Clock,
{
    retention.validate()?;
    t_k.to_u64()?;
    observed_at.to_u64()?;
    let cx_id = base.constellation().cx_id;
    let existing = read_rows(vault, cx_id)?;
    let frequency = frequency_from_base(base.constellation())?
        .unwrap_or(0)
        .max(existing.total_count());
    let next_frequency = frequency
        .checked_add(1)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("recurrence frequency overflow"))?;
    let occurrence_id = OccurrenceId(frequency);
    let new_occurrence = Occurrence {
        id: occurrence_id,
        t_k,
        context,
        dedup_key_sha256,
    };

    let mut active = existing.occurrences;
    active.push(new_occurrence.clone());
    active.sort_by_key(|occurrence| (occurrence.t_k, occurrence.id));
    let rolled = select_rollup(&active, retention, observed_at)?;
    let summary = merge_summary(existing.rollup_summary, &rolled);

    let mut recurrence_rows = vec![(
        recurrence_key(cx_id, occurrence_id.0),
        encode_recurrence_row(&StoredRecurrenceRow::Occurrence(new_occurrence))?,
    )];
    for occurrence in &rolled {
        recurrence_rows.push((
            recurrence_key(cx_id, occurrence.id.0),
            encode_recurrence_row(&StoredRecurrenceRow::RolledOccurrence {
                id: occurrence.id,
                dedup_key_sha256: occurrence.dedup_key_sha256,
                t_k: Some(occurrence.t_k),
                context_sha256: Some(sha256(&occurrence.context.bytes)),
            })?,
        ));
    }
    if let Some(summary) = &summary {
        recurrence_rows.push((
            recurrence_summary_key(cx_id),
            encode_recurrence_row(&StoredRecurrenceRow::RollupSummary(summary.clone()))?,
        ));
    }

    base.constellation_mut()
        .scalars
        .insert(FREQUENCY_SCALAR.to_string(), next_frequency as f64);
    Ok(RecurrenceAppend {
        updated_base: base,
        recurrence_rows,
        occurrence_id,
    })
}

pub fn read_series<C>(vault: &AsterVault<C>, cx_id: CxId) -> Result<RecurrenceSeries>
where
    C: Clock,
{
    Ok(read_series_readback(vault, cx_id)?.series)
}

pub fn read_series_readback<C>(
    vault: &AsterVault<C>,
    cx_id: CxId,
) -> Result<RecurrenceSeriesReadback>
where
    C: Clock,
{
    let (rows, stats) = read_rows_with_stats(vault, cx_id)?;
    let frequency = if rows.has_tombstone {
        rows.total_count()
    } else {
        base_frequency(vault, cx_id)?.max(rows.total_count())
    };
    Ok(RecurrenceSeriesReadback {
        series: RecurrenceSeries {
            cx_id,
            cadence_secs: cadence_secs(&rows.occurrences),
            occurrences: rows.occurrences,
            frequency,
            rollup_summary: rows.rollup_summary,
        },
        stats,
    })
}

pub fn occurrence_count<C>(vault: &AsterVault<C>, cx_id: CxId) -> Result<u64>
where
    C: Clock,
{
    let rows = read_rows(vault, cx_id)?;
    if rows.has_tombstone {
        return Ok(rows.total_count());
    }
    Ok(base_frequency(vault, cx_id)?.max(rows.total_count()))
}

fn base_frequency<C: Clock>(vault: &AsterVault<C>, cx_id: CxId) -> Result<u64> {
    let Some(base) = read_base(vault, cx_id)? else {
        return Ok(0);
    };
    let base = base.constellation();
    Ok(frequency_from_base(base)?.unwrap_or(0))
}

pub fn recurrence_summary_key(cx_id: CxId) -> Vec<u8> {
    recurrence_key(cx_id, SUMMARY_OCCURRENCE_ID)
}

pub fn encode_recurrence_row(row: &StoredRecurrenceRow) -> Result<Vec<u8>> {
    serde_json::to_vec(row)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("encode recurrence row: {error}")))
}

pub fn decode_recurrence_row(bytes: &[u8]) -> Result<StoredRecurrenceRow> {
    serde_json::from_slice(bytes)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("decode recurrence row: {error}")))
}

fn read_base<C: Clock>(vault: &AsterVault<C>, cx_id: CxId) -> Result<Option<BaseRowRewrite>> {
    vault
        .read_cf_at(vault.snapshot(), ColumnFamily::Base, &base_key(cx_id))?
        .map(|bytes| BaseRowRewrite::decode(&bytes))
        .transpose()
}

fn read_rows<C: Clock>(vault: &AsterVault<C>, cx_id: CxId) -> Result<SeriesRows> {
    Ok(read_rows_with_stats(vault, cx_id)?.0)
}

fn read_rows_with_stats<C: Clock>(
    vault: &AsterVault<C>,
    cx_id: CxId,
) -> Result<(SeriesRows, RecurrenceReadStats)> {
    let range = recurrence_prefix_range(cx_id);
    let mut occurrences = Vec::new();
    let mut rolled_occurrences = Vec::new();
    let mut rollup_summary = None;
    let mut has_tombstone = false;
    let mut stats = RecurrenceReadStats::default();
    let rows = vault.scan_cf_range_at(vault.snapshot(), ColumnFamily::Recurrence, &range)?;
    stats.range_scan_rows = rows.len();
    for (_, value) in rows {
        stats.decoded_rows += 1;
        match decode_recurrence_row(&value)? {
            StoredRecurrenceRow::Occurrence(occurrence) => {
                stats.occurrence_rows += 1;
                occurrences.push(occurrence);
            }
            StoredRecurrenceRow::RollupSummary(summary) => {
                stats.rollup_summary_rows += 1;
                rollup_summary = Some(summary);
            }
            StoredRecurrenceRow::RolledOccurrence {
                id,
                dedup_key_sha256,
                t_k,
                context_sha256,
            } => {
                stats.rolled_occurrence_rows += 1;
                if let Some(dedup_key_sha256) = dedup_key_sha256 {
                    rolled_occurrences.push(RolledOccurrenceEvidence {
                        id,
                        dedup_key_sha256,
                        t_k,
                        context_sha256,
                    });
                }
            }
            StoredRecurrenceRow::Tombstone { .. } => {
                stats.tombstone_rows += 1;
                has_tombstone = true;
            }
        }
    }
    occurrences.sort_by_key(|occurrence| (occurrence.t_k, occurrence.id));
    Ok((
        SeriesRows {
            occurrences,
            rolled_occurrences,
            rollup_summary,
            has_tombstone,
        },
        stats,
    ))
}

#[derive(Debug)]
struct SeriesRows {
    occurrences: Vec<Occurrence>,
    rolled_occurrences: Vec<RolledOccurrenceEvidence>,
    rollup_summary: Option<RollupSummary>,
    has_tombstone: bool,
}

#[derive(Clone, Copy, Debug)]
struct RolledOccurrenceEvidence {
    id: OccurrenceId,
    dedup_key_sha256: [u8; 32],
    t_k: Option<EpochSecs>,
    context_sha256: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug)]
struct ExistingOccurrenceEvidence {
    id: OccurrenceId,
    t_k: Option<EpochSecs>,
    context_sha256: Option<[u8; 32]>,
}

impl ExistingOccurrenceEvidence {
    fn matches(self, t_k: EpochSecs, context: &OccurrenceContext) -> bool {
        self.t_k == Some(t_k) && self.context_sha256 == Some(sha256(&context.bytes))
    }
}

impl SeriesRows {
    fn total_count(&self) -> u64 {
        self.occurrences.len() as u64
            + self
                .rollup_summary
                .as_ref()
                .map_or(0, |summary| summary.count_rolled)
    }

    fn dedup_evidence(&self, key: [u8; 32]) -> Option<ExistingOccurrenceEvidence> {
        self.occurrences
            .iter()
            .find(|occurrence| occurrence.dedup_key_sha256 == Some(key))
            .map(|occurrence| ExistingOccurrenceEvidence {
                id: occurrence.id,
                t_k: Some(occurrence.t_k),
                context_sha256: Some(sha256(&occurrence.context.bytes)),
            })
            .or_else(|| {
                self.rolled_occurrences
                    .iter()
                    .find(|occurrence| occurrence.dedup_key_sha256 == key)
                    .map(|occurrence| ExistingOccurrenceEvidence {
                        id: occurrence.id,
                        t_k: occurrence.t_k,
                        context_sha256: occurrence.context_sha256,
                    })
            })
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex32(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn frequency_from_base(cx: &Constellation) -> Result<Option<u64>> {
    let Some(value) = cx.scalars.get(FREQUENCY_SCALAR) else {
        return Ok(None);
    };
    if !value.is_finite() || *value < 0.0 || value.fract() != 0.0 {
        return Err(CalyxError::aster_corrupt_shard(
            "recurrence frequency scalar must be a non-negative integer",
        ));
    }
    Ok(Some(*value as u64))
}

fn select_rollup(
    active: &[Occurrence],
    retention: RetentionPolicy,
    observed_at: EpochSecs,
) -> Result<Vec<Occurrence>> {
    let observed = observed_at.to_u64()?;
    let threshold = observed.saturating_sub(retention.max_age_secs);
    let mut rolled = active
        .iter()
        .filter(|occurrence| occurrence.t_k.to_u64().is_ok_and(|time| time < threshold))
        .cloned()
        .collect::<Vec<_>>();
    let remaining = active.len().saturating_sub(rolled.len());
    if remaining > retention.max_occurrences {
        let target_new = active
            .len()
            .div_ceil(10)
            .max(remaining - retention.max_occurrences);
        let mut added = 0;
        for occurrence in active {
            if added >= target_new {
                break;
            }
            if rolled.iter().any(|old| old.id == occurrence.id) {
                continue;
            }
            rolled.push(occurrence.clone());
            added += 1;
        }
    }
    rolled.sort_by_key(|occurrence| (occurrence.t_k, occurrence.id));
    rolled.dedup_by_key(|occurrence| occurrence.id);
    Ok(rolled)
}

fn merge_summary(existing: Option<RollupSummary>, rolled: &[Occurrence]) -> Option<RollupSummary> {
    if rolled.is_empty() {
        return existing;
    }
    let oldest_t = existing
        .as_ref()
        .map_or(rolled[0].t_k, |summary| summary.oldest_t.min(rolled[0].t_k));
    let count_rolled =
        existing.as_ref().map_or(0, |summary| summary.count_rolled) + rolled.len() as u64;
    let period_estimate_secs = cadence_secs(rolled).or_else(|| {
        existing
            .as_ref()
            .map(|summary| summary.period_estimate_secs)
    });
    Some(RollupSummary {
        oldest_t,
        count_rolled,
        period_estimate_secs: period_estimate_secs.unwrap_or(0.0),
    })
}

pub fn cadence_secs(occurrences: &[Occurrence]) -> Option<f64> {
    if occurrences.len() < 2 {
        return None;
    }
    let mut gaps = occurrences
        .windows(2)
        .map(|pair| (pair[1].t_k.0 - pair[0].t_k.0) as f64)
        .collect::<Vec<_>>();
    gaps.sort_by(f64::total_cmp);
    let mid = gaps.len() / 2;
    Some(if gaps.len() % 2 == 0 {
        (gaps[mid - 1] + gaps[mid]) / 2.0
    } else {
        gaps[mid]
    })
}

fn recurrence_error(code: &'static str, message: impl Into<String>) -> CalyxError {
    let remediation = match code {
        CALYX_RECURRENCE_CONTEXT_TOO_LARGE => "store only a bounded recurrence context blob",
        CALYX_RECURRENCE_INVALID_RETENTION => "use a positive recurrence max_occurrences value",
        CALYX_RECURRENCE_OCCURRENCE_CONFLICT => {
            "reuse an occurrence identity only for byte-identical event evidence"
        }
        _ => "inspect recurrence series inputs",
    };
    CalyxError {
        code,
        message: message.into(),
        remediation,
    }
}
