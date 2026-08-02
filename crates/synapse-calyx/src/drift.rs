//! Phase-4 Calyx blind-spot and MMD distribution-drift detection in hygiene
//! (#1674).
//!
//! Two off-runtime maintenance analyses over one panel's physical `Base` CF
//! constellations:
//!
//! * **Blind spots** — cross-lens anomalies where one lens is confident that a
//!   record is close to a neighbor while a second lens on that same record/
//!   neighbor pair disagrees. This consumes the `calyx-loom` calibrated
//!   blind-spot detector: the per-record `delta = lens_a_similarity −
//!   lens_b_neighbor_mean` is scored against a per-slot-pair empirical
//!   calibration so a healthy corpus yields a bounded, `alpha`-controlled
//!   false-positive rate.
//! * **MMD drift** — per-lens maximum-mean-discrepancy two-sample test between a
//!   reference (older) window and a recent window of the lens's feature vectors,
//!   consuming the `calyx-assay` Gaussian-kernel MMD estimator. Drift findings
//!   are persisted to the native `Reactive` CF (the reactive-trigger/drift-event
//!   lane) so `#1677`/`#1681` can consume them later, then read back so every
//!   returned count is proven against the bytes.
//!
//! This is a distinct analysis from the `#1673` temporal `SynapseCalyxDriftReport`
//! (CUSUM/MMD over event-rate time series): this module measures distribution
//! drift of the *lens feature space* itself, not the event-arrival process.
//! Everything fails closed with structured `SynapseCalyxError` values and never
//! invents a fallback.

use std::collections::BTreeMap;

use calyx_assay::{
    DEFAULT_MMD_ALPHA, DEFAULT_MMD_PERMUTATIONS, DEFAULT_MMD_SEED, MmdConfig,
    gaussian_mmd_with_config,
};
use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{CxId, SlotId, SlotVector};
use calyx_forge::{Backend, KnnMetric};
use calyx_loom::{
    BlindSpotCalibration, BlindSpotCalibrationParams, Severity, detect_blind_spot_calibrated,
};
use serde::{Deserialize, Serialize};

use crate::{
    SYNAPSE_INTELLIGENCE_MAX_RECORDS, SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault,
};

/// Default calibration sample floor for the blind-spot detector (loom default).
pub const SYNAPSE_BLIND_SPOT_MIN_SAMPLES: usize = 50;
/// Default false-positive rate for the calibrated blind-spot detector.
pub const SYNAPSE_BLIND_SPOT_ALPHA: f32 = 0.05;
/// Default cap on blind-spot alerts surfaced in one pass.
pub const SYNAPSE_BLIND_SPOT_MAX_ALERTS: usize = 256;
/// Minimum samples per MMD window side (assay `MIN_MMD_SAMPLES`).
pub const SYNAPSE_DRIFT_MIN_WINDOW: usize = 4;
/// Maximum samples per MMD window side (assay `MAX_MMD_SAMPLES` per pooled side).
pub const SYNAPSE_DRIFT_MAX_WINDOW: usize = 1_024;
/// Default fraction of the most-recent records forming the recent window.
pub const SYNAPSE_DRIFT_DEFAULT_RECENT_FRACTION: f32 = 0.3;

const REACTIVE_DRIFT_PREFIX: &[u8; 8] = b"RDRIFT1\0";

// ---------------------------------------------------------------------------
// Blind spots
// ---------------------------------------------------------------------------

/// Bounded request describing one blind-spot scan over a panel.
#[derive(Clone, Copy, Debug)]
pub struct SynapseCalyxBlindSpotParams {
    pub panel_version: u32,
    pub max_records: usize,
    pub min_samples: usize,
    pub alpha: f32,
    pub max_alerts: usize,
}

impl SynapseCalyxBlindSpotParams {
    #[must_use]
    pub const fn new(panel_version: u32) -> Self {
        Self {
            panel_version,
            max_records: SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            min_samples: SYNAPSE_BLIND_SPOT_MIN_SAMPLES,
            alpha: SYNAPSE_BLIND_SPOT_ALPHA,
            max_alerts: SYNAPSE_BLIND_SPOT_MAX_ALERTS,
        }
    }
}

/// One flagged cross-lens blind-spot anomaly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxBlindSpotAlert {
    pub cx_id: String,
    pub slot_a: u16,
    pub slot_b: u16,
    pub lens_a_similarity: f32,
    pub lens_b_neighbor_mean: f32,
    pub delta: f32,
    pub severity: String,
    pub calibration_sample_count: usize,
    pub calibration_alpha: f32,
    pub calibration_p_value: f32,
    pub calibration_percentile: f32,
    pub threshold_delta: f32,
}

/// Result of one blind-spot scan (read-only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxBlindSpotReport {
    pub panel_version: u32,
    pub records_scanned: usize,
    pub records_measured: usize,
    pub n_lenses: usize,
    pub slot_pairs_evaluated: usize,
    pub slot_pairs_uncalibrated: usize,
    pub alerts_total: usize,
    pub alerts: Vec<SynapseCalyxBlindSpotAlert>,
}

// ---------------------------------------------------------------------------
// MMD distribution drift
// ---------------------------------------------------------------------------

/// Bounded request describing one MMD lens-drift scan over a panel.
#[derive(Clone, Copy, Debug)]
pub struct SynapseCalyxPanelDriftParams {
    pub panel_version: u32,
    pub max_records: usize,
    pub recent_fraction: f32,
    pub permutations: usize,
    pub alpha: f64,
}

impl SynapseCalyxPanelDriftParams {
    #[must_use]
    pub const fn new(panel_version: u32) -> Self {
        Self {
            panel_version,
            max_records: SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            recent_fraction: SYNAPSE_DRIFT_DEFAULT_RECENT_FRACTION,
            permutations: DEFAULT_MMD_PERMUTATIONS,
            alpha: DEFAULT_MMD_ALPHA,
        }
    }
}

/// One lens's MMD reference-vs-recent distribution-drift measurement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxLensDrift {
    pub slot: u16,
    pub dimension: usize,
    pub reference_n: usize,
    pub recent_n: usize,
    pub mmd2: f64,
    pub p_value: f64,
    pub bandwidth: f64,
    pub significant: bool,
    pub persisted: bool,
}

/// Result of one MMD lens-drift scan with the physical `Reactive` CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxPanelDriftReport {
    pub panel_version: u32,
    pub records_scanned: usize,
    pub records_measured: usize,
    pub recent_fraction: f32,
    pub permutations: usize,
    pub lenses_evaluated: usize,
    pub lenses_insufficient: usize,
    pub drifted_lenses: usize,
    pub lens_drift: Vec<SynapseCalyxLensDrift>,
    pub reactive_cf_rows_after: usize,
    pub drift_rows_persisted: usize,
}

/// Persisted drift finding row shape written to the native `Reactive` CF.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct PersistedDriftFinding {
    panel_version: u32,
    slot: u16,
    dimension: usize,
    reference_n: usize,
    recent_n: usize,
    mmd2: f64,
    p_value: f64,
    bandwidth: f64,
    significant: bool,
    observed_seq: u64,
}

/// One record reduced to its dense slot vectors, in `Base` CF scan order.
struct DriftRecord {
    cx_id: CxId,
    slots: BTreeMap<SlotId, Vec<f32>>,
}

struct DriftCorpus {
    records: Vec<DriftRecord>,
    records_scanned: usize,
}

impl SynapseCalyxVault {
    /// Scans a panel for cross-lens blind spots: records where one lens judges a
    /// record close to a neighbor while a second lens on that same pair
    /// disagrees, scored against a per-slot-pair calibration so a healthy corpus
    /// stays within the `alpha` false-positive rate. Read-only.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the `Base` CF cannot be
    /// scanned, a constellation fails to decode, the math backend is
    /// unavailable, or the calibrated detector rejects a non-finite delta.
    #[allow(clippy::too_many_lines)]
    pub fn blind_spot_scan(
        &self,
        params: &SynapseCalyxBlindSpotParams,
    ) -> Result<SynapseCalyxBlindSpotReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("blind_spot_scan");
        let max_records = params
            .max_records
            .clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let min_samples = params.min_samples.max(SYNAPSE_BLIND_SPOT_MIN_SAMPLES);
        let corpus = self.load_drift_corpus(params.panel_version, max_records)?;
        let backend = self.math_runtime.backend();

        // Enumerate the distinct dense lenses present in the corpus.
        let mut lens_ids: Vec<SlotId> = Vec::new();
        for record in &corpus.records {
            for slot in record.slots.keys() {
                if !lens_ids.contains(slot) {
                    lens_ids.push(*slot);
                }
            }
        }
        lens_ids.sort_unstable();

        let mut alerts: Vec<SynapseCalyxBlindSpotAlert> = Vec::new();
        let mut slot_pairs_evaluated = 0usize;
        let mut slot_pairs_uncalibrated = 0usize;

        for (i, &slot_a) in lens_ids.iter().enumerate() {
            for &slot_b in lens_ids.iter().skip(i + 1) {
                // Records carrying both lenses, restricted to the modal (dim_a,
                // dim_b) shape so cosine kNN on A and cosine on B are defined.
                let members = paired_dense_members(&corpus.records, slot_a, slot_b);
                let Some(group) = modal_shape_group(&members) else {
                    continue;
                };
                if group.len() < min_samples {
                    slot_pairs_uncalibrated += 1;
                    continue;
                }
                let dim_a = group[0].1.len();
                let nearest = nearest_neighbor_indices(backend, &group, dim_a)?;

                // delta_i = a_sim_i − b_sim_i for every record with a neighbor.
                let mut deltas: Vec<f32> = Vec::with_capacity(group.len());
                let mut per_record: Vec<(usize, f32, f32)> = Vec::with_capacity(group.len());
                for (index, neighbor) in nearest.iter().enumerate() {
                    let Some((neighbor_index, a_sim)) = *neighbor else {
                        continue;
                    };
                    let b_sim = cosine_similarity(group[index].2, group[neighbor_index].2);
                    let delta = a_sim - b_sim;
                    if !delta.is_finite() {
                        continue;
                    }
                    deltas.push(delta);
                    per_record.push((index, a_sim, b_sim));
                }
                if deltas.len() < min_samples {
                    slot_pairs_uncalibrated += 1;
                    continue;
                }

                let calibration = match BlindSpotCalibration::from_deltas(
                    deltas.iter().copied(),
                    BlindSpotCalibrationParams {
                        min_samples,
                        alpha: params.alpha,
                    },
                ) {
                    Ok(calibration) => calibration,
                    Err(error) => {
                        // Uncalibratable pair (e.g. below the sample floor) is a
                        // data condition, not a fault: report it, keep scanning.
                        tracing::debug!(
                            code = "SYNAPSE_BLIND_SPOT_UNCALIBRATED",
                            slot_a = slot_a.get(),
                            slot_b = slot_b.get(),
                            error = %error.message,
                            "blind-spot pair could not be calibrated"
                        );
                        slot_pairs_uncalibrated += 1;
                        continue;
                    }
                };
                slot_pairs_evaluated += 1;

                for (index, a_sim, b_sim) in per_record {
                    let cx_id = group[index].0;
                    let alert = detect_blind_spot_calibrated(
                        cx_id,
                        slot_a,
                        slot_b,
                        a_sim,
                        b_sim,
                        &calibration,
                    )
                    .map_err(|error| {
                        SynapseCalyxError::from_calyx("evaluate calibrated blind spot", &error)
                    })?;
                    if let Some(alert) = alert {
                        alerts.push(blind_spot_alert(&alert, a_sim, b_sim));
                    }
                }
            }
        }

        // Deterministic, most-severe-first ordering, then cap.
        alerts.sort_by(|a, b| {
            b.delta
                .total_cmp(&a.delta)
                .then_with(|| a.cx_id.cmp(&b.cx_id))
                .then(a.slot_a.cmp(&b.slot_a))
                .then(a.slot_b.cmp(&b.slot_b))
        });
        let alerts_total = alerts.len();
        alerts.truncate(params.max_alerts.max(1));

        Ok(SynapseCalyxBlindSpotReport {
            panel_version: params.panel_version,
            records_scanned: corpus.records_scanned,
            records_measured: corpus.records.len(),
            n_lenses: lens_ids.len(),
            slot_pairs_evaluated,
            slot_pairs_uncalibrated,
            alerts_total,
            alerts,
        })
    }

    /// Measures per-lens distribution drift by a Gaussian-kernel MMD two-sample
    /// test between a reference (older) window and a recent window of each dense
    /// lens's feature vectors, persists every evaluated finding to the native
    /// `Reactive` CF, and reads the CF back.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the `Base` CF cannot be
    /// scanned, a constellation fails to decode, the MMD estimator hard-fails
    /// (a per-lens low-signal/insufficient-sample condition is reported, not
    /// raised), or the `Reactive` CF write/readback fails.
    #[allow(clippy::too_many_lines)]
    pub fn mmd_panel_drift(
        &self,
        params: &SynapseCalyxPanelDriftParams,
    ) -> Result<SynapseCalyxPanelDriftReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("mmd_panel_drift");
        let max_records = params
            .max_records
            .clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let recent_fraction = params.recent_fraction.clamp(0.05, 0.95);
        let permutations = params.permutations.max(1);
        let alpha = if params.alpha.is_finite() && params.alpha > 0.0 && params.alpha < 1.0 {
            params.alpha
        } else {
            DEFAULT_MMD_ALPHA
        };
        let corpus = self.load_drift_corpus(params.panel_version, max_records)?;

        // Gather each lens's vectors in Base scan order (oldest first).
        let mut by_slot: BTreeMap<SlotId, Vec<Vec<f32>>> = BTreeMap::new();
        for record in &corpus.records {
            for (slot, vector) in &record.slots {
                by_slot.entry(*slot).or_default().push(vector.clone());
            }
        }

        let config = MmdConfig {
            bandwidth: None,
            permutations,
            seed: DEFAULT_MMD_SEED,
            alpha,
        };
        let observed_seq = self.latest_seq();

        let mut lens_drift: Vec<SynapseCalyxLensDrift> = Vec::new();
        let mut writes: Vec<SynapseCalyxCfWrite> = Vec::new();
        let mut lenses_insufficient = 0usize;
        let mut drifted_lenses = 0usize;

        for (slot, vectors) in by_slot {
            // Restrict to the modal dimension so the window rows share a shape.
            let Some(shaped) = modal_dimension_vectors(&vectors) else {
                lenses_insufficient += 1;
                continue;
            };
            let dimension = shaped[0].len();
            let split = reference_split(shaped.len(), recent_fraction);
            let reference = &shaped[..split];
            let recent = &shaped[split..];
            if reference.len() < SYNAPSE_DRIFT_MIN_WINDOW || recent.len() < SYNAPSE_DRIFT_MIN_WINDOW
            {
                lenses_insufficient += 1;
                continue;
            }
            let reference_f64 = to_f64_rows(reference, SYNAPSE_DRIFT_MAX_WINDOW);
            let recent_f64 = to_f64_rows(recent, SYNAPSE_DRIFT_MAX_WINDOW);

            let report = match gaussian_mmd_with_config(&reference_f64, &recent_f64, &config) {
                Ok(report) => report,
                Err(error) => {
                    // A degenerate lens (zero pairwise distance / too few usable
                    // samples) is unmeasurable, not a fault: report and continue.
                    if is_unmeasurable(&error) {
                        tracing::debug!(
                            code = "SYNAPSE_DRIFT_LENS_UNMEASURABLE",
                            slot = slot.get(),
                            error = %error.message,
                            "MMD drift lens unmeasurable"
                        );
                        lenses_insufficient += 1;
                        continue;
                    }
                    return Err(SynapseCalyxError::from_calyx(
                        "estimate MMD lens drift",
                        &error,
                    ));
                }
            };

            if report.significant {
                drifted_lenses += 1;
            }
            let finding = PersistedDriftFinding {
                panel_version: params.panel_version,
                slot: slot.get(),
                dimension,
                reference_n: reference_f64.len(),
                recent_n: recent_f64.len(),
                mmd2: report.mmd2,
                p_value: report.p_value,
                bandwidth: report.bandwidth,
                significant: report.significant,
                observed_seq,
            };
            writes.push(SynapseCalyxCfWrite {
                cf: ColumnFamily::Reactive,
                key: reactive_drift_key(params.panel_version, slot.get()),
                value: encode_json(&finding)?,
            });
            lens_drift.push(SynapseCalyxLensDrift {
                slot: slot.get(),
                dimension,
                reference_n: reference_f64.len(),
                recent_n: recent_f64.len(),
                mmd2: report.mmd2,
                p_value: report.p_value,
                bandwidth: report.bandwidth,
                significant: report.significant,
                persisted: true,
            });
        }

        let drift_rows_persisted = writes.len();
        if !writes.is_empty() {
            self.write_cf_batch(writes)?;
            self.flush()?;
        }
        let reactive_cf_rows_after = self.count_cf_latest(ColumnFamily::Reactive)?;

        Ok(SynapseCalyxPanelDriftReport {
            panel_version: params.panel_version,
            records_scanned: corpus.records_scanned,
            records_measured: corpus.records.len(),
            recent_fraction,
            permutations,
            lenses_evaluated: lens_drift.len(),
            lenses_insufficient,
            drifted_lenses,
            lens_drift,
            reactive_cf_rows_after,
            drift_rows_persisted,
        })
    }

    /// Scans the `Base` CF once and returns the dense-slot corpus for a panel in
    /// scan order (oldest first), for the drift/blind-spot analyses.
    fn load_drift_corpus(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> Result<DriftCorpus, SynapseCalyxError> {
        let rows = self.scan_cf_latest(ColumnFamily::Base)?;
        let snapshot = self.read_snapshot();
        let mut records = Vec::new();
        let mut records_scanned = 0usize;
        for (_, value) in rows {
            let base = decode_constellation_base(&value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode Base constellation", &error)
            })?;
            if base.panel_version != panel_version {
                continue;
            }
            records_scanned += 1;
            if records.len() >= max_records {
                continue;
            }
            // Slot vectors live in the per-slot CFs; a Base row decodes to
            // `Absent` for every slot, so reading them from it produced an empty
            // drift corpus that measured nothing (issue #1894).
            let constellation = self.hydrated_constellation(base.cx_id, snapshot)?;
            let slots = constellation
                .slots
                .iter()
                .filter_map(|(slot, vector)| dense_vector(vector).map(|dense| (*slot, dense)))
                .collect();
            records.push(DriftRecord {
                cx_id: constellation.cx_id,
                slots,
            });
        }
        Ok(DriftCorpus {
            records,
            records_scanned,
        })
    }
}

/// One record's shared presence on a lens pair: `(cx_id, vector_a, vector_b)`.
type PairedMember<'a> = (CxId, &'a Vec<f32>, &'a Vec<f32>);

/// Records carrying both dense lenses, as `(cx_id, vector_a, vector_b)`.
fn paired_dense_members(
    records: &[DriftRecord],
    slot_a: SlotId,
    slot_b: SlotId,
) -> Vec<PairedMember<'_>> {
    let mut members = Vec::new();
    for record in records {
        if let (Some(a), Some(b)) = (record.slots.get(&slot_a), record.slots.get(&slot_b)) {
            members.push((record.cx_id, a, b));
        }
    }
    members
}

/// Restricts a member set to the modal `(dim_a, dim_b)` shape so both lenses'
/// cosine operations are defined over a uniform dimension.
fn modal_shape_group<'a>(members: &[PairedMember<'a>]) -> Option<Vec<PairedMember<'a>>> {
    let mut counts: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for (_, a, b) in members {
        if a.is_empty() || b.is_empty() {
            continue;
        }
        *counts.entry((a.len(), b.len())).or_default() += 1;
    }
    let (shape, _) = counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then(right.0.cmp(&left.0)))?;
    let group: Vec<_> = members
        .iter()
        .filter(|(_, a, b)| a.len() == shape.0 && b.len() == shape.1)
        .map(|(cx, a, b)| (*cx, *a, *b))
        .collect();
    (group.len() >= 2).then_some(group)
}

/// For each record, its nearest non-self neighbor index and cosine score on lens
/// A, computed with the forge backend cosine kNN.
fn nearest_neighbor_indices(
    backend: &dyn Backend,
    group: &[PairedMember<'_>],
    dim_a: usize,
) -> Result<Vec<Option<(usize, f32)>>, SynapseCalyxError> {
    let count = group.len();
    let mut flat = Vec::with_capacity(count * dim_a);
    for (_, a, _) in group {
        flat.extend_from_slice(a);
    }
    let k = 2.min(count).max(1);
    let batch = backend
        .knn(&flat, &flat, count, dim_a, k, KnnMetric::Cosine)
        .map_err(|error| forge_math_error("blind-spot nearest-neighbor kNN", &error))?;
    let mut nearest = vec![None; count];
    for (query_index, slot) in nearest.iter_mut().enumerate() {
        let base = query_index * batch.k;
        for offset in 0..batch.k {
            let candidate = batch.indices[base + offset];
            if candidate == query_index {
                continue;
            }
            *slot = Some((candidate, batch.scores[base + offset]));
            break;
        }
    }
    Ok(nearest)
}

fn blind_spot_alert(
    alert: &calyx_loom::BlindSpotAlert,
    lens_a_similarity: f32,
    lens_b_neighbor_mean: f32,
) -> SynapseCalyxBlindSpotAlert {
    let (sample_count, alpha, p_value, percentile, threshold_delta) = alert
        .calibration
        .as_ref()
        .map_or((0, 0.0, 0.0, 0.0, 0.0), |evidence| {
            (
                evidence.sample_count,
                evidence.alpha,
                evidence.p_value,
                evidence.percentile,
                evidence.threshold_delta,
            )
        });
    SynapseCalyxBlindSpotAlert {
        cx_id: alert.cx_id.to_string(),
        slot_a: alert.a.get(),
        slot_b: alert.b.get(),
        lens_a_similarity,
        lens_b_neighbor_mean,
        delta: alert.delta,
        severity: severity_label(alert.severity).to_owned(),
        calibration_sample_count: sample_count,
        calibration_alpha: alpha,
        calibration_p_value: p_value,
        calibration_percentile: percentile,
        threshold_delta,
    }
}

const fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
    }
}

fn dense_vector(vector: &SlotVector) -> Option<Vec<f32>> {
    match vector {
        SlotVector::Dense { data, .. } => Some(data.clone()),
        SlotVector::Sparse { .. } | SlotVector::Multi { .. } | SlotVector::Absent { .. } => None,
    }
}

/// Restricts a lens's vectors to its modal dimension, preserving scan order.
fn modal_dimension_vectors(vectors: &[Vec<f32>]) -> Option<Vec<Vec<f32>>> {
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for vector in vectors {
        if !vector.is_empty() {
            *counts.entry(vector.len()).or_default() += 1;
        }
    }
    let (dim, _) = counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)))?;
    let shaped: Vec<Vec<f32>> = vectors
        .iter()
        .filter(|vector| vector.len() == dim)
        .cloned()
        .collect();
    (!shaped.is_empty()).then_some(shaped)
}

/// Reference/recent split index: the reference window is the older prefix.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn reference_split(n: usize, recent_fraction: f32) -> usize {
    let recent = ((n as f32) * recent_fraction).round() as usize;
    n.saturating_sub(recent.clamp(1, n))
}

/// Converts f32 rows to f64, capping the window at `max` most-recent-preserving
/// rows (keeps the tail so a truncated reference stays contiguous with recent).
fn to_f64_rows(rows: &[Vec<f32>], max: usize) -> Vec<Vec<f64>> {
    let start = rows.len().saturating_sub(max);
    rows[start..]
        .iter()
        .map(|row| row.iter().map(|value| f64::from(*value)).collect())
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return f32::NAN;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot = x.mul_add(*y, dot);
        norm_a = x.mul_add(*x, norm_a);
        norm_b = y.mul_add(*y, norm_b);
    }
    if norm_a <= 0.0 || norm_b <= 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

fn is_unmeasurable(error: &calyx_core::CalyxError) -> bool {
    matches!(
        error.code,
        "CALYX_ASSAY_LOW_SIGNAL" | "CALYX_ASSAY_INSUFFICIENT_SAMPLES"
    )
}

fn reactive_drift_key(panel_version: u32, slot: u16) -> Vec<u8> {
    let mut key = Vec::with_capacity(REACTIVE_DRIFT_PREFIX.len() + 6);
    key.extend_from_slice(REACTIVE_DRIFT_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key.extend_from_slice(&slot.to_be_bytes());
    key
}

fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, SynapseCalyxError> {
    serde_json::to_vec(value).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_DRIFT_ENCODE",
            format!("encode drift finding CF row: {error}"),
            "inspect the drift finding shape; a derived Reactive CF row failed to serialize",
        )
    })
}

fn forge_math_error(action: &str, error: &calyx_forge::ForgeError) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_DRIFT_FORGE",
        format!("{action}: Forge math backend failed: {error}"),
        "repair the process-local/host GPU Source of Truth or select math_backend=cpu, then retry",
    )
}
