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
use calyx_core::{AbsentReason, Anchor, AnchorKind, SlotVector};
use serde::{Deserialize, Serialize};

use crate::{SYNAPSE_INTELLIGENCE_MAX_RECORDS, SynapseCalyxError, SynapseCalyxVault};

/// Metadata key naming the column family a constellation was measured from.
///
/// Defined here, at the layer that *reads* it for the #1940 orphan probe, and
/// re-exported by `synapse_storage::constellations` as `META_SOURCE_CF` so the
/// writer and the reader cannot drift apart into two spellings of one key.
pub const METADATA_SOURCE_CF: &str = "synapse_source_cf";
/// Metadata key holding the lowercase-hex source row key a constellation was
/// measured from. See [`METADATA_SOURCE_CF`].
pub const METADATA_SOURCE_KEY_HEX: &str = "synapse_source_key_hex";

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

/// Per-lens grounded coverage over a panel: of the records where this lens
/// actually measured something, how many carry a grounded anchor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxSlotGroundingCoverage {
    pub slot: u16,
    /// Records where this lens produced a real measurement.
    ///
    /// A slot can be stored and still carry nothing. A sparse lens measured over
    /// a record that has no text for it yields `Sparse { entries: [] }` — which
    /// is not `Absent`, so it used to be counted here as covered. `calyx-search`
    /// has always defined that vector the other way (`field_doc_count`: "a row
    /// whose vector is empty is a document that does not have this field at
    /// all"), so coverage was reporting as present exactly what the search layer
    /// excludes from BM25's `N`. See `records_empty_measurement`.
    pub records_present: usize,
    /// Records where this lens is stored but measured nothing.
    ///
    /// Reported rather than silently folded into absence, for the reason #1915
    /// gave: "skipped" and "measured zero" must not be indistinguishable in the
    /// payload. A high count here is a real signal — it means the lens is
    /// declared on a panel whose records frequently lack its source field.
    pub records_empty_measurement: usize,
    /// Records where this lens *refused* this row and left `Absent{Error}`.
    ///
    /// Distinct from both `records_present` and `records_empty_measurement`,
    /// and the reason #1924 could be closed without a silent loss: a lens that
    /// declines an over-limit input no longer takes the whole constellation
    /// down, so the loss has to be countable somewhere or it becomes invisible.
    /// An ordinary `Absent{NotApplicable}` — a slot that simply does not apply
    /// to this record — is still skipped and is NOT counted here; only an
    /// explicit refusal is. #1918 is the precedent: "skipped" and "refused"
    /// must not read as the same thing.
    pub records_slot_refused: usize,
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
    /// **#1962.** True when the domain carries no anchor of *any* kind, as
    /// distinct from carrying some and falling under the coverage floor.
    ///
    /// These are categorically different operational states and only one of them
    /// is "provisional". With zero anchor kinds there is no outcome axis at all:
    /// bits, sufficiency, synergy and the ensemble card have nothing to measure
    /// *about*, the grounding kernel's `0.20 * groundedness` selection term is
    /// zero for every candidate, and guard calibration cannot find an adjudicated
    /// exemplar. Those results are not degraded — they are undefined. A single
    /// `provisional` flag could not say which of the two a caller was holding.
    pub no_outcome_axis: bool,
    pub distinct_anchor_kinds: usize,
    pub anchor_kind_coverage: Vec<SynapseCalyxAnchorKindCoverage>,
    pub slot_coverage: Vec<SynapseCalyxSlotGroundingCoverage>,
    /// The largest ungrounded regions: lenses ranked by ungrounded record count.
    pub largest_ungrounded_slots: Vec<SynapseCalyxSlotGroundingCoverage>,
    /// Physical `Base` CF row count read back at call time.
    pub base_cf_rows: usize,
    /// Provenance of the bounded-hold walk this report was folded from (#1968).
    ///
    /// `walk.atomic()` false means the coverage numbers were accumulated across
    /// an interval rather than at one instant, so two reports taken on a live
    /// vault are not directly diffable.
    pub walk: crate::SynapseCalyxCfWalk,
}

/// Compact provisional verdict for one domain, the reusable mechanism read paths
/// consult to decide whether their results may control or must only advise.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxDomainGroundingVerdict {
    pub panel_version: u32,
    pub grounded_fraction: f32,
    pub coverage_floor: f32,
    pub provisional: bool,
    /// The domain carries no anchor of any kind, so results over it are
    /// undefined rather than provisional (#1962).
    pub no_outcome_axis: bool,
}

/// Mutable accumulator for one lens's grounded/present counts.
#[derive(Default)]
struct SlotAccumulator {
    records_present: usize,
    records_empty_measurement: usize,
    records_slot_refused: usize,
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

        let mut records_scanned = 0usize;
        let mut records_measured = 0usize;
        let mut grounded_records = 0usize;
        // record-count per anchor kind (a record counts once per kind).
        let mut kind_records: BTreeMap<String, usize> = BTreeMap::new();
        let mut slots: BTreeMap<u16, SlotAccumulator> = BTreeMap::new();

        let snapshot = self.read_snapshot();
        // #1968: paged rather than materialized. This fold accumulates per-slot
        // and per-anchor-kind totals; it never needed the whole `Base` CF
        // resident, and holding the `Base` row-guard across the materialization
        // stalled every constellation writer for the duration.
        let walk = self.walk_cf_latest(
            ColumnFamily::Base,
            crate::SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
            |_key, value| {
                let base = decode_constellation_base(value).map_err(|error| {
                    SynapseCalyxError::from_calyx("decode Base constellation", &error)
                })?;
                if base.panel_version != panel_version {
                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                }
                records_scanned += 1;
                if records_measured >= max_records {
                    // Deliberately not `Stop`: `records_scanned` is a count over
                    // the whole panel and `max_records` bounds only the
                    // *measured* subset, so the walk must reach the end of the
                    // CF for that denominator to mean what it says.
                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                }
                records_measured += 1;
                // Per-slot presence is measured from hydrated vectors: a Base row
                // decodes every slot to `Absent`, so the `is_absent` skip below
                // discarded every slot on every record and the grounding report
                // described zero lenses (issue #1894).
                let constellation = self.hydrated_constellation(base.cx_id, snapshot)?;

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
                        // A lens refusal is an absence with a reason, and it is
                        // the only absence worth counting: it means this row
                        // *had* something to measure and the lens declined it
                        // (#1924).
                        if let SlotVector::Absent {
                            reason: AbsentReason::Error(detail),
                        } = vector
                        {
                            slots.entry(slot.get()).or_default().records_slot_refused += 1;
                            tracing::debug!(
                                code = "CALYX_SLOT_REFUSAL_OBSERVED",
                                panel_version,
                                slot = slot.get(),
                                cx_id = %base.cx_id,
                                detail = %detail,
                                "grounding coverage counted a per-slot lens refusal"
                            );
                        }
                        continue;
                    }
                    let entry = slots.entry(slot.get()).or_default();
                    if is_empty_measurement(vector) {
                        entry.records_empty_measurement += 1;
                        continue;
                    }
                    entry.records_present += 1;
                    if record_is_grounded {
                        entry.grounded_records += 1;
                    }
                }
                Ok(crate::SynapseCalyxWalkStep::Continue)
            },
        )?;
        let base_cf_rows = walk.rows_visited;

        let ungrounded_records = records_measured.saturating_sub(grounded_records);
        let grounded_fraction = if records_measured == 0 {
            0.0
        } else {
            grounded_records as f32 / records_measured as f32
        };
        let coverage_floor = SYNAPSE_GROUNDING_COVERAGE_FLOOR;
        let provisional = grounded_fraction < coverage_floor;
        // Computed before the reason so the reason can name which of the two
        // conditions holds (#1962). `records_measured == 0` is neither: an empty
        // pass has not established anything about the domain's outcome axis.
        let no_outcome_axis = records_measured > 0 && kind_records.is_empty();
        let provisional_reason = provisional.then(|| {
            if no_outcome_axis {
                format!(
                    "this domain carries no anchor of any kind over its {records_measured} \
                     measured record(s), so it has no outcome axis: every bits/sufficiency/ \
                     synergy/kernel-groundedness result over it is undefined rather than \
                     provisional, and guard calibration has no adjudicated exemplar to find. \
                     Check whether the panel is observation-shaped by design (the panel catalog's \
                     outcome_bearing declaration) before treating this as a missing write"
                )
            } else {
                format!(
                    "grounded coverage {grounded_fraction:.4} is below the {coverage_floor:.4} \
                     floor; results over this domain must be tagged provisional"
                )
            }
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
                    records_empty_measurement: acc.records_empty_measurement,
                    records_slot_refused: acc.records_slot_refused,
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
            no_outcome_axis,
            distinct_anchor_kinds,
            anchor_kind_coverage,
            slot_coverage,
            largest_ungrounded_slots,
            base_cf_rows,
            walk,
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
            no_outcome_axis: report.no_outcome_axis,
        })
    }
}

/// One panel generation's census row: how many `Base` constellations carry this
/// exact `panel_version`, and how many of them are grounded.
///
/// Deliberately NOT per-lens. Per-lens coverage requires hydrating every record
/// from its per-slot CFs (issue #1894), which is what makes
/// [`SynapseCalyxVault::grounding_gap_report`] a heavy pass. The two numbers a
/// coverage question actually needs — *how many records exist at this version*
/// and *how many carry a grounded anchor* — both live on the `Base` row itself,
/// so this row is filled by a decode with no hydration at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxPanelCensusEntry {
    pub panel_version: u32,
    /// `Base` constellations carrying this exact panel version.
    pub records: usize,
    /// Records carrying at least one grounded anchor (confidence > 0).
    pub grounded_records: usize,
    /// Record counts per grounded anchor-kind label. A record with two grounded
    /// kinds counts once under each.
    pub anchor_kind_records: BTreeMap<String, usize>,
    /// Oldest and newest `created_at` (Calyx stamps milliseconds) seen at this
    /// version. A superseded generation whose newest record predates the active
    /// generation's oldest is a *closed* generation; overlap means writes are
    /// still landing on two versions at once, which is a defect.
    pub earliest_created_at_ms: Option<u64>,
    pub latest_created_at_ms: Option<u64>,
    /// Each record's declared provenance at this generation: source CF name →
    /// the set of source row keys (lowercase hex) measured from it (#1940).
    ///
    /// This is what turns the orphan count from a subtraction into a probe. The
    /// old count was `active_version_records - source_cf_rows`, which is a
    /// difference between two populations that are not the same set: the
    /// minuend counts one generation, the subtrahend counts every row of the
    /// source CF including rows measured onto a *superseded* generation and
    /// rows not yet measured at all. On the live vault 2026-08-01 that
    /// arithmetic reported 667 orphans where the probe finds 227.
    ///
    /// A set rather than a count because the question is membership — "does
    /// this record's own source row still exist" — and no count can answer it.
    pub source_key_hexes: BTreeMap<String, BTreeSet<String>>,
    /// Source identities for the subset of records carrying at least one
    /// grounded anchor. Kept separately because panel-version bumps change the
    /// constellation id while preserving the source identity; exact set
    /// difference is therefore the only sound stranded-anchor detector.
    pub grounded_source_key_hexes: BTreeMap<String, BTreeSet<String>>,
    /// Grounded records whose source identity is absent. They cannot be joined
    /// across generations and must remain explicitly unknown.
    pub grounded_unattributed_records: usize,
    /// Records at this generation carrying no source provenance at all, so the
    /// probe cannot even be attempted for them (#1940). Reported, never folded
    /// into either the covered or the orphaned count.
    pub unattributed_records: usize,
}

impl SynapseCalyxPanelCensusEntry {
    /// Grounded fraction over this generation's own records.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "record counts are bounded by the physical Base CF row count"
    )]
    pub fn grounded_fraction(&self) -> f32 {
        if self.records == 0 {
            return 0.0;
        }
        self.grounded_records as f32 / self.records as f32
    }
}

/// Whole-vault panel census: every panel generation physically present in the
/// `Base` CF, from ONE decode-only scan.
///
/// Why this exists (issues #1927 ask 1, #1920 ask 1). Both asked for the same
/// missing readback from opposite directions:
///
/// * #1927 — a panel version bump left the active generation holding 389 of
///   22,999 source rows, and the only way to notice was to call
///   `grounding_gap` against a *guessed* version number and compare it by hand
///   to `corpus_histogram`.
/// * #1920 — "add a per-panel anchored-fraction readback that does not require
///   running `grounding_gap` over a whole panel. Right now the only way to learn
///   that a panel is unmeasurable is to try to measure it and read the refusal."
///
/// One scan answers both, and it enumerates the versions rather than taking them
/// as input: a generation nobody remembered to ask about is exactly the one that
/// strands records. `records_at()` returning 0 for a version that is *in* the
/// catalog and `unknown` holding a version that is *not* are different facts and
/// are reported as such.
///
/// Cost: one `scan_cf_latest(Base)` plus one `decode_constellation_base` per
/// row. No `hydrated_constellation` call, so no per-slot CF read — that is the
/// entire difference from `grounding_gap_report`, and the reason this is
/// affordable on a periodic tick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxPanelCensus {
    /// One entry per distinct panel version present, ascending.
    pub entries: Vec<SynapseCalyxPanelCensusEntry>,
    /// Physical `Base` CF row count read back at call time.
    pub base_cf_rows: usize,
    /// Rows whose `Base` encoding would not decode.
    ///
    /// Counted and surfaced, never skipped. `base_cf_rows - decode_failures`
    /// must equal the sum of every entry's `records`; a caller can check that
    /// invariant, which is what stops a corpus with unreadable rows from
    /// masquerading as a smaller clean one (the #1918 lesson, applied here).
    pub decode_failures: usize,
    /// The first decode failure's detail, so a non-zero count is actionable
    /// rather than merely alarming.
    pub first_decode_failure: Option<String>,
    pub measured_at_unix_ms: Option<u64>,
    /// Provenance of the bounded-hold walk this census was folded from (#1968).
    ///
    /// `base_cf_rows` used to be the length of a whole-`Base` materialization
    /// taken under one row-guard hold, so it was an exact count at one instant
    /// by construction. It is now folded page by page, and whether the pages
    /// shared one committed sequence is a property of the run rather than of
    /// the code — so it is reported. `walk.atomic()` false means this census
    /// describes an interval, not an instant, and must not be diffed against
    /// another census as though both were snapshots.
    pub walk: crate::SynapseCalyxCfWalk,
}

impl SynapseCalyxPanelCensus {
    /// Records present at one exact panel version (0 when the version is absent).
    #[must_use]
    pub fn records_at(&self, panel_version: u32) -> usize {
        self.entry(panel_version).map_or(0, |entry| entry.records)
    }

    /// The census row for one exact panel version, if that version is present.
    #[must_use]
    pub fn entry(&self, panel_version: u32) -> Option<&SynapseCalyxPanelCensusEntry> {
        self.entries
            .iter()
            .find(|entry| entry.panel_version == panel_version)
    }

    /// Sum of every entry's `records`. Equals `base_cf_rows - decode_failures`
    /// on a healthy vault.
    #[must_use]
    pub fn records_total(&self) -> usize {
        self.entries.iter().map(|entry| entry.records).sum()
    }
}

impl SynapseCalyxVault {
    /// Builds the grounded anchor lineage for one source CF from physical Base
    /// rows at the exact superseded generations supplied by the panel catalog.
    ///
    /// Mutable source rows cannot reconstruct an old `cx_id` from their current
    /// bytes. The Base row is the source of truth for the historical id and the
    /// anchor observation, so a panel-bump carry must join by stable source key
    /// and read the old anchor from that row.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the Base rows cannot be scanned or decoded.
    pub fn grounded_anchor_lineage_by_source_key(
        &self,
        source_cf: &str,
        superseded_versions_newest_first: &[u32],
    ) -> Result<BTreeMap<String, Vec<Anchor>>, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("grounded_anchor_lineage_by_source_key");
        let rank: BTreeMap<u32, usize> = superseded_versions_newest_first
            .iter()
            .copied()
            .enumerate()
            .map(|(rank, version)| (version, rank))
            .collect();
        let mut indexed: BTreeMap<String, BTreeMap<String, (usize, Anchor)>> = BTreeMap::new();
        let mut decode_failures = 0usize;
        let mut first_failure = None::<String>;
        self.walk_cf_latest(
            ColumnFamily::Base,
            crate::SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
            |key, value| {
                let base = match decode_constellation_base(value) {
                    Ok(base) => base,
                    Err(error) => {
                        decode_failures += 1;
                        first_failure.get_or_insert_with(|| {
                            format!("key_hex={} error={error}", hex_lower(key))
                        });
                        return Ok(crate::SynapseCalyxWalkStep::Continue);
                    }
                };
                let Some(&generation_rank) = rank.get(&base.panel_version) else {
                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                };
                if base.metadata.get(METADATA_SOURCE_CF).map(String::as_str) != Some(source_cf) {
                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                }
                let Some(source_key) = base.metadata.get(METADATA_SOURCE_KEY_HEX) else {
                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                };
                let by_kind = indexed.entry(source_key.clone()).or_default();
                for anchor in base
                    .anchors
                    .iter()
                    .filter(|anchor| anchor.confidence.is_finite() && anchor.confidence > 0.0)
                {
                    let kind = anchor_kind_label(&anchor.kind);
                    match by_kind.get(&kind) {
                        Some((seen_rank, _)) if *seen_rank <= generation_rank => {}
                        _ => {
                            by_kind.insert(kind, (generation_rank, anchor.clone()));
                        }
                    }
                }
                Ok(crate::SynapseCalyxWalkStep::Continue)
            },
        )?;
        if decode_failures != 0 {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_ANCHOR_LINEAGE_DECODE_FAILED",
                format!(
                    "cannot build exact anchor lineage for source_cf={source_cf}: {decode_failures} Base row(s) failed to decode; first={first_failure:?}"
                ),
                "repair the named Base row corruption, verify the vault chain, and rerun the carry; never carry from a partial lineage index",
            ));
        }
        Ok(indexed
            .into_iter()
            .map(|(source_key, kinds)| {
                (
                    source_key,
                    kinds.into_values().map(|(_, anchor)| anchor).collect(),
                )
            })
            .collect())
    }

    /// Censuses every panel generation in the `Base` CF in one decode-only pass.
    ///
    /// Read-only and unbounded by design: it must see *every* row, because its
    /// whole job is to find the generations nobody thought to ask about. A cap
    /// would make "this version holds 0 records" and "the scan stopped before
    /// reaching them" indistinguishable, which is the failure this readback
    /// exists to remove.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the `Base` CF cannot be
    /// scanned. A row that fails to *decode* is counted into `decode_failures`
    /// and does not abort the pass: one unreadable row must not cost the census
    /// of every other generation, and a silently-dropped row would be worse than
    /// either.
    #[expect(
        clippy::too_many_lines,
        reason = "one paged physical scan must retain all generation counters and first-failure evidence in a single pass"
    )]
    pub fn panel_census(&self) -> Result<SynapseCalyxPanelCensus, SynapseCalyxError> {
        // Hot-path boundary (#1686): a whole-Base scan is off-runtime
        // maintenance work and must never be driven from a tagged reflex tick.
        crate::lowering::hot_context::assert_cold_calyx("panel_census");

        let mut by_version: BTreeMap<u32, SynapseCalyxPanelCensusEntry> = BTreeMap::new();
        let mut decode_failures = 0usize;
        let mut first_decode_failure: Option<String> = None;

        // #1968: paged rather than materialized. This fold never needed every
        // `Base` row at once — it counts per panel version — and collecting
        // 106k dense-slot rows to walk them once held the `Base` row-guard for
        // 290 ms on average and 2.65 s at worst, stalling the constellation
        // writers that land in the same family.
        let walk = self.walk_cf_latest(
            ColumnFamily::Base,
            crate::SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
            |key, value| {
                let base = match decode_constellation_base(value) {
                    Ok(base) => base,
                    Err(error) => {
                        decode_failures += 1;
                        if first_decode_failure.is_none() {
                            first_decode_failure =
                                Some(format!("key_hex={} error={error}", hex_lower(key)));
                        }
                        return Ok(crate::SynapseCalyxWalkStep::Continue);
                    }
                };
                let entry = by_version.entry(base.panel_version).or_insert_with(|| {
                    SynapseCalyxPanelCensusEntry {
                        panel_version: base.panel_version,
                        records: 0,
                        grounded_records: 0,
                        anchor_kind_records: BTreeMap::new(),
                        earliest_created_at_ms: None,
                        latest_created_at_ms: None,
                        source_key_hexes: BTreeMap::new(),
                        grounded_source_key_hexes: BTreeMap::new(),
                        grounded_unattributed_records: 0,
                        unattributed_records: 0,
                    }
                });
                entry.records += 1;
                // #1940: the declared provenance of each record, kept so the
                // orphan count can be a per-record probe against the source CF
                // rather than a subtraction of two counts taken over different
                // populations.
                match (
                    base.metadata.get(METADATA_SOURCE_CF),
                    base.metadata.get(METADATA_SOURCE_KEY_HEX),
                ) {
                    (Some(source_cf), Some(source_key_hex)) => {
                        entry
                            .source_key_hexes
                            .entry(source_cf.clone())
                            .or_default()
                            .insert(source_key_hex.clone());
                    }
                    _ => entry.unattributed_records += 1,
                }
                entry.earliest_created_at_ms = Some(
                    entry
                        .earliest_created_at_ms
                        .map_or(base.created_at, |seen| seen.min(base.created_at)),
                );
                entry.latest_created_at_ms = Some(
                    entry
                        .latest_created_at_ms
                        .map_or(base.created_at, |seen| seen.max(base.created_at)),
                );
                // Anchors are carried on the Base row itself (they are encoded
                // into it alongside the scalars), which is precisely why this
                // pass does not need to hydrate. Slot *vectors* are the thing
                // that lives in the per-slot CFs (#1894), and this census reads
                // no slot vector.
                let grounded_kinds = grounded_anchor_kinds(&base.anchors);
                if !grounded_kinds.is_empty() {
                    entry.grounded_records += 1;
                    match (
                        base.metadata.get(METADATA_SOURCE_CF),
                        base.metadata.get(METADATA_SOURCE_KEY_HEX),
                    ) {
                        (Some(source_cf), Some(source_key_hex)) => {
                            entry
                                .grounded_source_key_hexes
                                .entry(source_cf.clone())
                                .or_default()
                                .insert(source_key_hex.clone());
                        }
                        _ => entry.grounded_unattributed_records += 1,
                    }
                }
                for kind in grounded_kinds {
                    *entry.anchor_kind_records.entry(kind).or_default() += 1;
                }
                Ok(crate::SynapseCalyxWalkStep::Continue)
            },
        )?;
        let base_cf_rows = walk.rows_visited;

        if decode_failures > 0 {
            tracing::error!(
                code = "SYNAPSE_CALYX_PANEL_CENSUS_DECODE_FAILURES",
                decode_failures,
                base_cf_rows,
                first_decode_failure = ?first_decode_failure,
                "Base rows would not decode during the panel census; every count below is over \
                 the rows that DID decode, and base_cf_rows - decode_failures is the denominator \
                 to check against"
            );
        }

        Ok(SynapseCalyxPanelCensus {
            entries: by_version.into_values().collect(),
            base_cf_rows,
            decode_failures,
            first_decode_failure,
            walk,
            measured_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok()),
        })
    }
}

/// Lowercase hex for a physical row key, so a decode failure names the exact row.
fn hex_lower(key: &[u8]) -> String {
    use std::fmt::Write as _;
    key.iter()
        .fold(String::with_capacity(key.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
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
#[must_use]
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

/// True when a slot is stored but carries no measurement.
///
/// Only sparse and multi-vector shapes can answer this. For those, "no entries"
/// is definitionally "no terms were found in this document" — `calyx-search`
/// already relies on exactly that reading to keep empty rows out of BM25's `N`
/// and `avgdl`, while retaining the row so a delta can mask it.
///
/// A dense vector is deliberately NOT included, even when every component is
/// zero. For a dense lens zero is a *value*, not an absence: a z-score of a
/// record sitting exactly at the mean is 0.0, and reading that as "unmeasured"
/// would invent the opposite error to the one this fixes.
const fn is_empty_measurement(vector: &SlotVector) -> bool {
    match vector {
        SlotVector::Sparse { entries, .. } => entries.is_empty(),
        SlotVector::Multi { tokens, .. } => tokens.is_empty(),
        _ => false,
    }
}
