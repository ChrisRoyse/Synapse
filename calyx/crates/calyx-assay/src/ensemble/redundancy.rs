mod plan;
mod sketch;

mod cuda;

use std::collections::{BTreeMap, BTreeSet};

use calyx_core::{CalyxError, Result, SlotId};

#[cfg(not(feature = "cuda"))]
use crate::cuda_strict::cuda_unavailable;
use crate::cuda_strict::strict_cuda_requested;
use crate::ksg::MIN_ASSAY_SAMPLES;
use crate::nmi::partitioned_histogram_nmi;

use self::cuda::ensemble_redundancy_from_lenses_cuda_strict_impl;

use super::model::{
    ENSEMBLE_CARD_SCHEMA_VERSION, EnsembleCard, EnsembleLensInput, EnsemblePairRedundancyEvidence,
    EnsembleRedundancyEvidence, EnsembleRedundancyMethod,
};

pub use plan::{
    DEFAULT_LINEAR_CKA_SEED, LINEAR_CKA_JACKKNIFE_BLOCKS, LINEAR_CKA_TUPLES_PER_ROW,
    LinearCkaTuplePlan, MAX_LINEAR_CKA_TUPLES, MIN_LINEAR_CKA_TUPLES, linear_cka_tuple_plan,
};
pub use sketch::{LinearCkaSketch, linear_cka_sketch_from_row_fn, linear_cka_sketch_from_rows};

pub const LINEAR_CKA_REDUNDANCY_METHOD: &str = "debiased_linear_cka_hsic1_u4_v1";
const EXACT_TUPLE_DESIGN: &str = "complete_four_subset_enumeration_v1";
const SAMPLED_TUPLE_DESIGN: &str = "blake3_counter_uniform_four_distinct_with_replacement_v1";
const EXACT_UNCERTAINTY_METHOD: &str = "none_complete_tuple_population";
const SAMPLED_UNCERTAINTY_METHOD: &str = "delete_32_group_jackknife_ratio_v1";
const GATE_SCORE_METHOD: &str = "max_0_raw_plus_4_mc_se_clamped_1_fail_closed_v1";

#[derive(Clone, Debug)]
pub struct EnsembleRedundancySketchInput {
    name: String,
    slot: SlotId,
    nmi_signature: Vec<f32>,
    linear_cka: LinearCkaSketch,
}

impl EnsembleRedundancySketchInput {
    pub fn new(
        name: impl Into<String>,
        slot: SlotId,
        nmi_signature: Vec<f32>,
        linear_cka: LinearCkaSketch,
    ) -> Self {
        Self {
            name: name.into(),
            slot,
            nmi_signature,
            linear_cka,
        }
    }
}

pub fn ensemble_redundancy_from_lenses(
    lenses: &[EnsembleLensInput],
    nmi_bins: usize,
) -> Result<EnsembleRedundancyEvidence> {
    if strict_cuda_requested() {
        return ensemble_redundancy_from_lenses_cuda_strict(lenses, nmi_bins);
    }
    let row_count = lenses.first().map(|lens| lens.vectors.len()).unwrap_or(0);
    let plan = linear_cka_tuple_plan(row_count)?;
    let mut sketches = Vec::with_capacity(lenses.len());
    for lens in lenses {
        let linear_cka = linear_cka_sketch_from_rows(&plan, &lens.vectors)?;
        sketches.push(EnsembleRedundancySketchInput::new(
            lens.name.clone(),
            lens.slot,
            row_signature(&lens.vectors)?,
            linear_cka,
        ));
    }
    ensemble_redundancy_from_sketches(&plan, &sketches, nmi_bins)
}

pub fn ensemble_redundancy_from_lenses_cuda_strict(
    lenses: &[EnsembleLensInput],
    nmi_bins: usize,
) -> Result<EnsembleRedundancyEvidence> {
    ensemble_redundancy_from_lenses_cuda_strict_impl(lenses, nmi_bins)
}

pub fn ensemble_redundancy_from_sketches(
    plan: &LinearCkaTuplePlan,
    lenses: &[EnsembleRedundancySketchInput],
    nmi_bins: usize,
) -> Result<EnsembleRedundancyEvidence> {
    validate_sketch_inputs(plan, lenses)?;
    let mut pairs = Vec::new();
    for a in 0..lenses.len() {
        for b in (a + 1)..lenses.len() {
            let linear_cka = sketch::estimate_pair(
                &lenses[a].linear_cka,
                &lenses[b].linear_cka,
                plan.is_exact(),
            )?;
            // A refusal here used to say only "x" or "y": the operator got a
            // degenerate-column error with no way to tell *which* of a dozen
            // lenses caused it, on a path that then aborted the whole card.
            // Name both sides (#1897's localisation rule, #1943).
            let nmi = partitioned_histogram_nmi(
                &lenses[a].nmi_signature,
                &lenses[b].nmi_signature,
                nmi_bins,
            )
            .map_err(|error| CalyxError {
                message: format!(
                    "NMI redundancy for x={} slot {} vs y={} slot {}: {}",
                    lenses[a].name, lenses[a].slot, lenses[b].name, lenses[b].slot, error.message
                ),
                ..error
            })?
            .nmi;
            pairs.push(EnsemblePairRedundancyEvidence {
                a: lenses[a].name.clone(),
                b: lenses[b].name.clone(),
                slot_a: lenses[a].slot,
                slot_b: lenses[b].slot,
                linear_cka,
                nmi,
            });
        }
    }
    Ok(EnsembleRedundancyEvidence {
        method: redundancy_method(plan),
        pairs,
    })
}

pub(super) fn validate_evidence(
    lenses: &[EnsembleLensInput],
    evidence: &EnsembleRedundancyEvidence,
) -> Result<()> {
    let roster = lenses
        .iter()
        .map(|lens| (lens.slot, lens.name.as_str()))
        .collect::<BTreeMap<_, _>>();
    validate_evidence_for_roster(&roster, evidence)
}

/// Validates the redundancy provenance required at persisted-card trust boundaries.
///
/// Serde keeps legacy cards decodable for diagnostic migration, but evidence consumers must require
/// the current schema so relabeling malformed evidence as legacy cannot bypass validation.
pub fn validate_ensemble_card_redundancy(card: &EnsembleCard) -> Result<()> {
    if card.schema_version != ENSEMBLE_CARD_SCHEMA_VERSION {
        return Err(CalyxError::assay_degenerate_input(format!(
            "unsupported EnsembleCard schema {}; expected {ENSEMBLE_CARD_SCHEMA_VERSION}",
            card.schema_version
        )));
    }
    let method = card.redundancy_method.clone().ok_or_else(|| {
        CalyxError::assay_degenerate_input(
            "current-schema EnsembleCard is missing redundancy method metadata",
        )
    })?;
    let mut roster = BTreeMap::new();
    let mut names = BTreeSet::new();
    let conditioning_roster = card
        .lenses
        .iter()
        .map(|lens| (lens.slot, lens.name.as_str()))
        .collect::<Vec<_>>();
    for lens in &card.lenses {
        if roster.insert(lens.slot, lens.name.as_str()).is_some()
            || !names.insert(lens.name.as_str())
        {
            return Err(CalyxError::assay_degenerate_input(
                "EnsembleCard lens names and slots must be unique",
            ));
        }
    }
    crate::logistic::validate_conditioning_provenance(
        &card.conditioning,
        &conditioning_roster,
        card.n_samples,
    )?;
    let mut pairs = Vec::with_capacity(card.pairs.len());
    for pair in &card.pairs {
        let linear_cka = pair.redundancy.clone().ok_or_else(|| {
            CalyxError::assay_degenerate_input(format!(
                "current-schema EnsembleCard pair {}:{} is missing redundancy evidence",
                pair.slot_a, pair.slot_b
            ))
        })?;
        if !pair.corr.is_finite() || (pair.corr - linear_cka.mc_gate_upper_estimate).abs() > 1.0e-6
        {
            return Err(CalyxError::assay_degenerate_input(format!(
                "EnsembleCard pair {}:{} corr {} != redundancy gate {}",
                pair.slot_a, pair.slot_b, pair.corr, linear_cka.mc_gate_upper_estimate
            )));
        }
        pairs.push(EnsemblePairRedundancyEvidence {
            a: pair.a.clone(),
            b: pair.b.clone(),
            slot_a: pair.slot_a,
            slot_b: pair.slot_b,
            linear_cka,
            nmi: pair.nmi,
        });
    }
    validate_evidence_for_roster(&roster, &EnsembleRedundancyEvidence { method, pairs })
}

fn validate_evidence_for_roster(
    roster: &BTreeMap<SlotId, &str>,
    evidence: &EnsembleRedundancyEvidence,
) -> Result<()> {
    validate_redundancy_method_metadata(&evidence.method)?;
    let expected_pairs = roster.len().saturating_sub(1) * roster.len() / 2;
    if evidence.pairs.len() != expected_pairs {
        return Err(CalyxError::assay_degenerate_input(format!(
            "ensemble redundancy pairs {} != expected {expected_pairs}",
            evidence.pairs.len()
        )));
    }
    let mut pair_keys = BTreeSet::new();
    for pair in &evidence.pairs {
        let Some(expected_a) = roster.get(&pair.slot_a) else {
            return Err(CalyxError::assay_degenerate_input(format!(
                "redundancy evidence has unknown slot {}",
                pair.slot_a
            )));
        };
        let Some(expected_b) = roster.get(&pair.slot_b) else {
            return Err(CalyxError::assay_degenerate_input(format!(
                "redundancy evidence has unknown slot {}",
                pair.slot_b
            )));
        };
        if pair.slot_a == pair.slot_b || pair.a != *expected_a || pair.b != *expected_b {
            return Err(CalyxError::assay_degenerate_input(
                "redundancy evidence pair names do not match its slots",
            ));
        }
        let key = if pair.slot_a < pair.slot_b {
            (pair.slot_a, pair.slot_b)
        } else {
            (pair.slot_b, pair.slot_a)
        };
        if !pair_keys.insert(key) {
            return Err(CalyxError::assay_degenerate_input(
                "redundancy evidence contains a duplicate pair",
            ));
        }
        validate_pair(pair)?;
    }
    Ok(())
}

pub fn validate_redundancy_method_metadata(method: &EnsembleRedundancyMethod) -> Result<()> {
    let (tuple_design, uncertainty_method, uncertainty_blocks) = if method.exact {
        (EXACT_TUPLE_DESIGN, EXACT_UNCERTAINTY_METHOD, 0)
    } else {
        (
            SAMPLED_TUPLE_DESIGN,
            SAMPLED_UNCERTAINTY_METHOD,
            LINEAR_CKA_JACKKNIFE_BLOCKS,
        )
    };
    let valid = method.metric == LINEAR_CKA_REDUNDANCY_METHOD
        && method.tuple_design == tuple_design
        && method.row_count >= MIN_ASSAY_SAMPLES
        && method.tuple_count > 0
        && valid_seed_hex(&method.seed_hex)
        && valid_blake3(&method.tuple_plan_blake3)
        && method.uncertainty_method == uncertainty_method
        && method.uncertainty_blocks == uncertainty_blocks
        && method.gate_score_method == GATE_SCORE_METHOD;
    if !valid {
        return Err(CalyxError::assay_degenerate_input(
            "ensemble redundancy method metadata is incomplete or unsupported",
        ));
    }
    Ok(())
}

fn valid_seed_hex(seed: &str) -> bool {
    seed.strip_prefix("0x")
        .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn valid_blake3(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_pair(pair: &EnsemblePairRedundancyEvidence) -> Result<()> {
    let estimate = &pair.linear_cka;
    let expected_point = estimate.raw_signed_point.max(0.0);
    let valid = estimate.raw_signed_point.is_finite()
        && (-1.0..=1.0).contains(&estimate.raw_signed_point)
        && estimate.redundancy_point.is_finite()
        && (0.0..=1.0).contains(&estimate.redundancy_point)
        && (estimate.redundancy_point - expected_point).abs() <= 1.0e-5
        && estimate.mc_standard_error.is_finite()
        && estimate.mc_standard_error >= 0.0
        && estimate.mc_gate_upper_estimate.is_finite()
        && (estimate.redundancy_point..=1.0).contains(&estimate.mc_gate_upper_estimate)
        && pair.nmi.is_finite()
        && (0.0..=1.0).contains(&pair.nmi);
    if !valid {
        return Err(CalyxError::assay_degenerate_input(format!(
            "invalid redundancy evidence for {} and {}",
            pair.a, pair.b
        )));
    }
    Ok(())
}

fn validate_sketch_inputs(
    plan: &LinearCkaTuplePlan,
    lenses: &[EnsembleRedundancySketchInput],
) -> Result<()> {
    if plan.row_count() < MIN_ASSAY_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "ensemble redundancy requires at least {MIN_ASSAY_SAMPLES} rows; got {}",
            plan.row_count()
        )));
    }
    let mut slots = BTreeSet::new();
    let mut names = BTreeSet::new();
    for lens in lenses {
        if !slots.insert(lens.slot) || !names.insert(lens.name.as_str()) {
            return Err(CalyxError::assay_insufficient_samples(
                "ensemble redundancy lens names and slots must be unique",
            ));
        }
        if !lens.linear_cka.matches(plan) {
            return Err(CalyxError::assay_degenerate_input(format!(
                "lens {} was not sketched with the shared tuple plan",
                lens.name
            )));
        }
        if lens.nmi_signature.len() != plan.row_count() {
            return Err(CalyxError::assay_insufficient_samples(format!(
                "lens {} NMI signature rows {} != {}",
                lens.name,
                lens.nmi_signature.len(),
                plan.row_count()
            )));
        }
    }
    Ok(())
}

fn redundancy_method(plan: &LinearCkaTuplePlan) -> EnsembleRedundancyMethod {
    EnsembleRedundancyMethod {
        metric: LINEAR_CKA_REDUNDANCY_METHOD.to_string(),
        tuple_design: if plan.is_exact() {
            EXACT_TUPLE_DESIGN
        } else {
            SAMPLED_TUPLE_DESIGN
        }
        .to_string(),
        row_count: plan.row_count(),
        tuple_count: plan.tuple_count(),
        seed_hex: format!("0x{:016x}", plan.seed()),
        tuple_plan_blake3: plan.digest_hex(),
        exact: plan.is_exact(),
        uncertainty_method: if plan.is_exact() {
            EXACT_UNCERTAINTY_METHOD
        } else {
            SAMPLED_UNCERTAINTY_METHOD
        }
        .to_string(),
        uncertainty_blocks: if plan.is_exact() {
            0
        } else {
            LINEAR_CKA_JACKKNIFE_BLOCKS
        },
        gate_score_method: GATE_SCORE_METHOD.to_string(),
    }
}

/// Domain separator for the NMI signature's projection direction. Bumping it
/// changes every signature, so it is part of the method's identity.
const NMI_SIGNATURE_PROJECTION_METHOD: &[u8] = b"calyx-ensemble-nmi-signature-projection-v1";

/// Reduces each row of a lens to one scalar for the pairwise NMI redundancy
/// term, by projecting it onto a **fixed pseudorandom direction**.
///
/// # Why not the row mean (#1943)
///
/// The signature used to be the row mean — a projection onto the all-ones
/// direction — and that direction is degenerate for a large and *deliberate*
/// class of lenses: any encoder whose rows share a sum has an exactly constant
/// mean. A one-hot, an L1-normalized hash, a densified single-cell categorical
/// slot are all in that class, and a deterministic-encoder panel is mostly made
/// of them. A constant column has zero entropy, so the NMI term refused it and
/// took the entire capability card down with it — a whole-pass abort caused by
/// the choice of projection, not by the data.
///
/// The all-ones direction also discards the only thing a one-hot carries: the
/// mean of a one-hot is `1/width` no matter *which* index fired, so even where
/// it did not refuse it measured nothing.
///
/// A fixed pseudorandom direction removes both faults. By the
/// Johnson-Lindenstrauss lemma a random projection preserves pairwise distances
/// in expectation, so binning it is a coarse but faithful sketch of the
/// row-to-row structure; and two rows that differ in the vector differ in the
/// projection except on a measure-zero set. The direction is derived by blake3
/// from the width alone under a fixed domain separator, so it is reproducible
/// from the data with no stored state, and identical inputs give identical
/// signatures on every host.
///
/// A signature that is *still* constant now means the lens itself is constant,
/// which is a real finding about the lens rather than an artefact of the
/// reduction.
pub fn ensemble_nmi_signature(rows: &[Vec<f32>]) -> Result<Vec<f32>> {
    row_signature(rows)
}

fn row_signature(rows: &[Vec<f32>]) -> Result<Vec<f32>> {
    let Some(width) = rows.first().map(Vec::len) else {
        return Ok(Vec::new());
    };
    if width == 0 {
        return Err(CalyxError::assay_degenerate_input(
            "ensemble NMI signature row 0 is empty",
        ));
    }
    for (index, row) in rows.iter().enumerate() {
        if row.len() != width {
            return Err(CalyxError::assay_degenerate_input(format!(
                "ensemble NMI signature row {index} has width {} but row 0 has width {width}",
                row.len()
            )));
        }
        if let Some(position) = row.iter().position(|value| !value.is_finite()) {
            return Err(CalyxError::assay_degenerate_input(format!(
                "ensemble NMI signature row {index} is non-finite at dimension {position}"
            )));
        }
    }

    // Standardize each dimension before projecting. Without it one large-scale
    // dimension owns the projection: a record vector carrying an epoch
    // millisecond field (~1.7e12) alongside unit-scale fields projects to a
    // value whose f32 mantissa cannot represent the contribution of anything
    // else, so every row rounds to the *same* scalar and a lens with 31,421
    // distinct vectors sketches as a constant (#1943). Standardizing is also
    // what makes a random projection meaningful: Johnson-Lindenstrauss
    // preserves distances in the space it is applied to, and unstandardized
    // distances are dominated by whichever field happens to have the largest
    // units.
    let count = rows.len() as f64;
    let mut mean = vec![0.0f64; width];
    for row in rows {
        for (accumulator, value) in mean.iter_mut().zip(row) {
            *accumulator += f64::from(*value);
        }
    }
    for value in &mut mean {
        *value /= count;
    }
    let mut sigma = vec![0.0f64; width];
    for row in rows {
        for ((accumulator, value), centre) in sigma.iter_mut().zip(row).zip(&mean) {
            let delta = f64::from(*value) - centre;
            *accumulator += delta * delta;
        }
    }
    for value in &mut sigma {
        *value = (*value / count).sqrt();
    }

    let direction = projection_direction(width);
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let mut projected = 0.0f64;
            for (((value, centre), scale), weight) in
                row.iter().zip(&mean).zip(&sigma).zip(&direction)
            {
                // A zero-variance dimension carries no row-to-row information
                // and would divide by zero; it contributes nothing, which is
                // exactly its information content.
                if *scale > 0.0 {
                    projected += (f64::from(*value) - centre) / scale * f64::from(*weight);
                }
            }
            if !projected.is_finite() {
                return Err(CalyxError::assay_degenerate_input(format!(
                    "ensemble NMI signature row {index} projected to a non-finite value"
                )));
            }
            Ok(projected as f32)
        })
        .collect()
}

/// The fixed pseudorandom unit-scale direction for a given width.
///
/// Each coordinate is a blake3 draw mapped to `[-1, 1)`, then the whole vector
/// is scaled by `1/sqrt(width)` so the projection of a standardized row does
/// not grow with the lens dimension — a 2048-wide lens and a 2-wide lens land
/// on comparable scales, which matters because the signature is binned over its
/// own range.
fn projection_direction(width: usize) -> Vec<f32> {
    let scale = 1.0 / (width as f64).sqrt();
    (0..width)
        .map(|index| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(NMI_SIGNATURE_PROJECTION_METHOD);
            hasher.update(&(width as u128).to_le_bytes());
            hasher.update(&(index as u128).to_le_bytes());
            let bytes = hasher.finalize();
            let mut draw = [0_u8; 8];
            draw.copy_from_slice(&bytes.as_bytes()[..8]);
            // u64 -> [-1, 1): exact in f64, and deterministic on every host.
            let unit = (u64::from_le_bytes(draw) as f64) / (2.0_f64.powi(64));
            ((unit * 2.0 - 1.0) * scale) as f32
        })
        .collect()
}
