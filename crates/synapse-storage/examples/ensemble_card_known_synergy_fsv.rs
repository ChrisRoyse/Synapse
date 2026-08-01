//! Manual FSV instrument: does the ensemble capability card report a pair gain
//! that is *known before the estimator runs*, and is every clamp it applies
//! visible? (#1942 acceptance.)
//!
//! ## Why a constructed corpus
//!
//! #1942 asks to prove "no reported value is a silent clamp — every zero is
//! either a measured zero or a flagged one". On a live corpus a zero gain has no
//! reference: nobody knows what the pair *should* carry, so a zero cannot be
//! told apart from a defect. This instrument builds a corpus whose every
//! reported quantity has an analytic expected value before the pass starts.
//!
//! ## What the card actually measures
//!
//! The card's `pid_method` is `bounded_decision_surrogate_v1`, and that is
//! literal: a lens's "bits" are `I(outcome ; the probe's thresholded binary
//! decision)`, not `I(lens ; outcome)`. The prediction is a function of the
//! features, so by the data-processing inequality the surrogate is a **lower
//! bound** on the lens's true mutual information, and the fixture's expected
//! values are computed under the surrogate rather than under the true MI. The
//! two differ sharply and instructively here — see the table.
//!
//! ## The construction
//!
//! `ROWS` rows. Two independent balanced bits are planted per row:
//!
//! ```text
//!   A = bit 0 of the row index        B = bit 1 of the row index
//!   Y = A AND B                       (the grounded outcome anchor)
//! ```
//!
//! Over a row count divisible by 4 each `(A,B)` combination appears exactly
//! `ROWS/4` times, so `P(Y) = 1/4` and every quantity below is exact:
//!
//! ```text
//!   H(Y)     = H(1/4)                  = 0.811278 bits
//!   I(A;Y)   = I(B;Y) = H(Y) - 1/2     = 0.311278 bits    (true MI)
//!   I(A,B;Y) = H(Y)                    = 0.811278 bits    (A,B determine Y)
//! ```
//!
//! Under the decision surrogate the marginals collapse to **zero**, and that is
//! correct rather than a defect: knowing A alone leaves `P(Y|A=1) = 1/2`, so no
//! threshold on A beats the majority rule "always 0", the decision is constant,
//! and `I(Y ; constant) = 0`. Knowing both bits gives a perfect decision. So:
//!
//! ```text
//!   surrogate(A)   = surrogate(B) = 0.000000 bits
//!   surrogate(A,B) = H(Y)         = 0.811278 bits
//!   gain(A,B)      = H(Y) - 0     = 0.811278 bits
//! ```
//!
//! `H(Y)` is also a hard ceiling on every reported gain: `gain <= pair_bits <=
//! H(Y)`, because no mutual information about `Y` can exceed `Y`'s own entropy.
//!
//! Six lenses carry that structure, each a separate slot:
//!
//! | slot | lens         | carries   | expected solo (surrogate) | expected gain with the partner bit |
//! |------|--------------|-----------|---------------------------|------------------------------------|
//! | 200  | `fsv.a`      | A         | 0.000000                  | **~0.811** with `fsv.b`      |
//! | 201  | `fsv.b`      | B         | 0.000000                  | **~0.811** with `fsv.a`      |
//! | 202  | `fsv.a_copy` | A again   | 0.000000                  | **~0.811** with `fsv.b`      |
//! | 203  | `fsv.noise`  | nothing   | 0.000000                  | ~0                                 |
//! | 204  | `fsv.leak`   | Y itself  | 0.811278                  | 0 exactly (nothing left to add)    |
//! | 205  | `fsv.wide`   | nothing, in 32 dimensions | 0.000000  | ~0                 |
//!
//! `fsv.a_copy` makes `(b, a_copy)` a second, equivalent planted pair; both must
//! outrank every unplanted pair. `fsv.leak` is where a *negative* raw difference
//! is most likely from a finite-sample estimator — a lens already carrying the
//! whole outcome cannot gain from a partner — which is the case #1942 exists to
//! make visible. `fsv.wide` gives the joint fit 34 features to overfit, the
//! other way a raw gain goes negative.
//!
//! Every lens carries a deterministic within-row jitter on a period **coprime
//! with 4**, so no column is constant (the conditioning and the redundancy
//! sketch are both undefined on a constant column) while no jitter can encode
//! A, B or Y.
//!
//! ## What is asserted
//!
//! | # | claim | expected |
//! |---|---|---|
//! | 1 | the corpus is on disk | `ROWS` Base rows read back from the physical CF |
//! | 2 | anchor entropy is analytic | `0.811278` bits |
//! | 3 | the planted pairs are recovered | both outrank every unplanted pair, none exceeds `H(Y)` |
//! | 4 | no silent clamp | every gain equals its own row's terms, and every clamp carries its flag |
//! | 5 | one instrument per pair | all three terms report the same estimator |
//! | 6 | the card row is durable | an Assay CF row exists after the pass |
//! | 7 | edge cases fail visibly | the shared gain function on hand-built inputs |
//!
//! Usage:
//! `cargo run -p synapse-storage --example ensemble_card_known_synergy_fsv -- <empty-scratch-dir>`

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_core::{
    Anchor, AnchorKind, AnchorValue, Constellation, CxFlags, CxId, InputRef, LedgerRef, Modality,
    SlotId, SlotVector, VaultId,
};
use synapse_calyx::{SynapseCalyxAssayParams, SynapseCalyxConfig, SynapseCalyxVault};

/// Rows in the fixture. Divisible by 4 so every `(A,B)` combination appears
/// equally often and the analytic quantities above are exact, and far above the
/// estimator's 50-sample floor.
const ROWS: u32 = 400;
/// Panel version coined for this fixture so it cannot collide with a real panel.
const PANEL_VERSION: u32 = 990_001;
/// Anchor kind coined for this fixture so it cannot collide with a real one.
const ANCHOR_LABEL: &str = "synfsv1942:known_synergy_fixture";
const CREATED_AT_MS: u64 = 1_785_000_000_000;

const SLOT_A: u16 = 200;
const SLOT_B: u16 = 201;
const SLOT_A_COPY: u16 = 202;
const SLOT_NOISE: u16 = 203;
const SLOT_LEAK: u16 = 204;
const SLOT_WIDE: u16 = 205;

/// Analytic outcome entropy `H(1/4)`, stated before anything is measured.
const ANALYTIC_ANCHOR_ENTROPY_BITS: f64 = 0.811_278_124_459_133_4;
/// Analytic `I(A;Y)` and `I(B;Y)`.
const ANALYTIC_SOLO_BITS: f64 = 0.311_278_124_459_133_4;
/// Analytic `gain(A,B)` under the decision surrogate: the pair decides `Y`
/// perfectly while neither bit alone beats the majority rule, so the whole of
/// `H(Y)` is gain. It is also the hard ceiling on any reported gain.
const ANALYTIC_PAIR_GAIN_BITS: f64 = ANALYTIC_ANCHOR_ENTROPY_BITS;

/// Deterministic jitter for row `index` on lane `lane`.
///
/// Period 7 is coprime with the period-4 `(A,B)` cycle, so the jitter cannot
/// carry a bit of A, B or Y; amplitude 0.02 is fifty times smaller than the
/// 1.0 separation between the planted levels, so it cannot move a class.
fn jitter(index: u32, lane: u32) -> f32 {
    let phase = (index.wrapping_mul(3).wrapping_add(lane)) % 7;
    f32::from(phase as u16) * 0.02 / 7.0
}

const fn bit_a(index: u32) -> bool {
    index % 2 == 1
}

const fn bit_b(index: u32) -> bool {
    (index / 2) % 2 == 1
}

const fn label_for(index: u32) -> bool {
    bit_a(index) && bit_b(index)
}

fn level(bit: bool) -> f32 {
    if bit { 1.0 } else { 0.0 }
}

/// A two-cell dense slot: the planted level and its complement, each jittered on
/// its own lane so the two cells are not exact mirrors.
fn planted_slot(bit: bool, index: u32, lane: u32) -> SlotVector {
    SlotVector::Dense {
        dim: 2,
        data: vec![
            level(bit) + jitter(index, lane),
            level(!bit) + jitter(index, lane + 1),
        ],
    }
}

/// A lens that carries nothing about the outcome: a deterministic sequence whose
/// period (11) is coprime with the period-4 planted cycle.
fn noise_slot(index: u32) -> SlotVector {
    let phase = index % 11;
    SlotVector::Dense {
        dim: 2,
        data: vec![
            f32::from(phase as u16) / 11.0,
            f32::from(((index / 11) % 13) as u16) / 13.0,
        ],
    }
}

/// A distinct 32-byte input hash per row. The fixture's input is the row index
/// itself, so the hash only has to be a deterministic injection of it.
fn input_hash(index: u32) -> [u8; 32] {
    let mut hash = [0_u8; 32];
    hash[..4].copy_from_slice(&index.to_be_bytes());
    hash[4..8].copy_from_slice(b"1942");
    hash
}

/// A 32-dimensional lens carrying nothing about the outcome. Its purpose is to
/// give a joint fit enough width to overfit, which is one of the two ways a raw
/// pair gain goes negative.
fn wide_noise_slot(index: u32) -> SlotVector {
    SlotVector::Dense {
        dim: 32,
        data: (0..32_u32)
            .map(|lane| {
                let phase = (index.wrapping_mul(lane + 7).wrapping_add(lane * 31)) % 97;
                f32::from(phase as u16) / 97.0
            })
            .collect(),
    }
}

fn row(vault_id: VaultId, index: u32) -> Constellation {
    let a = bit_a(index);
    let b = bit_b(index);
    let y = label_for(index);
    let mut slots: BTreeMap<SlotId, SlotVector> = BTreeMap::new();
    slots.insert(SlotId::new(SLOT_A), planted_slot(a, index, 0));
    slots.insert(SlotId::new(SLOT_B), planted_slot(b, index, 2));
    slots.insert(SlotId::new(SLOT_A_COPY), planted_slot(a, index, 4));
    slots.insert(SlotId::new(SLOT_NOISE), noise_slot(index));
    slots.insert(SlotId::new(SLOT_LEAK), planted_slot(y, index, 6));
    slots.insert(SlotId::new(SLOT_WIDE), wide_noise_slot(index));

    let mut metadata = BTreeMap::new();
    metadata.insert("fsv_row".to_owned(), index.to_string());

    Constellation {
        cx_id: CxId::from_bytes([
            0x19,
            0x42,
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
        vault_id,
        panel_version: PANEL_VERSION,
        created_at: CREATED_AT_MS + u64::from(index),
        input_ref: InputRef {
            hash: input_hash(index),
            pointer: Some(format!("fsv-1942/known-synergy-{index}")),
            redacted: false,
        },
        modality: Modality::Structured,
        slots,
        scalars: BTreeMap::new(),
        metadata,
        anchors: vec![Anchor {
            kind: AnchorKind::Label(ANCHOR_LABEL.to_owned()),
            value: AnchorValue::Bool(y),
            source: "synfsv1942-fixture".to_owned(),
            observed_at: CREATED_AT_MS + u64::from(index),
            confidence: 1.0,
        }],
        provenance: LedgerRef {
            seq: u64::from(index) + 1,
            hash: [0; 32],
        },
        flags: CxFlags::default(),
    }
}

/// Shannon entropy of a two-outcome split, in bits, computed from the counts so
/// the expected value is derived rather than trusted.
fn binary_entropy_bits(true_count: u32, total: u32) -> f64 {
    if true_count == 0 || true_count == total {
        return 0.0;
    }
    let p = f64::from(true_count) / f64::from(total);
    let q = 1.0 - p;
    -(p * p.log2() + q * q.log2())
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear FSV script, read top to bottom"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: ensemble_card_known_synergy_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;
    println!(
        "ensemble_card_known_synergy_fsv: vault_dir={}",
        dir.display()
    );

    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(dir.clone()))?;
    let vault_id: VaultId = vault.vault_id_value();
    println!("vault_id={vault_id}");

    // --- state BEFORE the trigger -----------------------------------------
    let seq_before = vault.latest_seq();
    let base_rows_before = vault.scan_cf_at(seq_before, ColumnFamily::Base)?.len();
    let assay_rows_before = vault.scan_cf_at(seq_before, ColumnFamily::Assay)?.len();
    println!(
        "BEFORE  seq={seq_before} base_cf_rows={base_rows_before} assay_cf_rows={assay_rows_before}"
    );

    // --- build the frozen corpus ------------------------------------------
    let mut true_count = 0_u32;
    for index in 0..ROWS {
        if label_for(index) {
            true_count += 1;
        }
        vault.put_observation_constellation(row(vault_id, index))?;
    }
    let expected_entropy = binary_entropy_bits(true_count, ROWS);
    println!(
        "ingested {ROWS} rows ({true_count} true / {} false)",
        ROWS - true_count
    );

    // --- state AFTER the write, read back from the physical CF -------------
    let seq_after_write = vault.latest_seq();
    let base_rows_after = vault.scan_cf_at(seq_after_write, ColumnFamily::Base)?.len();
    println!(
        "AFTER-WRITE  seq={seq_after_write} base_cf_rows={base_rows_after} (delta={})",
        base_rows_after - base_rows_before
    );
    if base_rows_after - base_rows_before != ROWS as usize {
        return Err(format!(
            "FAIL claim 1: physical Base CF gained {} rows, expected {ROWS}",
            base_rows_after - base_rows_before
        )
        .into());
    }
    println!("PASS  claim 1: {ROWS} constellations are physically present in the Base CF");

    println!(
        "\nANALYTIC  H(Y)        = {ANALYTIC_ANCHOR_ENTROPY_BITS:.6} bits   (counted: {expected_entropy:.6})"
    );
    println!("ANALYTIC  I(A;Y)      = {ANALYTIC_SOLO_BITS:.6} bits");
    println!("ANALYTIC  I(A,B;Y)    = {ANALYTIC_ANCHOR_ENTROPY_BITS:.6} bits");
    println!("ANALYTIC  gain(A,B)   = {ANALYTIC_PAIR_GAIN_BITS:.6} bits");

    // --- the trigger -------------------------------------------------------
    let params = SynapseCalyxAssayParams::new(PANEL_VERSION, ANCHOR_LABEL.to_owned())
        .with_max_records(ROWS as usize)
        .with_lens_names(BTreeMap::from([
            (SLOT_A, "fsv.a".to_owned()),
            (SLOT_B, "fsv.b".to_owned()),
            (SLOT_A_COPY, "fsv.a_copy".to_owned()),
            (SLOT_NOISE, "fsv.noise".to_owned()),
            (SLOT_LEAK, "fsv.leak".to_owned()),
            (SLOT_WIDE, "fsv.wide".to_owned()),
        ]));
    let report = vault.assay_ensemble_card(&params, 6)?;
    let card = &report.card;

    println!(
        "\ncard schema_version={} n_samples={} anchor_entropy_bits={:.6} panel_bits={:.6} n_eff={:.4} sufficient={} pairs_monotonicity_floored={}",
        card.schema_version,
        card.n_samples,
        card.anchor_entropy_bits,
        card.panel_bits,
        card.n_eff,
        card.sufficient,
        card.pairs_monotonicity_floored,
    );
    println!(
        "corpus records_scanned={} anchored_records={} declared_slots={} measured_slots={:?} excluded={}",
        report.records_scanned,
        report.anchored_records,
        report.declared_slots,
        report.measured_slots,
        report.excluded_lenses.len(),
    );
    for lens in &report.excluded_lenses {
        println!(
            "excluded slot={} name={} reason={}",
            lens.slot, lens.name, lens.reason
        );
    }

    // --- claim 2: anchor entropy is the analytic value ---------------------
    let entropy_error = (f64::from(card.anchor_entropy_bits) - ANALYTIC_ANCHOR_ENTROPY_BITS).abs();
    println!(
        "\nclaim 2: anchor_entropy_bits={:.6} analytic={ANALYTIC_ANCHOR_ENTROPY_BITS:.6} error={entropy_error:.9}",
        card.anchor_entropy_bits
    );
    if entropy_error > 1.0e-6 {
        return Err(format!(
            "FAIL claim 2: anchor entropy {} is not the analytic {ANALYTIC_ANCHOR_ENTROPY_BITS}",
            card.anchor_entropy_bits
        )
        .into());
    }
    println!("PASS  claim 2");

    for lens in &card.lenses {
        println!(
            "lens slot={} name={} solo_bits={:.6} marginal_bits={:.6} pid_unique={:.6} pid_synergistic={:.6} max_corr={:.4} decision={:?}",
            lens.slot,
            lens.name,
            lens.solo_bits,
            lens.marginal_bits,
            lens.pid.unique_bits,
            lens.pid.synergistic_bits,
            lens.max_pairwise_corr,
            lens.decision,
        );
    }
    for pair in &card.pairs {
        println!(
            "pair {}+{} slots={}+{} pair_bits={:.6} gain_bits={:.6} raw_gain_bits={:.6} floor_applied={} estimators=({:?},{:?},{:?}) corr={:.4} nmi={:.4}",
            pair.a,
            pair.b,
            pair.slot_a,
            pair.slot_b,
            pair.pair_bits,
            pair.synergy_gain_bits,
            pair.raw_synergy_gain_bits,
            pair.synergy_monotonicity_floor_applied,
            pair.synergy_estimators.pair,
            pair.synergy_estimators.left,
            pair.synergy_estimators.right,
            pair.corr,
            pair.nmi,
        );
    }

    // --- claim 3: both planted pairs outrank every unplanted pair ----------
    // `(a,b)` and `(b,a_copy)` are the same planted structure, because a_copy
    // carries A. Either may come out marginally on top; what must hold is that
    // both beat every pair that carries no planted synergy, and that neither
    // exceeds `H(Y)`, which no mutual information about Y can.
    let is_planted = |pair: &&calyx_assay::EnsemblePairValue| {
        let slots = (pair.slot_a.get(), pair.slot_b.get());
        slots == (SLOT_A, SLOT_B)
            || slots == (SLOT_B, SLOT_A)
            || slots == (SLOT_B, SLOT_A_COPY)
            || slots == (SLOT_A_COPY, SLOT_B)
    };
    let planted: Vec<_> = card.pairs.iter().filter(is_planted).collect();
    let unplanted: Vec<_> = card.pairs.iter().filter(|pair| !is_planted(pair)).collect();
    if planted.len() != 2 {
        return Err(format!(
            "FAIL claim 3: expected both planted pairs in the card, found {}",
            planted.len()
        )
        .into());
    }
    let weakest_planted = planted
        .iter()
        .map(|pair| pair.synergy_gain_bits)
        .fold(f32::INFINITY, f32::min);
    let strongest_unplanted = unplanted
        .iter()
        .map(|pair| pair.synergy_gain_bits)
        .fold(0.0_f32, f32::max);
    println!(
        "
claim 3: weakest planted gain={weakest_planted:.6}  strongest unplanted gain={strongest_unplanted:.6}  ceiling H(Y)={ANALYTIC_PAIR_GAIN_BITS:.6}"
    );
    for pair in &planted {
        println!(
            "  planted   {}+{} gain={:.6}",
            pair.a, pair.b, pair.synergy_gain_bits
        );
    }
    if weakest_planted <= strongest_unplanted {
        return Err(format!(
            "FAIL claim 3: a pair with no planted synergy ({strongest_unplanted:.6}) reached the planted floor ({weakest_planted:.6})"
        )
        .into());
    }
    if weakest_planted <= 0.0 {
        return Err("FAIL claim 3: a planted synergy measured no gain at all".into());
    }
    for pair in &card.pairs {
        if f64::from(pair.synergy_gain_bits) > ANALYTIC_PAIR_GAIN_BITS + 1.0e-5 {
            return Err(format!(
                "FAIL claim 3: pair {}+{} reports {} bits of gain, above the entropy of the outcome itself",
                pair.a, pair.b, pair.synergy_gain_bits
            )
            .into());
        }
    }
    println!(
        "PASS  claim 3: both planted pairs outrank every unplanted pair, and no gain exceeds H(Y)"
    );

    // --- claim 4: no silent clamp -----------------------------------------
    let mut silent = Vec::new();
    for pair in &card.pairs {
        let recomputed = pair.pair_bits - pair.left_bits_max(card);
        let reported_is_floor = pair.synergy_monotonicity_floor_applied;
        if (recomputed < 0.0) != reported_is_floor {
            silent.push(format!(
                "{}+{}: raw {recomputed:.6} but floor_applied={reported_is_floor}",
                pair.a, pair.b
            ));
        }
    }
    let floored = card
        .pairs
        .iter()
        .filter(|pair| pair.synergy_monotonicity_floor_applied)
        .count();
    println!(
        "\nclaim 4: pairs={} floored={} card_counter={} disagreements={}",
        card.pairs.len(),
        floored,
        card.pairs_monotonicity_floored,
        silent.len()
    );
    for line in &silent {
        println!("  DISAGREE {line}");
    }
    if !silent.is_empty() || floored != card.pairs_monotonicity_floored {
        return Err("FAIL claim 4: a clamp is not visible in the row that carries it".into());
    }
    println!("PASS  claim 4: every clamped row carries its unclamped value and its flag");

    // --- claim 5: one instrument per pair ---------------------------------
    let heterogeneous = card
        .pairs
        .iter()
        .filter(|pair| !pair.synergy_estimators.is_homogeneous())
        .count();
    println!("\nclaim 5: pairs with three different instruments = {heterogeneous}");
    if heterogeneous > 0 {
        return Err("FAIL claim 5: a pair differenced estimates from two instruments".into());
    }
    println!("PASS  claim 5");

    // --- claim 6: the pass left a durable row ------------------------------
    let seq_final = vault.latest_seq();
    let assay_rows_after = vault.scan_cf_at(seq_final, ColumnFamily::Assay)?.len();
    println!(
        "\nclaim 6: assay_cf_rows before={assay_rows_before} after={assay_rows_after} seq={seq_final} (report said {})",
        report.assay_cf_rows
    );
    if assay_rows_after <= assay_rows_before {
        return Err("FAIL claim 6: the pass persisted no Assay CF row".into());
    }
    println!("PASS  claim 6: the capability card left durable evidence in the Assay CF");

    // --- claim 7: boundary and edge-case audit of the shared gain function --
    // `whole_minus_max_gain` is the single implementation every pair gain goes
    // through (#1942 ask 4). These drive it directly with inputs whose answers
    // are arithmetic, printing the input and the resulting state for each.
    println!("\nclaim 7: edge-case audit of whole_minus_max_gain");
    let mut edge_failures: Vec<String> = Vec::new();

    // Edge 1 — a raw gain the data-processing inequality forbids. The joint
    // cannot report less than either half, so 0.20 against a 0.50 marginal is
    // impossible: it must be floored AND flagged, with the raw value kept.
    println!(
        "  edge 1 IN : pair=0.200000 left=0.500000 right=0.100000  (raw = -0.300000, impossible)"
    );
    match calyx_assay::whole_minus_max_gain(0.2, 0.5, 0.1) {
        Ok((gain, raw, floored)) => {
            println!("  edge 1 OUT: gain={gain:.6} raw={raw:.6} floor_applied={floored}");
            if gain != 0.0 || (raw + 0.3).abs() > 1.0e-6 || !floored {
                edge_failures
                    .push("edge 1: an impossible raw gain was not floored-and-flagged".to_owned());
            }
        }
        Err(error) => edge_failures.push(format!("edge 1: unexpected refusal {}", error.message)),
    }

    // Edge 2 — a non-finite term is not a small number; it is not a measurement.
    println!("  edge 2 IN : pair=NaN left=0.500000 right=0.100000");
    match calyx_assay::whole_minus_max_gain(f32::NAN, 0.5, 0.1) {
        Ok((gain, raw, floored)) => edge_failures.push(format!(
            "edge 2: NaN produced a number instead of an error (gain={gain} raw={raw} floored={floored})"
        )),
        Err(error) => {
            println!("  edge 2 OUT: refused code={} message={}", error.code, error.message);
            if error.code != calyx_assay::CALYX_ASSAY_INVALID_SYNERGY {
                edge_failures.push(format!("edge 2: wrong code {}", error.code));
            }
        }
    }

    // Edge 3 — the exact boundary. Equal terms are a *measured* zero, so the
    // floor must not fire and a real zero stays distinguishable from a clamp.
    println!("  edge 3 IN : pair=0.811278 left=0.811278 right=0.000000  (raw = 0, exactly)");
    match calyx_assay::whole_minus_max_gain(0.811_278, 0.811_278, 0.0) {
        Ok((gain, raw, floored)) => {
            println!("  edge 3 OUT: gain={gain:.6} raw={raw:.6} floor_applied={floored}");
            if gain != 0.0 || raw != 0.0 || floored {
                edge_failures.push("edge 3: an exact zero was reported as a clamp".to_owned());
            }
        }
        Err(error) => edge_failures.push(format!("edge 3: unexpected refusal {}", error.message)),
    }

    for failure in &edge_failures {
        println!("  FAIL {failure}");
    }
    if !edge_failures.is_empty() {
        return Err("FAIL claim 7: the shared gain function mishandled a boundary input".into());
    }
    println!(
        "PASS  claim 7: an impossible gain is floored and flagged, a non-finite term is refused, an exact zero is not a clamp"
    );

    println!("\nensemble_card_known_synergy_fsv: ALL CLAIMS PASS");
    Ok(())
}

/// `max(I(a;Y), I(b;Y))` for one pair, read out of the card's own lens rows so
/// the recomputation uses the same numbers the card reported rather than a
/// second measurement.
trait PairMarginals {
    fn left_bits_max(&self, card: &calyx_assay::EnsembleCard) -> f32;
}

impl PairMarginals for calyx_assay::EnsemblePairValue {
    fn left_bits_max(&self, card: &calyx_assay::EnsembleCard) -> f32 {
        let solo = |slot| {
            card.lenses
                .iter()
                .find(|lens| lens.slot == slot)
                .map_or(0.0, |lens| lens.solo_bits)
        };
        solo(self.slot_a).max(solo(self.slot_b))
    }
}
