//! Recurrence-series rows stored in Aster's dedicated recurrence CF.

use crate::cf::{ColumnFamily, base_key, recurrence_key, recurrence_prefix_range};
use crate::dedup::{EpochSecs, OccurrenceId};
use crate::vault::{AsterVault, encode};
use calyx_core::{CalyxError, Clock, Constellation, CxId, Result, VaultStore};
use serde::{Deserialize, Serialize};

pub const CALYX_RECURRENCE_CONTEXT_TOO_LARGE: &str = "CALYX_RECURRENCE_CONTEXT_TOO_LARGE";
pub const CALYX_RECURRENCE_INVALID_RETENTION: &str = "CALYX_RECURRENCE_INVALID_RETENTION";
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
        rolled_into: OccurrenceId,
    },
    Tombstone {
        id: OccurrenceId,
    },
}

#[derive(Clone, Debug)]
pub struct RecurrenceAppend {
    pub updated_base: Constellation,
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
        let append = build_append(vault, base, t_k, context, observed_at, retention)?;
        let occurrence_id = append.occurrence_id;
        vault.commit_recurrence_batch_locked(append.recurrence_rows, Some(append.updated_base))?;
        Ok(occurrence_id)
    })
}

pub(crate) fn build_append<C>(
    vault: &AsterVault<C>,
    mut base: Constellation,
    t_k: EpochSecs,
    context: OccurrenceContext,
    observed_at: EpochSecs,
    retention: RetentionPolicy,
) -> Result<RecurrenceAppend>
where
    C: Clock,
{
    retention.validate()?;
    t_k.to_u64()?;
    observed_at.to_u64()?;
    let existing = read_rows(vault, base.cx_id)?;
    let frequency = frequency_from_base(&base)?
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
    };

    let mut active = existing.occurrences;
    active.push(new_occurrence.clone());
    active.sort_by_key(|occurrence| (occurrence.t_k, occurrence.id));
    let rolled = select_rollup(&active, retention, observed_at)?;
    let summary = merge_summary(existing.rollup_summary, &rolled);

    let mut recurrence_rows = vec![(
        recurrence_key(base.cx_id, occurrence_id.0),
        encode_recurrence_row(&StoredRecurrenceRow::Occurrence(new_occurrence))?,
    )];
    for occurrence in &rolled {
        recurrence_rows.push((
            recurrence_key(base.cx_id, occurrence.id.0),
            encode_recurrence_row(&StoredRecurrenceRow::Tombstone { id: occurrence.id })?,
        ));
    }
    if let Some(summary) = &summary {
        recurrence_rows.push((
            recurrence_summary_key(base.cx_id),
            encode_recurrence_row(&StoredRecurrenceRow::RollupSummary(summary.clone()))?,
        ));
    }

    base.scalars
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
    Ok(frequency_from_base(&base)?.unwrap_or(0))
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

fn read_base<C: Clock>(vault: &AsterVault<C>, cx_id: CxId) -> Result<Option<Constellation>> {
    vault
        .read_cf_at(vault.snapshot(), ColumnFamily::Base, &base_key(cx_id))?
        .map(|bytes| encode::decode_constellation_base(&bytes))
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
            StoredRecurrenceRow::RolledOccurrence { .. } => stats.rolled_occurrence_rows += 1,
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
            rollup_summary,
            has_tombstone,
        },
        stats,
    ))
}

#[derive(Debug)]
struct SeriesRows {
    occurrences: Vec<Occurrence>,
    rollup_summary: Option<RollupSummary>,
    has_tombstone: bool,
}

impl SeriesRows {
    fn total_count(&self) -> u64 {
        self.occurrences.len() as u64
            + self
                .rollup_summary
                .as_ref()
                .map_or(0, |summary| summary.count_rolled)
    }
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
        _ => "inspect recurrence series inputs",
    };
    CalyxError {
        code,
        message: message.into(),
        remediation,
    }
}
