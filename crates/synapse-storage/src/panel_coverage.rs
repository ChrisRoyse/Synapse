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
//! # Anchor debt is a work queue, not a scan (#1984)
//!
//! The census does not only *count* stranded anchors — it names them. Every
//! stranded row is already identified here by `(source CF, source key, the
//! superseded generation holding its anchors)`, and
//! [`PanelCoverageRow::anchors_stranded_identities`] carries that list out
//! instead of discarding it behind the count.
//!
//! The count alone forced the repair to be scan-shaped: "this panel owes 2,120
//! anchors" says nothing about *which* rows, so the only available repair was to
//! re-measure the entire source CF under a wall-clock budget and hope the
//! stranded rows fell inside it. Measured on the deployed daemon 2026-08-06 that
//! is what happened — 44 pages, 60,089 ms, `inserted_rows=0`, and the five panel
//! debts unchanged, tick after tick. Work proportional to the corpus (939,605
//! `Base` rows) cannot converge on a debt of 2,879 rows.
//!
//! The identity list is the same thing Postgres' visibility map is to VACUUM: a
//! record of *where the work is*, so the repair pays for the debt rather than
//! for the corpus. It is bounded by [`SYN_ANCHOR_DEBT_IDENTITY_CAP`] per panel
//! and reports [`PanelCoverageRow::anchors_stranded_identities_truncated`] when
//! it binds, because a silently shortened work queue would be a repair that
//! reports completion having skipped rows.
//!
//! Two neighbouring populations are counted apart and never folded in, because
//! each has the opposite remedy from actionable debt:
//!
//! * [`PanelCoverageRow::anchors_stranded_source_absent`] — the anchor's own
//!   source row is gone, so no re-measure can rebuild the record it must be
//!   written against. Sacred superseded history (#2021), never replay debt.
//! * [`PanelCoverageRow::anchors_stranded_source_cf_unmeasured`] — the census
//!   did not read that source CF at all, so nothing is known. Unknown is not
//!   zero, and it is reported as its own number rather than as completion.
//!
//! # Two authorities declare a generation, not one (#2062)
//!
//! [`builtin_panel_catalog`] is a compile-time table, so it structurally cannot
//! name a generation minted at runtime. The vault-global panel generation
//! allocator can and does: its Registry-CF `owners` map claims every generation
//! anything ever wrote to, as `builtin:<panel>` for a reserved one and
//! `dynamic:<panel>:<operation_id>` for an allocated one.
//!
//! Joining the census against the catalog alone therefore produced a **half
//! false** red light. The live daemon reported 98 generations holding 62,566
//! rows as claimed by nothing, and health held `calyx_panel_coverage` at
//! `error` on that basis. Every one of them was claimed — by
//! `dynamic:syn-graphpos-app-v1:…`, `dynamic:syn-graphpos-process-v1:…` and
//! `dynamic:syn-path-hierarchy-v1:…` — in an authority the census did not read.
//! A permanently-red instrument is a broken instrument, and this one was red for
//! rows that were attributable all along.
//!
//! So the census now reads both, and the finding splits three ways:
//!
//! * claimed by the catalog — an ordinary [`PanelCoverageRow`];
//! * claimed only by the allocator — an
//!   [`PanelCoverageReport::owned_dynamic_generations`] rollup, per owning panel
//!   rather than per generation, because a derived publisher mints one
//!   generation per pass and 98 census rows is a report nobody reads;
//! * claimed by **neither** — [`PanelCoverageReport::unknown_panel_versions`],
//!   which is now the genuine finding it always claimed to be.
//!
//! The retirement ledger travels with the owners map, and it is what makes the
//! rollup a retention statement rather than an inventory: a retired generation
//! has a named successor that is already durable, so it is closed by
//! *declaration* instead of by the timestamp heuristic
//! [`SupersededGeneration::closed`] has to use, and its ungrounded rows join
//! [`PanelCoverageReport::superseded_reclaim_candidates`].
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

/// Exact stranded anchor identities enumerated per panel per census (#1984).
///
/// The census counts every stranded row; this bounds how many it *names*. The
/// bound exists because the report is held in memory for a whole maintenance
/// tick and read by `health`, not because a longer queue would be wrong — at
/// ~80 bytes per hex identity, 4,096 caps one panel's queue at roughly 330 KB.
///
/// Sized against the measured debt rather than guessed: the deployed daemon's
/// worst panel carried 2,120 stranded anchors and the whole vault carried 2,879,
/// so this holds every real identity with headroom. When it does bind,
/// [`PanelCoverageRow::anchors_stranded_identities_truncated`] says so and the
/// repair refuses to call a pass complete — a truncated work queue that reported
/// completion would be a repair claiming to have finished rows it never saw.
pub const SYN_ANCHOR_DEBT_IDENTITY_CAP: usize = 4_096;

/// One anchor stranded by a panel-version bump, named exactly (#1984).
///
/// All three components are load-bearing for the repair:
///
/// * `source_cf` + `source_key_hex` are the authoritative row the active
///   generation must be re-measured from, which is the precondition Calyx puts
///   on writing an anchor at all.
/// * `superseded_panel_version` is the declared generation the anchors are
///   carried *from*. A record id is a content address over `(input_bytes,
///   panel_version, vault_salt)`, so the historical `cx_id` cannot be recomputed
///   from the row's current bytes (#1981/#1982) — the lineage must be declared,
///   not inferred.
///
/// Ordered so a work queue built from it is deterministic across ticks, which is
/// what makes a resume cursor meaningful.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StrandedAnchorIdentity {
    pub source_cf: String,
    pub source_key_hex: String,
    /// The newest superseded generation that still holds this row's anchors.
    pub superseded_panel_version: u32,
}

/// Generations named individually inside one owned-generation rollup (#2062).
///
/// A derived publisher mints one generation per five-minute pass, so the
/// population is unbounded in principle and 98 of them were live when this was
/// written. The rollup names a bounded prefix and says so when it binds, for
/// the same reason [`SYN_ANCHOR_DEBT_IDENTITY_CAP`] does: a list silently
/// shortened is a report that looks complete and is not.
pub const SYN_OWNED_GENERATION_NAME_CAP: usize = 64;

/// The panel generation allocator's ownership authority, as the census join
/// reads it (#2062).
///
/// Both halves are load-bearing and neither substitutes for the other.
/// `owners` answers *who wrote this generation* — the question the catalog
/// could not answer for a runtime-minted id, and the whole reason 62,566
/// attributable rows read as unattributable. `retired` answers *is anything
/// still writing there*, which is what separates the one live derived
/// generation from superseded history that reclaim may consider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelGenerationOwnership {
    /// Generation -> `builtin:<panel_name>` or `dynamic:<panel>:<operation_id>`.
    pub owners: BTreeMap<u32, String>,
    /// Retired generation -> the generation that superseded it.
    pub retired: BTreeMap<u32, u32>,
}

/// How a generation's owner string names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerKind {
    Builtin,
    Dynamic,
}

impl OwnerKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Dynamic => "dynamic",
        }
    }
}

impl PanelGenerationOwnership {
    /// Splits an owner claim into `(kind, panel_name)`.
    ///
    /// Returns `None` for an owner string in neither declared shape. That is
    /// not treated as ownership: an unparseable claim names no panel, so the
    /// generation stays in `unknown_panel_versions` where a human will see it,
    /// rather than being folded into a rollup under a guessed name.
    fn claim(&self, panel_version: u32) -> Option<(OwnerKind, &str)> {
        let owner = self.owners.get(&panel_version)?;
        if let Some(rest) = owner.strip_prefix("dynamic:") {
            let panel_name = rest.split(':').next().filter(|name| !name.is_empty())?;
            return Some((OwnerKind::Dynamic, panel_name));
        }
        owner
            .strip_prefix("builtin:")
            .filter(|name| !name.is_empty())
            .map(|name| (OwnerKind::Builtin, name))
    }
}

/// Every `Base` generation one panel owns through the allocator, rolled up
/// (#2062).
///
/// Per panel rather than per generation on purpose. The three derived
/// publishers mint a generation per pass, so a per-generation report grows
/// without bound and answers the wrong question — what an operator needs to
/// know is how many generations one panel is carrying, how many of them are
/// still live, and how many rows are sitting behind the live one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedGenerationRollup {
    /// Panel name taken from the owner claim, never inferred from the number.
    pub panel_name: String,
    /// `dynamic` for an allocated generation, `builtin` for a reserved one that
    /// the catalog nonetheless does not name.
    pub owner_kind: String,
    pub generations_present: usize,
    /// Generations with no recorded successor. **Healthy is exactly one.**
    /// More than one means a publisher minted without retiring its
    /// predecessor, which is the #2062 leak still open.
    pub live_generations: Vec<u32>,
    /// Generations with a durably recorded successor, bounded by
    /// [`SYN_OWNED_GENERATION_NAME_CAP`].
    pub retired_generations: Vec<u32>,
    pub retired_generations_truncated: bool,
    pub newest_generation: u32,
    pub records: usize,
    pub live_records: usize,
    pub retired_records: usize,
    pub grounded_records: usize,
    /// Grounded records on retired generations. Sacred: an anchor is not
    /// regenerated by re-running the derived pass, so these are excluded from
    /// the reclaim candidates below exactly as declared superseded ones are.
    pub retired_grounded_records: usize,
}

/// One panel's coverage and grounding row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the public coverage row preserves independent catalog, denominator, lifecycle, grounding, and reclaim facts"
)]
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
    /// Grounded records a superseded generation holds that the active one does
    /// not and whose source row still exists — replayable anchors **stranded by
    /// a panel-version bump** (#1980, #2021).
    ///
    /// Exact source-row identities grounded on a superseded generation but not
    /// on the active generation. A record id is
    /// a content address over `(input_bytes, panel_version, vault_salt)` and an
    /// anchor is keyed by the `cx_id` it was written against, so a bump re-keys
    /// every record and orphans every anchor on the previous generation. The
    /// re-measured corpus is then born ungrounded.
    ///
    /// This is the difference between "this panel was never anchored" and "this
    /// panel WAS anchored and the bump lost it" — two states with opposite
    /// remedies, and until this field existed they produced the identical
    /// report. `syn-episode-v1` sat at 171 stranded and read as an ordinary
    /// grounding gap.
    ///
    /// Zero once the backfill's carry-forward has run over every surviving
    /// source row. Anchors whose TTL-managed source has expired remain sacred
    /// superseded history, but are not actionable backfill debt.
    pub anchors_stranded_on_superseded: usize,
    /// The exact rows behind that count, as the repair's work queue (#1984).
    ///
    /// Bounded by [`SYN_ANCHOR_DEBT_IDENTITY_CAP`] and sorted, so a driver can
    /// walk it with a resume cursor and make monotonic progress across ticks
    /// instead of restarting a corpus-wide scan every time. Empty whenever
    /// `anchors_stranded_on_superseded` is zero.
    pub anchors_stranded_identities: Vec<StrandedAnchorIdentity>,
    /// True when the enumeration above hit the cap, so it names fewer rows than
    /// the count reports. Never silent: a repair that consumed a truncated queue
    /// and called the pass complete would have skipped rows nothing recorded.
    pub anchors_stranded_identities_truncated: bool,
    /// Grounded superseded identities the active generation does not hold and
    /// whose own source row is **gone** (#2021).
    ///
    /// Not debt and never repairable: an anchor is written against the
    /// constellation of its source row, and no re-measure rebuilds a row that no
    /// longer exists. Counting these as debt is what made an exhaustive sweep
    /// report `backfill_owed=true` forever. Counted here so "irrecoverable" is a
    /// reported number rather than the invisible difference between two other
    /// numbers.
    pub anchors_stranded_source_absent: usize,
    /// Grounded superseded identities on a source CF the census did not read.
    ///
    /// Unknown, not zero. A missing CF census proves nothing about whether those
    /// anchors are stranded, and manufacturing completion from an absent
    /// measurement is the failure this whole readback exists to remove.
    pub anchors_stranded_source_cf_unmeasured: usize,
    /// Grounded superseded records with no source identity. They cannot be
    /// compared across generations and are reported as unknown, never silently
    /// treated as either stranded or carried.
    pub anchors_stranding_identity_unknown: usize,
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
    pub const fn backfill_owed(&self) -> bool {
        (self.coverage_below_floor || self.anchors_stranded_on_superseded > 0)
            && self.backfill_source_cf.is_some()
    }

    /// True when this panel's active generation is short of its source CF and a
    /// re-measure path exists (#1984).
    ///
    /// Split out from [`Self::backfill_owed`] because the two debts are repaired
    /// by different mechanisms and must not share a scheduling slot: coverage
    /// debt is repaired by a paged sweep whose cost is the corpus, anchor debt
    /// by a work queue whose cost is the debt. Selecting one target for both is
    /// what let three unrepairable outcome anchors deny 208,496 rows of coverage
    /// backfill for the life of a process (#2061).
    #[must_use]
    pub const fn coverage_backfill_owed(&self) -> bool {
        self.coverage_below_floor && self.backfill_source_cf.is_some()
    }

    /// True when this panel carries exact stranded anchors AND a path exists to
    /// re-anchor them (#1984).
    #[must_use]
    pub const fn anchor_debt_owed(&self) -> bool {
        self.anchors_stranded_on_superseded > 0 && self.backfill_source_cf.is_some()
    }

    /// True when this panel carries exact stranded anchors and **no** path
    /// exists to re-anchor them.
    ///
    /// Reported rather than dropped. `syn-graphpos-app-v1` sat at 59 stranded
    /// anchors with `backfill_source_cf: None` on the deployed daemon: nothing
    /// can repair it, and reporting that as "nothing owed" is the silent-success
    /// failure this whole readback exists to remove.
    #[must_use]
    pub const fn anchor_debt_unbackfillable(&self) -> bool {
        self.anchors_stranded_on_superseded > 0 && self.backfill_source_cf.is_none()
    }

    /// Why the maintainer must sweep this panel. Coverage and anchor debt are
    /// independent: a completed version backfill is exactly when coverage is
    /// 1.0 while historical anchors may still be stranded.
    #[must_use]
    pub const fn backfill_reason(&self) -> Option<&'static str> {
        match (
            self.coverage_below_floor,
            self.anchors_stranded_on_superseded > 0,
        ) {
            (true, true) => Some("coverage_and_anchor_debt"),
            (true, false) => Some("coverage_debt"),
            (false, true) => Some("anchor_debt"),
            (false, false) => None,
        }
    }
}

/// Whole-vault panel coverage and grounding report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelCoverageReport {
    /// One row per declared panel, in catalog order.
    pub panels: Vec<PanelCoverageRow>,
    /// Panel versions physically present in `Base` that **neither** authority
    /// claims — not the catalog, and not the allocator's owners map (#2062).
    ///
    /// Reported rather than dropped: an unclaimed generation is exactly the
    /// kind of thing that strands records invisibly. Non-empty is a genuine
    /// finding now, which it was not while runtime-minted generations landed
    /// here purely because the census read one of the two authorities.
    pub unknown_panel_versions: Vec<(u32, usize)>,
    /// Generations the allocator owns that the catalog does not name, rolled up
    /// per owning panel (#2062).
    ///
    /// This is where the three derived-snapshot publishers' generations are
    /// attributed. A rollup carrying more than one entry in
    /// [`OwnedGenerationRollup::live_generations`] is the leak still open, and
    /// is named again in [`Self::dynamic_panels_multi_live`].
    pub owned_dynamic_generations: Vec<OwnedGenerationRollup>,
    /// Dynamic panels holding more than one live generation with records — a
    /// publisher that minted without retiring its predecessor (#2062).
    ///
    /// Named `panel (n live: …)` so a log line identifies the write path.
    /// Empty is the healthy state.
    pub dynamic_panels_multi_live: Vec<String>,
    /// Generations reserved as `builtin:<panel>` that no catalog entry names,
    /// with their record counts (#2062).
    ///
    /// Attributed, so not an unclaimed-generation finding — but a declaration
    /// gap of its own: the boot reservation list and
    /// [`builtin_panel_catalog`] are separately maintained, so a version can be
    /// reserved and then not appear in any panel's `panel_version` or
    /// `superseded_versions`. Reported apart rather than folded into either
    /// neighbouring list, because attributing it silently would replace one
    /// blind spot with another.
    pub reserved_generations_absent_from_catalog: Vec<(u32, usize)>,
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
    /// Panels whose anchors were stranded on a superseded generation (#1980).
    ///
    /// A finding, not an observation: grounding is the axis every bits,
    /// sufficiency and kernel result is defined against, so a panel that lost
    /// its anchors to a version bump has silently become unmeasurable while
    /// every coverage number stays green. `syn-episode-v1` reached the active
    /// generation with 171 of 171 records re-measured, coverage 1.0, and 0 of
    /// the 171 anchors it had the generation before.
    ///
    /// Named `panel@active_version` with the stranded count and the generation
    /// holding them, because the remedy — drive the backfill so the carry-
    /// forward runs — needs to know which generation to carry FROM.
    pub anchors_stranded_panels: Vec<String>,
    /// Panels carrying stranded anchors that **no** re-measure path can repair
    /// (#1984), named `panel@version` with the count and the generation holding
    /// them.
    ///
    /// A strict subset of [`Self::anchors_stranded_panels`] and the half of it
    /// the maintainer can do nothing about, so the two are never conflated: a
    /// repair that reported "nothing owed" because the only owed panel was
    /// unrepairable would be indistinguishable from a repair that had finished.
    pub anchor_debt_unbackfillable_panels: Vec<String>,
    /// Whole-vault stranded anchor total across every panel (#1984). The number
    /// an unattended repair must drive to zero.
    pub anchors_stranded_total: usize,
    /// Stranded anchors whose own source row is gone, so no re-measure can put
    /// the record back under them (#2021). Sacred history, never repair debt —
    /// and reported so "irrecoverable" never has to be inferred.
    pub anchors_stranded_source_absent_total: usize,
    /// Stranded-anchor candidates on a source CF the census did not read.
    /// Unknown, and reported as unknown rather than as zero.
    pub anchors_stranded_source_cf_unmeasured_total: usize,
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
    /// Catalog lineage entries the generation allocator attributes to a
    /// **different** panel (#2093).
    ///
    /// Two authorities describe who wrote a generation: `builtin_panel_catalog`,
    /// a compile-time constant a human edits, and the allocator's owners map, a
    /// durable row the reserving panel wrote when the generation was its active
    /// one. They can only disagree by a declaration error, and when they do the
    /// census believes the catalog — so the error is silent everywhere it
    /// matters. `syn-graphpos-app-v1` carried `syn-agent-event-v1`'s retired
    /// `1_665_001` for four months and 59 replayable anchors were reported
    /// unrepairable the whole time, because the panel they had been reassigned
    /// to declares no re-measure path.
    ///
    /// **Empty is the healthy state**, and empty is also what a vault too young
    /// to hold the generation reports: an absent owner claim is not evidence.
    pub catalog_lineage_misattributed: Vec<String>,
    /// Generations the allocator holds live — owned, and with no recorded
    /// successor (#2081 ask 4).
    pub allocator_live_count: usize,
    /// Of those, the ones the physical `Base` census can see, i.e. that hold at
    /// least one row.
    ///
    /// Always below [`Self::allocator_live_count`], and that gap is **not** by
    /// itself a finding: every catalog generation is reserved at vault open so
    /// no writer can name an unowned one, and the three derived-snapshot panels
    /// never write at their reserved base at all. The actionable half of the gap
    /// is [`Self::allocator_live_dynamic_without_records`].
    pub census_live_count: usize,
    /// The actionable half of that gap: **dynamic** live owner claims with no
    /// `Base` row behind them, bounded by [`SYN_OWNED_GENERATION_NAME_CAP`].
    ///
    /// A successful derived publish always commits at least one constellation,
    /// so a live dynamic claim the census cannot see never published — it is one
    /// permanently consumed owner row out of a bounded `MAX_OWNERS = 10_000`,
    /// minted by the pre-#2081 ordering that allocated before the spectral pass
    /// that fails. Empty on a vault whose publishes have all either committed or
    /// failed before allocating.
    ///
    /// Reported and never auto-retired — see the derivation comment in
    /// `build_panel_coverage_report` for why a zero-row reading cannot
    /// distinguish a failed publish from one that has allocated and not yet
    /// committed.
    pub allocator_live_dynamic_without_records: Vec<String>,
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
                    usize::from(panel.anchors_stranded_on_superseded > 0),
                    panel.anchors_stranded_on_superseded,
                    panel.uncovered_rows().unwrap_or(0),
                    u32::MAX - panel.panel_version,
                )
            })
    }

    /// The panel most owed a **coverage** sweep, by absolute uncovered rows
    /// (#1984, #2061).
    ///
    /// Anchor debt is deliberately not a tiebreak here. It was, and a panel with
    /// three unrepairable anchors therefore outranked a panel with 174,993
    /// uncovered rows on every tick — the maintainer selected the tiny debt,
    /// refused it, and returned before the large one was ever considered. The
    /// two debts now have separate schedulers, so this one answers only the
    /// question it can act on: where does a page of sweeping remove the most
    /// unmeasured rows.
    #[must_use]
    pub fn most_owed_coverage_backfill(&self) -> Option<&PanelCoverageRow> {
        self.panels
            .iter()
            .filter(|panel| panel.coverage_backfill_owed())
            .max_by_key(|panel| {
                (
                    panel.uncovered_rows().unwrap_or(0),
                    u32::MAX - panel.panel_version,
                )
            })
    }

    /// Every panel owed a coverage sweep, most uncovered rows first (#2070).
    ///
    /// A list rather than a single target, for the third time and the same
    /// reason. `most_owed_coverage_backfill` names exactly one panel per tick,
    /// and when that panel's first page cannot be measured the tick ends there:
    /// on the deployed daemon `syn-timeline-v1` (174,993 uncovered) failed on
    /// page 1 with `CALYX_ASTER_PANEL_SLOT_SET_IMMUTABLE` on four consecutive
    /// ticks, so `syn-process-v1` (991) moved **zero** rows and
    /// `syn-agent-event-v1` went backwards on live ingest. #2030 fixed this
    /// shape for one unmeasurable record and #2061 for one unrepairable anchor
    /// identity; this is the same shape for one unmeasurable page.
    ///
    /// Ties break on the lower panel version so the order is deterministic
    /// across ticks, which is what makes a resume cursor meaningful.
    #[must_use]
    pub fn coverage_backfill_targets(&self) -> Vec<&PanelCoverageRow> {
        let mut targets: Vec<&PanelCoverageRow> = self
            .panels
            .iter()
            .filter(|panel| panel.coverage_backfill_owed())
            .collect();
        targets.sort_by_key(|panel| {
            (
                std::cmp::Reverse(panel.uncovered_rows().unwrap_or(0)),
                panel.panel_version,
            )
        });
        targets
    }

    /// Every panel owed an anchor-debt repair, largest debt first (#1984).
    ///
    /// A list rather than a single target on purpose. One target per tick is
    /// what made an unrepairable head-of-queue item a denial of service for
    /// every other panel (#2061, and #2030 before it with a different cause),
    /// and identity-driven repair costs the debt rather than the corpus — so
    /// every debt-bearing panel fits in one tick and none has to wait behind
    /// another.
    ///
    /// Ties break on the lower panel version so the order is deterministic
    /// across ticks, which is what makes a resume cursor meaningful.
    #[must_use]
    pub fn anchor_debt_targets(&self) -> Vec<&PanelCoverageRow> {
        let mut targets: Vec<&PanelCoverageRow> = self
            .panels
            .iter()
            .filter(|panel| panel.anchor_debt_owed())
            .collect();
        targets.sort_by_key(|panel| {
            (
                std::cmp::Reverse(panel.anchors_stranded_on_superseded),
                panel.panel_version,
            )
        });
        targets
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
#[expect(
    clippy::too_many_lines,
    reason = "one catalog/census join must retain all generation ownership, grounding, denominator, and reclaim accounting in one report"
)]
pub fn build_panel_coverage_report(
    census: &SynapseCalyxPanelCensus,
    source_cf_rows: &BTreeMap<String, u64>,
    source_cf_keys: &BTreeMap<String, BTreeSet<String>>,
    ownership: &PanelGenerationOwnership,
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
    let mut anchors_stranded_panels: Vec<String> = Vec::new();
    let mut anchor_debt_unbackfillable_panels: Vec<String> = Vec::new();
    let mut anchors_stranded_total = 0usize;
    let mut anchors_stranded_source_absent_total = 0usize;
    let mut anchors_stranded_source_cf_unmeasured_total = 0usize;
    let mut records_exceed_source_panels = Vec::new();
    let mut orphaned_source_missing_panels = Vec::new();

    for entry in &catalog {
        claimed_versions.push(entry.panel_version);
        claimed_versions.extend_from_slice(entry.superseded_versions);

        let active = census.entry(entry.panel_version);
        let active_version_records = active.map_or(0, |row| row.records);
        let grounded_records = active.map_or(0, |row| row.grounded_records);
        let grounded_fraction = active.map_or(
            0.0,
            synapse_calyx::SynapseCalyxPanelCensusEntry::grounded_fraction,
        );
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

        // #1982. Compare source identities, not aggregate counts. The active
        // and superseded counts describe different populations; a large live
        // writer population can otherwise hide every stranded historical row.
        let active_grounded_keys = active
            .map(|row| &row.grounded_source_key_hexes)
            .cloned()
            .unwrap_or_default();
        // #1984: the generation each key's anchors live on is retained, not just
        // the key. The historical `cx_id` cannot be recomputed from a mutable
        // row's current bytes (#1981/#1982), so a repair must be told which
        // declared generation to carry FROM. `superseded_versions` is ordered
        // newest-superseded first and `or_insert` keeps that first writer, so a
        // key present on two closed generations names the newest one — the same
        // choice the carry-forward's lineage index makes per anchor kind.
        let mut superseded_grounded_keys: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
        let mut anchors_stranding_identity_unknown = 0usize;
        for version in entry.superseded_versions {
            let Some(row) = census.entry(*version) else {
                continue;
            };
            anchors_stranding_identity_unknown = anchors_stranding_identity_unknown
                .saturating_add(row.grounded_unattributed_records);
            for (source_cf, keys) in &row.grounded_source_key_hexes {
                let by_key = superseded_grounded_keys
                    .entry(source_cf.clone())
                    .or_default();
                for key in keys {
                    by_key.entry(key.clone()).or_insert(*version);
                }
            }
        }
        // #2021: an anchor can be replayed only while its source event exists.
        // TTL-managed audit CFs deliberately expire source rows while Calyx
        // retains their constellations as sacred history. Counting those
        // irrecoverable rows as backfill debt made an exhaustive sweep report
        // `backfill_owed=true` forever. The physical source-key census is the
        // authority: retain the old anchor, report it through the orphan
        // counters below, and owe replay only for keys the source still holds.
        // A missing CF census proves nothing and therefore contributes no
        // actionable debt; the coverage/orphan readbacks remain unknown rather
        // than manufacturing completion from an absent measurement.
        //
        // #1984 keeps the surviving identities instead of counting them and
        // dropping them. Three populations leave this loop, and each has a
        // different remedy: repairable debt (named), irrecoverable history
        // (counted), and unmeasured (counted, never treated as zero).
        let mut anchors_stranded_identities: Vec<StrandedAnchorIdentity> = Vec::new();
        let mut anchors_stranded_on_superseded = 0usize;
        let mut anchors_stranded_source_absent = 0usize;
        let mut anchors_stranded_source_cf_unmeasured = 0usize;
        if entry.carry_superseded_anchors {
            for (source_cf, keys) in &superseded_grounded_keys {
                let active_keys = active_grounded_keys.get(source_cf);
                let present_source_keys = source_cf_keys.get(source_cf);
                for (key, superseded_panel_version) in keys {
                    if active_keys.is_some_and(|active| active.contains(key)) {
                        // Already carried: the active generation holds this
                        // source identity grounded. Not debt.
                        continue;
                    }
                    let Some(present_source_keys) = present_source_keys else {
                        anchors_stranded_source_cf_unmeasured += 1;
                        continue;
                    };
                    if !present_source_keys.contains(key) {
                        anchors_stranded_source_absent += 1;
                        continue;
                    }
                    anchors_stranded_on_superseded += 1;
                    if anchors_stranded_identities.len() < SYN_ANCHOR_DEBT_IDENTITY_CAP {
                        anchors_stranded_identities.push(StrandedAnchorIdentity {
                            source_cf: source_cf.clone(),
                            source_key_hex: key.clone(),
                            superseded_panel_version: *superseded_panel_version,
                        });
                    }
                }
            }
        }
        let anchors_stranded_identities_truncated =
            anchors_stranded_on_superseded > anchors_stranded_identities.len();
        anchors_stranded_total += anchors_stranded_on_superseded;
        anchors_stranded_source_absent_total += anchors_stranded_source_absent;
        anchors_stranded_source_cf_unmeasured_total += anchors_stranded_source_cf_unmeasured;
        if anchors_stranded_on_superseded > 0 {
            let from = superseded_versions_present
                .iter()
                .filter(|generation| generation.grounded_records > 0)
                .map(|generation| {
                    format!(
                        "{}({})",
                        generation.panel_version, generation.grounded_records
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            anchors_stranded_panels.push(format!(
                "{}@{} ({} anchored record(s) stranded; active holds {}, superseded generation(s) {} hold {})",
                entry.panel_name,
                entry.panel_version,
                anchors_stranded_on_superseded,
                grounded_records,
                from,
                superseded_grounded_records,
            ));
            // #1984: the half of the stranded set nothing can repair is named
            // here, every tick, rather than only in the branch that runs when no
            // panel at all is owed. `syn-graphpos-app-v1` carried 59 stranded
            // anchors with no re-measure path while four repairable panels kept
            // that branch from ever being reached.
            if entry.backfill_source_cf.is_none() {
                anchor_debt_unbackfillable_panels.push(format!(
                    "{}@{} ({anchors_stranded_on_superseded} anchored record(s) stranded on \
                     generation(s) {from}; no backfill_source_cf, so no re-measure can re-anchor \
                     them)",
                    entry.panel_name, entry.panel_version,
                ));
            }
        }
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
            anchors_stranded_on_superseded,
            anchors_stranded_identities,
            anchors_stranded_identities_truncated,
            anchors_stranded_source_absent,
            anchors_stranded_source_cf_unmeasured,
            anchors_stranding_identity_unknown,
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

    // #2062. Every census generation the catalog does not claim is offered to
    // the allocator's owners map before it is called unknown. Three outcomes,
    // and the whole point is that they are three and not one.
    let mut unknown_panel_versions: Vec<(u32, usize)> = Vec::new();
    let mut reserved_generations_absent_from_catalog: Vec<(u32, usize)> = Vec::new();
    let mut rollups: BTreeMap<(String, &'static str), OwnedGenerationRollup> = BTreeMap::new();
    for row in &census.entries {
        if claimed_versions.contains(&row.panel_version) {
            continue;
        }
        let Some((owner_kind, panel_name)) = ownership.claim(row.panel_version) else {
            // Claimed by neither authority. THE finding: rows exist under a
            // generation nothing ever declared, so no surface reads them and
            // no re-measure knows how to rebuild them.
            unknown_panel_versions.push((row.panel_version, row.records));
            // Stranded by definition, so it belongs in the same total as the
            // declared superseded generations, and its grounded records in the
            // sacred total. It never contributes reclaim candidates: with no
            // owner and no catalog entry there is no known source CF and no
            // known active counterpart, so none of the four reclaim conditions
            // can even be evaluated.
            superseded_records_total += row.records;
            superseded_grounded_records_total += row.grounded_records;
            continue;
        };
        if owner_kind == OwnerKind::Builtin {
            reserved_generations_absent_from_catalog.push((row.panel_version, row.records));
        }
        let retired_by = ownership.retired.get(&row.panel_version).copied();
        let rollup = rollups
            .entry((panel_name.to_owned(), owner_kind.as_str()))
            .or_insert_with(|| OwnedGenerationRollup {
                panel_name: panel_name.to_owned(),
                owner_kind: owner_kind.as_str().to_owned(),
                generations_present: 0,
                live_generations: Vec::new(),
                retired_generations: Vec::new(),
                retired_generations_truncated: false,
                newest_generation: row.panel_version,
                records: 0,
                live_records: 0,
                retired_records: 0,
                grounded_records: 0,
                retired_grounded_records: 0,
            });
        rollup.generations_present += 1;
        rollup.newest_generation = rollup.newest_generation.max(row.panel_version);
        rollup.records += row.records;
        rollup.grounded_records += row.grounded_records;
        if retired_by.is_some() {
            rollup.retired_records += row.records;
            rollup.retired_grounded_records += row.grounded_records;
            if rollup.retired_generations.len() < SYN_OWNED_GENERATION_NAME_CAP {
                rollup.retired_generations.push(row.panel_version);
            } else {
                rollup.retired_generations_truncated = true;
            }
            // A retired generation is closed by DECLARATION, not by the
            // timestamp comparison `SupersededGeneration::closed` has to fall
            // back on: the successor named in the retirement record was already
            // durable when the record was written, so nothing can still be
            // writing here. Its ungrounded rows are therefore exactly the
            // #1927 ask 3 population — superseded, closed, and holding nothing
            // the successor snapshot does not already carry — and they belong
            // in the same upper bound. Grounded ones stay out: an anchor is an
            // observed outcome and no re-publish regenerates it.
            //
            // Still an upper bound and still not a delete list, for the same
            // reason as every other contributor to this number.
            superseded_records_total += row.records;
            superseded_grounded_records_total += row.grounded_records;
            superseded_reclaim_candidates_total += row.records.saturating_sub(row.grounded_records);
        } else {
            rollup.live_records += row.records;
            if rollup.live_generations.len() < SYN_OWNED_GENERATION_NAME_CAP {
                rollup.live_generations.push(row.panel_version);
            }
        }
    }
    // --- The catalog's lineage, checked against the allocator (#2093) ---
    //
    // Every generation a catalog entry declares — its active one and every
    // entry in its lineage — was reserved `builtin:<panel_name>` by
    // `ensure_builtin_panel_generation_reservations` at the time it *was* the
    // active one. That reservation is durable and is never rewritten, so the
    // allocator remembers which panel actually wrote each generation long after
    // the catalog constant moved on. A catalog entry claiming a generation the
    // allocator attributes to a different panel is therefore a provable
    // mis-declaration, not a judgement call.
    //
    // This is the check nothing performed. #1983 moved `syn-agent-event-v1` off
    // `1_665_001` and appended that generation to `syn-graphpos-app-v1`'s
    // lineage — the entry above it in the same file — and for four months the
    // census believed it: 16,650 agent-event rows counted as a graph panel's
    // superseded history, 59 replayable anchors reported as permanently
    // unrepairable because the panel they had been reassigned to has no
    // re-measure path, and `panel_coverage=error` pinning `health.ok=false`
    // with no action an operator could take (#2093).
    //
    // Silent when the allocator has no claim: a generation retired before this
    // vault existed was never reserved here, and absence of evidence is
    // reported as nothing rather than as a finding.
    let mut catalog_lineage_misattributed: Vec<String> = Vec::new();
    for entry in &catalog {
        for (role, version) in std::iter::once(("active", entry.panel_version)).chain(
            entry
                .superseded_versions
                .iter()
                .map(|version| ("superseded", *version)),
        ) {
            let Some((owner_kind, owner_panel)) = ownership.claim(version) else {
                continue;
            };
            if owner_panel == entry.panel_name {
                continue;
            }
            catalog_lineage_misattributed.push(format!(
                "{}@{version} declared as this panel's {role} generation, but the allocator owns \
                 it as {}:{owner_panel} holding {} record(s); one of the two declarations is wrong \
                 and the allocator's is the one that was written by the panel that wrote the rows",
                entry.panel_name,
                owner_kind.as_str(),
                census.entry(version).map_or(0, |row| row.records),
            ));
        }
    }

    // --- Allocator-live vs census-live (#2081 ask 4) ---
    //
    // A generation with an owner row and no `Base` rows produces no census
    // entry, so every surface downstream of the census is blind to it. The two
    // totals are published side by side because neither alone can express that,
    // and their difference has **two** populations in it that must not be
    // conflated:
    //
    // * `builtin:` reservations that have not been written to. Expected and
    //   permanent. `ensure_builtin_panel_generation_reservations` claims every
    //   catalog generation at vault open precisely so no writer can name an
    //   unowned one, and the three derived-snapshot panels never write at their
    //   reserved base at all. Measured on a fresh vault: 14 of these before a
    //   single row exists. Treating them as a finding would make the field
    //   permanently red, which is a broken instrument rather than a signal.
    // * `dynamic:` claims with no rows. **This is #2081's population** — one
    //   permanently consumed owner row per publish whose compute failed after
    //   allocating, out of a bounded `MAX_OWNERS = 10_000`. A successful publish
    //   always commits at least one constellation, so a live dynamic claim with
    //   no `Base` row behind it did not publish.
    //
    // Only the second is named and only the second raises anything.
    //
    // Reported, never retired. Retirement would need `supersede_panel_
    // generations`, whose refusal to retire a generation ABOVE the successor is
    // the guard that stops a race with a publish in flight — and an orphan
    // minted after the last successful publish is above it by construction. A
    // census reading zero rows also cannot distinguish "this publish failed"
    // from "this publish has allocated and has not committed its batch yet", so
    // acting on it would mark a live publish's rows reclaimable the instant they
    // land. See #2081 ask 3.
    let census_live_generations: BTreeSet<u32> = census
        .entries
        .iter()
        .map(|row| row.panel_version)
        .filter(|version| !ownership.retired.contains_key(version))
        .collect();
    let allocator_live_generations: BTreeSet<u32> = ownership
        .owners
        .keys()
        .copied()
        .filter(|version| !ownership.retired.contains_key(version))
        .collect();
    let allocator_live_dynamic_without_records: Vec<String> = allocator_live_generations
        .iter()
        .filter(|version| !census_live_generations.contains(version))
        .filter(|version| matches!(ownership.claim(**version), Some((OwnerKind::Dynamic, _))))
        .take(SYN_OWNED_GENERATION_NAME_CAP)
        .map(|version| {
            format!(
                "{version} owned by {}",
                ownership
                    .owners
                    .get(version)
                    .map_or("<unreadable owner claim>", String::as_str)
            )
        })
        .collect();
    let allocator_live_count = allocator_live_generations.len();
    let census_live_count = census_live_generations.len();

    let owned_dynamic_generations: Vec<OwnedGenerationRollup> = rollups.into_values().collect();
    let dynamic_panels_multi_live: Vec<String> = owned_dynamic_generations
        .iter()
        .filter(|rollup| rollup.live_generations.len() > 1)
        .map(|rollup| {
            format!(
                "{} ({} live generation(s) {:?} holding {} record(s); a publisher minted without \
                 retiring its predecessor)",
                rollup.panel_name,
                rollup.live_generations.len(),
                rollup.live_generations,
                rollup.live_records,
            )
        })
        .collect();

    PanelCoverageReport {
        panels,
        unknown_panel_versions,
        owned_dynamic_generations,
        dynamic_panels_multi_live,
        reserved_generations_absent_from_catalog,
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
        anchors_stranded_panels,
        anchor_debt_unbackfillable_panels,
        anchors_stranded_total,
        anchors_stranded_source_absent_total,
        anchors_stranded_source_cf_unmeasured_total,
        records_exceed_source_panels,
        catalog_lineage_misattributed,
        allocator_live_count,
        census_live_count,
        allocator_live_dynamic_without_records,
        measured_at_unix_ms: census.measured_at_unix_ms,
    }
}
