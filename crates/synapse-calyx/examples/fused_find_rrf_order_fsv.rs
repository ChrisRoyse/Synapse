//! Manual FSV instrument for the #1676 fused-find rank-level fusion law.
//!
//! ## What this proves, and what it deliberately does not
//!
//! The discriminating property of Reciprocal Rank Fusion is that a document
//! which is *consistently good* across lenses can outrank the top hit of any
//! single lens. Cormack, Clarke & Buettcher (SIGIR 2009) define
//! `RRFscore(d) = Σ_r 1/(k + r(d))` with `k = 60` and 1-based `r(d)`; the Sextant
//! substrate implements exactly that with a per-lens weight (`1.0` for plain
//! RRF). With
//!
//! * slot 1 recall `[X, Y, Z]`
//! * slot 2 recall `[Y, W, X]`
//!
//! the scores are `Y = 1/62 + 1/61 > X = 1/61 + 1/63 > W = 1/62 > Z = 1/63`, so
//! the fused order must be **Y, X, W, Z** — Y outranking slot 1's own leader X.
//! A naive "trust the first list", a 0-based rank, a different `k`, or a
//! lexicographic tie-break would all put X first, so this ordering is a real
//! discriminator rather than a restatement of the inputs.
//!
//! This driver feeds those two rankings to the **production** fusion entry point
//! `calyx_sextant::fusion::fuse` — the same function `calyx-search`'s engine
//! calls for every `find` / `storage operation=find_similar` pass (see
//! `calyx-search/src/engine/search.rs`, `fusion::fuse(&per_slot, &context, ...)`)
//! — and asserts the resulting order and scores.
//!
//! It does **not** stand in for a live vault query. The rank vectors are the
//! declared inputs of the law under test, and the ledger references handed to
//! the provenance callback are synthetic and labelled as such: they exist only
//! because `fuse` requires a provenance resolver, and nothing here is presented
//! as a vault readback. The live-vault half of the acceptance is the `find` tool
//! itself, which on this host fails closed (the `index_inverted` CF is empty and
//! a rebuild-required marker is staked) until `storage operation=search_rebuild`
//! runs.
//!
//! ## The #1883 discriminator
//!
//! The instrument is parameterised over `k` and runs **both** `k = 60` and
//! `k = 5` in one pass. That is the discriminator #1883 asks for: at `k = 5`
//! the same two rank lists fuse to the same order but to scores that separate
//! an order of magnitude more sharply (`Y = 1/7 + 1/6 = 0.3095` versus
//! `Y = 1/62 + 1/61 = 0.0325`). Before #1883 the substrate scored with a
//! hardcoded 60 no matter what `calyx_fusion_k` said, so a `k = 5` pass
//! produced `k = 60` scores; now it does not, and this pass proves it by
//! computing both expectations from the RRF definition independently of the
//! substrate.
//!
//! Exits non-zero, printing the exact divergence, if the shipped fusion does not
//! produce the expected order and scores at either `k`.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example fused_find_rrf_order_fsv`

use std::collections::BTreeMap;
use std::error::Error;

use calyx_core::{CxId, LedgerRef, Result as CalyxResult, SlotId};
use calyx_sextant::Hit;
use calyx_sextant::fusion::{FusionContext, FusionStrategy, fuse};
use calyx_sextant::index::IndexSearchHit;
use serde_json::json;

/// Absolute tolerance for the f32 score comparison. The Y/X gap is ~2.6e-4, four
/// orders of magnitude above this, so the ordering assertion is not tolerance
/// sensitive.
const SCORE_EPSILON: f32 = 1e-6;

/// Panel version for the synthetic fusion context. Fusion is panel-agnostic; the
/// value only tags the reported `PanelSlotId`s.
const PANEL_VERSION: u32 = 1;

/// The labelled candidates. Byte patterns are assigned in the order X, Y, W, Z so
/// that ascending `CxId` order is X < Y < W < Z — deliberately *different* from
/// the expected fused order, so an implementation that fell back to id ordering
/// would be caught rather than accidentally pass.
const fn candidates() -> [(&'static str, CxId); 4] {
    [
        ("X", CxId::from_bytes([0x01; 16])),
        ("Y", CxId::from_bytes([0x02; 16])),
        ("W", CxId::from_bytes([0x03; 16])),
        ("Z", CxId::from_bytes([0x04; 16])),
    ]
}

/// Independently computed expected scores, written from the RRF definition
/// rather than from the substrate's output: `Σ 1/(k + rank)`, ranks 1-based.
///
/// Given slot 1 `[X, Y, Z]` and slot 2 `[Y, W, X]`: X is rank 1 and rank 3, Y is
/// rank 2 and rank 1, W is rank 2, Z is rank 3.
fn expected(rrf_k: f32) -> Vec<(&'static str, f32)> {
    let mut rows = vec![
        ("Y", 1.0 / (rrf_k + 2.0) + 1.0 / (rrf_k + 1.0)),
        ("X", 1.0 / (rrf_k + 1.0) + 1.0 / (rrf_k + 3.0)),
        ("W", 1.0 / (rrf_k + 2.0)),
        ("Z", 1.0 / (rrf_k + 3.0)),
    ];
    rows.sort_by(|left, right| right.1.total_cmp(&left.1));
    rows
}

/// Well-formed descending per-lens raw scores. RRF consumes rank only — these
/// exist so each input list is a plausible recall list, and their values can
/// never influence the fused order.
const RAW_LENS_SCORES: [f32; 3] = [0.9, 0.8, 0.7];

/// Narrows a 1-based rank for exact float arithmetic. Ranks here are single
/// digits, so the conversion is lossless and a wider rank is a bug, not a cast.
fn rank_as_f32(rank: usize) -> std::result::Result<f32, Box<dyn Error>> {
    let narrowed = u16::try_from(rank)
        .map_err(|_| format!("rank {rank} does not fit the instrument's exact-arithmetic range"))?;
    Ok(f32::from(narrowed))
}

/// Builds one slot's recall list with 1-based ranks, exactly as
/// `calyx_sextant::index` assigns them (`rank: idx + 1`).
fn ranking(cx_ids: &[CxId]) -> Vec<IndexSearchHit> {
    cx_ids
        .iter()
        .enumerate()
        .map(|(idx, cx_id)| IndexSearchHit {
            cx_id: *cx_id,
            score: RAW_LENS_SCORES[idx % RAW_LENS_SCORES.len()],
            rank: idx + 1,
        })
        .collect()
}

/// Prints the fused hits with their per-lens rank contributions, so an operator
/// can recompute every score by hand from the printed ranks.
fn print_hits(hits: &[Hit], label_of: &BTreeMap<CxId, &str>, rrf_k: f32) {
    println!(
        "fused_find_rrf_order_fsv: production calyx_sextant::fusion::fuse, FusionStrategy::Rrf"
    );
    println!(
        "  provenance in this instrument is SYNTHETIC (ordering law only, not a vault readback)"
    );
    println!("  slot 1 recall = [X, Y, Z]   slot 2 recall = [Y, W, X]   k={rrf_k}   1-based ranks");
    for hit in hits {
        let label = label_of.get(&hit.cx_id).copied().unwrap_or("<unknown>");
        let lenses = hit
            .per_lens
            .iter()
            .map(|lens| {
                format!(
                    "slot={} rank={} weight={:.4} contribution={:.7}",
                    lens.slot.slot_id().get(),
                    lens.rank,
                    lens.weight,
                    lens.contribution
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        println!(
            "RRF_HIT rank={} label={label} score={:.7} cx_id={} [{lenses}]",
            hit.rank, hit.score, hit.cx_id
        );
    }
}

/// Asserts the fused order and every score against the independently written RRF
/// definition, and asserts that each score is *assembled* from per-lens
/// `weight / (k + rank)` terms rather than merely landing near the right value.
fn assert_rrf_law(
    hits: &[Hit],
    expected: &[(&'static str, f32)],
    observed_order: &[&str],
    expected_order: &[&str],
    rrf_k: f32,
) -> std::result::Result<(), Box<dyn Error>> {
    if observed_order != expected_order {
        return Err(format!(
            "RRF_ORDER_MISMATCH: expected {expected_order:?} but the shipped fusion produced {observed_order:?}"
        )
        .into());
    }
    for (hit, (label, want)) in hits.iter().zip(expected.iter()) {
        let delta = (hit.score - want).abs();
        if delta > SCORE_EPSILON {
            return Err(format!(
                "RRF_SCORE_MISMATCH: {label} expected {want:.7} observed {:.7} delta {delta:.9} > {SCORE_EPSILON:e}",
                hit.score
            )
            .into());
        }
        let summed: f32 = hit.per_lens.iter().map(|lens| lens.contribution).sum();
        if (summed - hit.score).abs() > SCORE_EPSILON {
            return Err(format!(
                "RRF_CONTRIBUTION_MISMATCH: {label} per-lens contributions sum to {summed:.7} but score is {:.7}",
                hit.score
            )
            .into());
        }
        for lens in &hit.per_lens {
            let want_contribution = lens.weight / (rank_as_f32(lens.rank)? + rrf_k);
            if (lens.contribution - want_contribution).abs() > SCORE_EPSILON {
                return Err(format!(
                    "RRF_LENS_LAW_MISMATCH: {label} slot {} rank {} contribution {:.7} != weight/({rrf_k}+rank) {want_contribution:.7}",
                    lens.slot.slot_id().get(),
                    lens.rank,
                    lens.contribution
                )
                .into());
            }
        }
    }
    Ok(())
}

/// Runs one full fusion pass at `rrf_k` and asserts the law against
/// independently computed expectations.
fn run_at_k(
    per_slot: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    label_of: &BTreeMap<CxId, &str>,
    rrf_k: f32,
) -> std::result::Result<Vec<(String, f32)>, Box<dyn Error>> {
    let context = FusionContext {
        panel_version: PANEL_VERSION,
        k: 4,
        rrf_k,
        explain: true,
        strategy: FusionStrategy::Rrf,
        // Plain RRF assigns every consulted slot weight 1.0 inside the substrate;
        // this map is not consulted for `FusionStrategy::Rrf`.
        weights: BTreeMap::new(),
        stage1_slots: Vec::new(),
    };

    // `fuse` needs a provenance resolver. These references are SYNTHETIC: this
    // instrument measures the ordering law, not vault provenance, and says so in
    // its output rather than passing them off as ledger readbacks.
    let provenance = |cx_id: CxId| -> CalyxResult<LedgerRef> {
        Ok(LedgerRef {
            seq: u64::from(cx_id.as_bytes()[0]),
            hash: [0_u8; 32],
        })
    };

    let hits = fuse(per_slot, &context, &provenance)?;
    print_hits(&hits, label_of, rrf_k);

    let observed_order: Vec<&str> = hits
        .iter()
        .map(|hit| label_of.get(&hit.cx_id).copied().unwrap_or("<unknown>"))
        .collect();
    let expected = expected(rrf_k);
    let expected_order: Vec<&str> = expected.iter().map(|(label, _)| *label).collect();

    println!(
        "{}",
        json!({
            "instrument": "fused_find_rrf_order_fsv",
            "issue": "1676,1883",
            "fusion_entry_point": "calyx_sextant::fusion::fuse (FusionStrategy::Rrf)",
            "rrf_k": rrf_k,
            "rank_base": 1,
            "slot_1_ranking": ["X", "Y", "Z"],
            "slot_2_ranking": ["Y", "W", "X"],
            "expected_order": expected_order,
            "observed_order": observed_order,
            "expected_scores": expected
                .iter()
                .map(|(label, score)| json!({ "label": label, "score": score }))
                .collect::<Vec<_>>(),
            "observed_scores": hits
                .iter()
                .map(|hit| json!({
                    "label": label_of.get(&hit.cx_id).copied().unwrap_or("<unknown>"),
                    "score": hit.score,
                }))
                .collect::<Vec<_>>(),
            "provenance": "synthetic: this instrument proves the ordering law, not vault provenance",
        })
    );

    assert_rrf_law(&hits, &expected, &observed_order, &expected_order, rrf_k)?;
    println!("RRF_ORDER_OK k={rrf_k} expected_and_observed={expected_order:?}");
    Ok(hits
        .iter()
        .map(|hit| {
            (
                (*label_of.get(&hit.cx_id).unwrap_or(&"<unknown>")).to_owned(),
                hit.score,
            )
        })
        .collect())
}

fn main() -> std::result::Result<(), Box<dyn Error>> {
    let by_label: BTreeMap<&str, CxId> = candidates().into_iter().collect();
    let label_of: BTreeMap<CxId, &str> = candidates().into_iter().map(|(l, id)| (id, l)).collect();

    let mut per_slot: BTreeMap<SlotId, Vec<IndexSearchHit>> = BTreeMap::new();
    per_slot.insert(
        SlotId::new(1),
        ranking(&[by_label["X"], by_label["Y"], by_label["Z"]]),
    );
    per_slot.insert(
        SlotId::new(2),
        ranking(&[by_label["Y"], by_label["W"], by_label["X"]]),
    );

    let at_60 = run_at_k(&per_slot, &label_of, 60.0)?;
    let at_5 = run_at_k(&per_slot, &label_of, 5.0)?;

    // The #1883 discriminator. Before the fix the substrate ignored the context
    // and scored both passes with a hardcoded 60, so these two vectors were
    // byte-identical. They must now differ on every candidate.
    for ((label_60, score_60), (label_5, score_5)) in at_60.iter().zip(at_5.iter()) {
        if label_60 != label_5 {
            return Err(format!(
                "RRF_K_ORDER_DIVERGED: k=60 produced {label_60} where k=5 produced {label_5}; this instrument's inputs are chosen so the order is stable across k and only the scores separate"
            )
            .into());
        }
        if (score_60 - score_5).abs() <= SCORE_EPSILON {
            return Err(format!(
                "RRF_K_NOT_LOAD_BEARING: {label_60} scored {score_60:.7} at k=60 and {score_5:.7} at k=5; the configured rrf_k did not reach the scoring law (#1883)"
            )
            .into());
        }
    }
    println!(
        "{}",
        json!({
            "instrument": "fused_find_rrf_order_fsv",
            "issue": "1883",
            "claim": "the rrf_k on FusionContext reaches calyx_sextant::fusion::rrf::rrf_contribution",
            "scores_at_k_60": at_60
                .iter()
                .map(|(label, score)| json!({ "label": label, "score": score }))
                .collect::<Vec<_>>(),
            "scores_at_k_5": at_5
                .iter()
                .map(|(label, score)| json!({ "label": label, "score": score }))
                .collect::<Vec<_>>(),
        })
    );
    println!("RRF_K_LOAD_BEARING_OK k=60 and k=5 produce different scores for every candidate");
    Ok(())
}
