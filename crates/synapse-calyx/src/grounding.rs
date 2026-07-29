//! Phase-3 Calyx grounding-gap reporting (#1670).
//!
//! This module is the Synapse-side facade for the grounding-gap report: it reads
//! the physical `Base` CF constellations for one panel (domain), counts which
//! records and lenses carry a *grounded* anchor (an anchor with positive
//! confidence) versus which are ungrounded, and reports the coverage per anchor
//! kind and per lens plus the largest ungrounded regions.
//!
//! It also computes the domain's **provisional verdict**: the honesty gate that
//! the epic's control doctrine turns on. A domain whose grounded coverage falls
//! below [`SYNAPSE_GROUNDING_COVERAGE_FLOOR`] is *provisional* — any assay or
//! oracle result derived from it must be tagged provisional and may only advise,
//! never control. This module never invents a fallback and fails closed with
//! structured `SynapseCalyxError` values. It is read-only: every returned number
//! is proven against the physical `Base` CF bytes at call time.

use std::collections::{BTreeMap, BTreeSet};

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{Anchor, AnchorKind, SlotVector};
use serde::{Deserialize, Serialize};

use crate::{SYNAPSE_INTELLIGENCE_MAX_RECORDS, SynapseCalyxError, SynapseCalyxVault};

/// Coverage floor governing the grounded-vs-provisional control-doctrine boundary.
///
/// A domain (panel) at or above this fraction of grounded records is
/// grounded+calibrated and may control; below it the domain is provisional and
/// may only advise. Operator-tunable default, revisitable (handbook §3 grounding
/// gate; the epic's control doctrine).
pub const SYNAPSE_GROUNDING_COVERAGE_FLOOR: f32 = 0.5;

/// Cap on the number of largest-ungrounded lenses surfaced in one report.
pub const SYNAPSE_GROUNDING_MAX_UNGROUNDED_SLOTS: usize = 32;

/// Per-anchor-kind grounded coverage over a panel: how many records carry at
/// least one grounded anchor of this kind, as a fraction of measured records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxAnchorKindCoverage {
    pub anchor_kind: String,
    pub grounded_records: usize,
    pub coverage_fraction: f32,
}

/// Per-lens grounded coverage over a panel: of the records where this lens is
/// present (non-absent), how many carry a grounded anchor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxSlotGroundingCoverage {
    pub slot: u16,
    pub records_present: usize,
    pub grounded_records: usize,
    pub ungrounded_records: usize,
    pub coverage_fraction: f32,
    /// True when this lens's grounded coverage is below the domain floor: any
    /// result read from this lens must carry the provisional marker.
    pub provisional: bool,
}

/// Result of one grounding-gap pass over a panel (domain), proven against the
/// physical `Base` CF at call time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxGroundingGapReport {
    pub panel_version: u32,
    /// Total `Base` constellations for this panel seen at the vault.
    pub records_scanned: usize,
    /// Records actually loaded into this pass (clamped by `max_records`).
    pub records_measured: usize,
    /// Records carrying at least one grounded anchor (confidence > 0).
    pub grounded_records: usize,
    pub ungrounded_records: usize,
    pub grounded_fraction: f32,
    pub coverage_floor: f32,
    /// The load-bearing control-doctrine marker: true when the domain's grounded
    /// coverage is below the floor, so every assay/oracle result derived from
    /// this domain must be tagged provisional.
    pub provisional: bool,
    /// Human-readable reason the domain is provisional, when it is.
    pub provisional_reason: Option<String>,
    pub distinct_anchor_kinds: usize,
    pub anchor_kind_coverage: Vec<SynapseCalyxAnchorKindCoverage>,
    pub slot_coverage: Vec<SynapseCalyxSlotGroundingCoverage>,
    /// The largest ungrounded regions: lenses ranked by ungrounded record count.
    pub largest_ungrounded_slots: Vec<SynapseCalyxSlotGroundingCoverage>,
    /// Physical `Base` CF row count read back at call time.
    pub base_cf_rows: usize,
}

/// Compact provisional verdict for one domain, the reusable mechanism read paths
/// consult to decide whether their results may control or must only advise.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxDomainGroundingVerdict {
    pub panel_version: u32,
    pub grounded_fraction: f32,
    pub coverage_floor: f32,
    pub provisional: bool,
}

/// Mutable accumulator for one lens's grounded/present counts.
#[derive(Default)]
struct SlotAccumulator {
    records_present: usize,
    grounded_records: usize,
}

impl SynapseCalyxVault {
    /// Reports the grounding gaps for one panel (domain): per-anchor-kind and
    /// per-lens grounded coverage, the largest ungrounded regions, and the
    /// domain's provisional verdict. Read-only: every count is recomputed from
    /// the physical `Base` CF at call time so a before/after routine confirmation
    /// shows the exact coverage delta of the anchors written.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the `Base` CF cannot be
    /// scanned or a constellation row fails to decode.
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    pub fn grounding_gap_report(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> Result<SynapseCalyxGroundingGapReport, SynapseCalyxError> {
        // Hot-path boundary (#1686): grounding-gap reporting is an off-runtime
        // maintenance read and must never be driven from a tagged tick.
        crate::lowering::hot_context::assert_cold_calyx("grounding_gap_report");
        let max_records = max_records.clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);

        let rows = self.scan_cf_latest(ColumnFamily::Base)?;
        let base_cf_rows = rows.len();

        let mut records_scanned = 0usize;
        let mut records_measured = 0usize;
        let mut grounded_records = 0usize;
        // record-count per anchor kind (a record counts once per kind).
        let mut kind_records: BTreeMap<String, usize> = BTreeMap::new();
        let mut slots: BTreeMap<u16, SlotAccumulator> = BTreeMap::new();

        for (_, value) in rows {
            let constellation = decode_constellation_base(&value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode Base constellation", &error)
            })?;
            if constellation.panel_version != panel_version {
                continue;
            }
            records_scanned += 1;
            if records_measured >= max_records {
                continue;
            }
            records_measured += 1;

            let grounded_kinds = grounded_anchor_kinds(&constellation.anchors);
            let record_is_grounded = !grounded_kinds.is_empty();
            if record_is_grounded {
                grounded_records += 1;
            }
            for kind in &grounded_kinds {
                *kind_records.entry(kind.clone()).or_default() += 1;
            }
            for (slot, vector) in &constellation.slots {
                if is_absent(vector) {
                    continue;
                }
                let entry = slots.entry(slot.get()).or_default();
                entry.records_present += 1;
                if record_is_grounded {
                    entry.grounded_records += 1;
                }
            }
        }

        let ungrounded_records = records_measured.saturating_sub(grounded_records);
        let grounded_fraction = if records_measured == 0 {
            0.0
        } else {
            grounded_records as f32 / records_measured as f32
        };
        let coverage_floor = SYNAPSE_GROUNDING_COVERAGE_FLOOR;
        let provisional = grounded_fraction < coverage_floor;
        let provisional_reason = provisional.then(|| {
            format!(
                "grounded coverage {grounded_fraction:.4} is below the {coverage_floor:.4} floor; \
                 results over this domain must be tagged provisional"
            )
        });

        let anchor_kind_coverage = kind_records
            .into_iter()
            .map(|(anchor_kind, count)| SynapseCalyxAnchorKindCoverage {
                anchor_kind,
                grounded_records: count,
                coverage_fraction: if records_measured == 0 {
                    0.0
                } else {
                    count as f32 / records_measured as f32
                },
            })
            .collect::<Vec<_>>();
        let distinct_anchor_kinds = anchor_kind_coverage.len();

        let mut slot_coverage = slots
            .into_iter()
            .map(|(slot, acc)| {
                let coverage_fraction = if acc.records_present == 0 {
                    0.0
                } else {
                    acc.grounded_records as f32 / acc.records_present as f32
                };
                SynapseCalyxSlotGroundingCoverage {
                    slot,
                    records_present: acc.records_present,
                    grounded_records: acc.grounded_records,
                    ungrounded_records: acc.records_present.saturating_sub(acc.grounded_records),
                    coverage_fraction,
                    provisional: coverage_fraction < coverage_floor,
                }
            })
            .collect::<Vec<_>>();
        slot_coverage.sort_by_key(|coverage| coverage.slot);

        let mut largest_ungrounded_slots = slot_coverage.clone();
        largest_ungrounded_slots.sort_by(|a, b| {
            b.ungrounded_records
                .cmp(&a.ungrounded_records)
                .then(a.slot.cmp(&b.slot))
        });
        largest_ungrounded_slots.retain(|entry| entry.ungrounded_records > 0);
        largest_ungrounded_slots.truncate(SYNAPSE_GROUNDING_MAX_UNGROUNDED_SLOTS);

        Ok(SynapseCalyxGroundingGapReport {
            panel_version,
            records_scanned,
            records_measured,
            grounded_records,
            ungrounded_records,
            grounded_fraction,
            coverage_floor,
            provisional,
            provisional_reason,
            distinct_anchor_kinds,
            anchor_kind_coverage,
            slot_coverage,
            largest_ungrounded_slots,
            base_cf_rows,
        })
    }

    /// Returns the compact provisional verdict for one domain: the reusable
    /// control-doctrine mechanism a read path calls to decide whether a result
    /// derived from the domain may control (grounded) or must only advise
    /// (provisional). Recomputed from the physical `Base` CF at call time.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the `Base` CF cannot be
    /// scanned or a constellation row fails to decode.
    pub fn domain_grounding_verdict(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> Result<SynapseCalyxDomainGroundingVerdict, SynapseCalyxError> {
        let report = self.grounding_gap_report(panel_version, max_records)?;
        Ok(SynapseCalyxDomainGroundingVerdict {
            panel_version: report.panel_version,
            grounded_fraction: report.grounded_fraction,
            coverage_floor: report.coverage_floor,
            provisional: report.provisional,
        })
    }
}

/// Returns the set of anchor-kind labels carried by a record's *grounded*
/// anchors (confidence > 0). An ungrounded (zero/negative confidence) anchor is
/// never counted, matching the grounded-anchor definition the assay path uses.
fn grounded_anchor_kinds(anchors: &[Anchor]) -> BTreeSet<String> {
    anchors
        .iter()
        .filter(|anchor| anchor.confidence > 0.0)
        .map(|anchor| anchor_kind_label(&anchor.kind))
        .collect()
}

/// Stable string label for an anchor kind, matching the `snake_case` serde names
/// the assay path parses back (`Label(name)` reports the bare name).
pub fn anchor_kind_label(kind: &AnchorKind) -> String {
    match kind {
        AnchorKind::TestPass => "test_pass".to_owned(),
        AnchorKind::TieFormed => "tie_formed".to_owned(),
        AnchorKind::Thumbs => "thumbs".to_owned(),
        AnchorKind::Label(name) => name.clone(),
        AnchorKind::Reward => "reward".to_owned(),
        AnchorKind::SpeakerMatch => "speaker_match".to_owned(),
        AnchorKind::StyleHold => "style_hold".to_owned(),
        AnchorKind::Recurrence => "recurrence".to_owned(),
    }
}

const fn is_absent(vector: &SlotVector) -> bool {
    matches!(vector, SlotVector::Absent { .. })
}
