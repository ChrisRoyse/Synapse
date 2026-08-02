//! Manual FSV for #1961: blind-spot alerts on the active panel were a
//! one-hot x cyclic encoding artifact.
//!
//! ## The defect
//!
//! The detector's rule is "lens A is confident that this record is close to a
//! neighbour while lens B disagrees". But the neighbour is chosen to *maximise*
//! similarity under A, so `lens_a_similarity` is the argmax — it is at or near
//! A's ceiling for every record whose A-similarity distribution is degenerate.
//! A one-hot lens returns cosine exactly `1.0` for any two records sharing a
//! category and can return nothing else, so on such a lens the rule collapses to
//! "B's similarity is low, offset by a constant": a lens-B outlier detector
//! wearing a cross-lens label.
//!
//! On `syn-timeline-v1 @ 1900001` that produced 46 `severity=high` alerts of
//! which 30 sat on `delta = 2.0`, the maximum the metric can take, all with
//! `lens_a_similarity = 1.0` and `lens_b_neighbor_mean = -1.0` (two timestamps
//! half a day apart under a 24-hour cyclic encoding).
//!
//! ## What this harness proves
//!
//! 1. **Synthetic**, with hand-computed expectations: the degeneracy measurement
//!    and the calibration-resolution rule return exactly the values first
//!    principles say they must, including at their boundaries.
//! 2. **Real corpus**, against a frozen copy of the live vault: the artifact
//!    alerts are gone, every refusal names its measured evidence, and the report
//!    now states the alert set's own discriminative power.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example blind_spot_discrimination_fsv -- <vault-copy-dir>`
//! where `<vault-copy-dir>` contains `db-daemon/` and `machine-salt.bin`.

use std::error::Error;
use std::path::PathBuf;

use calyx_core::{CxId, SlotId};
use calyx_loom::{
    BlindSpotCalibration, BlindSpotCalibrationParams, MAX_DISCRIMINATIVE_MODAL_SHARE,
    MIN_DISCRIMINATIVE_DISTINCT, SIMILARITY_DISTINCT_TOLERANCE, SimilarityDiscrimination,
    detect_blind_spot_calibrated,
};
use synapse_calyx::{
    SynapseCalyxBlindSpotParams, SynapseCalyxConfig, SynapseCalyxTuningConfig, SynapseCalyxVault,
};

/// The panel the issue was filed against: the active operator-facing panel.
const PANEL: u32 = 1_900_001;

/// Every synthetic check carries the value first principles says it must
/// produce, so a wrong answer is a failure rather than a number to interpret.
struct Failures(Vec<String>);

impl Failures {
    fn check(
        &mut self,
        label: &str,
        observed: impl std::fmt::Debug,
        expected: impl std::fmt::Debug,
    ) {
        let observed = format!("{observed:?}");
        let expected = format!("{expected:?}");
        let verdict = if observed == expected { "PASS" } else { "FAIL" };
        println!("  [{verdict}] {label}: observed={observed} expected={expected}");
        if verdict == "FAIL" {
            self.0
                .push(format!("{label}: observed={observed} expected={expected}"));
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear audit; splitting separates a measurement from the check that makes it admissible"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut failures = Failures(Vec::new());

    // -----------------------------------------------------------------------
    // Part 1 — synthetic, hand-computed expectations
    // -----------------------------------------------------------------------
    println!("== Part 1: synthetic degeneracy measurement (expected values computed by hand) ==");
    println!(
        "  constants: MIN_DISCRIMINATIVE_DISTINCT={MIN_DISCRIMINATIVE_DISTINCT} \
         MAX_DISCRIMINATIVE_MODAL_SHARE={MAX_DISCRIMINATIVE_MODAL_SHARE} \
         SIMILARITY_DISTINCT_TOLERANCE={SIMILARITY_DISTINCT_TOLERANCE:e}"
    );

    // Edge case A — a one-hot lens's nearest-neighbour similarity: constant 1.0.
    // 100 identical values => 1 distinct value, modal share 1.0, definitional.
    let one_hot = vec![1.0f32; 100];
    let d = SimilarityDiscrimination::measure(&one_hot)?;
    println!("\n  input A: 100 x 1.0  (a one-hot lens's NN similarity)");
    failures.check("A.distinct_values", d.distinct_values, 1);
    failures.check("A.modal_share", d.modal_share, 1.0f32);
    failures.check("A.min/max", (d.min, d.max), (1.0f32, 1.0f32));
    failures.check("A.observed_range", d.observed_range(), 0.0f32);
    failures.check("A.is_definitional", d.is_definitional(), true);

    // Edge case B — the two-point set {1.0, 0.0} an exact one-hot produces.
    // 2 distinct < MIN_DISCRIMINATIVE_DISTINCT(3) => definitional on the
    // distinct-count rule alone, before the modal-share rule is consulted.
    let mut two_point = vec![1.0f32; 99];
    two_point.push(0.0);
    let d = SimilarityDiscrimination::measure(&two_point)?;
    println!("\n  input B: 99 x 1.0 + 1 x 0.0  (exact one-hot two-point set)");
    failures.check("B.distinct_values", d.distinct_values, 2);
    failures.check("B.modal_value", d.modal_value, 1.0f32);
    failures.check("B.modal_share", d.modal_share, 0.99f32);
    failures.check(
        "B.is_definitional (distinct rule)",
        d.is_definitional(),
        true,
    );

    // Edge case C — the modal-share rule must fire *independently* of the
    // distinct-count rule. 99 + 1 + 1 = 101 values, 3 distinct (so the count
    // rule passes), modal share 99/101 = 0.980198... >= 0.98 => still definitional.
    let mut modal_heavy = vec![1.0f32; 99];
    modal_heavy.push(0.0);
    modal_heavy.push(0.5);
    let d = SimilarityDiscrimination::measure(&modal_heavy)?;
    let expected_share = 99.0f32 / 101.0f32;
    println!("\n  input C: 99 x 1.0 + 0.0 + 0.5  (3 distinct, modal share 99/101)");
    failures.check(
        "C.distinct_values >= MIN",
        d.distinct_values >= MIN_DISCRIMINATIVE_DISTINCT,
        true,
    );
    failures.check("C.modal_share", d.modal_share, expected_share);
    failures.check(
        "C.modal_share >= ceiling",
        d.modal_share >= MAX_DISCRIMINATIVE_MODAL_SHARE,
        true,
    );
    failures.check("C.is_definitional (modal rule)", d.is_definitional(), true);

    // Edge case D — a genuinely varying lens must NOT be refused.
    let varied: Vec<f32> = (0..100i16).map(|i| f32::from(i) / 100.0).collect();
    let d = SimilarityDiscrimination::measure(&varied)?;
    println!("\n  input D: 100 distinct values 0.00..0.99  (a discriminative lens)");
    failures.check("D.distinct_values", d.distinct_values, 100);
    failures.check("D.modal_share", d.modal_share, 0.01f32);
    failures.check("D.is_definitional", d.is_definitional(), false);

    // Edge case E — ulp noise must not inflate the distinct count. The live
    // corpus contains both 1.0 and 1.000_000_1 from the same one-hot lens; if
    // those counted as two values the degeneracy test would be defeated by
    // floating-point noise alone.
    let ulp = vec![1.0f32, 1.000_000_1, 1.0, 0.999_999_9];
    let d = SimilarityDiscrimination::measure(&ulp)?;
    println!("\n  input E: 1.0, 1.0000001, 1.0, 0.9999999  (ulp noise from one lens)");
    failures.check("E.distinct_values", d.distinct_values, 1);
    failures.check("E.is_definitional", d.is_definitional(), true);

    // Edge case F — invalid input fails closed, never returns a verdict.
    println!("\n  input F: empty slice, and a slice containing NaN  (must fail closed)");
    failures.check(
        "F.empty is refused",
        SimilarityDiscrimination::measure(&[]).is_err(),
        true,
    );
    failures.check(
        "F.NaN is refused",
        SimilarityDiscrimination::measure(&[1.0, f32::NAN]).is_err(),
        true,
    );

    // -----------------------------------------------------------------------
    // Part 2 — calibration resolution and severity, hand-computed
    // -----------------------------------------------------------------------
    println!(
        "\n== Part 2: conformal calibration resolution (alpha=0.05 needs ceil(1/0.05)=20 distinct) =="
    );
    let params = BlindSpotCalibrationParams {
        min_samples: 50,
        alpha: 0.05,
    };

    // The exact live shape on `syn-timeline-v1 @ 1900001`: 923 samples over five
    // distinct deltas, with 30 of them on the metric's maximum. 30/923 = 0.0325,
    // which is below alpha=0.05, so the conformal rule *does* emit these — the
    // certificate is real arithmetic. What it certifies is that 3.25% of the
    // corpus sits at the top of a variable with five reachable values, which is
    // a fact about the encoding. That is precisely why resolution has to gate
    // the verdict.
    let five_valued: Vec<f32> = std::iter::repeat_n(2.0f32, 30)
        .chain(std::iter::repeat_n(1.9, 1))
        .chain(std::iter::repeat_n(1.0, 15))
        .chain(std::iter::repeat_n(0.5, 400))
        .chain(std::iter::repeat_n(0.0, 477))
        .collect();
    assert_eq!(five_valued.len(), 923, "the live sample count");
    let cal = BlindSpotCalibration::from_deltas(five_valued.iter().copied(), params)?;
    println!(
        "\n  calibration G: 923 samples over 5 distinct deltas, 30 on the maximum (the live shape)"
    );
    failures.check("G.distinct_deltas", cal.discrimination().distinct_values, 5);
    failures.check("G.resolves_alpha", cal.resolves_alpha(), false);

    // The maximum delta under an unresolvable calibration must NOT rate high —
    // that is the exact verdict #1961 ask 4 says has to stop happening.
    let alert = detect_blind_spot_calibrated(
        CxId::from_bytes([7u8; 16]),
        SlotId::new(1),
        SlotId::new(4),
        1.0,
        -1.0,
        &cal,
    )?
    .ok_or("G: the maximum delta should still be emitted, only demoted")?;
    println!(
        "  G.max-delta alert: delta={} severity={:?} p_value={} distinct={} resolves={}",
        alert.delta,
        alert.severity,
        alert.calibration.as_ref().map_or(f32::NAN, |e| e.p_value),
        alert.calibration.as_ref().map_or(0, |e| e.distinct_deltas),
        alert.calibration.as_ref().is_some_and(|e| e.resolves_alpha),
    );
    failures.check(
        "G.severity is not High",
        alert.severity,
        calyx_loom::Severity::Low,
    );
    failures.check(
        "G.evidence.resolves_alpha",
        alert.calibration.as_ref().is_some_and(|e| e.resolves_alpha),
        false,
    );

    // A resolvable calibration must still be able to rate high, or the fix has
    // simply disabled the detector.
    let many_valued: Vec<f32> = (0..1000)
        .map(|i| f32::from(i16::try_from(i).unwrap_or(0)) / 1000.0)
        .collect();
    let cal = BlindSpotCalibration::from_deltas(many_valued.iter().copied(), params)?;
    println!("\n  calibration H: 1000 samples over 1000 distinct deltas");
    failures.check(
        "H.distinct_deltas",
        cal.discrimination().distinct_values,
        1000,
    );
    failures.check("H.resolves_alpha", cal.resolves_alpha(), true);
    // delta = 0.999 is the maximum: p_value = 1/1000 = 0.001 <= alpha/3 = 0.0167 => High.
    let alert = detect_blind_spot_calibrated(
        CxId::from_bytes([8u8; 16]),
        SlotId::new(1),
        SlotId::new(4),
        1.0,
        0.001,
        &cal,
    )?
    .ok_or("H: the maximum delta must be emitted")?;
    println!(
        "  H.max-delta alert: delta={} severity={:?} p_value={}",
        alert.delta,
        alert.severity,
        alert.calibration.as_ref().map_or(f32::NAN, |e| e.p_value),
    );
    failures.check(
        "H.severity is High",
        alert.severity,
        calyx_loom::Severity::High,
    );
    // The median delta must not be flagged at all (p_value ~ 0.5 > alpha).
    failures.check(
        "H.median delta emits nothing",
        detect_blind_spot_calibrated(
            CxId::from_bytes([9u8; 16]),
            SlotId::new(1),
            SlotId::new(4),
            1.0,
            0.5,
            &cal,
        )?
        .is_none(),
        true,
    );

    // -----------------------------------------------------------------------
    // Part 3 — the real corpus
    // -----------------------------------------------------------------------
    let Some(root) = std::env::args().nth(1).map(PathBuf::from) else {
        println!("\n== Part 3 skipped: no <vault-copy-dir> argument ==");
        return finish(&failures);
    };
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let config = SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    };
    let vault = SynapseCalyxVault::open(config)?;

    println!("\n== Part 3: real corpus, frozen vault copy ==");
    println!("  vault = {}", vault_dir.display());
    println!("  latest_seq = {}", vault.latest_seq());

    let report = vault.blind_spot_scan(&SynapseCalyxBlindSpotParams::new(PANEL))?;
    println!(
        "\n  panel {} records_measured={} n_lenses={}",
        report.panel_version, report.records_measured, report.n_lenses
    );
    println!(
        "  directions: evaluated={} uncalibrated={} nondiscriminative={}",
        report.slot_pairs_evaluated,
        report.slot_pairs_uncalibrated,
        report.slot_pairs_nondiscriminative
    );
    println!(
        "  alerts_total={} distinct_signatures={} distinct_deltas={}",
        report.alerts_total, report.alert_distinct_signatures, report.alert_distinct_deltas
    );

    println!("\n  refused directions (lens A's confidence is definitional):");
    for pair in &report.nondiscriminative_pairs {
        println!(
            "    {} -> {}  {}  n={} distinct={} modal={:.6} share={:.6} range=[{:.6},{:.6}]",
            pair.slot_a,
            pair.slot_b,
            pair.code,
            pair.records,
            pair.lens_a_distinct_values,
            pair.lens_a_modal_value,
            pair.lens_a_modal_share,
            pair.lens_a_observed_min,
            pair.lens_a_observed_max,
        );
    }

    println!("\n  surviving alerts:");
    for alert in &report.alerts {
        println!(
            "    {} {}->{} delta={:.6} sev={} p={:.6} distinct_deltas={} resolves={} b_range=[{:.4},{:.4}] dissent={:.4}",
            alert.cx_id,
            alert.slot_a,
            alert.slot_b,
            alert.delta,
            alert.severity,
            alert.calibration_p_value,
            alert.calibration_distinct_deltas,
            alert.calibration_resolves_alpha,
            alert.lens_b_observed_min,
            alert.lens_b_observed_max,
            alert.lens_b_dissent_fraction,
        );
    }

    // The binding assertions about the real corpus.
    println!("\n  assertions over the real corpus:");
    let saturating: Vec<_> = report
        .alerts
        .iter()
        .filter(|a| a.delta >= 2.0 - SIMILARITY_DISTINCT_TOLERANCE)
        .collect();
    failures.check(
        "no alert sits on the metric's maximum (delta=2.0)",
        saturating.len(),
        0usize,
    );
    let uncertified_high: Vec<_> = report
        .alerts
        .iter()
        .filter(|a| a.severity == "high" && !a.calibration_resolves_alpha)
        .collect();
    failures.check(
        "no severity=high on an unresolvable calibration",
        uncertified_high.len(),
        0usize,
    );
    let onehot_as_a: Vec<_> = report.alerts.iter().filter(|a| a.slot_a == 1).collect();
    failures.check(
        "slot 1 (kind_onehot) is never lens A of a surviving alert",
        onehot_as_a.len(),
        0usize,
    );
    failures.check(
        "every refusal carries measured evidence",
        report
            .nondiscriminative_pairs
            .iter()
            .all(|p| p.records > 0 && !p.detail.is_empty()),
        true,
    );
    failures.check(
        "refusal count matches the reported diagnostics",
        report.nondiscriminative_pairs.len(),
        report.slot_pairs_nondiscriminative,
    );

    finish(&failures)
}

fn finish(failures: &Failures) -> Result<(), Box<dyn Error>> {
    println!("\n================================================================");
    if failures.0.is_empty() {
        println!("ALL CHECKS PASSED");
        Ok(())
    } else {
        println!("{} CHECK(S) FAILED:", failures.0.len());
        for failure in &failures.0 {
            println!("  - {failure}");
        }
        Err("blind_spot_discrimination_fsv failed".into())
    }
}
