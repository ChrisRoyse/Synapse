//! Manual FSV for #1953: does the leakage detector fire on the real case, and
//! stay silent on the cases that merely resemble it?
//!
//! ## The real case, restated
//!
//! On `syn-mcp-usage-v1 @ 1776006` — the corpus chosen *because* it clears every
//! grounding floor — `sufficiency` reported `sufficient = true, deficit_bits =
//! 0`, with `panel_bits` equal to `anchor_entropy_bits` to every digit:
//!
//! ```text
//! anchor_entropy_bits  0.969319224357605
//! slot 86              0.969319224357605   distinct_values = 2
//! distinct_outcomes    2
//! ```
//!
//! The anchor `synapse:mcp_tool_call_outcome` is built from `&record.status`;
//! slot 86 `syn.mcp_usage.status_onehot.v1` is a one-hot of
//! `record["status"]`. The same field. The panel "predicted" the outcome
//! because it contained it.
//!
//! ## Why the negative cases carry the weight
//!
//! A detector that fires on everything is as useless as one that never fires,
//! and this one gates a `sufficient` verdict — so a false positive would
//! declare a genuinely capable panel insufficient. The three near-misses below
//! are the ones that separate "is the label" from things that look like it:
//!
//! * a **perfect predictor with different structure** (many values collapsing
//!   onto two outcomes) reaches the same ceiling and is NOT leakage;
//! * a **degenerate anchor** (`H = 0`) makes every lens score 0, which would
//!   match on bits alone;
//! * a lens **just below the ceiling** is the ordinary strong-lens case.
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example anchor_leakage_fsv`

use calyx_assay::sufficiency::detect_anchor_leakage;
use std::error::Error;

/// The figures read off the live daemon on 2026-08-02.
///
/// The daemon rendered these as `0.969319224357605`, which is this `f32`
/// printed at `f64` precision — the two quantities are bit-identical `f32`s,
/// which is exactly the observation that exposed the leakage.
const LIVE_ANCHOR_ENTROPY_BITS: f32 = 0.969_319_2;
const LIVE_SLOT_86_BITS: f32 = 0.969_319_2;

struct Case {
    name: &'static str,
    slot: u16,
    lens_bits: f32,
    lens_distinct: usize,
    anchor_bits: f32,
    anchor_distinct: usize,
    expect_leak: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("anchor_leakage_fsv  (#1953)");

    let cases = [
        Case {
            name: "the real case: slot 86 IS the anchor",
            slot: 86,
            lens_bits: LIVE_SLOT_86_BITS,
            lens_distinct: 2,
            anchor_bits: LIVE_ANCHOR_ENTROPY_BITS,
            anchor_distinct: 2,
            expect_leak: true,
        },
        Case {
            name: "perfect predictor, different cardinality (69 -> 2)",
            slot: 84,
            lens_bits: LIVE_ANCHOR_ENTROPY_BITS,
            lens_distinct: 69,
            anchor_bits: LIVE_ANCHOR_ENTROPY_BITS,
            anchor_distinct: 2,
            expect_leak: false,
        },
        Case {
            name: "strong lens just below the ceiling",
            slot: 93,
            lens_bits: 0.947_038_5,
            lens_distinct: 2,
            anchor_bits: LIVE_ANCHOR_ENTROPY_BITS,
            anchor_distinct: 2,
            expect_leak: false,
        },
        Case {
            name: "degenerate anchor (H=0), lens also 0",
            slot: 89,
            lens_bits: 0.0,
            lens_distinct: 1,
            anchor_bits: 0.0,
            anchor_distinct: 1,
            expect_leak: false,
        },
        Case {
            name: "single-outcome anchor with a 1-valued lens",
            slot: 89,
            lens_bits: 0.0,
            lens_distinct: 1,
            anchor_bits: 0.5,
            anchor_distinct: 1,
            expect_leak: false,
        },
        Case {
            name: "weak lens, matching cardinality",
            slot: 88,
            lens_bits: 0.003_427_863,
            lens_distinct: 2,
            anchor_bits: LIVE_ANCHOR_ENTROPY_BITS,
            anchor_distinct: 2,
            expect_leak: false,
        },
        Case {
            name: "NaN bits must not be treated as a match",
            slot: 90,
            lens_bits: f32::NAN,
            lens_distinct: 2,
            anchor_bits: LIVE_ANCHOR_ENTROPY_BITS,
            anchor_distinct: 2,
            expect_leak: false,
        },
        Case {
            name: "3-outcome anchor with a 3-valued lens at the ceiling",
            slot: 82,
            lens_bits: 1.584_962_5,
            lens_distinct: 3,
            anchor_bits: 1.584_962_5,
            anchor_distinct: 3,
            expect_leak: true,
        },
    ];

    let mut failures = 0_u32;
    println!(
        "\n  {:<52} {:>10} {:>6} {:>10} {:>6}  {:<8} got",
        "case", "lens_bits", "|vals|", "anchor", "|out|", "expect"
    );
    for case in &cases {
        let found = detect_anchor_leakage(
            case.slot,
            case.lens_bits,
            case.lens_distinct,
            case.anchor_bits,
            case.anchor_distinct,
        );
        let leaked = found.is_some();
        let ok = leaked == case.expect_leak;
        if !ok {
            failures += 1;
        }
        println!(
            "  {:<52} {:>10.6} {:>6} {:>10.6} {:>6}  {:<8} {}{}",
            case.name,
            case.lens_bits,
            case.lens_distinct,
            case.anchor_bits,
            case.anchor_distinct,
            if case.expect_leak { "LEAK" } else { "clean" },
            if leaked { "LEAK" } else { "clean" },
            if ok { "" } else { "   <-- WRONG" }
        );
    }

    // The real case must also report the numbers that justify the verdict, not
    // merely a boolean: an operator has to be able to see WHY it fired.
    println!("\n=== the real case, in full ===");
    match detect_anchor_leakage(86, LIVE_SLOT_86_BITS, 2, LIVE_ANCHOR_ENTROPY_BITS, 2) {
        Some(leak) => {
            println!("  slot                     = {}", leak.slot);
            println!("  lens_bits                = {:.15}", leak.lens_bits);
            println!(
                "  anchor_entropy_bits      = {:.15}",
                leak.anchor_entropy_bits
            );
            println!("  resolution_bits          = {:e}", leak.resolution_bits);
            println!("  lens_distinct_values     = {}", leak.lens_distinct_values);
            println!(
                "  anchor_distinct_outcomes = {}",
                leak.anchor_distinct_outcomes
            );
        }
        None => {
            println!("  NOT DETECTED — the live case does not trip the detector");
            failures += 1;
        }
    }

    println!("\n--- VERDICT ---");
    if failures == 0 {
        println!("  PASS: fires on the live leakage case, silent on all near-misses");
        Ok(())
    } else {
        Err(format!("{failures} case(s) wrong").into())
    }
}
