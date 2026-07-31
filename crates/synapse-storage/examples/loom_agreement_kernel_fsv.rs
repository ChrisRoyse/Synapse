//! Manual FSV instrument: does routing the weave cosine at the shared
//! dispatched kernel change the values Loom persists, and by how much? (#1917)
//!
//! ## Why this cannot be verified on the live vault alone
//!
//! The obvious check is to re-run `storage operation=intelligence
//! operation=weave` and compare `mean_agreement` before and after. That does
//! not work here, and the reason is worth recording so nobody tries it again.
//!
//! A bounded weave reports `records_scanned=556 records_woven=300`: it weaves a
//! **window** of the panel, and the vault is live and growing. Two runs an hour
//! apart weave two different 300-record windows, so any change in
//! `mean_agreement` is confounded with corpus drift. Measured directly — the
//! same bounded weave (`panel_version=1900001 knn_k=4 max_records=300`) that
//! #1917 recorded as `-0.19910509884357452` read back `-0.20971588790416718` on
//! the *unchanged* build, purely because the vault had grown. A comparison
//! whose noise floor exceeds the effect it is measuring proves nothing.
//!
//! What *is* stable across that drift is the row counts, so those stay a live
//! check. The value delta is measured here instead, against a fixed corpus.
//!
//! ## What this measures
//!
//! The production `calyx_loom::cross_term::agreement_scalar` against
//! [`agreement_scalar_pre_1917`] — the scalar loop it replaced, reproduced
//! verbatim from the pre-change source. That is the previous production code,
//! not a model of it, which is what makes the delta a measurement of the change
//! rather than of someone's idea of the change.
//!
//! Both are run over the same deterministic corpus at four dimensions chosen
//! from the shapes the weave actually sees, including one that is not a
//! multiple of the 8-wide accumulator so the kernel's scalar tail is exercised.
//!
//! ```text
//! cargo run --release -p synapse-storage --example loom_agreement_kernel_fsv
//! ```
//!
//! ## The claims
//!
//! 1. Known answers: identical, antiparallel and orthogonal vectors score
//!    1.0, -1.0 and 0.0 under the production kernel.
//! 2. The delta against the pre-#1917 loop is bounded by f32 reassociation.
//! 3. The dispatched and portable reduction paths agree **bit-for-bit** on this
//!    host, so the persisted value does not depend on which CPU wrote it.
//! 4. Every refusal the old loop made, the new one still makes, with the same
//!    error code.
//! 5. One refusal is new: finite inputs whose squares overflow f32 used to
//!    return a silently wrong number and now fail closed.

use std::error::Error;

use calyx_loom::cross_term::agreement_scalar;

/// The exact reduction `agreement_scalar` used before #1917, kept as the
/// reference the delta is measured against.
///
/// Reproduced verbatim, including the f32 accumulators and the left-associative
/// fold order — that fold order is the entire source of the difference, so
/// paraphrasing it would measure the wrong thing.
fn agreement_scalar_pre_1917(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    if !a.iter().all(|v| v.is_finite()) || !b.iter().all(|v| v.is_finite()) {
        return None;
    }
    let mut dot = 0.0_f32;
    let mut an = 0.0_f32;
    let mut bn = 0.0_f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        an += x * x;
        bn += y * y;
    }
    if an <= f32::EPSILON || bn <= f32::EPSILON {
        return None;
    }
    Some(dot / (an.sqrt() * bn.sqrt()))
}

/// Deterministic vector source. A seeded LCG, so a rerun on any host measures
/// the identical corpus and the numbers below are reproducible rather than
/// merely repeatable.
struct Lcg(u64);

impl Lcg {
    fn next_unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bits = (self.0 >> 40) as u32;
        // Spread over [-1, 1): lens outputs are signed, and a same-sign corpus
        // would hide cancellation, which is exactly where reassociation error
        // is largest.
        (f32::from(bits as u16) / 32_768.0) - 1.0
    }

    fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.next_unit()).collect()
    }
}

/// Distance in representable f32 steps. The honest unit for a fold-order
/// change: an absolute delta means nothing without the magnitude it sits on.
fn ulp_distance(left: f32, right: f32) -> u32 {
    if left == right {
        return 0;
    }
    let to_ordered = |value: f32| -> i32 {
        let bits = value.to_bits() as i32;
        if bits < 0 { i32::MIN - bits } else { bits }
    };
    to_ordered(left).abs_diff(to_ordered(right))
}

const PAIRS_PER_DIM: usize = 20_000;

/// The bound claim 2 asserts, in **absolute** terms.
///
/// A cosine is bounded to `[-1, 1]`, so its meaningful precision is absolute:
/// the quantity that gets persisted, averaged into `mean_agreement`, and
/// clamped into an edge weight lives on a fixed scale.
///
/// This instrument first asserted a ULP bound instead, and that was wrong in a
/// way worth recording, because the numbers looked alarming and were not.
/// Measured over the same corpus: `worst_abs = 2.086e-7` — under two f32 steps
/// at magnitude 1 — while `worst_ulp = 59802`. Both describe the same data.
/// The ULP figure is enormous because a corpus of signed random vectors
/// produces cosines that cluster near **zero**, where representable f32 values
/// are packed at denormal density, so a difference of 1e-9 spans tens of
/// thousands of them. ULP is the right unit for a value near 1.0 and a
/// meaningless one for a value near 0.0, and a cosine is a quantity that
/// routinely sits at both.
///
/// So the bound is stated absolutely, and ULP is reported only over pairs whose
/// magnitude makes it interpretable ([`ULP_REPORT_FLOOR`]) — as a diagnostic,
/// not as the criterion.
///
/// Four f32 epsilons. #1912 measured 1.192e-7 (one epsilon) for the forge
/// cosine over 20k rows at 32 dims; this corpus reaches 384 dims, and
/// reassociation error grows with the term count.
const MAX_ABS_DELTA: f32 = 4.0 * f32::EPSILON;

/// Magnitude above which a ULP distance is a meaningful statement.
const ULP_REPORT_FLOOR: f32 = 0.1;

/// Dimensions taken from the shapes the weave sees, plus one deliberate
/// non-multiple of the 8-wide accumulator.
const DIMS: &[(usize, &str)] = &[
    (8, "one accumulator register exactly, no tail"),
    (32, "the timeline panel's dense record-vector lane"),
    (33, "forces the kernel's scalar tail (32 + 1)"),
    (384, "embedder-scale, for the lanes a future panel adds"),
];

fn main() -> Result<(), Box<dyn Error>> {
    println!("FSV #1917 — calyx-loom agreement cosine on the shared dispatched kernel");
    println!(
        "dispatched CPU kernel family on this host: {}",
        calyx_forge::cpu::simd::backend_name()
    );
    println!();

    let mut ok = true;
    ok &= claim_1_known_answers();
    println!();
    ok &= claim_2_and_3_delta_and_determinism();
    println!();
    ok &= claim_4_and_5_refusals();

    println!();
    if ok {
        println!("ALL CLAIMS OK");
        Ok(())
    } else {
        Err("at least one claim FAILED — see above".into())
    }
}

fn claim_1_known_answers() -> bool {
    println!("CLAIM 1 — known answers");
    let base = vec![0.5_f32, -0.25, 0.75, 1.0, -0.5, 0.125, -0.875, 0.375];
    let antiparallel: Vec<f32> = base.iter().map(|v| -v).collect();
    // Orthogonal by construction: a swap-and-negate of adjacent pairs has zero
    // dot product with its source for any values, so this is exact, not lucky.
    let mut orthogonal = Vec::with_capacity(base.len());
    for pair in base.chunks_exact(2) {
        orthogonal.push(-pair[1]);
        orthogonal.push(pair[0]);
    }

    let cases: [(&str, &[f32], f32); 3] = [
        ("identical", &base, 1.0),
        ("antiparallel", &antiparallel, -1.0),
        ("orthogonal", &orthogonal, 0.0),
    ];
    let mut ok = true;
    for (name, other, expected) in cases {
        match agreement_scalar(&base, other) {
            Ok(value) => {
                // One ULP of slack only: these are exact answers, and anything
                // looser would accept a genuinely wrong kernel.
                let good = ulp_distance(value, expected) <= 1;
                ok &= good;
                println!(
                    "  {name:<14} expected={expected:+.7} got={value:+.7} ulp={} {}",
                    ulp_distance(value, expected),
                    if good { "OK" } else { "FAIL" }
                );
            }
            Err(error) => {
                ok = false;
                println!("  {name:<14} FAIL — refused: {error}");
            }
        }
    }
    ok
}

fn claim_2_and_3_delta_and_determinism() -> bool {
    println!("CLAIM 2 — delta against the pre-#1917 scalar loop");
    println!("CLAIM 3 — dispatched vs portable reduction, bit-for-bit");
    let mut ok = true;
    let mut worst_ulp_overall = 0_u32;
    let mut worst_abs_overall = 0.0_f32;

    for (dim, why) in DIMS {
        let mut rng = Lcg(0x5150_1917_D15D_A7C4);
        let mut differing = 0_usize;
        let mut worst_ulp = 0_u32;
        let mut worst_abs = 0.0_f32;
        let mut kernel_disagreement: Option<&'static str> = None;
        let mut compared = 0_usize;

        for _ in 0..PAIRS_PER_DIM {
            let left = rng.vector(*dim);
            let right = rng.vector(*dim);

            // Claim 3, on the same vectors claim 2 measures: the determinism
            // contract is checked here rather than asserted in a doc comment.
            if kernel_disagreement.is_none() {
                kernel_disagreement = calyx_forge::cpu::simd::reduction_paths_agree(&left, &right);
            }

            let Some(reference) = agreement_scalar_pre_1917(&left, &right) else {
                continue;
            };
            let produced = match agreement_scalar(&left, &right) {
                Ok(value) => value,
                Err(error) => {
                    ok = false;
                    println!(
                        "  dim={dim} FAIL — production kernel refused a pair the old loop accepted: {error}"
                    );
                    break;
                }
            };
            compared += 1;
            let delta = (reference - produced).abs();
            if delta != 0.0 {
                differing += 1;
                worst_abs = worst_abs.max(delta);
                // ULP only where it means something — see MAX_ABS_DELTA.
                if reference.abs() >= ULP_REPORT_FLOOR {
                    worst_ulp = worst_ulp.max(ulp_distance(reference, produced));
                }
            }
        }

        worst_ulp_overall = worst_ulp_overall.max(worst_ulp);
        worst_abs_overall = worst_abs_overall.max(worst_abs);

        // Two f32 folds of the same products differ by reassociation only, and
        // the criterion is absolute for the reason MAX_ABS_DELTA records.
        let bounded = worst_abs <= MAX_ABS_DELTA;
        ok &= bounded;
        println!(
            "  dim={dim:<4} pairs={compared:<6} differing={differing:<6} \
             worst_abs={worst_abs:.3e} (limit {MAX_ABS_DELTA:.3e})  \
             worst_ulp_above_{ULP_REPORT_FLOOR}={worst_ulp:<3} {}   ({why})",
            if bounded { "OK" } else { "FAIL" }
        );
        match kernel_disagreement {
            None => println!("           dispatched == portable, bit-for-bit                 OK"),
            Some(kernel) => {
                ok = false;
                println!("           FAIL — kernel `{kernel}` disagrees between paths");
            }
        }
    }

    println!(
        "  worst over all dimensions: {worst_ulp_overall} ULP, {worst_abs_overall:.3e} absolute"
    );
    ok
}

fn claim_4_and_5_refusals() -> bool {
    println!("CLAIM 4 — every refusal the old loop made, the new one still makes");
    let good = vec![1.0_f32, 2.0, 3.0, 4.0];
    let mut ok = true;

    let mut nan = good.clone();
    nan[2] = f32::NAN;
    let mut infinite = good.clone();
    infinite[1] = f32::INFINITY;

    let preserved: [(&str, &[f32], &[f32], &str); 5] = [
        (
            "dim mismatch",
            &good,
            &[1.0, 2.0, 3.0],
            "CALYX_LOOM_DIM_MISMATCH",
        ),
        ("empty input", &[], &[], "CALYX_LOOM_DIM_MISMATCH"),
        (
            "NaN on the left",
            &nan,
            &good,
            "CALYX_LOOM_NON_FINITE_VECTOR",
        ),
        (
            "+inf on the right",
            &good,
            &infinite,
            "CALYX_LOOM_NON_FINITE_VECTOR",
        ),
        (
            "zero vector",
            &good,
            &[0.0, 0.0, 0.0, 0.0],
            "CALYX_LOOM_ZERO_NORM_VECTOR",
        ),
    ];

    for (name, left, right, expected_code) in preserved {
        let old_refused = agreement_scalar_pre_1917(left, right).is_none();
        match agreement_scalar(left, right) {
            Ok(value) => {
                ok = false;
                println!("  {name:<20} FAIL — new kernel accepted it and returned {value}");
            }
            Err(error) => {
                let text = format!("{error}");
                let matched = text.contains(expected_code);
                ok &= matched && old_refused;
                println!(
                    "  {name:<20} old_refused={old_refused:<5} new={expected_code} {}",
                    if matched && old_refused { "OK" } else { "FAIL" }
                );
                if !matched {
                    println!("      got instead: {text}");
                }
            }
        }
    }

    println!();
    println!("CLAIM 5 — the one new refusal: finite inputs whose squares overflow f32");
    // 1e20^2 = 1e40, which is past f32::MAX (~3.4e38). Every element is finite,
    // so the old loop's finiteness pre-scan passed it, then divided by an
    // infinite norm and returned a number.
    let overflowing = vec![1.0e20_f32; 16];
    let partner = vec![1.0e20_f32; 16];
    println!(
        "  input: 16 x 1e20 (every element finite: {})",
        overflowing.iter().all(|v| v.is_finite())
    );
    match agreement_scalar_pre_1917(&overflowing, &partner) {
        Some(value) => {
            println!("  BEFORE (pre-#1917 loop): returned {value} — a wrong answer, silently")
        }
        None => println!("  BEFORE (pre-#1917 loop): refused"),
    }
    match agreement_scalar(&overflowing, &partner) {
        Ok(value) => {
            ok = false;
            println!("  AFTER  (production kernel): FAIL — accepted it and returned {value}");
        }
        Err(error) => {
            let text = format!("{error}");
            let named = text.contains("CALYX_LOOM_NON_FINITE_VECTOR") && text.contains("overflow");
            ok &= named;
            println!(
                "  AFTER  (production kernel): refused — {} {}",
                text,
                if named { "OK" } else { "FAIL" }
            );
        }
    }
    ok
}
