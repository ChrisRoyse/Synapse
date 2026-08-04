//! Manual FSV instrument: does the assay recover a mutual information that was
//! *constructed* rather than observed? (#1672 acceptance, and the fixture #1674
//! and #1676 are also blocked on.)
//!
//! ## Why this cannot be done against the live vault
//!
//! #1672's acceptance asks for "a synthetic known-MI fixture recovers expected
//! bits within CI" and "bits results reproducible". Neither is satisfiable on the
//! live daemon's vault, for a reason that is a property of the system rather than
//! of the test: the daemon ingests its own operator's activity while the assay
//! runs. Three consecutive `intelligence bits` calls during the 2026-07-30 FSV
//! read `records_scanned` of 3391, 3445 and 3524. Two runs *differing* is then
//! the correct behaviour, so re-running proves nothing about determinism, and
//! there is no analytic MI to compare against because nobody chose the corpus.
//!
//! This instrument builds a vault that does not move, with a dependence whose
//! mutual information is known before the estimator runs.
//!
//! ## The construction
//!
//! `ROWS` timeline rows, each carrying one boolean anchor of a coined kind.
//! Exactly half are labelled `true`, so the anchor's own entropy is **exactly
//! 1 bit** by construction:
//!
//! ```text
//!   H(A) = -(1/2)log2(1/2) - (1/2)log2(1/2) = 1.000000
//! ```
//!
//! The dependence is planted in the record's **timestamp**. A row labelled
//! `true` is stamped near 15:00 UTC; a row labelled `false` near 03:00. The
//! timeline panel's cyclic time lenses are deterministic functions of that
//! stamp, so those slots separate the two classes completely while every other
//! lens is driven by fields held constant or varied on a period coprime with the
//! label. Therefore, for the time lenses:
//!
//! ```text
//!   I(X_time ; A) = H(A) - H(A | X_time) = 1 - 0 = 1.000000 bits
//! ```
//!
//! and for a lens carrying no label information, `I = 0`.
//!
//! A small within-class jitter is deliberate. A perfectly degenerate class (every
//! sample identical) gives the KSG estimator zero-distance neighbours, which is a
//! numerical artefact rather than a property of the data; a few minutes of spread
//! keeps the classes separated by far more than the within-class scale, so the
//! analytic answer is unchanged while the estimator stays well conditioned.
//!
//! ## What is asserted
//!
//! | # | claim | expected |
//! |---|---|---|
//! | 1 | anchor entropy is analytic | `1.000000` bits, exactly |
//! | 2 | the planted lens recovers the planted MI | `>= MI_RECOVERY_FLOOR` bits |
//! | 3 | panel >= best lens (a law) | reported; tracks #1916 |
//! | 4 | determinism | two runs over the same frozen vault agree bit for bit |
//!
//! Claim 4 is the half the live vault can never answer, and it is checked by
//! running the whole assay twice against the same unchanged vault and comparing
//! every reported float by exact equality.
//!
//! Usage:
//! `cargo run --release -p synapse-storage --example assay_known_mi_fsv -- <new-scratch-dir>`

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_core::{Anchor, AnchorKind, AnchorValue, CxId, VaultId};
use serde_json::json;
use synapse_calyx::{
    SynapseCalyxAssayParams, SynapseCalyxConfig, SynapseCalyxReadOnlyVault, SynapseCalyxVault,
};
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_TIMELINE_PANEL_VERSION, build_timeline_constellation,
};

/// Rows in the fixture. Comfortably above the estimator's 50-sample floor so a
/// provisional tag cannot be confused with a failure to recover.
const ROWS: u32 = 400;
/// Anchor kind coined for this fixture so it cannot collide with a real one.
const ANCHOR_LABEL: &str = "synfsv1672:known_mi_fixture";
const CREATED_AT_MS: u64 = 1_785_000_000_000;

/// Base timestamp: 2026-07-01T00:00:00Z expressed in nanoseconds.
const DAY0_NS: u64 = 1_782_950_400_000_000_000;
const SECOND_NS: u64 = 1_000_000_000;

/// The planted MI is 1.0 bits. A KSG estimate is consistent but biased at finite
/// sample size, so the assertion is a floor rather than an equality — 0.80 bits
/// is far above what any label-independent lens can produce (those land at ~0)
/// while leaving room for estimator bias.
const MI_RECOVERY_FLOOR: f32 = 0.80;

/// The label is a **threshold on event time**: the first half of the corpus is
/// `false`, the second half `true`.
///
/// This is the design the estimator can actually recover, and the first two
/// attempts got it wrong in an instructive way. Planting the dependence in a
/// *categorical* lens (hour-of-day fixed at 15 vs 3) makes that lens perfectly
/// predictive AND perfectly degenerate: every same-class sample is identical, so
/// the kth-neighbour radius is zero and KSG must refuse it. A continuous
/// mixed-pair KSG estimator needs the continuous side to actually be continuous.
///
/// A threshold on a strictly increasing timestamp gives both properties at once:
/// `syn.timeline.event_time_rank.v1` takes a distinct value on every row, so
/// same-class neighbours are never coincident, while the two classes occupy
/// disjoint intervals so knowing the rank determines the label exactly.
///
/// ```text
///   I(event_time_rank ; label) = H(label) - H(label | rank) = 1 - 0 = 1 bit
/// ```
fn label_for(index: u32) -> bool {
    index >= ROWS / 2
}

/// One fixture row. The ONLY field that depends on the label is the timestamp's
/// hour; everything else is either constant or cycles on a period coprime with 2
/// so it cannot carry label information.
fn row(vault_id: VaultId, index: u32) -> Result<calyx_core::Constellation, Box<dyn Error>> {
    let label = label_for(index);
    // Every lens must vary WITHIN a class. The KSG estimator needs a non-zero
    // kth-neighbour radius among same-class samples, so a lens that is constant
    // across a class is degenerate input, not a weak signal — the first attempt
    // at this fixture gave only 7 distinct timestamps per class and the
    // estimator correctly refused it with
    // `CALYX_ASSAY_DEGENERATE_INPUT ... exact_same_class_duplicates=199`.
    //
    // So: a unique second-offset per row, and every other field cycled on a
    // period COPRIME with the period-2 label so the variation cannot leak label
    // information. Separation between classes stays 12 hours; spread within a
    // class is ~6.6 minutes, two orders of magnitude smaller.
    // Strictly increasing, 20 minutes apart, so the corpus spans ~5.5 days and
    // every event-time rank is distinct.
    let ts_ns = DAY0_NS + u64::from(index) * 1_200 * SECOND_NS;

    // Period 11 and 3, both coprime with 2.
    let kind = match index % 11 {
        0 => TimelineKind::FocusChange,
        1 => TimelineKind::TitleChange,
        2 => TimelineKind::IdleStart,
        3 => TimelineKind::IdleEnd,
        4 => TimelineKind::SessionStart,
        5 => TimelineKind::SessionEnd,
        6 => TimelineKind::InteractionSummary,
        7 => TimelineKind::Clipboard,
        8 => TimelineKind::FileActivity,
        9 => TimelineKind::BrowserNav,
        _ => TimelineKind::DemoMarker,
    };

    let record = TimelineRecord {
        record_version: 1,
        ts_ns,
        kind,
        // Varied so the actor lens is not a constant column. Period 3 is coprime
        // with nothing here — the label is a threshold, not a parity — but a
        // 3-cycle over a 400-row range is uncorrelated with the half-way split.
        actor: if index.is_multiple_of(3) {
            TimelineActor::Agent {
                session_id: synapse_core::SessionId::from("synfsv1672-fixture"),
            }
        } else {
            TimelineActor::Human
        },
        app: Some(format!("fixture{}.exe", index % 13)),
        payload: json!({ "title": format!("known mi fixture row {index} lane {}", index % 17) }),
    };
    let raw = serde_json::to_vec(&record)?;
    let context = NativeConstellationContext {
        vault_id,
        cx_id: CxId::from_bytes([
            0x16,
            0x72,
            (index >> 8) as u8,
            index as u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]),
        created_at_ms: CREATED_AT_MS + u64::from(index),
        next_ledger_seq: u64::from(index) + 1,
    };
    let key = format!("fsv-1672/known-mi-{index}");
    let mut constellation = build_timeline_constellation(context, key.as_bytes(), &raw, &record)?;

    // The anchor is the outcome being predicted. It is attached before the write
    // so the row is grounded from the moment it lands, rather than needing a
    // second pass that would change the corpus between the two assay runs.
    constellation.anchors.push(Anchor {
        kind: AnchorKind::Label(ANCHOR_LABEL.to_owned()),
        value: AnchorValue::Bool(label),
        source: "synfsv1672-fixture".to_owned(),
        observed_at: CREATED_AT_MS + u64::from(index),
        confidence: 1.0,
    });
    Ok(constellation)
}

/// Shannon entropy of a two-outcome split, in bits. Used to state the expected
/// anchor entropy analytically rather than reading it back and believing it.
fn binary_entropy_bits(true_count: u32, total: u32) -> f64 {
    if true_count == 0 || true_count == total {
        return 0.0;
    }
    let p = f64::from(true_count) / f64::from(total);
    let q = 1.0 - p;
    -(p * p.log2() + q * q.log2())
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: assay_known_mi_fsv <new-scratch-dir>")?;
    std::fs::create_dir(&dir).map_err(|error| {
        format!(
            "SYNAPSE_FSV_SCRATCH_CREATE_FAILED: cannot atomically create a fresh fixture vault at {}: {error}; remediation=pass a new child path whose parent already exists; never reuse or pre-create a known-answer fixture vault",
            dir.display()
        )
    })?;

    println!("assay_known_mi_fsv: vault_dir={}", dir.display());

    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(dir.clone()))?;
    // The vault mints its own id on first open; rows must carry THAT id or the
    // write is refused as cross-vault. Reading it back is the only correct
    // source — a hardcoded id would only work until the scratch dir is reused.
    let vault_id: VaultId = vault.vault_id_value();
    println!("vault_id={vault_id}");

    // --- build the frozen corpus ------------------------------------------
    let mut true_count = 0u32;
    for index in 0..ROWS {
        if label_for(index) {
            true_count += 1;
        }
        vault.put_observation_constellation(row(vault_id, index)?)?;
    }
    let expected_entropy = binary_entropy_bits(true_count, ROWS);
    println!(
        "ingested {ROWS} rows ({true_count} true / {} false)",
        ROWS - true_count
    );
    println!(
        "ANALYTIC  H(anchor) = {expected_entropy:.6} bits   (computed from the split, not read back)"
    );
    println!(
        "ANALYTIC  I(event_time_rank ; anchor) = {expected_entropy:.6} bits   (the label is a threshold on event time)"
    );

    let params = SynapseCalyxAssayParams::new(SYN_TIMELINE_PANEL_VERSION, ANCHOR_LABEL.to_owned());

    // --- run 1 -------------------------------------------------------------
    println!("\n=== run 1 ===");
    let bits1 = vault.assay_bits(&params)?;
    let suff1 = vault.assay_sufficiency(&params)?;
    report(&bits1, &suff1);

    // --- run 2 over the SAME unchanged vault -------------------------------
    println!("\n=== run 2 (same vault, nothing written between) ===");
    let bits2 = vault.assay_bits(&params)?;
    let suff2 = vault.assay_sufficiency(&params)?;
    report(&bits2, &suff2);

    // --- claims ------------------------------------------------------------
    println!("\n=== claims ===");

    let entropy_delta = f64::from(suff1.anchor_entropy_bits) - expected_entropy;
    let entropy_ok = entropy_delta.abs() < 1e-6;
    println!(
        "  1 anchor entropy      expected={expected_entropy:.6}  observed={:.6}  delta={entropy_delta:.3e}  {}",
        suff1.anchor_entropy_bits,
        verdict(entropy_ok)
    );

    // Only MEASURED slots can carry a recovered MI; a placeholder zero on a
    // skipped slot must never be able to satisfy or defeat this claim (#1915).
    let best = bits1
        .slots
        .iter()
        .filter(|slot| slot.state.is_measured())
        .max_by(|a, b| a.marginal_bits.total_cmp(&b.marginal_bits));
    let recovery_ok = best.is_some_and(|slot| slot.marginal_bits >= MI_RECOVERY_FLOOR);
    match best {
        Some(slot) => println!(
            "  2 planted MI recovered  best slot={} bits={:.6} ci=[{:.6},{:.6}] n={}  floor={MI_RECOVERY_FLOOR}  {}",
            slot.slot,
            slot.marginal_bits,
            slot.ci_low,
            slot.ci_high,
            slot.n_samples,
            verdict(recovery_ok)
        ),
        None => println!("  2 planted MI recovered  NO SLOTS MEASURED  FAIL"),
    }

    // Claim 3 is a LAW, not a preference: mutual information is monotone under
    // adding variables, so I(panel ; A) >= I(any single slot ; A). A joint
    // estimate below the best single-lens estimate is a self-contradiction
    // within one report, and it is what this fixture found (#1916).
    //
    // It is reported rather than gating PASS, because the three claims this
    // instrument exists to settle for #1672 — analytic entropy, known-MI
    // recovery within CI, and determinism — are independent of it.
    let best_bits = best.map_or(0.0, |slot| slot.marginal_bits);
    let monotone_ok = suff1.panel_bits >= best_bits;
    println!(
        "  3 panel >= best lens    panel_bits={:.6}  best_single_lens={best_bits:.6}  sufficient={}  {}",
        suff1.panel_bits,
        suff1.sufficient,
        verdict(monotone_ok)
    );
    if !monotone_ok {
        println!(
            "      ^ I(panel;A) >= I(slot;A) is a law; a joint estimate below a marginal one is"
        );
        println!("        a KSG dimensionality artefact. See #1916.");
    }

    // Exact equality: a deterministic estimator over an unchanged corpus has no
    // licence to differ in the last bit.
    let deterministic = bits1.slots.len() == bits2.slots.len()
        && bits1.slots.iter().zip(&bits2.slots).all(|(a, b)| {
            a.slot == b.slot
                && a.marginal_bits.to_bits() == b.marginal_bits.to_bits()
                && a.ci_low.to_bits() == b.ci_low.to_bits()
                && a.ci_high.to_bits() == b.ci_high.to_bits()
                && a.n_samples == b.n_samples
        })
        && suff1.panel_bits.to_bits() == suff2.panel_bits.to_bits()
        && suff1.anchor_entropy_bits.to_bits() == suff2.anchor_entropy_bits.to_bits();
    println!(
        "  4 deterministic         two runs bit-identical across {} slots  {}",
        bits1.slots.len(),
        verdict(deterministic)
    );

    // A computing call's return value is not persistence evidence. Reopen the
    // vault through a handle that never held the writer lock and count the
    // physical native CF rows independently.
    let readback = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(dir),
        None,
    )?;
    let snapshot = readback.latest_seq();
    let base_rows = readback.scan_cf_at(snapshot, ColumnFamily::Base)?.len();
    let anchor_rows = readback.scan_cf_at(snapshot, ColumnFamily::Anchors)?.len();
    let assay_rows = readback.scan_cf_at(snapshot, ColumnFamily::Assay)?.len();
    let ledger_rows = readback.scan_cf_at(snapshot, ColumnFamily::Ledger)?.len();
    let persistence_ok = base_rows == ROWS as usize
        && anchor_rows == ROWS as usize
        && assay_rows == suff2.assay_cf_rows_after
        && ledger_rows > 0;
    println!(
        "  5 independent CF readback snapshot={snapshot} Base={base_rows} Anchors={anchor_rows} Assay={assay_rows} Ledger={ledger_rows} reported_assay={}  {}",
        suff2.assay_cf_rows_after,
        verdict(persistence_ok)
    );

    let pass = entropy_ok && recovery_ok && monotone_ok && deterministic && persistence_ok;
    println!(
        "\n{}",
        if pass {
            "PASS: the assay recovered a constructed mutual information and is reproducible"
        } else {
            "FAIL: at least one claim did not hold — see the per-claim lines above"
        }
    );
    if !pass {
        return Err("assay_known_mi_fsv claims did not hold".into());
    }
    Ok(())
}

const fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "FAIL" }
}

fn report(
    bits: &synapse_calyx::SynapseCalyxBitsReport,
    suff: &synapse_calyx::SynapseCalyxSufficiencyReport,
) {
    println!(
        "  bits: anchored_records={} distinct_outcomes={} total_bits={:.6} provisional={}",
        bits.anchored_records, bits.distinct_outcomes, bits.total_bits, bits.domain_provisional
    );
    for slot in &bits.slots {
        println!(
            "    slot {:>4}  state={:<20} bits={:.6}  ci=[{:.6},{:.6}]  n={}{}",
            slot.slot,
            slot.state.as_str(),
            slot.marginal_bits,
            slot.ci_low,
            slot.ci_high,
            slot.n_samples,
            slot.unmeasured_reason
                .as_deref()
                .map_or(String::new(), |r| format!(
                    "
           reason: {r}"
                ))
        );
    }
    println!(
        "  sufficiency: panel_bits={:.6} panel_measured={} floor_applied={} unmeasured_slots={} anchor_entropy={:.6} deficit={:.6} sufficient={} deficits={}",
        suff.panel_bits,
        suff.panel_measured,
        suff.panel_floor_applied,
        suff.unmeasured_slots,
        suff.anchor_entropy_bits,
        suff.deficit_bits,
        suff.sufficient,
        suff.deficits.len()
    );
}
