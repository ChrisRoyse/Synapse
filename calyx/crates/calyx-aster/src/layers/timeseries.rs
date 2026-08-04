//! Time-series `(series, ts) -> point` layer with continuous rollups.
//!
//! Points sit on the same ordered transactional core as the other paradigm
//! layers, addressed by the `0x04` key-space discriminant. Timestamps are
//! Unix **nanoseconds**, big-endian encoded so a key range scan returns points
//! in ascending time order.
//!
//! Every [`TimeSeriesLayer::ts_write`] updates the 1-minute, 1-hour and 1-day
//! rollup accumulators in the **same** group-commit batch as the point, so a
//! rollup read is an O(1) point-read rather than an O(n) scan. Retention
//! (`DropAfter`) is enforced check-on-read for PH53; physical deletion is the
//! PH58 janitor's job.

use calyx_core::{CalyxError, Clock, Modality, Result, Seq};

use crate::cf::{ColumnFamily, KeyRange};
use crate::collection::{
    CALYX_INVALID_ARGUMENT, Collection, CollectionMode, FieldType, RetentionPolicy, Schema,
    collection_has_lens, ingest_collection_constellation,
};
use crate::index::{IndexMaintenance, collection_has_maintained_index, field_is_indexed};
use crate::layers::relational::{RecordKey, RecordValue, Row};
use crate::vault::AsterVault;
use calyx_ledger::{ActorId, EntryKind, PayloadBuilder, RedactionPolicy, SubjectId};

/// Key-space discriminant for time-series rows.
const DISC_TS: u8 = 0x04;
/// Sub-discriminant separating raw points from rollup accumulators.
const KIND_POINT: u8 = 0x00;
const KIND_ROLLUP: u8 = 0x01;
const POINT_VALUE_BYTES: usize = 8;
const ROLLUP_VALUE_BYTES: usize = 8 + 8 + 8 + 8;
const NANOS_PER_MINUTE: u64 = 60 * 1_000_000_000;
const NANOS_PER_HOUR: u64 = 60 * NANOS_PER_MINUTE;
const NANOS_PER_DAY: u64 = 24 * NANOS_PER_HOUR;
const NANOS_PER_MILLI: u64 = 1_000_000;

/// A caller reused one immutable `(collection, series, timestamp)` identity
/// for a different measurement. Replacing it cannot update min/max exactly
/// from the compact accumulator, so the layer refuses before any write.
pub const CALYX_TIMESERIES_POINT_CONFLICT: &str = "CALYX_TIMESERIES_POINT_CONFLICT";

/// Continuous rollup window granularities, maintained on every write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RollupWindow {
    OneMinute,
    OneHour,
    OneDay,
}

impl RollupWindow {
    /// All windows maintained on each write, in tag order.
    pub const ALL: [Self; 3] = [Self::OneMinute, Self::OneHour, Self::OneDay];

    fn tag(self) -> u8 {
        match self {
            Self::OneMinute => 0,
            Self::OneHour => 1,
            Self::OneDay => 2,
        }
    }

    fn span_nanos(self) -> u64 {
        match self {
            Self::OneMinute => NANOS_PER_MINUTE,
            Self::OneHour => NANOS_PER_HOUR,
            Self::OneDay => NANOS_PER_DAY,
        }
    }

    /// Floors `ts` to the start of the window it falls in.
    pub(crate) fn window_start(self, ts: u64) -> u64 {
        ts - (ts % self.span_nanos())
    }
}

/// A materialized rollup accumulator for one window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RollupValue {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

/// `(series, ts) -> point` key-encoding layer over a `TimeSeries` collection.
pub struct TimeSeriesLayer<'a, C: Clock> {
    vault: &'a AsterVault<C>,
}

impl<'a, C: Clock> TimeSeriesLayer<'a, C> {
    pub fn new(vault: &'a AsterVault<C>) -> Self {
        Self { vault }
    }

    /// Appends one measurement and folds it into every rollup window, all in a
    /// single atomic group commit.
    pub fn ts_write(&self, col: &Collection, series: u64, ts: u64, val: f64) -> Result<Seq> {
        if collection_has_lens(col) {
            if !val.is_finite() {
                return Err(invalid_argument(
                    "time-series value must be finite (NaN/inf rejected to protect rollups)",
                ));
            }
            let key = point_key(col, series, ts);
            let value = encode_point(val);
            let parts = [
                ("point_key", key.as_slice()),
                ("point_value", value.as_slice()),
            ];
            return ingest_collection_constellation(
                self.vault,
                col,
                "timeseries",
                &parts,
                Modality::Structured,
            );
        }
        require_ts_mode(col)?;
        if !val.is_finite() {
            return Err(invalid_argument(
                "time-series value must be finite (NaN/inf rejected to protect rollups)",
            ));
        }
        let snapshot = self.vault.latest_seq();
        let point_key = point_key(col, series, ts);
        if let Some(existing_bytes) =
            self.vault
                .read_cf_at(snapshot, ColumnFamily::TimeSeries, &point_key)?
        {
            let existing = decode_point(&existing_bytes)?;
            if existing.to_bits() == val.to_bits() {
                return Ok(snapshot);
            }
            return Err(point_conflict(format!(
                "time-series point already exists for collection={} series={series} ts={ts}: existing_bits={:016x} requested_bits={:016x}",
                col.name,
                existing.to_bits(),
                val.to_bits()
            )));
        }
        let pk = RecordKey::from_bytes(point_key.clone())?;
        let mut rows = Vec::with_capacity(1 + RollupWindow::ALL.len());
        rows.push((
            ColumnFamily::TimeSeries,
            point_key.clone(),
            encode_point(val),
        ));
        for window in RollupWindow::ALL {
            let key = rollup_key(col, series, window, window.window_start(ts));
            let current = self
                .vault
                .read_cf_at(snapshot, ColumnFamily::TimeSeries, &key)?
                .map(|bytes| decode_rollup(&bytes))
                .transpose()?;
            let updated = fold_rollup(current, val);
            rows.push((ColumnFamily::TimeSeries, key, encode_rollup(&updated)));
        }
        // Only touch secondary indexes — and the extra point read they require —
        // when the collection declares one. Unindexed series writes stay on the
        // point/rollup path and never coerce synthetic index fields.
        if collection_has_maintained_index(col) {
            let old_index_row = self
                .vault
                .read_cf_at(snapshot, ColumnFamily::TimeSeries, &point_key)?
                .map(|bytes| {
                    decode_point(&bytes).and_then(|old| ts_index_row(col, series, ts, old))
                })
                .transpose()?;
            let new_index_row = ts_index_row(col, series, ts, val)?;
            IndexMaintenance::stage_put(
                self.vault,
                &mut rows,
                col,
                &pk,
                old_index_row.as_ref(),
                &new_index_row,
            )?;
        }
        let subject = ledger_subject(&point_key);
        let payload = ledger_payload(col, series, ts, &point_key, val);
        self.vault.write_cf_batch_with_ledger_entry(
            rows,
            EntryKind::Ingest,
            subject,
            payload,
            ActorId::Service("calyx-aster-timeseries".to_string()),
        )
    }

    /// Returns `(ts, val)` pairs in `[start_ts, end_ts]` ascending. Points
    /// older than the collection's `DropAfter` horizon are skipped (check-on-
    /// read retention).
    pub fn ts_range(
        &self,
        col: &Collection,
        series: u64,
        start_ts: u64,
        end_ts: u64,
    ) -> Result<Vec<(u64, f64)>> {
        self.ts_range_at(self.vault.latest_seq(), col, series, start_ts, end_ts)
    }

    /// Snapshot-pinned variant of [`Self::ts_range`].
    pub fn ts_range_at(
        &self,
        snapshot: Seq,
        col: &Collection,
        series: u64,
        start_ts: u64,
        end_ts: u64,
    ) -> Result<Vec<(u64, f64)>> {
        require_ts_mode(col)?;
        if start_ts > end_ts {
            return Ok(Vec::new());
        }
        let horizon = self.retention_floor(col);
        let rows = self.vault.scan_cf_range_at(
            snapshot,
            ColumnFamily::TimeSeries,
            &point_range(col, series, start_ts, end_ts),
        )?;
        let mut out = Vec::with_capacity(rows.len());
        for (key, value) in rows {
            let ts = point_ts(col, series, &key)?;
            if let Some(floor) = horizon
                && ts < floor
            {
                continue;
            }
            out.push((ts, decode_point(&value)?));
        }
        Ok(out)
    }

    /// Point-reads the rollup accumulator covering `ts`'s window, or `None` if
    /// nothing has been written to that window yet.
    pub fn ts_rollup(
        &self,
        col: &Collection,
        series: u64,
        window: RollupWindow,
        ts: u64,
    ) -> Result<Option<RollupValue>> {
        require_ts_mode(col)?;
        let key = rollup_key(col, series, window, window.window_start(ts));
        self.vault
            .read_cf_at(self.vault.latest_seq(), ColumnFamily::TimeSeries, &key)?
            .map(|bytes| decode_rollup(&bytes))
            .transpose()
    }

    /// The oldest timestamp still visible under the collection's retention
    /// policy, or `None` when nothing is dropped. `now` comes from the
    /// injected clock (Unix ms) and is scaled to nanoseconds to match `ts`.
    fn retention_floor(&self, col: &Collection) -> Option<u64> {
        match &col.retention {
            RetentionPolicy::Forever | RetentionPolicy::RollupOnly => None,
            RetentionPolicy::DropAfter(duration) => {
                let now_nanos = self.vault.clock_now().saturating_mul(NANOS_PER_MILLI);
                let span = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
                Some(now_nanos.saturating_sub(span))
            }
        }
    }
}

/// Stable per-collection id scoping series rows. Distinct hash domain from the
/// other layers so cross-mode collisions are impossible.
pub fn collection_id(col: &Collection) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"calyx:timeseries:collection:v1");
    hasher.update(&col.tenant.0.to_be_bytes());
    hasher.update(&(col.name.len() as u16).to_be_bytes());
    hasher.update(col.name.as_bytes());
    u64::from_be_bytes(hasher.finalize().as_bytes()[0..8].try_into().unwrap())
}

/// `0x04 | 0x00 | cid | series | ts` (all big-endian).
pub fn point_key(col: &Collection, series: u64, ts: u64) -> Vec<u8> {
    let mut key = point_prefix(col, series);
    key.extend_from_slice(&ts.to_be_bytes());
    key
}

/// `0x04 | 0x01 | cid | series | window_tag | window_start`.
pub fn rollup_key(
    col: &Collection,
    series: u64,
    window: RollupWindow,
    window_start: u64,
) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + 8 + 8 + 1 + 8);
    key.push(DISC_TS);
    key.push(KIND_ROLLUP);
    key.extend_from_slice(&collection_id(col).to_be_bytes());
    key.extend_from_slice(&series.to_be_bytes());
    key.push(window.tag());
    key.extend_from_slice(&window_start.to_be_bytes());
    key
}

fn point_prefix(col: &Collection, series: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + 8 + 8);
    key.push(DISC_TS);
    key.push(KIND_POINT);
    key.extend_from_slice(&collection_id(col).to_be_bytes());
    key.extend_from_slice(&series.to_be_bytes());
    key
}

/// Half-open key range `[start_ts, end_ts]` (inclusive end via a trailing
/// byte above the fixed-width end key).
fn point_range(col: &Collection, series: u64, start_ts: u64, end_ts: u64) -> KeyRange {
    let mut end = point_key(col, series, end_ts);
    end.push(0x00);
    KeyRange {
        start: point_key(col, series, start_ts),
        end: Some(end),
    }
}

fn point_ts(col: &Collection, series: u64, key: &[u8]) -> Result<u64> {
    let prefix = point_prefix(col, series);
    let rest = key
        .strip_prefix(prefix.as_slice())
        .ok_or_else(|| corrupt("time-series scan returned a key outside the series prefix"))?;
    let ts_bytes = rest
        .get(0..8)
        .filter(|_| rest.len() == 8)
        .ok_or_else(|| corrupt("time-series point key has a malformed timestamp"))?;
    Ok(u64::from_be_bytes(ts_bytes.try_into().unwrap()))
}

/// Builds the synthetic index row for a time-series point, carrying only the
/// well-known fields (`series`, `ts`, `value`/`val`) a maintained index
/// references.
///
/// `series` and `ts` are full `u64`s. Schema-less indexes use native `U64`;
/// explicit schema fields keep their declared encoding. Signed conversions can
/// fail only when the declaring index pins that field to a signed type, so an
/// index on `value` is never blocked by a large series id or timestamp.
pub(crate) fn ts_index_row(col: &Collection, series: u64, ts: u64, val: f64) -> Result<Row> {
    let mut fields = Vec::new();
    if field_is_indexed(col, "series") {
        fields.push(("series", ts_u64_value(col, "series", series)?));
    }
    if field_is_indexed(col, "ts") {
        fields.push(("ts", ts_u64_value(col, "ts", ts)?));
    }
    if field_is_indexed(col, "value") {
        fields.push(("value", RecordValue::F64(val)));
    }
    if field_is_indexed(col, "val") {
        fields.push(("val", RecordValue::F64(val)));
    }
    Ok(Row::new(fields))
}

/// Encodes a `u64` index field (`series`/`ts`) by its declared schema type.
fn ts_u64_value(col: &Collection, field: &str, value: u64) -> Result<RecordValue> {
    match declared_field_type(col, field) {
        Some(FieldType::I64) => i64::try_from(value).map(RecordValue::I64).map_err(|_| {
            invalid_argument(format!("time-series {field} exceeds i64 indexable range"))
        }),
        Some(FieldType::U64) | None => Ok(RecordValue::U64(value)),
        Some(FieldType::Timestamp) => {
            i64::try_from(value)
                .map(RecordValue::Timestamp)
                .map_err(|_| {
                    invalid_argument(format!(
                        "time-series {field} exceeds timestamp indexable range"
                    ))
                })
        }
        Some(FieldType::Text) => Ok(RecordValue::Text(value.to_string())),
        Some(FieldType::Bytes) => Ok(RecordValue::Bytes(value.to_be_bytes().to_vec())),
        Some(FieldType::Bool) | Some(FieldType::F64) => Err(invalid_argument(format!(
            "time-series {field} index field must be U64, Bytes, Text, I64, or Timestamp"
        ))),
    }
}

fn declared_field_type(col: &Collection, field: &str) -> Option<FieldType> {
    let Some(Schema::SchemaFull(fields)) = &col.schema else {
        return None;
    };
    fields
        .iter()
        .find(|declared| declared.name == field)
        .map(|declared| declared.ty)
}

pub(crate) fn encode_point(val: f64) -> Vec<u8> {
    val.to_be_bytes().to_vec()
}

pub(crate) fn decode_point(bytes: &[u8]) -> Result<f64> {
    if bytes.len() != POINT_VALUE_BYTES {
        return Err(corrupt(format!(
            "time-series point value must be {POINT_VALUE_BYTES} bytes, got {}",
            bytes.len()
        )));
    }
    Ok(f64::from_be_bytes(bytes.try_into().unwrap()))
}

pub(crate) fn fold_rollup(current: Option<RollupValue>, val: f64) -> RollupValue {
    match current {
        None => RollupValue {
            count: 1,
            sum: val,
            min: val,
            max: val,
        },
        Some(acc) => RollupValue {
            count: acc.count + 1,
            sum: acc.sum + val,
            min: acc.min.min(val),
            max: acc.max.max(val),
        },
    }
}

pub(crate) fn encode_rollup(rollup: &RollupValue) -> Vec<u8> {
    let mut out = Vec::with_capacity(ROLLUP_VALUE_BYTES);
    out.extend_from_slice(&rollup.count.to_be_bytes());
    out.extend_from_slice(&rollup.sum.to_be_bytes());
    out.extend_from_slice(&rollup.min.to_be_bytes());
    out.extend_from_slice(&rollup.max.to_be_bytes());
    out
}

pub(crate) fn decode_rollup(bytes: &[u8]) -> Result<RollupValue> {
    if bytes.len() != ROLLUP_VALUE_BYTES {
        return Err(corrupt(format!(
            "time-series rollup value must be {ROLLUP_VALUE_BYTES} bytes, got {}",
            bytes.len()
        )));
    }
    Ok(RollupValue {
        count: u64::from_be_bytes(bytes[0..8].try_into().unwrap()),
        sum: f64::from_be_bytes(bytes[8..16].try_into().unwrap()),
        min: f64::from_be_bytes(bytes[16..24].try_into().unwrap()),
        max: f64::from_be_bytes(bytes[24..32].try_into().unwrap()),
    })
}

fn require_ts_mode(col: &Collection) -> Result<()> {
    if col.mode == CollectionMode::TimeSeries {
        Ok(())
    } else {
        Err(invalid_argument(format!(
            "time-series layer requires a TimeSeries collection, got {:?}",
            col.mode
        )))
    }
}

fn ledger_subject(point_key: &[u8]) -> SubjectId {
    SubjectId::Query(blake3::hash(point_key).as_bytes().to_vec())
}

fn ledger_payload(col: &Collection, series: u64, ts: u64, point_key: &[u8], val: f64) -> Vec<u8> {
    let mut payload = PayloadBuilder::default();
    payload
        .insert_str("collection_id", format!("{:016x}", collection_id(col)))
        .insert_str("series", series.to_string())
        .insert_str("ts", ts.to_string())
        .insert_str("point_hash", blake3::hash(point_key).to_hex().to_string())
        .insert_str("value_bits", format!("{:016x}", val.to_bits()));
    RedactionPolicy::default().apply_to_payload(&payload)
}

fn invalid_argument(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_INVALID_ARGUMENT,
        message: message.into(),
        remediation: "fix the time-series input",
    }
}

fn point_conflict(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_TIMESERIES_POINT_CONFLICT,
        message: message.into(),
        remediation: "use a new timestamp for a distinct measurement or replay the byte-identical value",
    }
}

fn corrupt(message: impl Into<String>) -> CalyxError {
    CalyxError::aster_corrupt_shard(message)
}
