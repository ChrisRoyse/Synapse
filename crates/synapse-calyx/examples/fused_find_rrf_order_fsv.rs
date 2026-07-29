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
//! Exits non-zero, printing the exact divergence, if the shipped fusion does not
//! produce the expected order and scores.
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
/// rather than from the substrate's output: `Σ 1/(60 + rank)`, ranks 1-based.
fn expected() -> Vec<(&'static str, f32)> {
    vec![
        ("Y", 1.0 / 62.0 + 1.0 / 61.0),
        ("X", 1.0 / 61.0 + 1.0 / 63.0),
        ("W", 1.0 / 62.0),
        ("Z", 1.0 / 63.0),
    ]
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
fn print_hits(hits: &[Hit], label_of: &BTreeMap<CxId, &str>) {
    println!(
        "fused_find_rrf_order_fsv: production calyx_sextant::fusion::fuse, FusionStrategy::Rrf"
    );
    println!(
        "  provenance in this instrument is SYNTHETIC (ordering law only, not a vault readback)"
    );
    println!("  slot 1 recall = [X, Y, Z]   slot 2 recall = [Y, W, X]   k=60   1-based ranks");
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
/// `weight / (60 + rank)` terms rather than merely landing near the right value.
fn assert_rrf_law(
    hits: &[Hit],
    expected: &[(&'static str, f32)],
    observed_order: &[&str],
    expected_order: &[&str],
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
            let want_contribution = lens.weight / (rank_as_f32(lens.rank)? + 60.0);
            if (lens.contribution - want_contribution).abs() > SCORE_EPSILON {
                return Err(format!(
                    "RRF_LENS_LAW_MISMATCH: {label} slot {} rank {} contribution {:.7} != weight/(60+rank) {want_contribution:.7}",
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

    let context = FusionContext {
        panel_version: PANEL_VERSION,
        k: 4,
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

    let hits = fuse(&per_slot, &context, &provenance)?;
    print_hits(&hits, &label_of);

    let observed_order: Vec<&str> = hits
        .iter()
        .map(|hit| label_of.get(&hit.cx_id).copied().unwrap_or("<unknown>"))
        .collect();
    let expected = expected();
    let expected_order: Vec<&str> = expected.iter().map(|(label, _)| *label).collect();

    println!(
        "{}",
        json!({
            "instrument": "fused_find_rrf_order_fsv",
            "issue": "1676",
            "fusion_entry_point": "calyx_sextant::fusion::fuse (FusionStrategy::Rrf)",
            "rrf_k": 60,
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

    assert_rrf_law(&hits, &expected, &observed_order, &expected_order)?;
    println!("RRF_ORDER_OK expected_and_observed={expected_order:?}");
    Ok(())
}
