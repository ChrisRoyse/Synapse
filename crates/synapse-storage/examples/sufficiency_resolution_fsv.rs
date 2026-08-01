//! Manual FSV for #1945: the sufficiency verdict must not turn on `f32` noise.
//!
//! Run:
//! ```text
//! cargo run --release -p synapse-storage --example sufficiency_resolution_fsv
//! ```
//!
//! Every case below states its input, the outcome the contract requires, and
//! why. The driver compares against that stated expectation and exits non-zero
//! on the first disagreement, so a regression fails loudly rather than printing
//! a wall of numbers nobody reads.
//!
//! The synthetic values are not invented from nothing: case 1 is the exact pair
//! measured on the real corpus (`syn-mcp-usage-v1` @ 1776006, anchor
//! `synapse:mcp_tool_call_outcome`, 800 anchored records) that produced
//! `sufficient=false deficit_bits=0.000000`, reproduced here as a fixed input
//! so the defect stays pinned after the live corpus has moved on.

use std::error::Error;
use std::process::ExitCode;

use calyx_assay::sufficiency::{
    SufficiencyVerdictBasis, estimator_resolution_bits, format_deficit_bits, sufficiency_verdict,
};

struct Case {
    name: &'static str,
    basis_bits: f32,
    anchor_entropy_bits: f32,
    expected_basis: SufficiencyVerdictBasis,
    expected_sufficient: bool,
    why: &'static str,
}

/// The sufficiency basis the card measured on the real corpus in #1945: it
/// renders as `0.848548114` at the nine decimals the issue printed.
const MEASURED_BASIS_BITS: f32 = 0.848_548_1;

/// The next representable `f32` above [`MEASURED_BASIS_BITS`], which renders as
/// `0.848548174` — the anchor entropy the same card measured.
///
/// Written as a one-ulp step rather than as a decimal literal on purpose. The
/// two values differ in their eighth significant figure, so as literals they
/// are indistinguishable to a reader and a well-meaning "excessive precision"
/// truncation would silently collapse them into the same `f32` and delete the
/// defect this case exists to pin.
fn one_ulp_above(value: f32) -> f32 {
    f32::from_bits(value.to_bits() + 1)
}

fn one_ulp_below(value: f32) -> f32 {
    f32::from_bits(value.to_bits() - 1)
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "1-real-corpus-6e-8-gap",
            // The exact pair the card printed at full precision in #1945. The
            // probe recovered the outcome exactly, so these are one quantity
            // computed twice, by two different code paths.
            basis_bits: MEASURED_BASIS_BITS,
            anchor_entropy_bits: one_ulp_above(MEASURED_BASIS_BITS),
            expected_basis: SufficiencyVerdictBasis::WithinEstimatorResolution,
            expected_sufficient: true,
            why: "gap is 6.0e-8 bits, under one f32 ulp at magnitude 0.85; \
                  a difference the instrument cannot resolve is not a difference",
        },
        Case {
            name: "2-exact-equality",
            basis_bits: MEASURED_BASIS_BITS,
            anchor_entropy_bits: MEASURED_BASIS_BITS,
            expected_basis: SufficiencyVerdictBasis::WithinEstimatorResolution,
            expected_sufficient: true,
            why: "I == H exactly: the panel measures the outcome, so it is sufficient",
        },
        Case {
            name: "3-genuine-shortfall-0.001-bits",
            basis_bits: MEASURED_BASIS_BITS - 0.001,
            anchor_entropy_bits: MEASURED_BASIS_BITS,
            expected_basis: SufficiencyVerdictBasis::ShortOfAnchorEntropy,
            expected_sufficient: false,
            why: "a 0.001-bit shortfall is ~2000x the resolution and must still fail; \
                  this is the case that proves the fix did not become slack",
        },
        Case {
            name: "4-panel-exceeds-anchor",
            basis_bits: 0.9,
            anchor_entropy_bits: MEASURED_BASIS_BITS,
            expected_basis: SufficiencyVerdictBasis::ExceedsAnchorEntropy,
            expected_sufficient: true,
            why: "resolvably above the anchor: sufficient, and distinguishable from a tie",
        },
        Case {
            name: "5-zero-entropy-anchor",
            basis_bits: 0.0,
            anchor_entropy_bits: 0.0,
            expected_basis: SufficiencyVerdictBasis::WithinEstimatorResolution,
            expected_sufficient: true,
            why: "boundary: both operands zero must not divide by zero or widen the \
                  tolerance; a degenerate anchor needs zero bits to predict",
        },
        Case {
            name: "6-tiny-magnitude-genuine-shortfall",
            basis_bits: 1.0e-6,
            anchor_entropy_bits: 1.0e-3,
            expected_basis: SufficiencyVerdictBasis::ShortOfAnchorEntropy,
            expected_sufficient: false,
            why: "boundary: at 1e-3 bits the resolution is ~5e-10, so a 1e-3 shortfall is \
                  still resolvable; a relative tolerance must not swallow small-magnitude gaps",
        },
        Case {
            name: "7-one-ulp-below-anchor",
            basis_bits: one_ulp_below(MEASURED_BASIS_BITS),
            anchor_entropy_bits: MEASURED_BASIS_BITS,
            expected_basis: SufficiencyVerdictBasis::WithinEstimatorResolution,
            expected_sufficient: true,
            why: "boundary: exactly one representable step below the anchor is the \
                  smallest difference f32 can express and must not decide the verdict",
        },
    ]
}

fn main() -> ExitCode {
    match run() {
        Ok(0) => {
            println!(
                "\nsufficiency_resolution_fsv: ALL {} CASES PASS",
                cases().len()
            );
            ExitCode::SUCCESS
        }
        Ok(failures) => {
            eprintln!("\nsufficiency_resolution_fsv: {failures} CASE(S) FAILED");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("sufficiency_resolution_fsv: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<usize, Box<dyn Error>> {
    println!("== #1945 sufficiency verdict resolution FSV ==");
    println!(
        "f32::EPSILON={:e}  tolerance = EPSILON * max(|basis|,|H|) * 4\n",
        f32::EPSILON
    );
    let mut failures = 0;
    for case in cases() {
        let (basis, resolution_bits, deficit_bits) =
            sufficiency_verdict(case.basis_bits, case.anchor_entropy_bits);
        let sufficient = basis.is_sufficient();
        let raw_gap = case.anchor_entropy_bits - case.basis_bits;
        let ok = basis == case.expected_basis && sufficient == case.expected_sufficient;
        // The self-contradiction #1945 was filed over: "insufficient, by
        // nothing". Assert it is now unreachable, on every case, not only the
        // ones that provoked it.
        let contradiction = !sufficient && deficit_bits == 0.0;
        println!(
            "[{}] {}\n     basis_bits={:.9} anchor_H={:.9} raw_gap={:e}\n     resolution={:e} verdict={} sufficient={} deficit={:e}\n     expected={} why={}",
            if ok && !contradiction { "PASS" } else { "FAIL" },
            case.name,
            case.basis_bits,
            case.anchor_entropy_bits,
            raw_gap,
            resolution_bits,
            basis.as_str(),
            sufficient,
            deficit_bits,
            case.expected_basis.as_str(),
            case.why,
        );
        if contradiction {
            eprintln!("     ^ CONTRADICTION: reported insufficient with a zero deficit (#1945)");
        }
        if !ok {
            eprintln!(
                "     ^ expected basis={} sufficient={}, got basis={} sufficient={}",
                case.expected_basis.as_str(),
                case.expected_sufficient,
                basis.as_str(),
                sufficient
            );
        }
        if !ok || contradiction {
            failures += 1;
        }
    }

    // The resolution must scale with magnitude. An absolute epsilon would be
    // constant here, which is precisely the failure mode the fix avoids.
    println!("\n-- resolution scales with magnitude (relative, not absolute) --");
    let mut previous = 0.0_f32;
    for magnitude in [1.0e-3_f32, 1.0e-1, 1.0, 10.0] {
        let resolution = estimator_resolution_bits(magnitude, magnitude);
        println!("   magnitude={magnitude:e} -> resolution={resolution:e}");
        if resolution <= previous {
            eprintln!("   ^ FAIL: resolution did not grow with magnitude");
            failures += 1;
        }
        previous = resolution;
    }

    // Ask 3: a non-zero deficit must never render as zero. The old `{:.6}`
    // behaviour is shown beside the new one so the defect is visible rather
    // than merely absent.
    println!("\n-- a non-zero deficit can never render as zero (ask 3) --");
    for (value, precision) in [
        (0.0_f32, 6),
        (1.0e-7_f32, 6),
        (4.9e-7_f32, 6),
        (1.0e-3_f32, 6),
        (1.0e-10_f32, 9),
    ] {
        let rendered = format_deficit_bits(value, precision);
        let old = format!("{value:.precision$}");
        let hidden = value != 0.0 && old.chars().all(|c| c == '0' || c == '.');
        let now_hidden = value != 0.0 && rendered.chars().all(|c| c == '0' || c == '.');
        println!(
            "   value={value:e} precision={precision}  old={old:<12} new={rendered:<12} {}",
            if hidden && !now_hidden {
                "<- was hidden by rounding, now visible"
            } else if now_hidden {
                "<- STILL HIDDEN"
            } else {
                ""
            }
        );
        if now_hidden {
            eprintln!("   ^ FAIL: a non-zero deficit still renders as zero");
            failures += 1;
        }
    }
    Ok(failures)
}
