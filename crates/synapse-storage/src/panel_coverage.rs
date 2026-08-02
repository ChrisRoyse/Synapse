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
//! # Retention: the #1927 ask 3 decision
//!
//! Ask 3 asked what happens to the 41,237 records (55% of the `Base` CF) sitting
//! on superseded generations, and to the constellations whose TTL'd source rows
//! the GC has expired. Both are answered here, and the answer to both is
//! **nothing is auto-deleted** — but for two different reasons, and conflating
//! them is the mistake this section exists to prevent.
//!
//! **Orphaned records — sacred, permanently.** A constellation whose source row
//! an audit TTL expired cannot be re-measured; it is the only surviving record of
//! the observation. Keeping it is not a deferral, it is the decision. The growth
//! it implies is bounded by the source CF's TTL policy, which is a decision about
//! the audit log, not about the measurement. Counted as
//! [`PanelCoverageReport::orphaned_records_total`].
//!
//! **Superseded records — reclaimable in principle, unprovable by a census.** A
//! constellation is derived state: a frozen pipeline re-measures it from its
//! source row. An *anchor* is not — it records an observed outcome with a source
//! and a confidence, and no re-measure regenerates it. So a superseded record
//! carrying an anchor is sacred, and one without an anchor holds nothing its
//! source row could not produce again.
//!
//! That makes the reclaim rule four conditions per record: its generation is
//! closed, it carries no grounded anchor, its own source row still exists, and
//! the same input is already measured at the active generation. This report can
//! evaluate three of them. It cannot evaluate the third, and the live vault
//! shows why that matters rather than being pedantic:
//! `syn-agent-transcript-v1` holds 40,501 superseded records against a
//! `CF_AGENT_TRANSCRIPTS` of 25,573 rows, so some superseded records certainly
//! correspond to source rows that are gone — and which ones is a per-record
//! lookup, not a count.
//!
//! Hence [`PanelCoverageReport::superseded_reclaim_candidates`] is an upper
//! bound with that name, and no reclaim runs off it. Auto-deleting `Base` rows on
//! a count that cannot prove regenerability would be exactly the silent
//! destruction the sacred/regenerable split exists to forbid.
//!
//! # Fail-visible, not fail-quiet
//!
//! A panel version present in the `Base` CF that no catalog entry claims is
//! reported in [`PanelCoverageReport::unknown_panel_versions`], never dropped.
//! `Base` rows that will not decode are counted, and
//! `base_cf_rows - decode_failures == records_total` is an invariant a caller can
//! check, so a corpus with unreadable rows cannot masquerade as a smaller clean
//! one.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use synapse_calyx::{SYNAPSE_GROUNDING_COVERAGE_FLOOR, SynapseCalyxPanelCensus};

use crate::constellations::builtin_panel_catalog;

/// One superseded generation, carrying the facts the retention decision in
/// [`PanelCoverageReport::superseded_grounded_records_total`] actually turns on
/// (#1927 ask 3).
///
/// The count alone was never enough. "41,237 records are on old generations" is
/// a number nobody can act on, because the two things you must know before
/// touching any of them — does it carry a grounded anchor, and is anything still
/// writing to it — were both computed by the census and then discarded on the
/// way out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersededGeneration {
    pub panel_version: u32,
    pub records: usize,
    /// Records at **this** generation carrying at least one grounded anchor.
    ///
    /// The load-bearing number for ask 3. A constellation is derived state — it
    /// is re-measured from its source row by a frozen pipeline — but an anchor
    /// is an observed real outcome with a source and a confidence, and nothing
    /// regenerates it. A superseded record with an anchor is therefore the only
    /// copy of something, and is sacred; a superseded record without one holds
    /// nothing its source row cannot produce again.
    pub grounded_records: usize,
    pub earliest_created_at_ms: Option<u64>,
    pub latest_created_at_ms: Option<u64>,
    /// True when no record at this generation is newer than the oldest record at
    /// the active generation — i.e. nothing has written here since the active
    /// generation opened.
    ///
    /// An *open* superseded generation is a defect, not a retention question:
    /// it means some write path is still measuring at a version the intelligence
    /// surfaces do not read, so those records are being stranded as they are
    /// created. That is the #1927 failure reappearing at the write path instead
    /// of in the history, and it must be fixed before any reclaim is considered
    /// — reclaiming from a generation something is still filling is a loop.
    ///
    /// `false` when either timestamp is missing, because unknown is not closed.
    pub closed: bool,
}

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
    /// Each superseded generation actually present (#1927 ask 3).
    pub superseded_versions_present: Vec<SupersededGeneration>,
    /// Of [`Self::superseded_records`], how many carry a grounded anchor.
    ///
    /// Sacred: an anchor is not regenerable from the source row. Non-zero here
    /// means a reclaim would destroy grounded intelligence unless the anchors
    /// were carried on to the active generation first.
    pub superseded_grounded_records: usize,
    /// Upper bound on superseded records that hold nothing their source row
    /// could not produce again — an **upper bound, not a delete list**. See
    /// [`PanelCoverageReport::superseded_reclaim_candidates`] for why this
    /// cannot be tightened by a census.
    pub superseded_reclaim_candidates: usize,
    /// Active-generation constellations whose source row no longer exists,
    /// because an audit TTL expired it.
    ///
    /// `active_version_records - source_cf_rows` on a panel where that is
    /// positive. These are **sacred and permanent**: nothing can re-measure a
    /// row that is gone, so the constellation is the only surviving record of
    /// the observation. This is the second of ask 3's two decisions, and its
    /// answer is "keep, forever" — the growth it implies is bounded by the
    /// source CF's TTL policy, not by deleting the measurement.
    pub orphaned_records: usize,
    /// Of [`Self::orphaned_records`], those whose source CF is declared
    /// TTL-managed, so the absence is the retention policy working (#1940).
    /// Expected, and **not** a finding.
    pub orphaned_source_evicted: usize,
    /// Of [`Self::orphaned_records`], those whose source CF has no TTL. A real
    /// integrity finding: the constellation's provenance points at a row that
    /// was never written or was destroyed outside the retention path (#1940).
    pub orphaned_source_missing: usize,
    /// Active-generation records carrying no source provenance at all, so no
    /// probe is possible for them. Reported apart from both other classes,
    /// because "cannot ask" is not "answered no" (#1940).
    pub unattributed_records: usize,
    /// The same probe over this panel's **superseded** generations (#1940).
    ///
    /// Reported because on this vault it is where every real orphan lives:
    /// measured 2026-08-01, the active generations held zero and the superseded
    /// ones held all 227. A census that reported only the active number would
    /// be correct and useless.
    ///
    /// It is also the missing half of the #1927 ask 3 reclaim rule. That rule's
    /// condition 3 — "its own source row still exists" — was documented as
    /// unanswerable by a census; it is answerable now, and a superseded record
    /// counted here is one no re-measure can rebuild, so it is **not**
    /// reclaimable however ungrounded and closed its generation is.
    pub superseded_orphaned_records: usize,
    /// Of those, the ones on a source CF with no TTL — an integrity finding on
    /// a superseded generation (#1940).
    pub superseded_orphaned_source_missing: usize,
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
    /// Of [`Self::superseded_records_total`], how many carry a grounded anchor.
    ///
    /// # The ask 3 decision, stated
    ///
    /// **Superseded `Base` rows are never auto-deleted.** Not "not yet" — the
    /// census cannot establish the precondition that would make an automatic
    /// delete safe, and a mechanism that deletes sacred data on an assumption is
    /// worse than 41,237 stranded rows.
    ///
    /// The rule a *deliberate* reclaim must satisfy, per record:
    ///
    /// 1. its generation is [`SupersededGeneration::closed`] — nothing is still
    ///    writing there;
    /// 2. it carries **no grounded anchor** — an anchor is an observed outcome
    ///    with a source and a confidence, and no re-measure regenerates it;
    /// 3. its own source row still exists in the panel's `backfill_source_cf` —
    ///    without it the record is not derived state, it is the last copy;
    /// 4. the same input is already measured at the active generation.
    ///
    /// Conditions 1, 2 and 4 are answerable from this report. **Condition 3 is
    /// not**, and that is why the count below its sibling is named
    /// `superseded_reclaim_candidates` rather than `reclaimable`. Measured on
    /// the live vault 2026-07-31: `syn-agent-transcript-v1` holds 40,501
    /// superseded records across two generations against a `CF_AGENT_TRANSCRIPTS`
    /// holding 25,573 rows, so some superseded records certainly correspond to
    /// source rows that no longer exist. Which ones cannot be known without
    /// looking up each record's source key — a per-record probe, not a count.
    ///
    /// So the reclaim path, if one is ever built, must confirm condition 3 for
    /// each record at the moment it acts. This report's job is to make the
    /// decision *sizable*, and to make the two sacred classes — grounded
    /// superseded records, and [`Self::orphaned_records_total`] — impossible to
    /// mistake for reclaimable ones.
    pub superseded_grounded_records_total: usize,
    /// Upper bound on the ask 3 reclaim set: superseded, ungrounded, on a closed
    /// generation, on a panel that has a re-measure path and whose active
    /// generation already covers its source CF.
    ///
    /// An **upper bound**. Condition 3 above is unproven for every record in it.
    pub superseded_reclaim_candidates: usize,
    /// Constellations whose own source row is gone, established by a per-record
    /// source-key probe rather than a subtraction (#1940). Sacred and permanent
    /// either way — see [`PanelCoverageRow::orphaned_records`].
    pub orphaned_records_total: usize,
    /// Of those, the ones whose source CF is declared TTL-managed: expected,
    /// and not a finding (#1940).
    pub orphaned_source_evicted_total: usize,
    /// Of those, the ones whose source CF has no TTL. **Non-zero is a real
    /// integrity finding** — a constellation whose provenance points at a row
    /// that was never written or was destroyed outside the retention path
    /// (#1940).
    pub orphaned_source_missing_total: usize,
    /// Active-generation records carrying no source provenance, so the probe
    /// cannot be attempted. Zero on the live vault 2026-08-01 (#1940).
    pub unattributed_records_total: usize,
    /// Records on **superseded** generations whose own source row is gone
    /// (#1940). On this vault that is where every real orphan lives, and each
    /// one is a record no re-measure can rebuild — so it fails the #1927 ask 3
    /// reclaim rule's condition 3 whatever else holds of it.
    pub superseded_orphaned_records_total: usize,
    /// Panels with a non-zero [`Self::orphaned_source_missing_total`] — the
    /// finding, as distinct from [`Self::records_exceed_source_panels`], which
    /// is an arithmetic observation that is *expected* on a TTL-managed source
    /// and was therefore permanently red (#1940).
    pub orphaned_source_missing_panels: Vec<String>,
    /// Superseded generations that are still being WRITTEN TO — the defect case.
    ///
    /// Named `panel@version` so a log line identifies the write path to fix.
    /// Empty is the healthy state; non-empty means records are being stranded as
    /// they are created, which is #1927's failure moved from the history to the
    /// live path.
    pub open_superseded_generations: Vec<String>,
    pub coverage_floor: f32,
    pub grounding_floor: f32,
    /// Full-CF panels below the coverage floor. Names, so a log line is readable.
    pub coverage_deficient_panels: Vec<String>,
    /// Coverage-deficient panels with NO re-measure path: the maintainer cannot
    /// fix these and says so rather than reporting a clean pass.
    pub unbackfillable_deficient_panels: Vec<String>,
    /// Outcome-bearing panels below the grounding floor.
    pub grounding_deficient_panels: Vec<String>,
    /// **#1962.** Outcome-bearing panels holding records but carrying **zero**
    /// anchor kinds — a strict subset of `grounding_deficient_panels`, and a
    /// categorically different state from the rest of it.
    ///
    /// A panel at 0.30 coverage has an outcome axis and not enough of it; a
    /// panel at 0.00 with no kinds has no outcome axis at all, so results over
    /// it are undefined rather than provisional. A single deficient count could
    /// not distinguish them, which is how the *active* panel sat with zero
    /// anchors of any kind while health reported a two-panel deficiency that
    /// read as thin coverage.
    ///
    /// Named `panel@version`, and the active generation is marked, because "one
    /// of the deficient panels is the one every operator-facing surface reads"
    /// is the operationally load-bearing half of the fact.
    pub no_outcome_axis_panels: Vec<String>,
    /// Panels holding more constellations at the active generation than their
    /// source CF holds rows.
    ///
    /// **An arithmetic observation, not a finding (#1940).** It is the expected
    /// steady state of any panel over a TTL-managed source: constellations are
    /// never auto-deleted, source rows are, so the panel legitimately outgrows
    /// its own source. Read it for the unbounded-growth question it answers,
    /// and read [`Self::orphaned_source_missing_panels`] for the integrity
    /// question. Treating this list as an anomaly is what made it permanently
    /// red on `syn-action-v1` and `syn-process-v1`, and a permanently-red field
    /// is a broken instrument.
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

/// One generation's orphan probe result: what happened to each record's own
/// source row (#1940).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OrphanProbe {
    /// Source row absent, and the source CF is declared TTL-managed. Expected:
    /// the retention policy working as designed.
    evicted: usize,
    /// Source row absent on a source CF with no TTL. A real integrity finding —
    /// the constellation's provenance points at a row that was never written or
    /// was destroyed outside the retention path.
    missing: usize,
    /// Records carrying no source provenance, so the probe cannot be attempted.
    unattributed: usize,
}

/// Probes each of a generation's records against the physical keys its source
/// CF holds right now.
///
/// This is the membership test the old subtraction could not perform. A record
/// is orphaned iff *its own* declared source key is absent from the source CF —
/// not iff the generation happens to hold more records than the CF holds rows.
///
/// Absent source-CF key sets mean the CF was not read (a subset-fed or derived
/// panel has no full-CF denominator, so its keys are not loaded). Those records
/// are neither covered nor orphaned here; they are simply not probed, and
/// counting them as orphans would invent 30,000 findings on `CF_KV` alone.
fn probe_orphans(
    active: Option<&synapse_calyx::SynapseCalyxPanelCensusEntry>,
    source_cf_keys: &BTreeMap<String, BTreeSet<String>>,
    source_ttl_managed: bool,
) -> OrphanProbe {
    let Some(active) = active else {
        return OrphanProbe::default();
    };
    let mut probe = OrphanProbe {
        unattributed: active.unattributed_records,
        ..OrphanProbe::default()
    };
    for (source_cf, keys) in &active.source_key_hexes {
        let Some(present) = source_cf_keys.get(source_cf) else {
            continue;
        };
        let absent = keys.iter().filter(|key| !present.contains(*key)).count();
        if source_ttl_managed {
            probe.evicted += absent;
        } else {
            probe.missing += absent;
        }
    }
    probe
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
    source_cf_keys: &BTreeMap<String, BTreeSet<String>>,
) -> PanelCoverageReport {
    let catalog = builtin_panel_catalog();
    let mut panels = Vec::with_capacity(catalog.len());
    let mut claimed_versions: Vec<u32> = Vec::new();
    let mut superseded_records_total = 0usize;
    let mut superseded_grounded_records_total = 0usize;
    let mut superseded_reclaim_candidates_total = 0usize;
    let mut orphaned_records_total = 0usize;
    let mut orphaned_source_evicted_total = 0usize;
    let mut orphaned_source_missing_total = 0usize;
    let mut unattributed_records_total = 0usize;
    let mut superseded_orphaned_records_total = 0usize;
    let mut open_superseded_generations: Vec<String> = Vec::new();
    let mut coverage_deficient_panels = Vec::new();
    let mut unbackfillable_deficient_panels = Vec::new();
    let mut grounding_deficient_panels = Vec::new();
    let mut no_outcome_axis_panels = Vec::new();
    let mut records_exceed_source_panels = Vec::new();
    let mut orphaned_source_missing_panels = Vec::new();

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

        // A superseded generation is "closed" relative to the ACTIVE
        // generation's oldest record: nothing written here since the active one
        // opened. Missing on either side means unknown, and unknown is not
        // closed — a generation whose timestamps the census could not read must
        // never be reported as safe to reason about.
        let active_earliest = active.and_then(|row| row.earliest_created_at_ms);
        let superseded_versions_present: Vec<SupersededGeneration> = entry
            .superseded_versions
            .iter()
            .filter_map(|version| {
                census.entry(*version).map(|row| SupersededGeneration {
                    panel_version: *version,
                    records: row.records,
                    grounded_records: row.grounded_records,
                    earliest_created_at_ms: row.earliest_created_at_ms,
                    latest_created_at_ms: row.latest_created_at_ms,
                    closed: match (row.latest_created_at_ms, active_earliest) {
                        (Some(latest), Some(active_oldest)) => latest <= active_oldest,
                        _ => false,
                    },
                })
            })
            .collect();
        let superseded_records: usize = superseded_versions_present
            .iter()
            .map(|generation| generation.records)
            .sum();
        let superseded_grounded_records: usize = superseded_versions_present
            .iter()
            .map(|generation| generation.grounded_records)
            .sum();
        superseded_records_total += superseded_records;
        superseded_grounded_records_total += superseded_grounded_records;
        for generation in &superseded_versions_present {
            if !generation.closed && generation.records > 0 {
                open_superseded_generations
                    .push(format!("{}@{}", entry.panel_name, generation.panel_version));
            }
        }

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

        // #1940: the orphan count is a per-record probe, not a subtraction.
        //
        // It used to be `active_version_records - source_cf_rows`, justified by
        // "on a full-CF panel the active generation holds one constellation per
        // source row". That premise is false whenever a superseded generation
        // holds records measured from rows still in the CF, and false again
        // whenever the CF holds rows nobody has measured yet — so the
        // difference is taken between two populations that are not the same
        // set. Measured on the live vault 2026-08-01: it reported 667 orphans
        // where a per-record source-key probe finds 227.
        //
        // Worse than the magnitude, the subtraction could not answer the
        // question the counter exists to raise. "Was the source row evicted, or
        // did it never exist?" is a membership test on a specific key, and no
        // difference of two counts can perform one.
        let probe = probe_orphans(active, source_cf_keys, entry.source_ttl_managed);
        // The same probe over every superseded generation. Without it the
        // census would report zero orphans and be *right about the active
        // generation while hiding every real one*: measured 2026-08-01, all 227
        // constellations on this vault whose source row is genuinely gone sit
        // on superseded generations, and none at an active one. Reporting only
        // the active number would move #1940's blind spot rather than remove
        // it.
        let superseded_probe = entry
            .superseded_versions
            .iter()
            .map(|version| {
                probe_orphans(
                    census.entry(*version),
                    source_cf_keys,
                    entry.source_ttl_managed,
                )
            })
            .fold(OrphanProbe::default(), |mut total, part| {
                total.evicted += part.evicted;
                total.missing += part.missing;
                total.unattributed += part.unattributed;
                total
            });
        let orphaned_records = probe.evicted + probe.missing;
        let superseded_orphaned_records = superseded_probe.evicted + superseded_probe.missing;
        orphaned_records_total += orphaned_records;
        orphaned_source_evicted_total += probe.evicted + superseded_probe.evicted;
        orphaned_source_missing_total += probe.missing + superseded_probe.missing;
        unattributed_records_total += probe.unattributed + superseded_probe.unattributed;
        superseded_orphaned_records_total += superseded_orphaned_records;
        if probe.missing > 0 || superseded_probe.missing > 0 {
            orphaned_source_missing_panels.push(entry.panel_name.to_owned());
        }
        let coverage_below_floor =
            coverage_fraction.is_some_and(|fraction| fraction < SYN_PANEL_COVERAGE_FLOOR);
        let grounding_below_floor =
            entry.outcome_bearing && grounded_fraction < SYNAPSE_GROUNDING_COVERAGE_FLOOR;

        // Ask 3's upper bound. Every clause is a precondition that can be
        // checked from counts; the one that cannot — "this record's own source
        // row still exists" — is deliberately absent, which is why this is a
        // candidate count and not a delete list.
        //
        // `!coverage_below_floor` matters as much as the rest: reclaiming from
        // an old generation while the active one has not yet covered the corpus
        // would delete the only measurement of records the backfill has not
        // reached. Ordering, not preference.
        let panel_reclaimable = entry.backfill_source_cf.is_some() && !coverage_below_floor;
        let superseded_reclaim_candidates: usize = if panel_reclaimable {
            superseded_versions_present
                .iter()
                .filter(|generation| generation.closed)
                .map(|generation| {
                    generation
                        .records
                        .saturating_sub(generation.grounded_records)
                })
                .sum()
        } else {
            0
        };
        superseded_reclaim_candidates_total += superseded_reclaim_candidates;

        if coverage_below_floor {
            coverage_deficient_panels.push(entry.panel_name.to_owned());
            if entry.backfill_source_cf.is_none() {
                unbackfillable_deficient_panels.push(entry.panel_name.to_owned());
            }
        }
        if grounding_below_floor {
            grounding_deficient_panels.push(entry.panel_name.to_owned());
            // #1962: zero kinds is not thin coverage. `active_version_records > 0`
            // matters — a panel with no records has not demonstrated anything
            // about its outcome axis, and calling that "no outcome axis" would
            // turn an empty generation into a finding.
            if active_version_records > 0 && anchor_kind_records.is_empty() {
                no_outcome_axis_panels.push(format!(
                    "{}@{} (active, {} records, 0 anchor kinds)",
                    entry.panel_name, entry.panel_version, active_version_records
                ));
            }
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
            superseded_grounded_records,
            superseded_reclaim_candidates,
            orphaned_records,
            orphaned_source_evicted: probe.evicted,
            orphaned_source_missing: probe.missing,
            unattributed_records: probe.unattributed,
            superseded_orphaned_records,
            superseded_orphaned_source_missing: superseded_probe.missing,
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
    // Its grounded records belong in the sacred total for the same reason, and
    // it never contributes reclaim candidates — a generation no catalog entry
    // claims has no known source CF and no known active counterpart, so none of
    // the four reclaim conditions can even be evaluated for it.
    superseded_records_total += unknown_panel_versions
        .iter()
        .map(|(_, records)| *records)
        .sum::<usize>();
    superseded_grounded_records_total += census
        .entries
        .iter()
        .filter(|row| !claimed_versions.contains(&row.panel_version))
        .map(|row| row.grounded_records)
        .sum::<usize>();

    PanelCoverageReport {
        panels,
        unknown_panel_versions,
        base_cf_rows: census.base_cf_rows,
        records_total: census.records_total(),
        decode_failures: census.decode_failures,
        first_decode_failure: census.first_decode_failure.clone(),
        superseded_records_total,
        superseded_grounded_records_total,
        superseded_reclaim_candidates: superseded_reclaim_candidates_total,
        orphaned_records_total,
        orphaned_source_evicted_total,
        orphaned_source_missing_total,
        unattributed_records_total,
        superseded_orphaned_records_total,
        orphaned_source_missing_panels,
        open_superseded_generations,
        coverage_floor: SYN_PANEL_COVERAGE_FLOOR,
        grounding_floor: SYNAPSE_GROUNDING_COVERAGE_FLOOR,
        coverage_deficient_panels,
        unbackfillable_deficient_panels,
        grounding_deficient_panels,
        no_outcome_axis_panels,
        records_exceed_source_panels,
        measured_at_unix_ms: census.measured_at_unix_ms,
    }
}
