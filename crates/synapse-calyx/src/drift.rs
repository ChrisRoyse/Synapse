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
//!
//!   **The gate that makes that a finding rather than an artifact (#1961).**
//!   The neighbor is chosen to *maximize* similarity under lens A, so
//!   `lens_a_similarity` is an argmax: "lens A is confident" is true by
//!   selection, not by observation, for any lens whose neighbor-similarity
//!   distribution is degenerate. A one-hot lens returns cosine exactly `1.0` for
//!   any two records sharing a category and can return nothing else, and on the
//!   live timeline panel *every* dense lens returned exactly `1.0` for every
//!   record — so the rule reduced to "lens B's similarity is low, offset by a
//!   constant". Each ordered `(A, B)` direction therefore measures A's
//!   distribution first ([`calyx_loom::SimilarityDiscrimination`]) and is
//!   **refused with its evidence** when A cannot discriminate. Both orderings of
//!   every pair are candidates, because the rule is directional.
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

use std::collections::{BTreeMap, BTreeSet};

#[cfg(feature = "calyx-cuda")]
use calyx_assay::gaussian_mmd_flat_with_config_cuda_budgeted;
use calyx_assay::{
    DEFAULT_MMD_ALPHA, DEFAULT_MMD_PERMUTATIONS, DEFAULT_MMD_SEED, MmdConfig,
    gaussian_mmd_flat_with_config_cpu_strict,
};
use calyx_aster::cf::{ColumnFamily, prefix_range};
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{CxId, SlotId, SlotVector};
use calyx_forge::{Backend, KnnMetric};
use calyx_loom::{
    BlindSpotCalibration, BlindSpotCalibrationParams, SIMILARITY_DISTINCT_TOLERANCE, Severity,
    SimilarityDiscrimination, detect_blind_spot_calibrated,
};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::{
    SYNAPSE_INTELLIGENCE_MAX_RECORDS, SynapseCalyxCfWrite, SynapseCalyxError,
    SynapseCalyxMathExecutionClass, SynapseCalyxVault,
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
/// Hard work bound for one MMD null-distribution estimate. The public request
/// is rejected above this ceiling; it is never silently truncated.
pub const SYNAPSE_DRIFT_MAX_PERMUTATIONS: usize = 10_000;
const REACTIVE_DRIFT_PREFIX: &[u8; 8] = b"RDRIFT1\0";
const REACTIVE_RECURRENCE_PREFIX: &[u8; 8] = b"RRECUR1\0";
const REACTIVE_NOVELTY_PREFIX: &[u8; 8] = b"RNOVEL1\0";
const REACTIVE_REGION_PREFIX: &[u8; 8] = b"RREGION1";
const REACTIVE_REGION_DELIVERY_CURSOR_KEY: &[u8] = b"reactive_delivery\0region_event_bus_v1";
const REACTIVE_NOVELTY_DELIVERY_CURSOR_KEY: &[u8] = b"reactive_delivery\0ward_novelty_v1";

/// Durable recurrence event stored in `Reactive` before live publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxPersistedRecurrenceFinding {
    pub subject_kind: String,
    pub subject_id: String,
    pub subject_cx_id: String,
    pub occurrence_id: u64,
    pub frequency: u64,
    pub observed_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxPersistedNoveltyFinding {
    pub panel_version: u32,
    pub query_cx_id: String,
    pub guard_id: String,
    pub action: String,
    pub failing_slots: Vec<u16>,
    pub ledger_seq: u64,
    pub ledger_hash: String,
}

/// Exact first-observation cause for a discrete region such as an application identity.
///
/// This is separate from Ward cosine novelty: the acceptance fact is
/// that an exact frozen identity has never occurred before.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxPersistedRegionFinding {
    pub region_kind: String,
    pub region_id: String,
    pub subject_cx_id: String,
    pub trigger_cx_id: String,
    pub occurrence_id: u64,
    pub frequency: u64,
    pub observed_seq: u64,
}

impl SynapseCalyxVault {
    /// Reads the durable Ward-novelty relay cursor.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the Registry row cannot be read or decoded.
    pub fn novelty_delivery_cursor(&self) -> Result<u64, SynapseCalyxError> {
        self.read_cf_latest(ColumnFamily::Registry, REACTIVE_NOVELTY_DELIVERY_CURSOR_KEY)?
            .map_or(Ok(0), |bytes| {
                serde_json::from_slice::<u64>(&bytes).map_err(|error| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_REACTIVE_CURSOR_CORRUPT",
                        format!("decode Ward novelty delivery cursor: {error}"),
                        "preserve the vault and repair the named Registry cursor from delivered notification and escalation evidence",
                    )
                })
            })
    }

    /// Advances and independently reads back the durable Ward-novelty relay cursor.
    ///
    /// # Errors
    ///
    /// Returns a structured error on cursor regression or when commit, flush, or readback fails.
    pub fn persist_novelty_delivery_cursor(
        &self,
        ledger_seq: u64,
    ) -> Result<u64, SynapseCalyxError> {
        let current = self.novelty_delivery_cursor()?;
        if ledger_seq < current {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_CURSOR_REGRESSION",
                format!(
                    "Ward novelty delivery cursor cannot regress from {current} to {ledger_seq}"
                ),
                "retain the greater committed cursor and inspect relay ordering before retrying",
            ));
        }
        self.write_cf_batch(vec![SynapseCalyxCfWrite {
            cf: ColumnFamily::Registry,
            key: REACTIVE_NOVELTY_DELIVERY_CURSOR_KEY.to_vec(),
            value: encode_json(&ledger_seq)?,
        }])?;
        self.flush()?;
        let readback = self.novelty_delivery_cursor()?;
        if readback != ledger_seq {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_CURSOR_READBACK_MISMATCH",
                format!("wrote Ward novelty delivery cursor {ledger_seq}, read back {readback}"),
                "stop relay, preserve the vault, and inspect the Registry commit before retrying",
            ));
        }
        Ok(readback)
    }

    /// Reads a bounded prefix of durable Ward-novelty outbox rows.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the Reactive range cannot be read or a row is corrupt.
    pub fn persisted_novelty_findings(
        &self,
        after_ledger_seq: u64,
        max_rows: usize,
    ) -> Result<Vec<SynapseCalyxPersistedNoveltyFinding>, SynapseCalyxError> {
        let rows = self.scan_cf_range_page_latest(
            ColumnFamily::Reactive,
            &prefix_range(REACTIVE_NOVELTY_PREFIX),
            None,
            max_rows.max(1),
        )?;
        let mut findings = Vec::with_capacity(rows.rows.len());
        for (key, value) in rows.rows {
            let finding: SynapseCalyxPersistedNoveltyFinding = serde_json::from_slice(&value)
                .map_err(|error| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_REACTIVE_READBACK_CORRUPT",
                        format!("decode Reactive CF Ward novelty row {}: {error}", crate::hex_bytes(&key)),
                        "preserve the vault and repair only the named corrupt derived row before replay",
                    )
                })?;
            if finding.ledger_seq > after_ledger_seq {
                findings.push(finding);
            }
        }
        findings.sort_by_key(|finding| finding.ledger_seq);
        Ok(findings)
    }

    /// Reads the durable event-bus relay cursor for exact region findings.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the Registry row cannot be read or decoded.
    pub fn region_delivery_cursor(&self) -> Result<u64, SynapseCalyxError> {
        self.read_cf_latest(ColumnFamily::Registry, REACTIVE_REGION_DELIVERY_CURSOR_KEY)?
            .map_or(Ok(0), |bytes| {
                serde_json::from_slice::<u64>(&bytes).map_err(|error| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_REACTIVE_CURSOR_CORRUPT",
                        format!("decode region delivery cursor: {error}"),
                        "preserve the vault and repair the named Registry cursor from delivered event evidence",
                    )
                })
            })
    }

    /// Advances and independently reads back the durable region relay cursor.
    ///
    /// # Errors
    ///
    /// Returns a structured error on cursor regression or when commit, flush, or readback fails.
    pub fn persist_region_delivery_cursor(
        &self,
        observed_seq: u64,
    ) -> Result<u64, SynapseCalyxError> {
        let current = self.region_delivery_cursor()?;
        if observed_seq < current {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_CURSOR_REGRESSION",
                format!("region delivery cursor cannot regress from {current} to {observed_seq}"),
                "retain the greater committed cursor and inspect relay ordering before retrying",
            ));
        }
        self.write_cf_batch(vec![SynapseCalyxCfWrite {
            cf: ColumnFamily::Registry,
            key: REACTIVE_REGION_DELIVERY_CURSOR_KEY.to_vec(),
            value: encode_json(&observed_seq)?,
        }])?;
        self.flush()?;
        let readback = self.region_delivery_cursor()?;
        if readback != observed_seq {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_CURSOR_READBACK_MISMATCH",
                format!("wrote region delivery cursor {observed_seq}, read back {readback}"),
                "stop relay, preserve the vault, and inspect the Registry commit before retrying",
            ));
        }
        Ok(readback)
    }

    /// Persists one exact first-observation region event and proves its bytes by
    /// an independent point read before returning it.
    ///
    /// # Errors
    ///
    /// Returns a structured error when encoding, commit, flush, or independent readback fails.
    pub fn persist_region_finding(
        &self,
        finding: &SynapseCalyxPersistedRegionFinding,
    ) -> Result<SynapseCalyxPersistedRegionFinding, SynapseCalyxError> {
        let key = reactive_region_key(finding);
        self.write_cf_batch(vec![SynapseCalyxCfWrite {
            cf: ColumnFamily::Reactive,
            key: key.clone(),
            value: encode_json(finding)?,
        }])?;
        self.flush()?;
        let bytes = self.read_cf_latest(ColumnFamily::Reactive, &key)?.ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_READBACK_MISSING",
                format!(
                    "Reactive CF region row disappeared after commit: kind={} id={} occurrence={}",
                    finding.region_kind, finding.region_id, finding.occurrence_id
                ),
                "stop writers, preserve the vault, and inspect the Reactive CF commit before retrying region delivery",
            )
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_READBACK_CORRUPT",
                format!(
                    "decode committed Reactive CF region row for kind={} id={} occurrence={}: {error}",
                    finding.region_kind, finding.region_id, finding.occurrence_id
                ),
                "stop writers, preserve the vault, and inspect the named Reactive CF row",
            )
        })
    }

    /// Reads a bounded prefix of exact first-observation region rows from the
    /// durable Reactive outbox. A corrupt matching row fails the whole read.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the Reactive range cannot be read or a row is corrupt.
    pub fn persisted_region_findings(
        &self,
        after_observed_seq: u64,
        max_rows: usize,
    ) -> Result<Vec<SynapseCalyxPersistedRegionFinding>, SynapseCalyxError> {
        let rows = self.scan_cf_range_page_latest(
            ColumnFamily::Reactive,
            &prefix_range(REACTIVE_REGION_PREFIX),
            None,
            max_rows.max(1),
        )?;
        let mut findings = Vec::with_capacity(rows.rows.len());
        for (key, value) in rows.rows {
            let finding: SynapseCalyxPersistedRegionFinding =
                serde_json::from_slice(&value).map_err(|error| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_REACTIVE_READBACK_CORRUPT",
                    format!("decode Reactive CF region row {}: {error}", crate::hex_bytes(&key)),
                    "preserve the vault and repair or remove only the named corrupt derived row before replay",
                )
                })?;
            if finding.observed_seq <= after_observed_seq {
                continue;
            }
            findings.push(finding);
        }
        Ok(findings)
    }

    /// Persists one Ward novelty finding and proves its committed bytes by point read.
    ///
    /// # Errors
    ///
    /// Returns a structured error when key construction, encoding, commit, flush, or readback fails.
    pub fn persist_novelty_finding(
        &self,
        finding: &SynapseCalyxPersistedNoveltyFinding,
    ) -> Result<SynapseCalyxPersistedNoveltyFinding, SynapseCalyxError> {
        let key = reactive_novelty_key(finding)?;
        self.write_cf_batch(vec![SynapseCalyxCfWrite {
            cf: ColumnFamily::Reactive,
            key: key.clone(),
            value: encode_json(finding)?,
        }])?;
        self.flush()?;
        let bytes = self.read_cf_latest(ColumnFamily::Reactive, &key)?.ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_READBACK_MISSING",
                format!(
                    "Reactive CF novelty row disappeared after commit: panel={} query={} ledger_seq={}",
                    finding.panel_version, finding.query_cx_id, finding.ledger_seq
                ),
                "stop writers, preserve the vault, and inspect the Reactive CF commit before retrying novelty delivery",
            )
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_READBACK_CORRUPT",
                format!(
                    "decode committed Reactive CF novelty row for panel={} query={} ledger_seq={}: {error}",
                    finding.panel_version, finding.query_cx_id, finding.ledger_seq
                ),
                "stop writers, preserve the vault, and inspect the named Reactive CF row",
            )
        })
    }

    /// Persists one caller-validated recurrence as a replay-stable outbox row
    /// and proves the committed bytes by an independent point read.
    ///
    /// # Errors
    ///
    /// Returns a structured error when encoding, commit, flush, or independent readback fails.
    pub fn persist_recurrence_finding(
        &self,
        finding: &SynapseCalyxPersistedRecurrenceFinding,
    ) -> Result<SynapseCalyxPersistedRecurrenceFinding, SynapseCalyxError> {
        let key = reactive_recurrence_key(finding);
        self.write_cf_batch(vec![SynapseCalyxCfWrite {
            cf: ColumnFamily::Reactive,
            key: key.clone(),
            value: encode_json(finding)?,
        }])?;
        self.flush()?;
        let bytes = self.read_cf_latest(ColumnFamily::Reactive, &key)?.ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_READBACK_MISSING",
                format!(
                    "Reactive CF recurrence row disappeared after commit: kind={} subject={} occurrence={}",
                    finding.subject_kind, finding.subject_id, finding.occurrence_id
                ),
                "stop writers, preserve the vault, and inspect the Reactive CF commit before retrying recurrence delivery",
            )
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_REACTIVE_READBACK_CORRUPT",
                format!(
                    "decode committed Reactive CF recurrence row for kind={} subject={} occurrence={}: {error}",
                    finding.subject_kind, finding.subject_id, finding.occurrence_id
                ),
                "stop writers, preserve the vault, and inspect the named Reactive CF row",
            )
        })
    }
}

pub fn reactive_novelty_key(
    finding: &SynapseCalyxPersistedNoveltyFinding,
) -> Result<Vec<u8>, SynapseCalyxError> {
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(REACTIVE_NOVELTY_PREFIX);
    key.extend_from_slice(&finding.ledger_seq.to_be_bytes());
    let query_cx_id = crate::parse_cx_id(&finding.query_cx_id)?;
    key.extend_from_slice(query_cx_id.as_bytes());
    Ok(key)
}

fn reactive_recurrence_key(finding: &SynapseCalyxPersistedRecurrenceFinding) -> Vec<u8> {
    let mut hasher = sha2::Sha256::new();
    hasher.update(finding.subject_kind.as_bytes());
    hasher.update([0]);
    hasher.update(finding.subject_id.as_bytes());
    let digest = hasher.finalize();
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(REACTIVE_RECURRENCE_PREFIX);
    key.extend_from_slice(&finding.observed_seq.to_be_bytes());
    key.extend_from_slice(&digest[..8]);
    key.extend_from_slice(&finding.occurrence_id.to_be_bytes());
    key
}

pub fn reactive_region_key(finding: &SynapseCalyxPersistedRegionFinding) -> Vec<u8> {
    let mut hasher = sha2::Sha256::new();
    hasher.update(finding.region_kind.as_bytes());
    hasher.update([0]);
    hasher.update(finding.region_id.as_bytes());
    let digest = hasher.finalize();
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(REACTIVE_REGION_PREFIX);
    key.extend_from_slice(&finding.observed_seq.to_be_bytes());
    key.extend_from_slice(&digest[..8]);
    key.extend_from_slice(&finding.occurrence_id.to_be_bytes());
    key
}

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
    /// Distinct delta values behind this pair's calibration (#1961 ask 2).
    pub calibration_distinct_deltas: usize,
    /// Whether that calibration can resolve `alpha` at all. When false the
    /// certificate proves only that the observation sits at the top of a nearly
    /// constant variable, and the severity is capped at `low`.
    pub calibration_resolves_alpha: bool,
    /// The lowest similarity lens B actually reached over this pair's records.
    pub lens_b_observed_min: f32,
    /// The highest similarity lens B actually reached over this pair's records.
    pub lens_b_observed_max: f32,
    /// Where `lens_b_neighbor_mean` sits inside lens B's own reached range:
    /// `0.0` at B's maximum, `1.0` at B's minimum (#1961 ask 3).
    ///
    /// A cyclic encoding reaches cosine `-1.0` for any two timestamps half a
    /// period apart, so an absolute `-1.0` says nothing on its own. Read against
    /// the range the encoding actually reached, it says how unusual the
    /// disagreement is *for that encoding*.
    pub lens_b_dissent_fraction: f32,
}

/// One slot-pair direction that was refused rather than evaluated, with the
/// measured evidence for the refusal (#1961 ask 1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxBlindSpotPairDiagnostic {
    pub slot_a: u16,
    pub slot_b: u16,
    pub code: String,
    pub detail: String,
    pub records: usize,
    pub lens_a_distinct_values: usize,
    pub lens_a_modal_value: f32,
    pub lens_a_modal_share: f32,
    pub lens_a_observed_min: f32,
    pub lens_a_observed_max: f32,
}

/// Result of one blind-spot scan (read-only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxBlindSpotReport {
    pub panel_version: u32,
    pub records_scanned: usize,
    pub records_measured: usize,
    /// Provenance of the panel-selective membership walk this corpus was folded
    /// from at one pinned snapshot.
    pub walk: crate::SynapseCalyxPanelBaseWalk,
    pub n_lenses: usize,
    /// Ordered `(A, B)` directions evaluated. Both directions of every unordered
    /// pair are candidates: "lens 4 is confident where lens 1 disagrees" is a
    /// different claim from its reverse, and only testing one of them silently
    /// dropped half the detector's reach.
    pub slot_pairs_evaluated: usize,
    pub slot_pairs_uncalibrated: usize,
    /// Directions refused because lens A's confidence is definitional.
    pub slot_pairs_nondiscriminative: usize,
    pub nondiscriminative_pairs: Vec<SynapseCalyxBlindSpotPairDiagnostic>,
    /// Distinct `(lens_a_similarity, lens_b_neighbor_mean)` tuples across every
    /// emitted alert. An alert set with few distinct signatures is enumerating
    /// its encoding, not its corpus, and a caller should not have to derive that
    /// by hand (#1961 ask 2).
    pub alert_distinct_signatures: usize,
    /// Distinct delta values across every emitted alert.
    pub alert_distinct_deltas: usize,
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
    /// Physical math resource this pass is allowed to activate. Explicit MCP
    /// requests use the configured backend; unattended maintenance declares
    /// background CPU so it never creates a CUDA context behind a game.
    pub math_execution_class: SynapseCalyxMathExecutionClass,
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
            math_execution_class: SynapseCalyxMathExecutionClass::Configured,
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
    /// Provenance of the panel-selective membership walk this corpus was folded
    /// from at one pinned snapshot.
    pub walk: crate::SynapseCalyxPanelBaseWalk,
    pub recent_fraction: f32,
    pub permutations: usize,
    pub math_execution_class: String,
    pub math_backend_used: String,
    pub lenses_evaluated: usize,
    pub lenses_insufficient: usize,
    pub drifted_lenses: usize,
    pub lens_drift: Vec<SynapseCalyxLensDrift>,
    pub reactive_cf_rows_after: usize,
    pub drift_rows_persisted: usize,
    /// Exact typed rows independently read from the Reactive CF after commit.
    pub persisted_findings: Vec<SynapseCalyxPersistedDriftFinding>,
}

/// Persisted drift finding row shape written to the native `Reactive` CF.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxPersistedDriftFinding {
    pub panel_version: u32,
    pub slot: u16,
    pub dimension: usize,
    pub reference_n: usize,
    pub recent_n: usize,
    pub mmd2: f64,
    pub p_value: f64,
    pub bandwidth: f64,
    pub significant: bool,
    pub observed_seq: u64,
}

/// One record reduced to its dense slot vectors, in `Base` CF scan order.
struct DriftRecord {
    cx_id: CxId,
    slots: BTreeMap<SlotId, Vec<f32>>,
}

struct DriftCorpus {
    records: Vec<DriftRecord>,
    records_scanned: usize,
    /// Provenance of the panel-selective membership walk this corpus was folded
    /// from. Kept so both callers report the exact generation and snapshot.
    walk: crate::SynapseCalyxPanelBaseWalk,
}

/// Exact MMD inputs after the estimator's own 1,024-row-per-side contract has
/// been applied. These remain `f32` until their one lens is evaluated.
struct MmdSlotSamples {
    dimension: usize,
    reference_rows: usize,
    recent_rows: usize,
    /// One allocation holding every retained reference row. Keeping the samples
    /// flat prevents live row allocations from pinning pages filled with dead
    /// per-record hydration transients.
    reference: Vec<f32>,
    /// One allocation holding every retained recent row; see `reference`.
    recent: Vec<f32>,
}

struct MmdCorpus {
    by_slot: BTreeMap<SlotId, MmdSlotSamples>,
    lenses_without_nonempty_shape: usize,
    records_scanned: usize,
    records_measured: usize,
    walk: crate::SynapseCalyxPanelBaseWalk,
}

#[derive(Clone, Copy)]
struct MmdSlotBounds {
    total: usize,
    reference_start: usize,
    reference_end: usize,
    recent_start: usize,
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
        let backend_lease = self.math_runtime.backend()?;
        let backend = &*backend_lease;

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
        let mut nondiscriminative_pairs: Vec<SynapseCalyxBlindSpotPairDiagnostic> = Vec::new();
        let mut slot_pairs_evaluated = 0usize;
        let mut slot_pairs_uncalibrated = 0usize;

        for (i, &slot_one) in lens_ids.iter().enumerate() {
            for &slot_two in lens_ids.iter().skip(i + 1) {
                // Records carrying both lenses, restricted to the modal (dim_a,
                // dim_b) shape so cosine kNN on A and cosine on B are defined.
                let members = paired_dense_members(&corpus.records, slot_one, slot_two);
                let Some(group) = modal_shape_group(&members) else {
                    continue;
                };
                if group.len() < min_samples {
                    // Both directions of this pair are unevaluable.
                    slot_pairs_uncalibrated += 2;
                    continue;
                }

                // "Lens A is confident where lens B disagrees" is directional:
                // it is a claim about A's neighborhood judged through B, and its
                // reverse is a different claim about a different neighborhood.
                // Evaluate both orderings.
                for forward in [true, false] {
                    let (slot_a, slot_b) = if forward {
                        (slot_one, slot_two)
                    } else {
                        (slot_two, slot_one)
                    };
                    match Self::evaluate_blind_spot_direction(
                        backend,
                        &group,
                        forward,
                        slot_a,
                        slot_b,
                        min_samples,
                        params.alpha,
                        &mut alerts,
                    )? {
                        DirectionOutcome::Evaluated => slot_pairs_evaluated += 1,
                        DirectionOutcome::Uncalibrated => slot_pairs_uncalibrated += 1,
                        DirectionOutcome::NonDiscriminative(diagnostic) => {
                            nondiscriminative_pairs.push(diagnostic);
                        }
                    }
                }
            }
        }

        let slot_pairs_nondiscriminative = nondiscriminative_pairs.len();

        // Deterministic, most-severe-first ordering, then cap.
        alerts.sort_by(|a, b| {
            b.delta
                .total_cmp(&a.delta)
                .then_with(|| a.cx_id.cmp(&b.cx_id))
                .then(a.slot_a.cmp(&b.slot_a))
                .then(a.slot_b.cmp(&b.slot_b))
        });
        let alerts_total = alerts.len();
        let alert_distinct_signatures = distinct_f32_tuples(
            alerts
                .iter()
                .map(|alert| (alert.lens_a_similarity, alert.lens_b_neighbor_mean)),
        );
        let alert_distinct_deltas = distinct_f32_values(alerts.iter().map(|alert| alert.delta));
        alerts.truncate(params.max_alerts.max(1));

        Ok(SynapseCalyxBlindSpotReport {
            panel_version: params.panel_version,
            records_scanned: corpus.records_scanned,
            records_measured: corpus.records.len(),
            walk: corpus.walk,
            n_lenses: lens_ids.len(),
            slot_pairs_evaluated,
            slot_pairs_uncalibrated,
            slot_pairs_nondiscriminative,
            nondiscriminative_pairs,
            alert_distinct_signatures,
            alert_distinct_deltas,
            alerts_total,
            alerts,
        })
    }

    /// Evaluates one ordered `(A, B)` direction of a lens pair, appending any
    /// alerts it certifies.
    ///
    /// The gate this applies before certifying anything is the whole point: the
    /// neighbor is chosen to *maximise* similarity under A, so "lens A is
    /// confident" is true by selection for every record whose A-similarity
    /// distribution is degenerate. A one-hot lens returns cosine exactly `1.0`
    /// for any two records sharing a category and can return nothing else, so
    /// on such a lens the rule reduces to "B's similarity is low" with a
    /// constant offset — a lens-B outlier detector wearing a cross-lens label.
    /// Measuring A's distribution first and refusing the direction when it
    /// cannot discriminate is what keeps an alert a finding (#1961).
    #[allow(clippy::too_many_arguments)]
    fn evaluate_blind_spot_direction(
        backend: &dyn Backend,
        group: &[PairedMember<'_>],
        forward: bool,
        slot_a: SlotId,
        slot_b: SlotId,
        min_samples: usize,
        alpha: f32,
        alerts: &mut Vec<SynapseCalyxBlindSpotAlert>,
    ) -> Result<DirectionOutcome, SynapseCalyxError> {
        let dim_a = role_vector(&group[0], forward).len();
        let nearest = nearest_neighbor_indices(backend, group, dim_a, forward)?;

        let mut deltas: Vec<f32> = Vec::with_capacity(group.len());
        let mut a_sims: Vec<f32> = Vec::with_capacity(group.len());
        let mut b_sims: Vec<f32> = Vec::with_capacity(group.len());
        let mut per_record: Vec<(usize, f32, f32)> = Vec::with_capacity(group.len());
        for (index, neighbor) in nearest.iter().enumerate() {
            let Some((neighbor_index, a_sim)) = *neighbor else {
                continue;
            };
            let b_sim = cosine_similarity(
                role_vector(&group[index], !forward),
                role_vector(&group[neighbor_index], !forward),
            );
            let delta = a_sim - b_sim;
            if !delta.is_finite() {
                continue;
            }
            deltas.push(delta);
            a_sims.push(a_sim);
            b_sims.push(b_sim);
            per_record.push((index, a_sim, b_sim));
        }
        if deltas.len() < min_samples {
            return Ok(DirectionOutcome::Uncalibrated);
        }

        let a_discrimination = SimilarityDiscrimination::measure(&a_sims).map_err(|error| {
            SynapseCalyxError::from_calyx("measure lens-A similarity discrimination", &error)
        })?;
        if a_discrimination.is_definitional() {
            return Ok(DirectionOutcome::NonDiscriminative(
                SynapseCalyxBlindSpotPairDiagnostic {
                    slot_a: slot_a.get(),
                    slot_b: slot_b.get(),
                    code: "SYNAPSE_BLIND_SPOT_LENS_A_DEFINITIONAL".to_owned(),
                    detail: format!(
                        "lens {} produces {} distinct nearest-neighbour similarities over {} records \
                         with {:.4} of them at {:.6}; its confidence is a property of the encoding, \
                         so \"lens {} is confident while lens {} disagrees\" is true by construction \
                         and cannot be evidence about any record",
                        slot_a.get(),
                        a_discrimination.distinct_values,
                        a_discrimination.n,
                        a_discrimination.modal_share,
                        a_discrimination.modal_value,
                        slot_a.get(),
                        slot_b.get(),
                    ),
                    records: a_discrimination.n,
                    lens_a_distinct_values: a_discrimination.distinct_values,
                    lens_a_modal_value: a_discrimination.modal_value,
                    lens_a_modal_share: a_discrimination.modal_share,
                    lens_a_observed_min: a_discrimination.min,
                    lens_a_observed_max: a_discrimination.max,
                },
            ));
        }

        let b_discrimination = SimilarityDiscrimination::measure(&b_sims).map_err(|error| {
            SynapseCalyxError::from_calyx("measure lens-B similarity discrimination", &error)
        })?;

        let calibration = match BlindSpotCalibration::from_deltas(
            deltas.iter().copied(),
            BlindSpotCalibrationParams { min_samples, alpha },
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
                return Ok(DirectionOutcome::Uncalibrated);
            }
        };

        for (index, a_sim, b_sim) in per_record {
            let cx_id = group[index].0;
            let alert =
                detect_blind_spot_calibrated(cx_id, slot_a, slot_b, a_sim, b_sim, &calibration)
                    .map_err(|error| {
                        SynapseCalyxError::from_calyx("evaluate calibrated blind spot", &error)
                    })?;
            if let Some(alert) = alert {
                alerts.push(blind_spot_alert(&alert, a_sim, b_sim, b_discrimination));
            }
        }
        Ok(DirectionOutcome::Evaluated)
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
        if !(1..=SYNAPSE_INTELLIGENCE_MAX_RECORDS).contains(&params.max_records) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_DRIFT_MAX_RECORDS_INVALID",
                format!(
                    "MMD drift max_records={} is outside the supported range 1..={SYNAPSE_INTELLIGENCE_MAX_RECORDS}",
                    params.max_records
                ),
                "set max_records to an integer inside the named range and retry; the request is never clamped",
            ));
        }
        if !params.recent_fraction.is_finite() || !(0.05..=0.95).contains(&params.recent_fraction) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_DRIFT_RECENT_FRACTION_INVALID",
                format!(
                    "MMD drift recent_fraction={} must be finite and inside 0.05..=0.95",
                    params.recent_fraction
                ),
                "set recent_fraction to a finite value inside the named range and retry; the request is never clamped",
            ));
        }
        if !(1..=SYNAPSE_DRIFT_MAX_PERMUTATIONS).contains(&params.permutations) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_DRIFT_PERMUTATIONS_INVALID",
                format!(
                    "MMD drift permutations={} is outside the supported range 1..={SYNAPSE_DRIFT_MAX_PERMUTATIONS}",
                    params.permutations
                ),
                "set permutations to an integer inside the named range and retry; the request is never clamped",
            ));
        }
        if !params.alpha.is_finite() || !(0.0..1.0).contains(&params.alpha) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_DRIFT_ALPHA_INVALID",
                format!(
                    "MMD drift alpha={} must be finite and strictly inside 0..1",
                    params.alpha
                ),
                "set alpha to a finite probability strictly between zero and one and retry; the request never substitutes a default",
            ));
        }
        let max_records = params.max_records;
        let recent_fraction = params.recent_fraction;
        let permutations = params.permutations;
        let alpha = params.alpha;
        let corpus = self.load_mmd_corpus(params.panel_version, max_records, recent_fraction)?;
        let retained_sample_elements = corpus.by_slot.values().try_fold(0usize, |total, samples| {
            total
                .checked_add(samples.reference.len())
                .and_then(|value| value.checked_add(samples.recent.len()))
                .ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_DRIFT_SAMPLE_ELEMENT_COUNT_OVERFLOW",
                        format!(
                            "panel {} retained MMD sample element count overflowed usize",
                            params.panel_version
                        ),
                        "preserve the vault and inspect the bounded MMD sample arenas before retrying",
                    )
                })
        })?;
        let retained_sample_bytes = retained_sample_elements
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_DRIFT_SAMPLE_BYTE_COUNT_OVERFLOW",
                    format!(
                        "panel {} retained MMD sample byte count overflowed usize",
                        params.panel_version
                    ),
                    "preserve the vault and inspect the bounded MMD sample arenas before retrying",
                )
            })?;
        let retained_sample_capacity_elements = corpus.by_slot.values().try_fold(
            0usize,
            |total, samples| {
                total
                    .checked_add(samples.reference.capacity())
                    .and_then(|value| value.checked_add(samples.recent.capacity()))
                    .ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_DRIFT_SAMPLE_CAPACITY_COUNT_OVERFLOW",
                            format!(
                                "panel {} retained MMD sample capacity overflowed usize",
                                params.panel_version
                            ),
                            "preserve the vault and inspect the bounded MMD sample arenas before retrying",
                        )
                    })
            },
        )?;
        let retained_sample_capacity_bytes = retained_sample_capacity_elements
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_DRIFT_SAMPLE_CAPACITY_BYTE_COUNT_OVERFLOW",
                    format!(
                        "panel {} retained MMD sample capacity byte count overflowed usize",
                        params.panel_version
                    ),
                    "preserve the vault and inspect the bounded MMD sample arenas before retrying",
                )
            })?;
        let retained_allocations = corpus
            .by_slot
            .values()
            .map(|samples| {
                usize::from(samples.reference.capacity() > 0)
                    + usize::from(samples.recent.capacity() > 0)
            })
            .sum::<usize>();
        let corpus_release = crate::release_process_memory("MMD bounded corpus load")?;
        tracing::info!(
            code = "SYNAPSE_CALYX_DRIFT_BOUNDED_CORPUS_MEMORY_RELEASED",
            panel_version = params.panel_version,
            retained_slots = corpus.by_slot.len(),
            retained_sample_elements,
            retained_sample_bytes,
            retained_sample_capacity_elements,
            retained_sample_capacity_bytes,
            retained_allocations,
            private_bytes_before = corpus_release.private_bytes_before,
            private_bytes_after = corpus_release.private_bytes_after,
            private_bytes_reclaimed = corpus_release.private_bytes_reclaimed,
            release_elapsed_us = corpus_release.elapsed_us,
            "released second-pass hydration transients while preserving only contiguous bounded MMD sample arenas"
        );
        let MmdCorpus {
            by_slot,
            lenses_without_nonempty_shape,
            records_scanned,
            records_measured,
            walk,
        } = corpus;

        let config = MmdConfig {
            bandwidth: None,
            permutations,
            seed: DEFAULT_MMD_SEED,
            alpha,
        };
        let observed_seq = self.latest_seq();

        let mut lens_drift: Vec<SynapseCalyxLensDrift> = Vec::new();
        let mut writes: Vec<SynapseCalyxCfWrite> = Vec::new();
        let mut lenses_insufficient = lenses_without_nonempty_shape;
        let mut drifted_lenses = 0usize;

        let has_evaluable_lens = by_slot.values().any(|samples| {
            samples.reference_rows >= SYNAPSE_DRIFT_MIN_WINDOW
                && samples.recent_rows >= SYNAPSE_DRIFT_MIN_WINDOW
        });
        let configured_math = if has_evaluable_lens
            && params.math_execution_class == SynapseCalyxMathExecutionClass::Configured
        {
            Some(self.math_runtime.backend()?)
        } else {
            None
        };
        let background_cpu = if has_evaluable_lens
            && params.math_execution_class == SynapseCalyxMathExecutionClass::BackgroundCpu
        {
            Some(crate::math::verified_background_cpu_backend()?)
        } else {
            None
        };
        let math_backend_used = if has_evaluable_lens {
            match params.math_execution_class {
                SynapseCalyxMathExecutionClass::BackgroundCpu => "cpu_background",
                SynapseCalyxMathExecutionClass::Configured => configured_math
                    .as_ref()
                    .ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_DRIFT_MATH_BACKEND_MISSING",
                            format!(
                                "panel {} has an evaluable MMD lens but the configured execution class produced no math lease",
                                params.panel_version
                            ),
                            "repair configured math runtime initialization; drift never substitutes another backend",
                        )
                    })?
                    .assay_backend()
                    .as_str(),
            }
        } else {
            "none_no_evaluable_lens"
        };
        tracing::info!(
            code = "SYNAPSE_CALYX_DRIFT_MATH_EXECUTION_CLASS",
            panel_version = params.panel_version,
            requested_execution_class = params.math_execution_class.as_str(),
            backend_used = math_backend_used,
            evaluable_lens = has_evaluable_lens,
            configured_runtime_activated = configured_math.is_some(),
            background_cpu_proved = background_cpu.is_some(),
            "selected the declared MMD math execution class without runtime fallback"
        );

        for (slot, samples) in by_slot {
            let MmdSlotSamples {
                dimension,
                reference_rows,
                recent_rows,
                reference,
                recent,
            } = samples;
            if reference_rows < SYNAPSE_DRIFT_MIN_WINDOW || recent_rows < SYNAPSE_DRIFT_MIN_WINDOW {
                lenses_insufficient += 1;
                continue;
            }
            let pooled_f64 = widen_mmd_sample_arena(
                params.panel_version,
                slot,
                dimension,
                reference_rows,
                recent_rows,
                &reference,
                &recent,
            )?;

            let estimate = match params.math_execution_class {
                SynapseCalyxMathExecutionClass::BackgroundCpu => {
                    gaussian_mmd_flat_with_config_cpu_strict(
                        &pooled_f64,
                        reference_rows,
                        recent_rows,
                        dimension,
                        &config,
                    )
                }
                SynapseCalyxMathExecutionClass::Configured => {
                    let lease = configured_math.as_ref().ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_DRIFT_MATH_BACKEND_MISSING",
                            format!(
                                "panel {} slot {} reached MMD estimation without its configured math lease",
                                params.panel_version,
                                slot.get()
                            ),
                            "repair the configured math-runtime lease invariant; drift never substitutes another backend",
                        )
                    })?;
                    match lease.assay_backend() {
                        crate::math::SynapseCalyxAssayBackend::Cpu(_) => {
                            gaussian_mmd_flat_with_config_cpu_strict(
                                &pooled_f64,
                                reference_rows,
                                recent_rows,
                                dimension,
                                &config,
                            )
                        }
                        #[cfg(feature = "calyx-cuda")]
                        crate::math::SynapseCalyxAssayBackend::Cuda(backend) => {
                            gaussian_mmd_flat_with_config_cuda_budgeted(
                                backend,
                                &pooled_f64,
                                reference_rows,
                                recent_rows,
                                dimension,
                                &config,
                            )
                        }
                    }
                }
            };
            drop(pooled_f64);
            drop(reference);
            drop(recent);
            let release = crate::release_process_memory("MMD lens estimator");
            let report = match (estimate, release) {
                (Err(estimate_error), Err(release_error)) => {
                    return Err(SynapseCalyxError::new(
                        "SYNAPSE_CALYX_DRIFT_ESTIMATE_AND_MEMORY_RELEASE_FAILED",
                        format!(
                            "panel {} slot {} MMD estimation failed with {}: {}; after every estimator-owned matrix and widened sample was dropped, allocator release also failed with {release_error}",
                            params.panel_version,
                            slot.get(),
                            estimate_error.code,
                            estimate_error.message
                        ),
                        "repair both the named MMD input/estimator failure and the process memory reclaimer before retrying",
                    ));
                }
                (Ok(_), Err(release_error)) => return Err(release_error),
                (estimate, Ok(release)) => {
                    tracing::info!(
                        code = "SYNAPSE_CALYX_DRIFT_LENS_MEMORY_RELEASED",
                        panel_version = params.panel_version,
                        slot = slot.get(),
                        private_bytes_before = release.private_bytes_before,
                        private_bytes_after = release.private_bytes_after,
                        private_bytes_reclaimed = release.private_bytes_reclaimed,
                        release_elapsed_us = release.elapsed_us,
                        "released one MMD lens's exact pair-distance/kernel workspace at its ownership boundary"
                    );
                    match estimate {
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
                    }
                }
            };

            if report.significant {
                drifted_lenses += 1;
            }
            let finding = SynapseCalyxPersistedDriftFinding {
                panel_version: params.panel_version,
                slot: slot.get(),
                dimension,
                reference_n: reference_rows,
                recent_n: recent_rows,
                mmd2: report.mmd2,
                p_value: report.p_value,
                bandwidth: report.bandwidth,
                significant: report.significant,
                observed_seq,
            };
            if report.significant {
                writes.push(SynapseCalyxCfWrite {
                    cf: ColumnFamily::Reactive,
                    key: reactive_drift_key(observed_seq, params.panel_version, slot.get()),
                    value: encode_json(&finding)?,
                });
            }
            lens_drift.push(SynapseCalyxLensDrift {
                slot: slot.get(),
                dimension,
                reference_n: reference_rows,
                recent_n: recent_rows,
                mmd2: report.mmd2,
                p_value: report.p_value,
                bandwidth: report.bandwidth,
                significant: report.significant,
                persisted: report.significant,
            });
        }

        drop(background_cpu);
        drop(configured_math);
        let runtime_status = self.math_runtime.status_snapshot();
        if let Some(code) = runtime_status.runtime_readback_code.as_deref() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_DRIFT_MATH_RELEASE_FAILED",
                format!(
                    "panel {} completed estimation but math runtime readback failed with {code}: {}",
                    params.panel_version,
                    runtime_status
                        .runtime_readback_error
                        .as_deref()
                        .unwrap_or("missing structured runtime error detail")
                ),
                "preserve the host GPU reservation ledger and inspect the named math release failure before retrying",
            ));
        }
        if math_backend_used == "cuda_budgeted"
            && (runtime_status.probe.status != "dormant_verified"
                || runtime_status.host_reservation_id.is_some()
                || runtime_status.host_reservation.is_some())
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_DRIFT_CUDA_RELEASE_UNPROVEN",
                format!(
                    "panel {} CUDA MMD ended with probe_status={} host_reservation_id={:?} host_reservation_live={}",
                    params.panel_version,
                    runtime_status.probe.status,
                    runtime_status.host_reservation_id,
                    runtime_status.host_reservation.is_some()
                ),
                "stop drift work and repair managed CUDA idle release; Synapse refuses to claim idle while its context or reservation is unproven",
            ));
        }
        let backend_release = crate::release_process_memory("MMD execution backend release")?;
        tracing::info!(
            code = "SYNAPSE_CALYX_DRIFT_MATH_RELEASE_PROVED",
            panel_version = params.panel_version,
            backend_used = math_backend_used,
            runtime_probe_status = runtime_status.probe.status,
            host_reservation_live = runtime_status.host_reservation.is_some(),
            private_bytes_before = backend_release.private_bytes_before,
            private_bytes_after = backend_release.private_bytes_after,
            private_bytes_reclaimed = backend_release.private_bytes_reclaimed,
            release_elapsed_us = backend_release.elapsed_us,
            "proved the selected MMD backend reached its declared post-pass ownership state"
        );

        let drift_rows_persisted = writes.len();
        if !writes.is_empty() {
            self.write_cf_batch(writes)?;
            self.flush()?;
        }
        let mut persisted_findings = Vec::with_capacity(drift_rows_persisted);
        for finding in lens_drift.iter().filter(|finding| finding.persisted) {
            let key = reactive_drift_key(observed_seq, params.panel_version, finding.slot);
            let bytes = self
                .read_cf_latest(ColumnFamily::Reactive, &key)?
                .ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_REACTIVE_READBACK_MISSING",
                        format!(
                            "Reactive CF drift row disappeared after commit: panel={} slot={}",
                            params.panel_version, finding.slot
                        ),
                        "stop writers, preserve the vault, and inspect the Reactive CF commit before retrying drift",
                    )
                })?;
            let decoded = serde_json::from_slice::<SynapseCalyxPersistedDriftFinding>(&bytes)
                .map_err(|error| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_REACTIVE_READBACK_CORRUPT",
                        format!(
                            "decode committed Reactive CF drift row for panel={} slot={}: {error}",
                            params.panel_version, finding.slot
                        ),
                        "stop writers, preserve the vault, and inspect the named Reactive CF row",
                    )
                })?;
            persisted_findings.push(decoded);
        }
        let reactive_cf_rows_after = self
            .count_cf_latest_bounded(ColumnFamily::Reactive)?
            .rows_visited;

        Ok(SynapseCalyxPanelDriftReport {
            panel_version: params.panel_version,
            records_scanned,
            records_measured,
            walk,
            recent_fraction,
            permutations,
            math_execution_class: params.math_execution_class.as_str().to_owned(),
            math_backend_used: math_backend_used.to_owned(),
            lenses_evaluated: lens_drift.len(),
            lenses_insufficient,
            drifted_lenses,
            lens_drift,
            reactive_cf_rows_after,
            drift_rows_persisted,
            persisted_findings,
        })
    }

    /// Selects the same newest `max_records` panel rows as the generic drift
    /// loader, but retains only the exact reference/recent tails consumed by
    /// MMD. A first hydration pass establishes each slot's modal dimension;
    /// the second pass can then select the final ordinals without ever owning
    /// the whole multi-lens vector corpus or cloning it into per-slot groups.
    #[allow(clippy::too_many_lines)]
    fn load_mmd_corpus(
        &self,
        panel_version: u32,
        max_records: usize,
        recent_fraction: f32,
    ) -> Result<MmdCorpus, SynapseCalyxError> {
        self.with_panel_read_snapshot(
            panel_version,
            crate::MMD_DRIFT_CORPUS_READER_LEASE_MS,
            |snapshot| {
            let mut selected = BTreeSet::new();
            let mut records_scanned = 0usize;
            let (walk, mut snapshot) = self.walk_panel_base_snapshot(
                snapshot,
                panel_version,
                |_snapshot, _key, value| {
                    let base = decode_constellation_base(value).map_err(|error| {
                        SynapseCalyxError::from_calyx("decode Base constellation", &error)
                    })?;
                    if base.panel_version != panel_version {
                        return Ok(crate::SynapseCalyxWalkStep::Continue);
                    }
                    records_scanned += 1;
                    selected.insert((base.created_at, base.cx_id));
                    if selected.len() > max_records {
                        let oldest = selected.first().copied().ok_or_else(|| {
                            SynapseCalyxError::new(
                                "SYNAPSE_CALYX_DRIFT_SELECTION_EMPTY",
                                "bounded chronological MMD selection lost its oldest candidate",
                                "preserve the vault and inspect the Base CF selection invariant",
                            )
                        })?;
                        selected.remove(&oldest);
                    }
                    Ok(crate::SynapseCalyxWalkStep::Continue)
                },
            )?;
            if records_scanned == 0 {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_DRIFT_CORPUS_EMPTY",
                    format!(
                        "MMD drift panel {panel_version} has no Base CF constellation at the pinned snapshot"
                    ),
                    "confirm the exact panel_version against panel lifecycle/coverage state, ingest at least one real constellation, then retry",
                ));
            }

            let mut dense_slots = BTreeSet::new();
            let mut dimensions: BTreeMap<SlotId, BTreeMap<usize, usize>> = BTreeMap::new();
            for (index, (_, cx_id)) in selected.iter().enumerate() {
                if index > 0
                    && index.is_multiple_of(crate::PANEL_BASE_SNAPSHOT_RENEW_ROWS)
                {
                    snapshot = self.renew_read_snapshot(snapshot)?;
                }
                let constellation = self.hydrated_constellation_at_snapshot(*cx_id, snapshot)?;
                for (slot, vector) in constellation.slots {
                    if let SlotVector::Dense { data, .. } = vector {
                        dense_slots.insert(slot);
                        if !data.is_empty() {
                            let count = dimensions.entry(slot).or_default().entry(data.len()).or_default();
                            *count = count.checked_add(1).ok_or_else(|| {
                                SynapseCalyxError::new(
                                    "SYNAPSE_CALYX_DRIFT_DIMENSION_COUNT_OVERFLOW",
                                    format!("panel {panel_version} slot {} modal-dimension count overflowed usize", slot.get()),
                                    "preserve the vault and inspect the named panel/slot cardinality",
                                )
                            })?;
                        }
                    }
                }
            }
            let census_release = crate::release_process_memory("MMD modal-dimension census")?;
            tracing::info!(
                code = "SYNAPSE_CALYX_DRIFT_CENSUS_MEMORY_RELEASED",
                panel_version,
                selected_records = selected.len(),
                dense_slots = dense_slots.len(),
                private_bytes_before = census_release.private_bytes_before,
                private_bytes_after = census_release.private_bytes_after,
                private_bytes_reclaimed = census_release.private_bytes_reclaimed,
                release_elapsed_us = census_release.elapsed_us,
                "released per-record hydration transients after the MMD modal-dimension census"
            );

            let mut bounds = BTreeMap::new();
            let mut by_slot = BTreeMap::new();
            for (slot, counts) in &dimensions {
                let (dimension, total) = counts
                    .iter()
                    .max_by(|left, right| left.1.cmp(right.1).then(left.0.cmp(right.0)))
                    .map(|(dimension, total)| (*dimension, *total))
                    .ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_DRIFT_MODAL_DIMENSION_EMPTY",
                            format!("panel {panel_version} slot {} has a dimension map but no dimension entry", slot.get()),
                            "preserve the vault and inspect the named dense slot's stored vectors",
                        )
                    })?;
                let reference_end = reference_split(total, recent_fraction);
                let reference_start = reference_end.saturating_sub(SYNAPSE_DRIFT_MAX_WINDOW);
                let recent_start = reference_end.max(total.saturating_sub(SYNAPSE_DRIFT_MAX_WINDOW));
                let reference_rows = reference_end - reference_start;
                let recent_rows = total - recent_start;
                let reference_elements = drift_sample_element_count(
                    panel_version,
                    *slot,
                    "reference rows",
                    reference_rows,
                    dimension,
                )?;
                let recent_elements = drift_sample_element_count(
                    panel_version,
                    *slot,
                    "recent rows",
                    recent_rows,
                    dimension,
                )?;
                let mut reference = Vec::new();
                reference
                    .try_reserve_exact(reference_elements)
                    .map_err(|error| {
                        drift_sample_reserve_error(
                            panel_version,
                            *slot,
                            "reference sample elements",
                            &error,
                        )
                    })?;
                let mut recent = Vec::new();
                recent
                    .try_reserve_exact(recent_elements)
                    .map_err(|error| {
                        drift_sample_reserve_error(
                            panel_version,
                            *slot,
                            "recent sample elements",
                            &error,
                        )
                    })?;
                bounds.insert(
                    *slot,
                    MmdSlotBounds {
                        total,
                        reference_start,
                        reference_end,
                        recent_start,
                    },
                );
                by_slot.insert(
                    *slot,
                    MmdSlotSamples {
                        dimension,
                        reference_rows,
                        recent_rows,
                        reference,
                        recent,
                    },
                );
            }

            let mut ordinals = bounds.keys().map(|slot| (*slot, 0usize)).collect::<BTreeMap<_, _>>();
            for (_, cx_id) in &selected {
                let constellation = self.hydrated_constellation_at_snapshot(*cx_id, snapshot)?;
                for (slot, vector) in constellation.slots {
                    let SlotVector::Dense { data, .. } = vector else {
                        continue;
                    };
                    let Some(samples) = by_slot.get_mut(&slot) else {
                        continue;
                    };
                    if data.len() != samples.dimension {
                        continue;
                    }
                    let ordinal = ordinals.get_mut(&slot).ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_DRIFT_SAMPLE_ORDINAL_MISSING",
                            format!("panel {panel_version} slot {} has samples but no ordinal counter", slot.get()),
                            "preserve the vault and inspect the MMD sample-selection invariant",
                        )
                    })?;
                    let bounds = bounds.get(&slot).ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_DRIFT_SAMPLE_BOUNDS_MISSING",
                            format!("panel {panel_version} slot {} has samples but no selection bounds", slot.get()),
                            "preserve the vault and inspect the MMD sample-selection invariant",
                        )
                    })?;
                    if (bounds.reference_start..bounds.reference_end).contains(ordinal) {
                        append_drift_sample(
                            panel_version,
                            slot,
                            "reference",
                            &data,
                            samples.reference_rows,
                            samples.dimension,
                            &mut samples.reference,
                        )?;
                    } else if *ordinal >= bounds.recent_start {
                        append_drift_sample(
                            panel_version,
                            slot,
                            "recent",
                            &data,
                            samples.recent_rows,
                            samples.dimension,
                            &mut samples.recent,
                        )?;
                    }
                    *ordinal = ordinal.checked_add(1).ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_DRIFT_SAMPLE_ORDINAL_OVERFLOW",
                            format!("panel {panel_version} slot {} sample ordinal overflowed usize", slot.get()),
                            "preserve the vault and inspect the named panel/slot cardinality",
                        )
                    })?;
                }
            }
            for (slot, samples) in &by_slot {
                let bounds = bounds.get(slot).ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_DRIFT_SAMPLE_BOUNDS_MISSING",
                        format!(
                            "panel {panel_version} slot {} reached readback without selection bounds",
                            slot.get()
                        ),
                        "preserve the vault and inspect the MMD sample-selection invariant",
                    )
                })?;
                let observed = ordinals.get(slot).copied().unwrap_or_default();
                let expected_reference = bounds.reference_end - bounds.reference_start;
                let expected_recent = bounds.total - bounds.recent_start;
                let expected_reference_elements = drift_sample_element_count(
                    panel_version,
                    *slot,
                    "reference readback",
                    expected_reference,
                    samples.dimension,
                )?;
                let expected_recent_elements = drift_sample_element_count(
                    panel_version,
                    *slot,
                    "recent readback",
                    expected_recent,
                    samples.dimension,
                )?;
                if observed != bounds.total
                    || samples.reference_rows != expected_reference
                    || samples.recent_rows != expected_recent
                    || samples.reference.len() != expected_reference_elements
                    || samples.recent.len() != expected_recent_elements
                {
                    return Err(SynapseCalyxError::new(
                        "SYNAPSE_CALYX_DRIFT_SAMPLE_READBACK_MISMATCH",
                        format!(
                            "panel {panel_version} slot {} bounded MMD readback mismatch: modal_total={} observed={} reference_rows={}/{} reference_elements={}/{} recent_rows={}/{} recent_elements={}/{}",
                            slot.get(), bounds.total, observed,
                            samples.reference_rows, expected_reference,
                            samples.reference.len(), expected_reference_elements,
                            samples.recent_rows, expected_recent,
                            samples.recent.len(), expected_recent_elements,
                        ),
                        "preserve the pinned vault and inspect the two-pass hydration/readback invariant",
                    ));
                }
            }

            let lenses_without_nonempty_shape = dense_slots
                .len()
                .checked_sub(dimensions.len())
                .ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_DRIFT_DENSE_SLOT_COUNT_INVALID",
                        format!(
                            "panel {panel_version} has {} modal-dimension slots but only {} observed dense slots",
                            dimensions.len(),
                            dense_slots.len()
                        ),
                        "preserve the vault and inspect the dense-slot census invariant",
                    )
                })?;
            Ok(MmdCorpus {
                by_slot,
                lenses_without_nonempty_shape,
                records_scanned,
                records_measured: selected.len(),
                walk,
            })
        },
        )
    }

    /// Scans the `Base` CF once and returns the newest bounded dense-slot corpus
    /// in persisted creation order, for the drift/blind-spot analyses.
    fn load_drift_corpus(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> Result<DriftCorpus, SynapseCalyxError> {
        self.with_panel_read_snapshot(
            panel_version,
            crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |snapshot| {
            let mut selected = BTreeSet::new();
            let mut records_scanned = 0usize;
            // Base keys are content hashes, not chronology (#2003). Retain the
            // newest bounded set by the server-stamped creation time and use cx_id
            // as the deterministic tie-breaker. Slot hydration happens only after
            // selection, so a large panel costs O(max_records) memory and reads.
            // Membership point reads and every hydration share this scope's one
            // registered snapshot lease: no latest read can introduce a post-pin id.
            let (walk, mut snapshot) = self.walk_panel_base_snapshot(
                snapshot,
                panel_version,
                |_snapshot, _key, value| {
                    let base = decode_constellation_base(value).map_err(|error| {
                        SynapseCalyxError::from_calyx("decode Base constellation", &error)
                    })?;
                    if base.panel_version != panel_version {
                        return Ok(crate::SynapseCalyxWalkStep::Continue);
                    }
                    records_scanned += 1;
                    selected.insert((base.created_at, base.cx_id));
                    if selected.len() > max_records {
                        let oldest = selected.first().copied().ok_or_else(|| {
                            SynapseCalyxError::new(
                                "SYNAPSE_CALYX_DRIFT_SELECTION_EMPTY",
                                "bounded chronological drift selection lost its oldest candidate",
                                "preserve the vault and inspect the Base CF selection invariant",
                            )
                        })?;
                        selected.remove(&oldest);
                    }
                    Ok(crate::SynapseCalyxWalkStep::Continue)
                },
            )?;
            if records_scanned == 0 {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_DRIFT_CORPUS_EMPTY",
                    format!(
                        "MMD drift panel {panel_version} has no Base CF constellation at the pinned snapshot"
                    ),
                    "confirm the exact panel_version against panel lifecycle/coverage state, ingest at least one real constellation, then retry",
                ));
            }
            let mut records = Vec::with_capacity(selected.len());
            for (index, (_, cx_id)) in selected.into_iter().enumerate() {
                if index > 0
                    && index.is_multiple_of(crate::PANEL_BASE_SNAPSHOT_RENEW_ROWS)
                {
                    snapshot = self.renew_read_snapshot(snapshot)?;
                }
                // Slot vectors live in the per-slot CFs; Base carries their typed
                // absence only (#1894), so hydrate exactly the selected records.
                let constellation = self.hydrated_constellation_at_snapshot(cx_id, snapshot)?;
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
                walk,
            })
        },
        )
    }
}

/// One record's shared presence on a lens pair: `(cx_id, vector_a, vector_b)`.
type PairedMember<'a> = (CxId, &'a Vec<f32>, &'a Vec<f32>);

/// Selects a paired member's first or second lens vector, so one code path can
/// evaluate both `(A, B)` orderings of a pair without cloning the group.
const fn role_vector<'a>(member: &PairedMember<'a>, first: bool) -> &'a Vec<f32> {
    if first { member.1 } else { member.2 }
}

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
    forward: bool,
) -> Result<Vec<Option<(usize, f32)>>, SynapseCalyxError> {
    let count = group.len();
    let mut flat = Vec::with_capacity(count * dim_a);
    for member in group {
        flat.extend_from_slice(if forward { member.1 } else { member.2 });
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
    lens_b: SimilarityDiscrimination,
) -> SynapseCalyxBlindSpotAlert {
    let evidence = alert.calibration.as_ref();
    let range = lens_b.observed_range();
    // Zero range means lens B is constant over this direction's records; the
    // disagreement then carries no information about position, so report 0.0
    // rather than dividing by zero and emitting a NaN that reads as a value.
    let dissent_fraction = if range > 0.0 {
        ((lens_b.max - lens_b_neighbor_mean) / range).clamp(0.0, 1.0)
    } else {
        0.0
    };
    SynapseCalyxBlindSpotAlert {
        cx_id: alert.cx_id.to_string(),
        slot_a: alert.a.get(),
        slot_b: alert.b.get(),
        lens_a_similarity,
        lens_b_neighbor_mean,
        delta: alert.delta,
        severity: severity_label(alert.severity).to_owned(),
        calibration_sample_count: evidence.map_or(0, |e| e.sample_count),
        calibration_alpha: evidence.map_or(0.0, |e| e.alpha),
        calibration_p_value: evidence.map_or(0.0, |e| e.p_value),
        calibration_percentile: evidence.map_or(0.0, |e| e.percentile),
        threshold_delta: evidence.map_or(0.0, |e| e.threshold_delta),
        calibration_distinct_deltas: evidence.map_or(0, |e| e.distinct_deltas),
        calibration_resolves_alpha: evidence.is_some_and(|e| e.resolves_alpha),
        lens_b_observed_min: lens_b.min,
        lens_b_observed_max: lens_b.max,
        lens_b_dissent_fraction: dissent_fraction,
    }
}

/// Outcome of evaluating one ordered `(A, B)` direction of a lens pair.
enum DirectionOutcome {
    Evaluated,
    Uncalibrated,
    NonDiscriminative(SynapseCalyxBlindSpotPairDiagnostic),
}

/// Counts distinct `f32` values at [`SIMILARITY_DISTINCT_TOLERANCE`], using the
/// same run-based rule the discrimination measurement uses so the two numbers
/// in one report are always computed the same way.
fn distinct_f32_values(values: impl IntoIterator<Item = f32>) -> usize {
    let mut sorted: Vec<f32> = values
        .into_iter()
        .filter(|value| value.is_finite())
        .collect();
    sorted.sort_by(f32::total_cmp);
    let mut distinct = 0usize;
    let mut run_start = f32::NAN;
    for value in sorted {
        if distinct > 0 && (value - run_start).abs() <= SIMILARITY_DISTINCT_TOLERANCE {
            continue;
        }
        distinct += 1;
        run_start = value;
    }
    distinct
}

/// Counts distinct `(a, b)` similarity signatures at the same tolerance.
fn distinct_f32_tuples(values: impl IntoIterator<Item = (f32, f32)>) -> usize {
    let mut sorted: Vec<(f32, f32)> = values
        .into_iter()
        .filter(|(a, b)| a.is_finite() && b.is_finite())
        .collect();
    sorted.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.total_cmp(&right.1)));
    let mut distinct = 0usize;
    let mut run_start = (f32::NAN, f32::NAN);
    for value in sorted {
        if distinct > 0
            && (value.0 - run_start.0).abs() <= SIMILARITY_DISTINCT_TOLERANCE
            && (value.1 - run_start.1).abs() <= SIMILARITY_DISTINCT_TOLERANCE
        {
            continue;
        }
        distinct += 1;
        run_start = value;
    }
    distinct
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

fn drift_sample_reserve_error(
    panel_version: u32,
    slot: SlotId,
    role: &str,
    error: &std::collections::TryReserveError,
) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_DRIFT_SAMPLE_RESERVE_FAILED",
        format!(
            "panel {panel_version} slot {} could not reserve its exact bounded {role}: {error}",
            slot.get()
        ),
        "preserve the vault, inspect the named slot's modal dimension/sample cardinality, and restore sufficient host memory before retrying",
    )
}

fn drift_sample_element_count(
    panel_version: u32,
    slot: SlotId,
    role: &str,
    rows: usize,
    dimension: usize,
) -> Result<usize, SynapseCalyxError> {
    rows.checked_mul(dimension).ok_or_else(|| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_DRIFT_SAMPLE_ELEMENT_COUNT_OVERFLOW",
            format!(
                "panel {panel_version} slot {} {role} element count overflowed usize: rows={rows} dimension={dimension}",
                slot.get()
            ),
            "preserve the vault and inspect the named slot's modal dimension/sample cardinality",
        )
    })
}

fn append_drift_sample(
    panel_version: u32,
    slot: SlotId,
    role: &str,
    data: &[f32],
    row_capacity: usize,
    dimension: usize,
    arena: &mut Vec<f32>,
) -> Result<(), SynapseCalyxError> {
    let element_capacity =
        drift_sample_element_count(panel_version, slot, role, row_capacity, dimension)?;
    let next_len = arena.len().checked_add(data.len()).ok_or_else(|| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_DRIFT_SAMPLE_ELEMENT_COUNT_OVERFLOW",
            format!(
                "panel {panel_version} slot {} {role} append overflowed usize at {} + {} elements",
                slot.get(),
                arena.len(),
                data.len()
            ),
            "preserve the vault and inspect the bounded MMD sample-selection invariant",
        )
    })?;
    if data.len() != dimension || next_len > element_capacity {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_DRIFT_SAMPLE_ARENA_BOUNDS_EXCEEDED",
            format!(
                "panel {panel_version} slot {} {role} append would exceed its exact contiguous arena: data_dimension={} modal_dimension={dimension} next_elements={next_len} capacity_elements={element_capacity}",
                slot.get(),
                data.len()
            ),
            "preserve the vault and inspect the named slot's modal-dimension and ordinal invariants",
        ));
    }
    arena.extend_from_slice(data);
    Ok(())
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

fn widen_mmd_sample_arena(
    panel_version: u32,
    slot: SlotId,
    dimension: usize,
    reference_rows: usize,
    recent_rows: usize,
    reference: &[f32],
    recent: &[f32],
) -> Result<Vec<f64>, SynapseCalyxError> {
    if reference_rows > SYNAPSE_DRIFT_MAX_WINDOW || recent_rows > SYNAPSE_DRIFT_MAX_WINDOW {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_DRIFT_SAMPLE_WINDOW_EXCEEDED",
            format!(
                "panel {panel_version} slot {} reached widening with reference_rows={reference_rows} recent_rows={recent_rows}, exceeding the per-side maximum {SYNAPSE_DRIFT_MAX_WINDOW}",
                slot.get()
            ),
            "preserve the vault and inspect the two-pass MMD sample bounds",
        ));
    }
    let expected_reference = drift_sample_element_count(
        panel_version,
        slot,
        "reference widening",
        reference_rows,
        dimension,
    )?;
    let expected_recent = drift_sample_element_count(
        panel_version,
        slot,
        "recent widening",
        recent_rows,
        dimension,
    )?;
    if reference.len() != expected_reference || recent.len() != expected_recent {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_DRIFT_SAMPLE_READBACK_MISMATCH",
            format!(
                "panel {panel_version} slot {} contiguous MMD widening mismatch: reference_elements={}/{} recent_elements={}/{}",
                slot.get(),
                reference.len(),
                expected_reference,
                recent.len(),
                expected_recent
            ),
            "preserve the vault and inspect the bounded MMD sample arenas before estimation",
        ));
    }
    let total_elements = expected_reference
        .checked_add(expected_recent)
        .ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_DRIFT_SAMPLE_ELEMENT_COUNT_OVERFLOW",
                format!(
                    "panel {panel_version} slot {} widened pooled element count overflowed usize",
                    slot.get()
                ),
                "preserve the vault and inspect the bounded MMD sample arenas before estimation",
            )
        })?;
    let mut pooled = Vec::new();
    pooled.try_reserve_exact(total_elements).map_err(|error| {
        drift_sample_reserve_error(
            panel_version,
            slot,
            "widened pooled sample elements",
            &error,
        )
    })?;
    pooled.extend(
        reference
            .iter()
            .chain(recent)
            .map(|value| f64::from(*value)),
    );
    Ok(pooled)
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

fn reactive_drift_key(observed_seq: u64, panel_version: u32, slot: u16) -> Vec<u8> {
    let mut key = Vec::with_capacity(REACTIVE_DRIFT_PREFIX.len() + 14);
    key.extend_from_slice(REACTIVE_DRIFT_PREFIX);
    key.extend_from_slice(&observed_seq.to_be_bytes());
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
