//! Phase-5 Compose: fused find-similar search (#1676).
//!
//! Synapse-side facade over `calyx-search`'s persisted per-slot recall → RRF
//! fusion → provenance path, with a bounded temporal post-boost (#1667) and an
//! explainable agree/disagree evidence surface. Two query modes:
//!
//! * **Query-by-example** — a record key (`cx_id`); its own stored slot vectors
//!   become the multi-slot query. "Find episodes like this one."
//! * **Query-by-text** — a text string measured through the active panel's text
//!   lenses (the BM25-able sparse slots).
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
//! `0` elsewhere) so a caller can compare pure vector recall vs. pure BM25 recall
//! against fusion. `k = 60` is Cormack et al.'s validated default and matches
//! the Sextant substrate; BM25's `k1`/`b` live in the frozen sparse-inverted
//! index built at rebuild time and are not re-parameterized per query.
//!
//! ## Seams
//! * **Ward guarded search (#1677)** — always `GuardChoice::Off` here; the field
//!   is wired so a follow-up can flip it to the calibrated in-region guard
//!   without touching callers. See [`SynapseCalyxFindReport::guard_note`].
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

use calyx_core::{Constellation, CxId, SlotId, VaultStore};
use calyx_registry::load_vault_panel_state;
use calyx_search::{
    FusionChoice, GuardChoice, PersistedSearchGeneration, PersistedSearchIndexes,
    REBUILD_REQUIRED_REMEDIATION, SearchBudget, SearchError, SearchFreshness, SearchOutcome,
    measure_query_vectors, read_rebuild_required_marker,
    search_outcome_with_query_vectors_freshness,
};
use calyx_sextant::TemporalScores;
use serde::{Deserialize, Serialize};

use crate::{
    SynapseCalyxError, SynapseCalyxTemporalCandidate, SynapseCalyxVault, hex_bytes, parse_cx_id,
};

/// Hard upper bound on `k` for one fused-find pass. Matches the temporal rerank
/// candidate cap so a temporally-boosted find never exceeds the #1667 bound.
pub const SYNAPSE_FIND_MAX_K: usize = 1_000;

/// The RRF rank constant used by the Sextant substrate, surfaced for evidence.
pub const SYNAPSE_FIND_RRF_K: u32 = 60;

/// Which record set the query is drawn from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SynapseCalyxFindQuery {
    /// Query-by-example: the stored slot vectors of this content-addressed
    /// record become the multi-slot query.
    ByExample { cx_id: String },
    /// Query-by-text: measured through the active panel's text lenses.
    ByText { text: String },
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
    /// recall, or a sparse title/text slot for pure BM25 recall).
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}

/// Result of one fused find-similar pass, with the physical generation readback.
///
/// Serialize-only: it embeds the immutable [`PersistedSearchGeneration`] manifest
/// readback (like the search-rebuild report).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SynapseCalyxFindReport {
    pub panel_version: u32,
    pub fusion: String,
    /// `by_example:<cx_id>` or `by_text`.
    pub query_kind: String,
    pub k: usize,
    pub rrf_k: u32,
    /// Slots that carried a query vector and a persisted index — the lenses the
    /// fusion actually consulted.
    pub consulted_slots: Vec<u16>,
    pub temporal_applied: bool,
    /// Ward guarded-search seam (#1677): currently off.
    pub guard_note: String,
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
    #[allow(clippy::too_many_lines)]
    pub fn find_similar(
        &self,
        params: &SynapseCalyxFindParams,
    ) -> Result<SynapseCalyxFindReport, SynapseCalyxError> {
        // Hot-path boundary (#1686): fused find is an off-runtime intelligence
        // query and must never be driven from a tagged reflex/capture tick.
        crate::lowering::hot_context::assert_cold_calyx("find_similar");

        let vault_dir = self.config.vault_dir.as_path();
        let state = load_vault_panel_state(vault_dir).map_err(|error| {
            SynapseCalyxError::from_calyx("load durable panel state for fused find", &error)
        })?;
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
        let (query_vectors, query_kind, self_cx) = match &params.query {
            SynapseCalyxFindQuery::ByExample { cx_id } => {
                let example = parse_cx_id(cx_id)?;
                let constellation = self.read_example_constellation(example, panel_version)?;
                let vectors = constellation
                    .slots
                    .iter()
                    .filter(|(slot, _)| indexed_slots.contains(slot))
                    .map(|(slot, vector)| (*slot, vector.clone()))
                    .collect::<Vec<_>>();
                (vectors, format!("by_example:{example}"), Some(example))
            }
            SynapseCalyxFindQuery::ByText { text } => {
                let measured = measure_query_vectors(&state, text)
                    .map_err(|error| find_index_error("measure fused-find text query", &error))?;
                let vectors = measured
                    .into_iter()
                    .filter(|(slot, _)| indexed_slots.contains(slot))
                    .collect::<Vec<_>>();
                (vectors, "by_text".to_owned(), None)
            }
        };

        if query_vectors.is_empty() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_FIND_NO_INDEXABLE_QUERY",
                format!(
                    "fused find over panel {panel_version} produced no query vector on any persisted-index slot"
                ),
                "supply a record/text that measures at least one lens present in the rebuilt search generation, or rebuild the panel search indexes",
            ));
        }

        let consulted_slots: Vec<u16> = {
            let mut slots = query_vectors
                .iter()
                .map(|(slot, _)| slot.get())
                .collect::<Vec<_>>();
            slots.sort_unstable();
            slots.dedup();
            slots
        };

        let k = params.k.clamp(1, SYNAPSE_FIND_MAX_K);
        // Ward guarded search is a documented seam (#1677); keep it off so the
        // result is the honest fused recall, never a silently guarded subset.
        let outcome: SearchOutcome = search_outcome_with_query_vectors_freshness(
            &self.vault,
            vault_dir,
            &state.panel,
            &query_vectors,
            k,
            params.fusion.to_choice(),
            GuardChoice::Off,
            params.filter.as_deref(),
            params.explain,
            SearchFreshness::Fresh,
            SearchBudget::disabled(),
            None,
        )
        .map_err(|error| find_index_error("run fused persisted search", &error))?;

        let mut hits = build_find_hits(&outcome, &consulted_slots, self_cx);

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
            k,
            rrf_k: SYNAPSE_FIND_RRF_K,
            consulted_slots,
            temporal_applied,
            guard_note: "seam (#1677): ward in-region guard is off; hits are the unguarded fused recall".to_owned(),
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
                    "example {cx_id} is on panel {}, but the active search generation is panel {panel_version}",
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
        });
    }
    // The self-match drop can leave a stale gap at rank 1; renumber so the
    // reported ranks are contiguous.
    for (index, hit) in out.iter_mut().enumerate() {
        hit.rank = index + 1;
    }
    out
}

/// Maps a substrate search error onto a Synapse error, naming the rebuild
/// remediation for stale/missing derived indexes so a query over a
/// missing/stale index always fails closed with the exact fix.
fn find_index_error(action: &str, error: &SearchError) -> SynapseCalyxError {
    let code = error.code();
    let message = format!("{action}: {}", error.message());
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
        "inspect the durable panel state and the named persisted search generation; rebuild the panel search indexes if the manifest is missing or corrupt",
    )
}

/// True when the live generation carries a multi-vector `MaxSim` token slot.
fn indexed_slots_have_maxsim(generation: &PersistedSearchGeneration) -> bool {
    generation
        .slots
        .iter()
        .any(|slot| slot.kind.starts_with("multi_maxsim"))
}
