//! Manual FSV: does `pair_gain` now fail closed on a broken measurement, and
//! does it say when the data-processing-inequality floor moved a value?
//!
//! ## The defect
//!
//! `gate::pair_gain_from_estimates` computed the pair gain inline as
//! `(pair.bits - left.bits.max(right.bits)).max(0.0)`, a second implementation
//! of the quantity `synergy::whole_minus_max_gain` documents itself as the
//! **single** implementation of (#1942 ask 4). Two consequences:
//!
//! * Rust's `f32::max` ignores `NaN`, so `NaN.max(0.0)` is `0.0`. A broken
//!   measurement was reported as "no synergy between these lenses" rather than
//!   failing — a wrong answer that looks exactly like a real one.
//! * A raw gain below zero violates the data-processing inequality and is an
//!   instrument fault, not a corpus fact. It was clamped to zero invisibly.
//!
//! ## What is proven here, and what is proven by composition
//!
//! `pair_gain_from_estimates` is `pub(crate)`, so an example cannot call it
//! directly and cannot inject a `NaN` estimate through the public
//! `AssayGate::pair_gain`, whose estimates come from a logistic probe over real
//! samples. So this harness proves two things and states the join:
//!
//! 1. **Directly, through the public API**: `AssayGate::pair_gain` over real
//!    separable samples returns a gain equal to `pair - max(left, right)` and
//!    reports the floor honestly.
//! 2. **Directly, on the shared function**: `whole_minus_max_gain` — which the
//!    fixed code now calls — rejects `NaN`, infinity and negative terms, and
//!    flags the floor.
//!
//! Together those are the claim: the path is wired to a function that fails
//! closed. Neither alone would be.
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example pair_gain_discipline_fsv`

use calyx_assay::gate::AssayGate;
use calyx_assay::synergy::whole_minus_max_gain;
use std::error::Error;

/// Builds `n` samples of `dim` features where the label is a deterministic
/// function of the features, so the probe has real signal to find.
fn samples(n: usize, dim: usize, seed: u64) -> (Vec<Vec<f32>>, Vec<bool>) {
    let mut values = Vec::with_capacity(n);
    let mut labels = Vec::with_capacity(n);
    let mut state = seed | 1;
    for _ in 0..n {
        // xorshift, so the corpus is reproducible without a rand dependency.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let positive = state.is_multiple_of(2);
        let base = if positive { 1.0_f32 } else { -1.0_f32 };
        values.push(
            (0..dim)
                .map(|i| ((state >> (i % 32)) as f32 % 7.0).mul_add(0.05, base))
                .collect::<Vec<f32>>(),
        );
        labels.push(positive);
    }
    (values, labels)
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("pair_gain_discipline_fsv");
    let mut failures = 0_u32;

    // --- 1. the shared function rejects what the inline formula absorbed ----
    // Known input, known expected output, stated before the run: each of these
    // must be an Err. Under the old inline arithmetic every one of them would
    // have produced a finite gain, and the NaN case specifically 0.0.
    println!("\n=== 1. whole_minus_max_gain fails closed on broken terms ===");
    let broken: [(&str, f32, f32, f32); 5] = [
        ("pair=NaN", f32::NAN, 1.0, 1.0),
        ("left=NaN", 2.0, f32::NAN, 1.0),
        ("right=inf", 2.0, 1.0, f32::INFINITY),
        ("pair negative", -1.0, 1.0, 1.0),
        ("left negative", 2.0, -1.0, 1.0),
    ];
    for (name, pair, left, right) in broken {
        let outcome = whole_minus_max_gain(pair, left, right);
        let rejected = outcome.is_err();
        let shown = match &outcome {
            Ok((gain, raw, floored)) => {
                format!("Ok(gain={gain}, raw={raw}, floored={floored})  <-- ACCEPTED")
            }
            Err(error) => format!("Err({})", error.code),
        };
        println!("  {name:<16} -> {shown}");
        if !rejected {
            failures += 1;
        }
    }
    // The specific old behaviour, named so the regression is unmistakable.
    let nan_absorbed = (f32::NAN - 1.0_f32.max(1.0)).max(0.0);
    println!(
        "  for contrast, the old inline arithmetic on pair=NaN yielded {nan_absorbed} (a real-looking zero)"
    );

    // --- 2. the floor is reported, not hidden -------------------------------
    println!("\n=== 2. the DPI floor is visible when it moves a value ===");
    // pair < max(left, right) is a data-processing-inequality violation.
    let (gain, raw, floored) = whole_minus_max_gain(0.5, 2.0, 1.0)?;
    println!("  pair=0.5 left=2.0 right=1.0 -> gain={gain} raw={raw} floored={floored}");
    let floor_ok = floored && (gain - 0.0).abs() < f32::EPSILON && raw < 0.0;
    if !floor_ok {
        failures += 1;
    }
    // And stays quiet when it does not.
    let (gain2, raw2, floored2) = whole_minus_max_gain(3.0, 2.0, 1.0)?;
    println!("  pair=3.0 left=2.0 right=1.0 -> gain={gain2} raw={raw2} floored={floored2}");
    let clean_ok = !floored2 && (gain2 - 1.0).abs() < 1e-6 && (raw2 - 1.0).abs() < 1e-6;
    if !clean_ok {
        failures += 1;
    }
    println!("  floored case correct = {floor_ok}; unfloored case correct = {clean_ok}");

    // --- 3. the public path is actually wired to it -------------------------
    // Real samples through the real probe. The assertion is the identity the
    // shared function guarantees: gain == max(0, pair - max(left, right)), and
    // the reported raw/floored agree with the reported bits.
    println!("\n=== 3. AssayGate::pair_gain over real samples obeys the identity ===");
    let gate = AssayGate::default();
    let (left, labels) = samples(400, 6, 0x51CE_D00D);
    let (right, _) = samples(400, 6, 0x0BAD_F00D);
    match gate.pair_gain(&left, &right, &labels) {
        Ok(found) => {
            let expected_raw = found.pair_bits - found.left_bits.max(found.right_bits);
            let expected_gain = expected_raw.max(0.0);
            println!(
                "  left_bits={:.6} right_bits={:.6} pair_bits={:.6}",
                found.left_bits, found.right_bits, found.pair_bits
            );
            println!(
                "  gain_bits={:.6} raw_gain_bits={:.6} monotonicity_floor_applied={}",
                found.gain_bits, found.raw_gain_bits, found.monotonicity_floor_applied
            );
            println!("  expected gain={expected_gain:.6} raw={expected_raw:.6}");
            let identity_ok = (found.gain_bits - expected_gain).abs() < 1e-5
                && (found.raw_gain_bits - expected_raw).abs() < 1e-5
                && found.monotonicity_floor_applied == (expected_raw < 0.0);
            println!("  identity holds = {identity_ok}");
            if !identity_ok {
                failures += 1;
            }
        }
        Err(error) => {
            // A probe that cannot measure this corpus is a harness problem, and
            // must be reported as one rather than counted as a pass.
            println!("  probe returned error[{}]: {}", error.code, error.message);
            println!("  HARNESS COULD NOT MEASURE - this proves nothing about the fix");
            failures += 1;
        }
    }

    println!("\n--- VERDICT ---");
    if failures == 0 {
        println!("  PASS: pair gain fails closed on broken terms and reports its floor");
        Ok(())
    } else {
        Err(format!("{failures} check(s) failed").into())
    }
}
