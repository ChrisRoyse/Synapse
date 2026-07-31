//! Per-panel coverage and grounding census (issues #1927 ask 1, #1920 ask 1).
//!
//! # What was missing
//!
//! Two issues asked for the same readback from opposite directions and both
//! stalled without it.
//!
//! **#1927.** `syn-agent-transcript-v1` bumped to panel version 1921001 and the
//! active generation was left holding **389 of 22,999** source rows — 1.7%. The
//! live write path was correct; the history simply never moved, because adding a
//! lens is documented as "one call plus a lazy backfill" and *nothing drove the
//! backfill*. Anything scoped to the active panel therefore saw 1.7% of the
//! corpus: `bits` measured 389 records, `kernel` claimed to explain a domain from
//! 389 records, and grounded anchors could not be written at all — an anchor
//! targets the `cx_id` derived from `(input_bytes, active_panel_version, salt)`,
//! and Calyx correctly refuses one whose constellation is absent
//! (`CALYX_STALE_DERIVED`). The only way to *notice* was to call
//! `hygiene grounding_gap` against a guessed version number and compare it by
//! hand to `storage corpus_histogram`.
//!
//! **#1920.** "Add a per-panel anchored-fraction readback that does not require
//! running `grounding_gap` over a whole panel. Right now the only way to learn
//! that a panel is unmeasurable is to try to measure it and read the refusal."
//!
//! # Why one report answers both
//!
//! Both questions are answered by two numbers per panel generation — how many
//! `Base` constellations carry it, and how many of those are grounded — and both
//! numbers live on the `Base` row itself. [`SynapseCalyxVault::panel_census`]
//! gets them from ONE decode-only scan with no per-slot hydration, which is the
//! whole reason this is affordable on a five-minute tick when
//! `grounding_gap_report` is not.
//!
//! This module joins that physical census against the *declared*
//! [`builtin_panel_catalog`], and the declarations are what turn counts into
//! verdicts:
//!
//! * [`crate::constellations::PanelSource::FullCf`] says a coverage fraction is meaningful for this
//!   panel. A sampled or prefix-filtered panel declares [`crate::constellations::PanelSource::SubsetOfCf`]
//!   and gets no fraction, because the denominator would be the wrong population.
//! * `outcome_bearing` says whether 0.0 grounded coverage is a gap or is correct
//!   (#1920 ask 3). A timeline row is an observation; it has no outcome to carry.
//! * `superseded_versions` separates "stranded on an old generation" from "never
//!   measured", two states with opposite remedies (#1927 ask 3).
//! * `backfill_source_cf` says whether the shortfall can be repaired at all. A
//!   panel with no re-measure path reports an **un-backfillable** shortfall
//!   rather than being quietly counted as covered.
//!
//! # Fail-visible, not fail-quiet
//!
//! A panel version present in the `Base` CF that no catalog entry claims is
//! reported in [`PanelCoverageReport::unknown_panel_versions`], never dropped.
//! `Base` rows that will not decode are counted, and
//! `base_cf_rows - decode_failures == records_total` is an invariant a caller can
//! check, so a corpus with unreadable rows cannot masquerade as a smaller clean
//! one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use synapse_calyx::{SYNAPSE_GROUNDING_COVERAGE_FLOOR, SynapseCalyxPanelCensus};

use crate::constellations::builtin_panel_catalog;

/// Fraction of its source CF an active panel generation must cover before the
/// panel is reported healthy (#1927 ask 4).
///
/// Not 1.0, and the reason is a race rather than a tolerance for shortfall: the
/// census scan and the live write path run concurrently, so rows committed
/// between the `Base` scan and the source-CF count legitimately appear in the
/// denominator and not the numerator. 0.95 absorbs that skew on every corpus in
/// this vault while leaving no room for a real backfill debt — the observed
/// failure was 0.017, not 0.94.
pub const SYN_PANEL_COVERAGE_FLOOR: f32 = 0.95;

/// One panel's coverage and grounding row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelCoverageRow {
    pub panel_name: String,
    /// The generation new records are written at.
    pub panel_version: u32,
    /// Declared in the catalog, never inferred (#1920 ask 3).
    pub outcome_bearing: bool,
    pub source_cf: Option<String>,
    /// True when `source_cf_rows` is this panel's real denominator.
    pub source_is_full_cf: bool,
    /// Physical row count of the source CF, counted only when it IS the
    /// denominator. `None` for subset-fed and derived panels, so a meaningless
    /// ratio is never available to be misread.
    pub source_cf_rows: Option<u64>,
    /// `Base` constellations at the active generation.
    pub active_version_records: usize,
    /// `active_version_records / source_cf_rows`, only for full-CF panels.
    ///
    /// **Reported unclamped.** It can exceed 1.0, and when it does that is a
    /// real condition rather than a rounding artefact — see
    /// [`Self::records_exceed_source`]. Clamping it to 1.0 would turn the one
    /// number that reveals that condition into a clean-looking `1.0000`, which
    /// is the same class of error as the 1.7% coverage this readback exists to
    /// surface: a number that looks fine because something rounded it.
    pub coverage_fraction: Option<f32>,
    /// True when a full-CF panel is below [`SYN_PANEL_COVERAGE_FLOOR`].
    pub coverage_below_floor: bool,
    /// True when this panel holds MORE constellations than its source CF holds
    /// rows.
    ///
    /// Measured on the live vault 2026-07-31: `syn-action-v1` held **665**
    /// constellations against **235** `CF_ACTION_LOG` rows, and `syn-process-v1`
    /// held **2** against **0**. Not a defect in the census — a real property of
    /// the vault. Those CFs carry an audit TTL (`CF_ACTION_LOG` 24h,
    /// `CF_PROCESS_HISTORY` 6h) and the GC expires the source rows, while the
    /// `Base` constellations measured from them are sacred data that is never
    /// auto-deleted. So the panel legitimately outlives its own source.
    ///
    /// This is the OPPOSITE condition to a coverage shortfall and has the
    /// opposite remedy — nothing is missing, and driving a backfill would do
    /// nothing — so it is a separate flag and is deliberately NOT counted as a
    /// coverage deficiency. It is surfaced because it is an unbounded-growth
    /// condition: every expired source row leaves a constellation behind
    /// forever, which is the same retention question #1927 ask 3 raises for
    /// superseded generations.
    pub records_exceed_source: bool,
    /// Records stranded on this panel's declared superseded generations.
    pub superseded_records: usize,
    /// Each superseded generation actually present, with its record count.
    pub superseded_versions_present: Vec<(u32, usize)>,
    /// Active-generation records carrying at least one grounded anchor.
    pub grounded_records: usize,
    pub grounded_fraction: f32,
    /// True only when this panel is declared outcome-bearing AND its grounded
    /// fraction is below the coverage floor. A deliberate 0.0 on an
    /// observation-shaped panel is never flagged here (#1920 ask 3).
    pub grounding_below_floor: bool,
    /// Record counts per grounded anchor kind at the active generation.
    pub anchor_kind_records: BTreeMap<String, usize>,
    /// Whether a re-measure path exists for this panel's shortfall.
    pub backfill_source_cf: Option<String>,
}

impl PanelCoverageRow {
    /// Whether the KSG estimator can run on this panel at all (#1920 ask 4).
    ///
    /// The estimator needs paired samples, and a *pair* is a record with both a
    /// lens measurement and a grounded anchor — so `grounded_records` IS the
    /// sample count, and `grounded_records < SYNAPSE_ASSAY_MIN_SAMPLES` means
    /// `bits`, `sufficiency` and `redundancy` cannot produce a number no matter
    /// which slot they are pointed at.
    ///
    /// #1920 ask 4 asked for the anchored fraction to become "an admission input
    /// for the intelligence surfaces … rather than each discovering it
    /// separately at call time". This is that predicate, answerable from this
    /// one readback before any surface is called. It does not change what those
    /// surfaces do when called — they still refuse honestly, and that refusal is
    /// correct — it removes the need to call one in order to learn the answer.
    #[must_use]
    pub const fn assay_measurable(&self) -> bool {
        self.grounded_records >= synapse_calyx::SYNAPSE_ASSAY_MIN_SAMPLES
    }

    /// Grounded records still needed before the estimator can run. `0` when it
    /// already can.
    #[must_use]
    pub const fn assay_samples_short(&self) -> usize {
        synapse_calyx::SYNAPSE_ASSAY_MIN_SAMPLES.saturating_sub(self.grounded_records)
    }

    /// Source rows this panel's active generation has not measured.
    ///
    /// `None` when no meaningful denominator exists.
    #[must_use]
    pub fn uncovered_rows(&self) -> Option<u64> {
        if !self.source_is_full_cf {
            return None;
        }
        let rows = self.source_cf_rows?;
        Some(rows.saturating_sub(self.active_version_records as u64))
    }

    /// True when this panel is short of its source CF and something can be done
    /// about it. This is the maintainer's work predicate (#1927 ask 2).
    #[must_use]
    pub fn backfill_owed(&self) -> bool {
        self.coverage_below_floor && self.backfill_source_cf.is_some()
    }
}

/// Whole-vault panel coverage and grounding report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelCoverageReport {
    /// One row per declared panel, in catalog order.
    pub panels: Vec<PanelCoverageRow>,
    /// Panel versions physically present in `Base` that no catalog entry claims,
    /// with their record counts. Reported rather than dropped: an unclaimed
    /// generation is exactly the kind of thing that strands records invisibly.
    pub unknown_panel_versions: Vec<(u32, usize)>,
    pub base_cf_rows: usize,
    /// Sum over every generation present. `base_cf_rows - decode_failures`.
    pub records_total: usize,
    pub decode_failures: usize,
    pub first_decode_failure: Option<String>,
    /// Every `Base` row not at some panel's active generation — the #1927 ask 3
    /// number, made visible so the retention decision can be taken on a
    /// measurement instead of an estimate.
    pub superseded_records_total: usize,
    pub coverage_floor: f32,
    pub grounding_floor: f32,
    /// Full-CF panels below the coverage floor. Names, so a log line is readable.
    pub coverage_deficient_panels: Vec<String>,
    /// Coverage-deficient panels with NO re-measure path: the maintainer cannot
    /// fix these and says so rather than reporting a clean pass.
    pub unbackfillable_deficient_panels: Vec<String>,
    /// Outcome-bearing panels below the grounding floor.
    pub grounding_deficient_panels: Vec<String>,
    /// Panels holding more constellations than their source CF holds rows,
    /// because an audit TTL expired the source rows while the `Base`
    /// constellations measured from them are never auto-deleted. Not a
    /// shortfall — an unbounded-growth condition, reported apart from one.
    pub records_exceed_source_panels: Vec<String>,
    pub measured_at_unix_ms: Option<u64>,
}

impl PanelCoverageReport {
    /// True when the physical row accounting is self-consistent.
    ///
    /// Every `Base` row is either decoded into exactly one generation's count or
    /// counted as a decode failure. A caller that cannot prove this must not
    /// treat the coverage numbers as complete.
    #[must_use]
    pub const fn accounting_holds(&self) -> bool {
        self.base_cf_rows == self.records_total + self.decode_failures
    }

    /// The panel most owed a backfill, by absolute uncovered rows.
    ///
    /// Absolute rows rather than the fraction on purpose: the tick has a fixed
    /// row budget, so the panel where a page of work removes the most stranded
    /// rows is the one to spend it on. Ties break on the lower panel version so
    /// the choice is deterministic across ticks.
    #[must_use]
    pub fn most_owed_backfill(&self) -> Option<&PanelCoverageRow> {
        self.panels
            .iter()
            .filter(|panel| panel.backfill_owed())
            .max_by_key(|panel| {
                (
                    panel.uncovered_rows().unwrap_or(0),
                    u32::MAX - panel.panel_version,
                )
            })
    }

    /// One-line summary for a log or a health detail field.
    #[must_use]
    pub fn summary_line(&self) -> String {
        self.panels
            .iter()
            .map(|panel| {
                let coverage = panel
                    .coverage_fraction
                    .map_or_else(|| "n/a".to_owned(), |fraction| format!("{fraction:.4}"));
                format!(
                    "{}@{}:records={} cov={} grounded={}/{} ({:.4}) superseded={}",
                    panel.panel_name,
                    panel.panel_version,
                    panel.active_version_records,
                    coverage,
                    panel.grounded_records,
                    panel.active_version_records,
                    panel.grounded_fraction,
                    panel.superseded_records,
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Joins a physical panel census against the declared catalog and the source-CF
/// row counts the caller measured.
///
/// `source_cf_rows` must hold a count for every CF that any catalog entry
/// declares as [`crate::constellations::PanelSource::FullCf`]. A missing count yields
/// `coverage_fraction = None` and `coverage_below_floor = false` — a panel whose
/// denominator was not measured is *unknown*, never *deficient*, because
/// reporting an unmeasured panel as failing is the same class of error as
/// reporting a failing one as ok.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "record and row counts are bounded by physical CF sizes"
)]
pub fn build_panel_coverage_report(
    census: &SynapseCalyxPanelCensus,
    source_cf_rows: &BTreeMap<String, u64>,
) -> PanelCoverageReport {
    let catalog = builtin_panel_catalog();
    let mut panels = Vec::with_capacity(catalog.len());
    let mut claimed_versions: Vec<u32> = Vec::new();
    let mut superseded_records_total = 0usize;
    let mut coverage_deficient_panels = Vec::new();
    let mut unbackfillable_deficient_panels = Vec::new();
    let mut grounding_deficient_panels = Vec::new();
    let mut records_exceed_source_panels = Vec::new();

    for entry in catalog {
        claimed_versions.push(entry.panel_version);
        claimed_versions.extend_from_slice(entry.superseded_versions);

        let active = census.entry(entry.panel_version);
        let active_version_records = active.map_or(0, |row| row.records);
        let grounded_records = active.map_or(0, |row| row.grounded_records);
        let grounded_fraction = active.map_or(0.0, |row| row.grounded_fraction());
        let anchor_kind_records = active
            .map(|row| row.anchor_kind_records.clone())
            .unwrap_or_default();

        let superseded_versions_present: Vec<(u32, usize)> = entry
            .superseded_versions
            .iter()
            .filter_map(|version| census.entry(*version).map(|row| (*version, row.records)))
            .collect();
        let superseded_records: usize = superseded_versions_present
            .iter()
            .map(|(_, records)| *records)
            .sum();
        superseded_records_total += superseded_records;

        let source_is_full_cf = entry.source.is_full_cf();
        let source_cf = entry.source.cf_name().map(str::to_owned);
        let source_cf_row_count = if source_is_full_cf {
            source_cf
                .as_deref()
                .and_then(|cf| source_cf_rows.get(cf).copied())
        } else {
            None
        };

        // Three cases, and collapsing any two of them loses a real distinction:
        //
        //   rows > 0            -> the ratio, UNCLAMPED (it may exceed 1.0).
        //   rows == 0, recs == 0 -> 1.0. Nothing exists and nothing is missing.
        //                          Reading 0/0 as a deficiency would flag every
        //                          panel registered but not yet written to.
        //   rows == 0, recs > 0  -> None. The ratio is genuinely undefined, and
        //                          `records_exceed_source` carries the fact that
        //                          matters. Returning 1.0 here would say "fully
        //                          covered" about a panel whose source CF the GC
        //                          has emptied underneath it — technically true
        //                          (nothing is uncovered) and thoroughly
        //                          misleading, which is the failure mode this
        //                          whole readback exists to remove.
        let coverage_fraction = source_cf_row_count.and_then(|rows| {
            if rows == 0 {
                (active_version_records == 0).then_some(1.0)
            } else {
                Some(active_version_records as f32 / rows as f32)
            }
        });
        let records_exceed_source =
            source_cf_row_count.is_some_and(|rows| active_version_records as u64 > rows);
        let coverage_below_floor =
            coverage_fraction.is_some_and(|fraction| fraction < SYN_PANEL_COVERAGE_FLOOR);
        let grounding_below_floor =
            entry.outcome_bearing && grounded_fraction < SYNAPSE_GROUNDING_COVERAGE_FLOOR;

        if coverage_below_floor {
            coverage_deficient_panels.push(entry.panel_name.to_owned());
            if entry.backfill_source_cf.is_none() {
                unbackfillable_deficient_panels.push(entry.panel_name.to_owned());
            }
        }
        if grounding_below_floor {
            grounding_deficient_panels.push(entry.panel_name.to_owned());
        }
        if records_exceed_source {
            records_exceed_source_panels.push(entry.panel_name.to_owned());
        }

        panels.push(PanelCoverageRow {
            panel_name: entry.panel_name.to_owned(),
            panel_version: entry.panel_version,
            outcome_bearing: entry.outcome_bearing,
            source_cf,
            source_is_full_cf,
            source_cf_rows: source_cf_row_count,
            active_version_records,
            coverage_fraction,
            coverage_below_floor,
            records_exceed_source,
            superseded_records,
            superseded_versions_present,
            grounded_records,
            grounded_fraction,
            grounding_below_floor,
            anchor_kind_records,
            backfill_source_cf: entry.backfill_source_cf.map(str::to_owned),
        });
    }

    let unknown_panel_versions: Vec<(u32, usize)> = census
        .entries
        .iter()
        .filter(|row| !claimed_versions.contains(&row.panel_version))
        .map(|row| (row.panel_version, row.records))
        .collect();
    // An unclaimed generation is stranded by definition: no active-panel surface
    // reads it, so it belongs in the same total as the declared superseded ones.
    superseded_records_total += unknown_panel_versions
        .iter()
        .map(|(_, records)| *records)
        .sum::<usize>();

    PanelCoverageReport {
        panels,
        unknown_panel_versions,
        base_cf_rows: census.base_cf_rows,
        records_total: census.records_total(),
        decode_failures: census.decode_failures,
        first_decode_failure: census.first_decode_failure.clone(),
        superseded_records_total,
        coverage_floor: SYN_PANEL_COVERAGE_FLOOR,
        grounding_floor: SYNAPSE_GROUNDING_COVERAGE_FLOOR,
        coverage_deficient_panels,
        unbackfillable_deficient_panels,
        grounding_deficient_panels,
        records_exceed_source_panels,
        measured_at_unix_ms: census.measured_at_unix_ms,
    }
}
