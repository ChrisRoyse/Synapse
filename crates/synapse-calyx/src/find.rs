//! Phase-5 Compose: fused find-similar search (#1676).
//!
//! Synapse-side facade over `calyx-search`'s persisted per-slot recall → RRF
//! fusion → provenance path, with a bounded temporal post-boost (#1667) and an
//! explainable agree/disagree evidence surface. Two query modes:
//!
//! * **Query-by-example** — a record key (`cx_id`); its own stored slot vectors
//!   become the multi-slot query. "Find episodes like this one."
//! * **Query-by-text** — a text string measured through the active panel's
//!   free-text lenses. On the Synapse panels that is the raw term-frequency
//!   lexical lane (`syn_sparse_text_tf`), which the index ranks with real BM25.
//!   The normalized signed lane beside it (`syn_sparse_text`) is deliberately
//!   *not* free-text queryable: L1 normalization leaves no term frequency to
//!   saturate and no document length to compare, so it could not rank by
//!   discriminativeness and a one-word title tied a record's own verbatim title
//!   (#1900). It remains a similarity feature for `by_example`.
//! * **Query-by-exact-value** — one whole-value hash-lane slot plus the field
//!   value, confirmed against the authoritative source row so a bucket collision
//!   is dropped instead of returned (#1899).
//!
//! Doctrine honored here:
//! * **No-flatten.** Slots stay typed and separate; recall runs per slot and the
//!   substrate fuses at the *rank* level (Reciprocal Rank Fusion), never by
//!   concatenating vectors. Fusion contributions are reported per lens.
//! * **Fail-closed.** A query over a missing or stale persisted index returns a
//!   structured error that names the exact rebuild remediation
//!   ([`calyx_search::REBUILD_REQUIRED_REMEDIATION`]); it never silently
//!   degrades to an ungrounded result. A rebuild-required marker staked by an
//!   in-flight mutation is refused up front.
//! * **Vault is the sole source of truth.** Every returned hit carries the verified ledger
//!   provenance the substrate attached from the Base rows; the physical
//!   generation manifest (sha256 + base seq + per-slot descriptors) is read
//!   back into the report so an operator can prove the result against bytes.
//! * **Encoders only.** No embedder is measured; text queries flow through the
//!   frozen algorithmic text lenses.
//!
//! ## Fusion formula
//! Reciprocal Rank Fusion with the substrate's `RRF_K = 60` constant. For a hit
//! `d`, over the searched slots `S`, `score(d) = Σ_{s∈S} w_s / (60 + rank_s(d))`
//! where `rank_s` is 1-based within slot `s`'s recall list and `w_s` is the
//! slot weight (`1.0` for plain `Rrf`; the panel's declared profile weights for
//! `WeightedRrf`). `SingleSlot(s)` isolates one lens (`w = 1.0` on `s`,
//! `0` elsewhere) so a caller can compare pure vector recall vs. pure lexical
//! recall against fusion. `k = 60` is Cormack et al.'s validated default and
//! matches the Sextant substrate; BM25's `k1`/`b` live in the built sparse index
//! and are not re-parameterized per query. Each consulted lane reports the exact
//! within-lane law it ranked by on
//! `generation.slots[].scoring_law`, because `sparse_dot` and `sparse_bm25` are
//! both "the sparse lane" and rank by entirely different laws (#1900).
//!
//! ## Seams
//! * **Ward guarded search (#1677)** — callers explicitly choose raw recall or
//!   calibrated in-region enforcement. Profile-backed guarding loads the exact
//!   requested panel's Ward profile, and the report carries every retained
//!   candidate's per-slot verdict plus every rejection's verdict or precise
//!   unscorable reason. An unguarded pass still proves that
//!   no guard evidence was applied; contradictory substrate state fails closed.
//! * **`MaxSim` late interaction** — the substrate already fuses any
//!   `multi_maxsim` slot; the report reads back whether the live generation
//!   carries one. The `Syn*` encoder catalog ships no multi-vector token slot
//!   yet (#1663), so today this is a documented, honest absence rather than a
//!   fabricated score.
//! * **Provisional grounding marker (#1670)** — fused-find hits are retrieval
//!   over the vault source of truth and each carries verified ledger provenance, so they are
//!   inherently grounded and need no provisional marker. If #1670 introduces a
//!   derived-answer provisional convention, [`SynapseCalyxFindReport::grounding_note`]
//!   is the seam that would carry it.

use std::collections::{BTreeMap, BTreeSet};

use calyx_core::{Constellation, CxId, SlotId, SlotVector, VaultStore};
use calyx_registry::{VaultPanelState, load_vault_panel_state};
use calyx_search::{
    FusionChoice, FusionTuning, GuardChoice, PersistedSearchGeneration, PersistedSearchIndexes,
    QueryMeasurement, REBUILD_REQUIRED_REMEDIATION, SearchBudget, SearchError, SearchFreshness,
    SearchOutcome, measure_query, read_rebuild_required_marker,
    search_outcome_with_query_vectors_freshness_cached,
};
use calyx_sextant::TemporalScores;
use serde::{Deserialize, Serialize};

use crate::{
    SynapseCalyxError, SynapseCalyxTemporalCandidate, SynapseCalyxVault, hex_bytes, parse_cx_id,
};

/// Hard upper bound on `k` for one fused-find pass. Matches the temporal rerank
/// candidate cap so a temporally-boosted find never exceeds the #1667 bound.
pub const SYNAPSE_FIND_MAX_K: usize = 1_000;

/// The RRF rank constant a vault uses when its `calyx_fusion_k` is untuned.
///
/// This is the workspace default re-exported, not a fourth declaration of it:
/// before #1883 the constant was independently written down in
/// `calyx-sextant`, `calyx-ledger`, here, and again as `DEFAULT_FUSION_K`, and
/// the configured knob reached none of them. The value that actually scores a
/// query is `SynapseCalyxTuningConfig::fusion_k`, reported per-query on
/// `SynapseCalyxFindReport::rrf_k`.
pub const SYNAPSE_FIND_RRF_K: u32 = calyx_core::RRF_K_DEFAULT;

/// The exact rank-level fusion law the substrate applies, stated for the `k`
/// the query actually ran under.
///
/// Surfaced verbatim so a caller can recompute every reported score from the
/// reported per-lens ranks. Cormack, Clarke & Buettcher (SIGIR 2009) define
/// `RRFscore(d) = Σ 1/(k+r(d))` with 1-based `r(d)`; the substrate adds the
/// per-lens weight `w_s` (always `1.0` for plain RRF). The `k` is the vault's
/// configured `calyx_fusion_k`, interpolated here rather than hardcoded so the
/// reported law can never describe a different scoring than the one that ran
/// (#1883).
#[must_use]
pub fn synapse_find_rrf_formula(rrf_k: u32) -> String {
    format!(
        "score(d) = SUM over consulted slots s of w_s / ({rrf_k} + rank_s(d)), rank_s 1-based, k={rrf_k} (Cormack et al., SIGIR 2009)"
    )
}

/// Machine-readable state code reported when the Ward in-region guard seam is
/// off, so a caller can branch on the guard state without parsing prose.
pub const SYNAPSE_FIND_GUARD_DISABLED_CODE: &str = "SYNAPSE_CALYX_FIND_GUARD_DISABLED";

/// Why the guard is off, stated as fact rather than as a promise.
///
/// This text asserted three things, and two of them stopped being true. It
/// claimed `calyx-ward` was not a direct dependency of any `synapse-*` crate
/// (it is — `synapse-calyx/Cargo.toml`) and that nothing in Synapse ever writes
/// the Guard CF (`crate::ward::guard_calibrate` does). Only the third — that
/// the CF is empty on the live vault — still holds, and for a reason this text
/// never named.
///
/// That matters for the same reason it mattered in `ward.rs` (#1919): an
/// operator following a stale remediation goes and builds a path that already
/// exists, and never looks at the thing actually blocking them. The blocker is
/// the panel binding, not the wiring.
const SYNAPSE_FIND_GUARD_DISABLED_REASON: &str = "this request explicitly selected guard=off, so these hits are raw fused recall and were NOT filtered for Ward in-region membership";

/// Exact, ordered prerequisites for enabling the calibrated in-region guard.
/// Each line names a physical artifact or code seam that must exist first.
const SYNAPSE_FIND_GUARD_ENABLE_REQUIREMENTS: &[&str] = &[
    "1. persist a calibrated Ward profile for the exact requested panel at Guard CF key profile\\0panel\\0<version>; calibration requires real adjudicated good and bad outcomes",
    "2. retry with guard_mode=in_region; omit guard_tau to use the conformal per-slot profile, or state a finite operator cosine tau in (0.0, 1.0]",
];

/// Which record set the query is drawn from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SynapseCalyxFindQuery {
    /// Query-by-example: the stored slot vectors of this content-addressed
    /// record become the multi-slot query.
    ByExample { cx_id: String },
    /// Query-by-text: measured through the active panel's text lenses.
    ByText { text: String },
    /// Query-by-exact-value: the caller asserts `value` is the **whole** field a
    /// specific hash-lane slot measures, and the lane answers which records
    /// carry exactly that value (#1899).
    ///
    /// A whole-string hash lane is excluded from free-text fusion on purpose —
    /// it produces a one-cell vector for any phrase, so a bucket collision would
    /// inject an entire unrelated record set into every text query with nothing
    /// in the result to distinguish it from a true match. Stating the assertion
    /// explicitly makes the collision the caller's declared intent, which is
    /// what allows each candidate to be confirmed against its source field
    /// before it is returned.
    ByExact { slot: u16, value: String },
}

/// Rank-level fusion strategy for the find pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxFindFusion {
    /// Unweighted Reciprocal Rank Fusion across every searched slot.
    Rrf,
    /// Reciprocal Rank Fusion using the panel's declared profile weights.
    WeightedRrf,
    /// Isolate a single lens (e.g. the record-vector slot for pure vector
    /// recall, or the sparse term-frequency slot for pure BM25 recall).
    SingleSlot { slot: u16 },
}

impl SynapseCalyxFindFusion {
    const fn to_choice(self) -> FusionChoice {
        match self {
            Self::Rrf => FusionChoice::Rrf,
            Self::WeightedRrf => FusionChoice::WeightedRrf,
            Self::SingleSlot { slot } => FusionChoice::SingleLensSlot(SlotId::new(slot)),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Rrf => "rrf",
            Self::WeightedRrf => "weighted_rrf",
            Self::SingleSlot { .. } => "single_slot",
        }
    }
}

/// Optional bounded temporal post-boost (#1667). When present the fused hits are
/// re-scored through the exact registered temporal policy for their panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxFindTemporal {
    pub query_time_secs: i64,
    pub tz_offset_secs: i32,
}

/// Bounded request for one fused find-similar pass.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxFindParams {
    pub query: SynapseCalyxFindQuery,
    pub k: usize,
    pub fusion: SynapseCalyxFindFusion,
    /// Optional Sextant filter expression (e.g. time-range / app) forwarded
    /// verbatim to the persisted-index filter path.
    pub filter: Option<String>,
    /// Attach the per-lens explain breakdown to each hit.
    pub explain: bool,
    /// Bounded temporal post-boost, applied only when the panel has a registered
    /// temporal policy.
    pub temporal: Option<SynapseCalyxFindTemporal>,
    /// Which panel generation to query (#1668). `None` targets the durable
    /// active panel, which is what every caller got before this field existed.
    ///
    /// Naming a version explicitly is how a non-active panel is reached — the
    /// vault manifest publishes exactly one `panel_ref`, so before this the
    /// active panel was not merely the default target but the only reachable
    /// one, and the corpora carrying the grounded intelligence are all on other
    /// panels.
    ///
    /// This selects ONE panel. It is deliberately not a list: slot ids are only
    /// meaningful within a panel, so fusing hits across panels would rank
    /// incomparable lenses against each other and reintroduce exactly the
    /// cross-panel ambiguity #1776 closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_version: Option<u32>,
    /// Explicit Ward enforcement. Defaults to off for compatibility; callers
    /// must opt into a calibrated profile or state an operator tau.
    #[serde(default)]
    pub guard: SynapseCalyxFindGuardMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxFindGuardMode {
    #[default]
    Off,
    InRegion {
        operator_tau: Option<f32>,
    },
}

impl SynapseCalyxFindGuardMode {
    const fn choice(self) -> GuardChoice {
        match self {
            Self::Off => GuardChoice::Off,
            Self::InRegion { .. } => GuardChoice::InRegion,
        }
    }

    const fn operator_tau(self) -> Option<f32> {
        match self {
            Self::Off => None,
            Self::InRegion { operator_tau } => operator_tau,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::InRegion { operator_tau: None } => "in_region_profile",
            Self::InRegion {
                operator_tau: Some(_),
            } => "in_region_operator_tau",
        }
    }
}

/// Resolves which panel generation one fused find runs against (#1668).
///
/// Mirrors Ward's `panel_for_guard_calibration` deliberately: the two surfaces
/// answer the same question ("which panel definition validates these slots?")
/// and answering it two different ways is how the guard and the search side
/// drifted apart in the first place.
///
/// Resolution order, fail-closed at every step:
///
/// 1. no version requested  -> the durable active panel (unchanged behaviour);
/// 2. version requested and a definition supplied -> that definition, whose
///    version must match the request exactly;
/// 3. version requested, none supplied -> the active panel, but only if it *is*
///    that version.
///
/// A definition is never synthesized, and a near-miss version is never
/// substituted: querying panel A's index with panel B's slot map would search
/// the wrong lenses and return hits that look well-formed.
fn panel_state_for_find(
    vault_dir: &std::path::Path,
    requested: Option<u32>,
    supplied: Option<&VaultPanelState>,
) -> Result<VaultPanelState, SynapseCalyxError> {
    let load_active = || {
        load_vault_panel_state(vault_dir).map_err(|error| {
            SynapseCalyxError::from_calyx("load durable panel state for fused find", &error)
        })
    };
    let Some(requested) = requested else {
        return load_active();
    };
    if let Some(state) = supplied {
        if state.panel.version != requested {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_FIND_PANEL_MISMATCH",
                format!(
                    "fused find requested panel {requested}, but the supplied panel definition is version {}",
                    state.panel.version
                ),
                "supply the panel contract for the exact version being queried; a query measured through another panel's slot map searches the wrong lenses and its hits cannot be compared to the requested panel's records",
            ));
        }
        return Ok(state.clone());
    }
    let active = load_active()?;
    if active.panel.version == requested {
        return Ok(active);
    }
    Err(SynapseCalyxError::new(
        "SYNAPSE_CALYX_FIND_PANEL_UNAVAILABLE",
        format!(
            "fused find requested panel {requested}, but no definition was supplied for it and the durable active panel is {}",
            active.panel.version
        ),
        "supply the panel contract for the requested version (a code-declared generation can be reconstructed with syn_active_panel_contract), or omit panel_version to query the active panel",
    ))
}

/// One lens's rank-level contribution to a fused hit's score.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxFindLensContribution {
    pub slot: u16,
    pub rank: usize,
    pub raw_score: f32,
    pub weight: f32,
    pub contribution: f32,
}

/// One fused, provenanced find hit with explainable per-lens evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxFindHit {
    pub cx_id: String,
    pub rank: usize,
    pub score: f32,
    /// Rank-level contributions summing (per RRF) to `score` before any
    /// temporal boost.
    pub per_lens: Vec<SynapseCalyxFindLensContribution>,
    /// Slots (from those consulted) whose recall ranked this record — the
    /// lenses that *support* the match.
    pub agree_slots: Vec<u16>,
    /// Consulted slots that did NOT rank this record — the lenses that *dissent*.
    pub disagree_slots: Vec<u16>,
    /// Present only when a temporal boost was applied.
    pub event_time_secs: Option<i64>,
    /// Present only when a temporal boost was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_scores: Option<TemporalScores>,
    /// Ledger sequence of the hit's verified provenance entry.
    pub provenance_seq: u64,
    /// Hex of the hit's verified provenance ledger hash — the source-of-truth proof.
    pub provenance_hash: String,
    pub freshness_built_at_seq: u64,
    pub freshness_base_seq: u64,
    pub freshness_policy: String,
    /// Full Ward decomposition for a retained guarded hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_verdict: Option<SynapseCalyxGuardVerdict>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxFindGuardSlotVerdict {
    pub slot: u16,
    pub cosine: f32,
    pub tau: f32,
    pub pass: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardVerdict {
    pub guard_id: String,
    pub overall_pass: bool,
    pub provisional: bool,
    pub action: Option<String>,
    pub per_slot: Vec<SynapseCalyxFindGuardSlotVerdict>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SynapseCalyxDroppedGuardHit {
    pub cx_id: String,
    pub reason: String,
    pub verdict: Option<SynapseCalyxGuardVerdict>,
}

/// Explicit, observable state of the Ward guarded-search seam (#1677) for one
/// fused find pass.
///
/// The seam is deliberately off. Leaving that implicit is the actual hazard: a
/// caller reading a hit list cannot otherwise distinguish "guarded, and these
/// survived" from "never guarded at all". Every field below is a readback of
/// what the substrate physically did — the flat operator tau it applied (none),
/// the candidates it dropped (none), and the per-hit guard verdicts it attached
/// (none) — not a restatement of the mode this code requested.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SynapseCalyxFindGuard {
    /// Guard mode this pass asked the substrate for. Always `off` today.
    pub requested_mode: String,
    /// Whether a guard actually filtered these hits. Always `false` today, and
    /// derived from substrate evidence rather than from `requested_mode`.
    pub applied: bool,
    /// Machine-readable guard state ([`SYNAPSE_FIND_GUARD_DISABLED_CODE`]).
    pub state_code: String,
    /// Flat operator cosine tau the substrate applied, if any.
    pub operator_tau: Option<f32>,
    /// Candidates a profile-backed guard dropped, read back from the outcome.
    pub dropped_candidates: usize,
    /// Returned hits that carry a per-hit guard verdict, counted from the hits.
    pub hits_with_guard_verdict: usize,
    pub dropped: Vec<SynapseCalyxDroppedGuardHit>,
    /// Why the guard is off, in operator terms.
    pub disabled_reason: Option<String>,
    /// Exact prerequisites for enabling the calibrated in-region guard.
    pub enable_requirements: Vec<String>,
}

/// What an exact-value probe physically asked the hash lane (#1899).
///
/// Carried on the report so a caller can see that the hit set is a
/// single-bucket probe and *not yet* an exact-match answer: a bucket collision
/// is byte-identical to a true match inside the index, so the returned
/// candidates are unconfirmed until each one's source field is re-read and
/// compared. `confirmation_required` states that obligation in the payload
/// rather than leaving it to documentation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxFindExactProbe {
    pub slot: u16,
    /// Slot key of the lens that measured the value.
    pub lens: String,
    /// The whole-field value the caller asserted.
    pub value: String,
    /// Sparse cells the value hashed into. A whole-string hash lights exactly
    /// one, so more than one cell here means the lane is not what it claims.
    pub probe_cells: Vec<u32>,
    pub confirmation_required: String,
}

/// The confirmation obligation an exact-value probe carries.
const EXACT_CONFIRMATION_REQUIRED: &str = "these hits are unconfirmed bucket candidates: a hash collision is indistinguishable from a true match inside the index, so each candidate's source field must be re-read and compared to the queried value before it is treated as an exact match; storage operation=find_similar query_mode=by_exact performs that confirmation and reports candidates_probed vs candidates_confirmed";

/// Result of one fused find-similar pass, with the physical generation readback.
///
/// Serialize-only: it embeds the immutable [`PersistedSearchGeneration`] manifest
/// readback (like the search-rebuild report).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SynapseCalyxFindReport {
    pub panel_version: u32,
    pub fusion: String,
    /// `by_example:<cx_id>`, `by_text`, or `by_exact:slot=<n>`.
    pub query_kind: String,
    /// Present only for `by_exact`: what the probe physically asked, and the
    /// confirmation still owed on its candidates (#1899).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact: Option<SynapseCalyxFindExactProbe>,
    pub k: usize,
    pub rrf_k: u32,
    /// The rank-level fusion law, so a caller can recompute every score from the
    /// reported per-lens ranks ([`synapse_find_rrf_formula`]).
    pub rrf_formula: String,
    /// Slots that carried a query vector and a persisted index — the lenses the
    /// fusion actually consulted.
    pub consulted_slots: Vec<u16>,
    pub temporal_applied: bool,
    /// Ward guarded-search seam (#1677): explicit, evidence-backed off state.
    pub guard: SynapseCalyxFindGuard,
    /// `MaxSim` late-interaction seam readback.
    pub maxsim_note: String,
    /// Grounding / provisional-marker seam note (#1670).
    pub grounding_note: String,
    /// The immutable persisted-search generation the query ran against.
    pub generation: PersistedSearchGeneration,
    pub hits: Vec<SynapseCalyxFindHit>,
}

impl SynapseCalyxVault {
    /// Runs one fused find-similar pass over the persisted per-slot indexes for
    /// the active panel: per-slot recall, RRF (or weighted / single-lens)
    /// fusion, an optional bounded temporal boost, and an agree/disagree
    /// evidence surface. Every hit carries verified ledger provenance; the
    /// physical generation manifest is read back into the report.
    ///
    /// This is a live retrieval computation and is asserted off the hot path;
    /// callers must run it off the MCP runtime (blocking pool).
    ///
    /// # Errors
    ///
    /// Fails closed with a structured error when: the durable panel state is
    /// unreadable; a rebuild-required marker is staked; the persisted search
    /// generation is missing or stale (naming the rebuild remediation); the
    /// example record is absent, from a different panel, or exposes no
    /// indexable slot; a text query measures no indexable lens vector; the
    /// fused search fails; or the requested temporal boost cannot be applied.
    pub fn find_similar(
        &self,
        params: &SynapseCalyxFindParams,
    ) -> Result<SynapseCalyxFindReport, SynapseCalyxError> {
        self.find_similar_in_panel(params, None)
    }

    /// [`Self::find_similar`], with an explicit panel contract for the
    /// generation named by `params.panel_version` (#1668).
    ///
    /// `synapse-calyx` cannot reconstruct a code-declared panel itself —
    /// `syn_active_panel_contract` lives in `synapse-storage`, which depends on
    /// this crate — so a non-active panel's definition is supplied by the
    /// caller, exactly as Ward's guard calibration takes one. Passing `None`
    /// restricts the query to the durable active panel.
    ///
    /// # Errors
    ///
    /// In addition to [`Self::find_similar`]'s failures: when `supplied`
    /// disagrees with the requested version, or when a non-active version is
    /// requested with no definition to validate its slots against.
    #[allow(clippy::too_many_lines)]
    pub fn find_similar_in_panel(
        &self,
        params: &SynapseCalyxFindParams,
        supplied: Option<&VaultPanelState>,
    ) -> Result<SynapseCalyxFindReport, SynapseCalyxError> {
        // Hot-path boundary (#1686): fused find is an off-runtime intelligence
        // query and must never be driven from a tagged reflex/capture tick.
        crate::lowering::hot_context::assert_cold_calyx("find_similar");

        let vault_dir = self.config.vault_dir.as_path();
        let state = panel_state_for_find(vault_dir, params.panel_version, supplied)?;
        let panel_version = state.panel.version;

        // Fail closed if a mutation staked a rebuild-required intent: derived
        // search state is stale until a rebuild republishes the manifest.
        if let Some(marker) =
            read_rebuild_required_marker(vault_dir, panel_version).map_err(|error| {
                find_index_error("read rebuild-required marker for fused find", &error)
            })?
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_FIND_REBUILD_REQUIRED",
                format!(
                    "persisted search generation for panel {panel_version} is marked stale by source={} detail={}",
                    marker.source, marker.detail
                ),
                REBUILD_REQUIRED_REMEDIATION,
            ));
        }

        // Open the immutable generation up front: this both fails closed on a
        // missing/stale index (naming the rebuild remediation) and yields the
        // physical manifest used to bound the query slot set and prove the
        // result against bytes.
        let generation = PersistedSearchIndexes::open(vault_dir, panel_version)
            .and_then(|indexes| indexes.generation())
            .map_err(|error| {
                find_index_error("open persisted search generation for fused find", &error)
            })?;
        let indexed_slots: BTreeSet<SlotId> = generation
            .slots
            .iter()
            .map(|slot| slot.panel_slot.slot_id())
            .collect();

        // Build the multi-slot query, restricted to slots that carry a
        // persisted index (non-indexed slots cannot contribute recall).
        let mut exact: Option<SynapseCalyxFindExactProbe> = None;
        let (query_vectors, query_kind, self_cx, measurement) = match &params.query {
            SynapseCalyxFindQuery::ByExample { cx_id } => {
                let example = parse_cx_id(cx_id)?;
                let constellation = self.read_example_constellation(example, panel_version)?;
                // Two independent reasons a stored slot cannot carry a query,
                // and both must be applied.
                //
                // `indexed_slots` drops lenses the generation does not index —
                // they cannot contribute recall.
                //
                // `Absent` drops lenses that never measured THIS record. An
                // absent slot is an explicit "this lens did not measure this
                // input", not a zero vector, so it is not a query: forwarding
                // one made the persisted search refuse the whole pass with
                // SYNAPSE_CALYX_FIND_INDEX_STALE ("slot N received an absent
                // query vector"), which names a stale index and sends the
                // operator to rebuild — when nothing is stale and the rebuild
                // cannot help. Measured on syn-mcp-usage-v1 @ 1776006: slot 87
                // (error_onehot) is populated on 242 of 1,240 records, so every
                // by_example probe on a record that simply did not error failed
                // that way. The active panel hid this because its records
                // populate every indexed slot.
                let vectors = constellation
                    .slots
                    .iter()
                    .filter(|(slot, vector)| {
                        indexed_slots.contains(slot) && !matches!(vector, SlotVector::Absent { .. })
                    })
                    .map(|(slot, vector)| (*slot, vector.clone()))
                    .collect::<Vec<_>>();
                (
                    vectors,
                    format!("by_example:{example}"),
                    Some(example),
                    None,
                )
            }
            SynapseCalyxFindQuery::ByExact { slot, value } => {
                let slot = SlotId::new(*slot);
                let measured = calyx_search::measure_exact_value(&state, slot, value)
                    .map_err(|error| find_index_error("measure exact-value find query", &error))?;
                if !indexed_slots.contains(&slot) {
                    return Err(SynapseCalyxError::new(
                        "SYNAPSE_CALYX_FIND_EXACT_SLOT_NOT_INDEXED",
                        format!(
                            "panel {panel_version} slot {slot} can measure the exact value, but the persisted generation carries no index for it; indexed slots are {:?}",
                            indexed_slots
                                .iter()
                                .map(|slot| slot.get())
                                .collect::<Vec<_>>()
                        ),
                        "rebuild the panel search generation so the hash lane is indexed, then retry",
                    ));
                }
                exact = Some(SynapseCalyxFindExactProbe {
                    slot: slot.get(),
                    lens: measured.lens_name.clone(),
                    value: value.clone(),
                    probe_cells: measured.probe_cells.clone(),
                    confirmation_required: EXACT_CONFIRMATION_REQUIRED.to_owned(),
                });
                (
                    vec![(slot, measured.vector)],
                    format!("by_exact:slot={}", slot.get()),
                    None,
                    None,
                )
            }
            SynapseCalyxFindQuery::ByText { text } => {
                // Measured over the panel's own slot set, not pre-restricted to
                // the indexed set, so an empty result can distinguish "no lens
                // is text-queryable" from "the text-queryable lens carries no
                // persisted index" (#1896). The restriction is applied after.
                let measured = measure_query(&state, text, None)
                    .map_err(|error| find_index_error("measure fused-find text query", &error))?;
                let vectors = measured
                    .vectors
                    .iter()
                    .filter(|(slot, _)| indexed_slots.contains(slot))
                    .map(|(slot, vector)| (*slot, vector.clone()))
                    .collect::<Vec<_>>();
                (vectors, "by_text".to_owned(), None, Some(measured))
            }
        };

        if query_vectors.is_empty() {
            return Err(no_indexable_query_error(
                panel_version,
                &indexed_slots,
                measurement.as_ref(),
            ));
        }

        // "Consulted" must mean what the FUSION consulted, not what the query
        // measured. Under `single_slot` the query still measures every
        // text-queryable lens on the panel, so deriving this from
        // `query_vectors` alone reported lenses the fusion never asked.
        //
        // It is not a cosmetic field: `build_find_hits` derives each hit's
        // `agree_slots` / `disagree_slots` from it, so an unasked lens was
        // reported as *dissenting* — evidence of disagreement from a lens that
        // never voted. Measured on syn-agent-transcript-v1 @ 1921001:
        // `single_slot=109` returned 5 hits and still reported
        // `consulted_slots=[107,109]`, and `single_slot=107` returned 0 hits
        // while reporting the same pair.
        let consulted_slots: Vec<u16> = {
            let mut slots = query_vectors
                .iter()
                .map(|(slot, _)| slot.get())
                .filter(|slot| match params.fusion {
                    SynapseCalyxFindFusion::SingleSlot { slot: only } => *slot == only,
                    SynapseCalyxFindFusion::Rrf | SynapseCalyxFindFusion::WeightedRrf => true,
                })
                .collect::<Vec<_>>();
            slots.sort_unstable();
            slots.dedup();
            slots
        };

        let k = params.k.clamp(1, SYNAPSE_FIND_MAX_K);
        // Query-by-example removes the query record itself after substrate
        // fusion. Ask for one additional bounded candidate so that removal
        // cannot underfill the caller's requested result count (#1844).
        let substrate_k = if self_cx.is_some() { k + 1 } else { k };
        // Guarding is an explicit caller decision. Profile-backed mode resolves
        // the exact panel profile and preserves every per-slot verdict; off
        // mode is validated below to have produced no hidden guard evidence.
        // The configured knob, not a constant: `calyx_fusion_k` reaches the
        // scoring law it names (#1883). Validated at config load, re-validated
        // here so an out-of-domain value fails the query loudly rather than
        // producing a silently misordered ranking.
        let rrf_k = self.config.tuning.fusion_k;
        let fusion_tuning = FusionTuning::new(rrf_k).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_FIND_FUSION_TUNING_INVALID",
                format!("configured calyx_fusion_k={rrf_k} cannot score a fused query: {error}"),
                "set calyx_fusion_k to a positive integer; the Cormack et al. default is 60",
            )
        })?;
        let outcome: SearchOutcome = search_outcome_with_query_vectors_freshness_cached(
            &self.vault,
            vault_dir,
            &state.panel,
            &query_vectors,
            substrate_k,
            params.fusion.to_choice(),
            params.guard.choice(),
            params.guard.operator_tau(),
            params.filter.as_deref(),
            params.explain,
            SearchFreshness::Fresh,
            SearchBudget::disabled(),
            None,
            fusion_tuning,
            None,
        )
        .map_err(|error| find_index_error("run fused persisted search", &error))?;

        // The guard state is asserted against what the substrate physically did,
        // not against the mode this code passed. Guard evidence on a pass that
        // requested `Off` means the substrate contract changed underneath us, so
        // fail closed instead of reporting an unguarded result we cannot prove.
        let mut guard = find_guard_readback(&outcome, params.guard)?;

        let mut hits = build_find_hits(&outcome, &consulted_slots, self_cx);
        hits.truncate(k);
        // The substrate guards before this facade removes the by-example
        // self-match and applies the caller's final k. Report verdict coverage
        // over the records actually returned, not that larger internal set.
        guard.hits_with_guard_verdict = hits
            .iter()
            .filter(|hit| hit.guard_verdict.is_some())
            .count();

        // Bounded temporal post-boost (#1667): reuse the fully-validated
        // registered-policy rerank over the fused candidates, then merge the
        // per-component temporal evidence and boosted order back in.
        let temporal_applied = if let Some(temporal) = params.temporal
            && !hits.is_empty()
        {
            hits = self.apply_find_temporal_boost(hits, temporal)?;
            true
        } else {
            false
        };

        let maxsim_note = if indexed_slots_have_maxsim(&generation) {
            "grounded: the live generation carries a multi_maxsim token slot; the substrate fuses its MaxSim late-interaction score at the rank level".to_owned()
        } else {
            "seam (#1663): no multi-vector token slot exists in the Syn* encoder catalog yet, so no MaxSim late-interaction lens is fused; add a token slot to enable it".to_owned()
        };

        Ok(SynapseCalyxFindReport {
            panel_version,
            fusion: params.fusion.name().to_owned(),
            query_kind,
            exact,
            k,
            rrf_k,
            rrf_formula: synapse_find_rrf_formula(rrf_k),
            consulted_slots,
            temporal_applied,
            guard,
            maxsim_note,
            grounding_note: "grounded: every hit carries verified ledger provenance read from the Base SoT; fused retrieval needs no provisional marker (see #1670 for the derived-answer marker convention)".to_owned(),
            generation,
            hits,
        })
    }

    /// Reads the example record's Base constellation and enforces that it
    /// belongs to the active panel generation (find-similar cannot cross panel
    /// generations because slot spaces differ).
    fn read_example_constellation(
        &self,
        cx_id: CxId,
        panel_version: u32,
    ) -> Result<Constellation, SynapseCalyxError> {
        let snapshot = self.vault.snapshot();
        let constellation = self.vault.get(cx_id, snapshot).map_err(|error| {
            SynapseCalyxError::from_calyx(
                &format!("read fused-find example Base row {cx_id}"),
                &error,
            )
        })?;
        if constellation.panel_version != panel_version {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_FIND_EXAMPLE_PANEL_MISMATCH",
                format!(
                    "example {cx_id} is on panel {}, but the search generation being queried is panel {panel_version}",
                    constellation.panel_version
                ),
                "supply an example record from the active panel generation, or rebuild the search indexes for the example's panel",
            ));
        }
        Ok(constellation)
    }

    /// Applies the exact registered temporal policy to the fused candidates and
    /// merges the boosted order + per-component evidence back into the find
    /// hits. Bounded by the #1667 temporal rerank contract.
    fn apply_find_temporal_boost(
        &self,
        hits: Vec<SynapseCalyxFindHit>,
        temporal: SynapseCalyxFindTemporal,
    ) -> Result<Vec<SynapseCalyxFindHit>, SynapseCalyxError> {
        let candidates: Vec<SynapseCalyxTemporalCandidate> = hits
            .iter()
            .map(|hit| SynapseCalyxTemporalCandidate {
                cx_id: hit.cx_id.clone(),
                base_score: hit.score,
            })
            .collect();
        let rerank = self.temporal_rerank_registered(
            &candidates,
            temporal.query_time_secs,
            temporal.tz_offset_secs,
        )?;
        let mut by_cx: BTreeMap<String, SynapseCalyxFindHit> = hits
            .into_iter()
            .map(|hit| (hit.cx_id.clone(), hit))
            .collect();
        let mut boosted = Vec::with_capacity(rerank.hits.len());
        for ranked in rerank.hits {
            let mut hit = by_cx.remove(&ranked.cx_id).ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_FIND_TEMPORAL_RANKING_CORRUPT",
                    format!(
                        "temporal rerank returned {} which was not a fused find candidate",
                        ranked.cx_id
                    ),
                    "inspect the temporal rerank candidate identity handling",
                )
            })?;
            hit.rank = ranked.rank;
            hit.score = ranked.score;
            hit.event_time_secs = Some(ranked.event_time_secs);
            hit.temporal_scores = Some(ranked.temporal_scores);
            boosted.push(hit);
        }
        Ok(boosted)
    }
}

/// Projects the substrate hits into the Synapse find report shape, dropping the
/// query record itself (query-by-example self-match) and deriving the
/// agree/disagree evidence from each hit's per-lens contributions.
fn build_find_hits(
    outcome: &SearchOutcome,
    consulted_slots: &[u16],
    self_cx: Option<CxId>,
) -> Vec<SynapseCalyxFindHit> {
    let consulted: BTreeSet<u16> = consulted_slots.iter().copied().collect();
    let mut out = Vec::with_capacity(outcome.hits.len());
    for hit in &outcome.hits {
        if self_cx == Some(hit.cx_id) {
            continue;
        }
        let agree: BTreeSet<u16> = hit
            .per_lens
            .iter()
            .map(|lens| lens.slot.slot_id().get())
            .collect();
        let disagree: Vec<u16> = consulted
            .iter()
            .filter(|slot| !agree.contains(slot))
            .copied()
            .collect();
        let per_lens = hit
            .per_lens
            .iter()
            .map(|lens| SynapseCalyxFindLensContribution {
                slot: lens.slot.slot_id().get(),
                rank: lens.rank,
                raw_score: lens.raw_score,
                weight: lens.weight,
                contribution: lens.contribution,
            })
            .collect();
        out.push(SynapseCalyxFindHit {
            cx_id: hit.cx_id.to_string(),
            rank: hit.rank,
            score: hit.score,
            per_lens,
            agree_slots: agree.into_iter().collect(),
            disagree_slots: disagree,
            event_time_secs: hit.event_time_secs,
            temporal_scores: hit.temporal_scores,
            provenance_seq: hit.provenance.seq,
            provenance_hash: hex_bytes(&hit.provenance.hash),
            freshness_built_at_seq: hit.freshness.built_at_seq,
            freshness_base_seq: hit.freshness.base_seq,
            freshness_policy: hit.freshness.policy.clone(),
            guard_verdict: hit
                .guard
                .as_ref()
                .map(|guard| guard_verdict(&guard.verdict)),
        });
    }
    // The self-match drop can leave a stale gap at rank 1; renumber so the
    // reported ranks are contiguous.
    for (index, hit) in out.iter_mut().enumerate() {
        hit.rank = index + 1;
    }
    out
}

/// Builds the explicit guard-state readback for one fused pass (#1677).
///
/// This never reports "off" or "applied" on the requested mode alone: it
/// validates the guard artifacts the substrate actually produced. An off pass
/// must have none. A profile-backed pass must attach a verdict to every retained
/// candidate; an empty candidate set is valid and therefore needs no verdict.
///
/// # Errors
///
/// Fails closed with `SYNAPSE_CALYX_FIND_GUARD_STATE_INCONSISTENT` when the
/// substrate attached guard evidence to an unguarded pass — a silent guard would
/// mean the caller is being handed a filtered subset while told it is raw recall.
fn find_guard_readback(
    outcome: &SearchOutcome,
    requested: SynapseCalyxFindGuardMode,
) -> Result<SynapseCalyxFindGuard, SynapseCalyxError> {
    let hits_with_guard_verdict = outcome
        .hits
        .iter()
        .filter(|hit| hit.guard.is_some())
        .count();
    let dropped_candidates = outcome.dropped_guard_hits.len();
    let operator_tau = outcome.guard_tau;
    if matches!(requested, SynapseCalyxFindGuardMode::Off)
        && (operator_tau.is_some() || dropped_candidates > 0 || hits_with_guard_verdict > 0)
    {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_FIND_GUARD_STATE_INCONSISTENT",
            format!(
                "fused find requested guard=off but the substrate returned guard evidence: operator_tau={operator_tau:?} dropped_candidates={dropped_candidates} hits_with_guard_verdict={hits_with_guard_verdict}"
            ),
            "the search substrate applied a guard to a pass that did not request one; inspect calyx-search's guard resolution before trusting any fused-find result set",
        ));
    }
    let applied = !matches!(requested, SynapseCalyxFindGuardMode::Off);
    if applied && operator_tau.is_none() && hits_with_guard_verdict != outcome.hits.len() {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_FIND_GUARD_STATE_INCONSISTENT",
            format!(
                "profile-backed guarded find returned {} retained candidates but only {hits_with_guard_verdict} carried Ward verdicts",
                outcome.hits.len()
            ),
            "inspect the calibrated profile required slots and the search guard projection; guarded recall must expose a verdict for every evaluated candidate",
        ));
    }
    if applied
        && outcome
            .dropped_guard_hits
            .iter()
            .any(|candidate| candidate.reason.trim().is_empty())
    {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_FIND_GUARD_STATE_INCONSISTENT",
            "guarded find dropped a candidate without an auditable reason".to_owned(),
            "inspect calyx-search's guard projection; every rejection must retain either its Ward verdict or the precise reason it could not be scored",
        ));
    }
    Ok(SynapseCalyxFindGuard {
        requested_mode: requested.name().to_owned(),
        applied,
        state_code: if applied {
            "SYNAPSE_CALYX_FIND_GUARD_APPLIED".to_owned()
        } else {
            SYNAPSE_FIND_GUARD_DISABLED_CODE.to_owned()
        },
        operator_tau,
        dropped_candidates,
        hits_with_guard_verdict,
        dropped: outcome
            .dropped_guard_hits
            .iter()
            .map(|hit| SynapseCalyxDroppedGuardHit {
                cx_id: hit.cx_id.to_string(),
                reason: hit.reason.clone(),
                verdict: hit.verdict.as_ref().map(guard_verdict),
            })
            .collect(),
        disabled_reason: (!applied).then(|| SYNAPSE_FIND_GUARD_DISABLED_REASON.to_owned()),
        enable_requirements: if applied {
            Vec::new()
        } else {
            SYNAPSE_FIND_GUARD_ENABLE_REQUIREMENTS
                .iter()
                .map(|line| (*line).to_owned())
                .collect()
        },
    })
}

fn guard_verdict(verdict: &calyx_ward::GuardVerdict) -> SynapseCalyxGuardVerdict {
    SynapseCalyxGuardVerdict {
        guard_id: verdict.guard_id.to_string(),
        overall_pass: verdict.overall_pass,
        provisional: verdict.provisional,
        action: verdict.action.as_ref().map(|action| match action {
            calyx_ward::NoveltyAction::NewRegion => "new_region".to_owned(),
            calyx_ward::NoveltyAction::Quarantine => "quarantine".to_owned(),
            calyx_ward::NoveltyAction::RejectClosed => "refuse".to_owned(),
        }),
        per_slot: verdict
            .per_slot
            .iter()
            .map(|slot| SynapseCalyxFindGuardSlotVerdict {
                slot: slot.slot.get(),
                cosine: slot.cos,
                tau: slot.tau,
                pass: slot.pass,
            })
            .collect(),
    }
}

/// Builds the empty-query failure so it names **why** no slot qualified.
///
/// Three unrelated conditions used to collapse into one message that said only
/// that no vector was produced (#1896): no lens on the panel is declared
/// text-queryable at all; a text-queryable lens exists but measured nothing
/// indexable from this particular query; or it measured fine but carries no
/// persisted index. Each has a different repair — declare the lens, change the
/// query, rebuild the generation — so the error reports the three counts that
/// separate them plus the per-slot skip reasons behind them.
fn no_indexable_query_error(
    panel_version: u32,
    indexed_slots: &BTreeSet<SlotId>,
    measurement: Option<&QueryMeasurement>,
) -> SynapseCalyxError {
    let Some(measurement) = measurement else {
        // by_example: the example's own stored slots never intersected the
        // persisted index, which is an index-coverage fact, not a query one.
        return SynapseCalyxError::new(
            "SYNAPSE_CALYX_FIND_NO_INDEXABLE_QUERY",
            format!(
                "fused find over panel {panel_version} produced no query vector on any persisted-index slot: the example record carries no slot that the persisted generation indexes (indexed_slots={})",
                indexed_slots.len()
            ),
            "supply an example record measured on a lens present in the rebuilt search generation, or run storage operation=search_rebuild so the generation covers the example's slots",
        );
    };

    let measured_but_unindexed: Vec<u16> = measurement
        .vectors
        .iter()
        .map(|(slot, _)| slot.get())
        .filter(|slot| !indexed_slots.contains(&SlotId::new(*slot)))
        .collect();
    let (message_tail, remediation) = if measurement.text_queryable_slots == 0 {
        (
            "no active lens on this panel declares itself text-queryable, so a text query can never produce a vector here regardless of index state".to_owned(),
            "declare a text-queryable lens on this panel (a sparse-text or hash encoder); a one-hot, cyclic-time or numeric lens cannot answer free text by construction",
        )
    } else if measurement.indexable_slots == 0 {
        (
            "every text-queryable lens measured this query to a non-indexable vector (an empty query, or text that tokenizes to nothing)".to_owned(),
            "supply query text that produces at least one token for the panel's text-queryable lens",
        )
    } else if measured_but_unindexed.is_empty() {
        (
            "the measured query slots were all excluded before scoring".to_owned(),
            "run storage operation=search_rebuild so the persisted generation covers the panel's text-queryable slots",
        )
    } else {
        (
            format!(
                "the query measured on slot(s) {measured_but_unindexed:?}, but the persisted generation indexes slot(s) {:?}, so no measured slot can be probed",
                indexed_slots
                    .iter()
                    .map(|slot| slot.get())
                    .collect::<Vec<_>>()
            ),
            "run storage operation=search_rebuild so the persisted generation covers the panel's text-queryable slots",
        )
    };
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_FIND_NO_INDEXABLE_QUERY",
        format!(
            "fused find over panel {panel_version} produced no query vector on any persisted-index slot: {message_tail}. {} indexed_slots={}",
            measurement.diagnostic(),
            indexed_slots.len()
        ),
        remediation,
    )
}

/// Maps a substrate search error onto a Synapse error, naming the rebuild
/// remediation for stale/missing derived indexes so a query over a
/// missing/stale index always fails closed with the exact fix.
///
/// A structured substrate error's **own** remediation travels with it (#1909).
/// It used to be replaced by the generic sentence below, so every code but one
/// reported the same fix regardless of what failed — which is how
/// `CALYX_SEARCH_DELTA_REBASE_REQUIRED` came to advise inspecting the manifest,
/// and how `CALYX_SEARCH_RECONCILED_REPLACEMENTS_MISSING` (#1907) would have.
/// The generic string is now the fallback for the `Io`/`Usage` variants that
/// genuinely carry no catalog remediation, not the default for everything.
fn find_index_error(action: &str, error: &SearchError) -> SynapseCalyxError {
    let code = error.code();
    let message = format!("{action}: {}", error.message());
    // A deliberate re-code, not a pass-through: a stale derived index is
    // reported on the Synapse surface as a find-specific state with the rebuild
    // remediation, because that is the action the operator has to take.
    if code == "CALYX_STALE_DERIVED" {
        return SynapseCalyxError::new(
            "SYNAPSE_CALYX_FIND_INDEX_STALE",
            message,
            REBUILD_REQUIRED_REMEDIATION,
        );
    }
    SynapseCalyxError::new(
        code,
        message,
        error.remediation().unwrap_or(
            "inspect the durable panel state and the named persisted search generation; rebuild the panel search indexes if the manifest is missing or corrupt",
        ),
    )
}

/// True when the live generation carries a multi-vector `MaxSim` token slot.
fn indexed_slots_have_maxsim(generation: &PersistedSearchGeneration) -> bool {
    generation
        .slots
        .iter()
        .any(|slot| slot.kind.starts_with("multi_maxsim"))
}
