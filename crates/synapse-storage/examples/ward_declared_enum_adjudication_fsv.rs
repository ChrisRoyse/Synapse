//! Manual FSV instrument: can Ward calibrate against a *declared* enum
//! adjudication, and does it still refuse everything it refused before? (#1919)
//!
//! ## Why this exists
//!
//! #1919 found Ward uncalibratable on the live vault and read the cause as one
//! lock — calibration is pinned to the single durable active panel, and that
//! panel carries no adjudicated outcome. Measuring all fourteen panels showed a
//! **second** lock underneath it, and the second one is the reason fixing the
//! first alone would not have helped.
//!
//! `syn-mcp-usage-v1` @ 1776006 holds ~1,000 records at 1.0000 grounded
//! coverage, split 791 good / 211 bad — by far the best calibration corpus on
//! the vault. Ward cannot use a single one of them, because
//! `synapse:mcp_tool_call_outcome` is written as an **enum** anchor and Ward's
//! corpus scan accepted only `AnchorValue::Bool`.
//!
//! That rule was right and stays right for an arbitrary enum. But this enum's
//! polarity is not arbitrary and is not Ward's to guess: `mcp_usage.rs` itself
//! counts `status == "ok"` as a success row and `status != "ok"` as an error
//! row, over these exact records, a few hundred lines from the anchor writer.
//! So the adjudication already exists in production code; it simply was not the
//! thing written to the anchor.
//!
//! `SYNAPSE_DECLARED_ENUM_ADJUDICATIONS` lets that decision be *declared* —
//! closed table, one entry, each citing the code that already makes the split.
//! This instrument proves the declaration works and, more importantly, that
//! everything Ward used to refuse it still refuses.
//!
//! ## Why it cannot be verified on the live vault
//!
//! `GUARD_PANEL_MISMATCH` fires *before* the corpus is ever scanned, so on the
//! live vault this code path is unreachable until #1919's second half (a guard
//! profile per panel) lands. The corpus is therefore constructed here, with a
//! split known before the vault is opened, and every verdict is read back from
//! the physical Guard CF rather than believed from a return value.
//!
//! ```text
//! cargo run --release -p synapse-storage --example ward_declared_enum_adjudication_fsv -- <empty-scratch-dir>
//! ```
//!
//! ## The corpus
//!
//! 550 timeline rows: **400 good + 150 bad**, a ratio close to the live
//! 791/211. 150 is not arbitrary — Ward requires `MIN_BAD_SCORES = 50`, and the
//! exact one-sided Clopper-Pearson bound needs `(1 - target_far)^n <= alpha`,
//! which at the content aspect's default target and `alpha = 0.05` is 99 bad
//! scores. 150 clears it; the live corpus's 211 would too.
//!
//! Good rows sit at hours 8-11 and bad rows at hours 20-23 — twelve hours
//! apart, so on the `hour_cyclic` lens (slot 4, dense `[sin, cos]`) they are
//! antipodal and a conformal threshold genuinely exists to be found. A corpus
//! where good and bad are geometrically identical would fail to certify for
//! reasons that have nothing to do with adjudication, and would prove nothing.

use std::error::Error;
use std::path::{Path, PathBuf};

use calyx_core::{Anchor, AnchorKind, AnchorValue, CxId, VaultId};
use serde_json::json;
use synapse_calyx::{
    SynapseCalyxConfig, SynapseCalyxGuardAspect, SynapseCalyxGuardCalibrateParams,
    SynapseCalyxGuardSlotSpec, SynapseCalyxVault,
};
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_TIMELINE_PANEL_VERSION, build_timeline_constellation,
    syn_active_panel_contract,
};

/// The declared kind. Must match `SYNAPSE_DECLARED_ENUM_ADJUDICATIONS`.
const DECLARED_KIND: &str = "synapse:mcp_tool_call_outcome";
/// A kind deliberately absent from the table, to prove the fail-closed default.
const UNDECLARED_KIND: &str = "synapse:fsv_1919_undeclared_outcome";
/// The declared good value.
const GOOD_VALUE: &str = "ok";
/// One of the many values that are not it.
const BAD_VALUE: &str = "error";

const GOOD_ROWS: u32 = 400;
const BAD_ROWS: u32 = 150;
const TOTAL_ROWS: u32 = GOOD_ROWS + BAD_ROWS;

/// The guarded lens: `syn.timeline.hour_cyclic.v1`, dense `[sin, cos]`.
const GUARD_SLOT: u16 = 4;

const CREATED_AT_MS: u64 = 1_785_000_000_000;
/// 2026-07-01T00:00:00Z in nanoseconds.
const DAY0_NS: u64 = 1_782_950_400_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;
const DAY_NS: u64 = 24 * HOUR_NS;
const MINUTE_NS: u64 = 60_000_000_000;

/// What anchor each row of a given corpus carries. The whole experiment is a
/// sweep over this.
#[derive(Clone, Copy)]
enum AnchorPlan {
    /// The declared enum kind, `ok` for good rows and `error` for bad ones.
    DeclaredEnum,
    /// The same values under a kind that is not in the table.
    UndeclaredEnum,
    /// The declared kind, but `ok` on every row — no bad case exists.
    DeclaredEnumAllGood,
    /// `Bool(true)` / `Bool(false)` — the path that always worked.
    Bool,
    /// The declared enum AND a contradicting Bool on the same record.
    ConflictingEnumAndBool,
}

fn anchor_for(plan: AnchorPlan, good: bool, observed_at: u64) -> Vec<Anchor> {
    let enum_anchor = |kind: &str, value: &str| Anchor {
        kind: AnchorKind::Label(kind.to_owned()),
        value: AnchorValue::Enum(value.to_owned()),
        source: "synfsv1919-fixture".to_owned(),
        observed_at,
        confidence: 1.0,
    };
    let bool_anchor = |value: bool| Anchor {
        kind: AnchorKind::Label(DECLARED_KIND.to_owned()),
        value: AnchorValue::Bool(value),
        source: "synfsv1919-fixture".to_owned(),
        observed_at,
        confidence: 1.0,
    };
    let value = if good { GOOD_VALUE } else { BAD_VALUE };
    match plan {
        AnchorPlan::DeclaredEnum => vec![enum_anchor(DECLARED_KIND, value)],
        AnchorPlan::UndeclaredEnum => vec![enum_anchor(UNDECLARED_KIND, value)],
        AnchorPlan::DeclaredEnumAllGood => vec![enum_anchor(DECLARED_KIND, GOOD_VALUE)],
        AnchorPlan::Bool => vec![bool_anchor(good)],
        // Deliberately contradictory: the enum says good, the Bool says bad.
        AnchorPlan::ConflictingEnumAndBool => {
            vec![enum_anchor(DECLARED_KIND, GOOD_VALUE), bool_anchor(false)]
        }
    }
}

/// One fixture row. `index < GOOD_ROWS` is a good row.
fn row(
    vault_id: VaultId,
    index: u32,
    plan: AnchorPlan,
) -> Result<calyx_core::Constellation, Box<dyn Error>> {
    let good = index < GOOD_ROWS;
    // Good rows at hours 8-11, bad rows at 20-23: antipodal on the 24h circle,
    // so slot 4's [sin, cos] separates them. Minute offsets keep every vector
    // distinct so leave-one-out scoring is not measuring a constant column.
    let hour = if good {
        8 + u64::from(index % 4)
    } else {
        20 + u64::from(index % 4)
    };
    let ts_ns = DAY0_NS
        + u64::from(index) * DAY_NS / u64::from(TOTAL_ROWS)
        + hour * HOUR_NS
        + u64::from(index % 47) * MINUTE_NS;

    let record = TimelineRecord {
        record_version: 1,
        ts_ns,
        kind: TimelineKind::FocusChange,
        actor: TimelineActor::Human,
        app: Some(format!("fsv1919-{}.exe", index % 7)),
        payload: json!({ "title": format!("ward fixture row {index}") }),
    };
    let raw = serde_json::to_vec(&record)?;
    let mut cx_bytes = [0_u8; 16];
    cx_bytes[0] = 0x19;
    cx_bytes[1] = 0x19;
    // Big-endian index in two bytes. `TOTAL_ROWS` is 550, so the high byte is
    // 0..=2 and the split is exact; the masks say so rather than relying on it.
    cx_bytes[2] = u8::try_from((index >> 8) & 0xFF).unwrap_or(0);
    cx_bytes[3] = u8::try_from(index & 0xFF).unwrap_or(0);
    let context = NativeConstellationContext {
        vault_id,
        cx_id: CxId::from_bytes(cx_bytes),
        created_at_ms: CREATED_AT_MS + u64::from(index),
        next_ledger_seq: u64::from(index) + 1,
    };
    let key = format!("fsv-1919/ward-{index}");
    let mut constellation = build_timeline_constellation(context, key.as_bytes(), &raw, &record)?;
    constellation
        .anchors
        .extend(anchor_for(plan, good, CREATED_AT_MS + u64::from(index)));
    Ok(constellation)
}

/// What one calibration attempt produced. `Err` carries the structured code.
struct Attempt {
    good: usize,
    bad: usize,
    unadjudicated: usize,
    conflicting: usize,
    scanned: usize,
    outcome: Result<Calibrated, String>,
}

struct Calibrated {
    persisted: bool,
    readback_calibrated: bool,
    guard_cf_profile_bytes: usize,
    guard_cf_rows_after: usize,
    tau: f32,
}

/// Builds a fresh vault under `root/name`, ingests the corpus under `plan`, and
/// runs one guard calibration against it.
fn attempt(root: &Path, name: &str, plan: AnchorPlan) -> Result<Attempt, Box<dyn Error>> {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir)?;
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(dir))?;
    let vault_id = vault.vault_id_value();

    for index in 0..TOTAL_ROWS {
        vault.put_observation_constellation(row(vault_id, index, plan)?)?;
    }

    // The guard requires a published durable active panel; without it the
    // refusal is NO_ACTIVE_PANEL and the corpus is never reached.
    //
    // Published AFTER the rows and after an explicit checkpoint, on purpose:
    // `publish_active_panel` reads the manifest `CURRENT`, and a freshly-opened
    // vault has not written one. The daemon does not hit this because its open
    // path checkpoints before `ensure_active_panel_published`.
    vault.flush()?;
    vault.checkpoint()?;
    let contract = syn_active_panel_contract(SYN_TIMELINE_PANEL_VERSION, CREATED_AT_MS)?
        .ok_or("no built-in contract for the timeline panel generation")?;
    vault.publish_active_panel(&contract.panel, &contract.registry)?;

    let mut params = SynapseCalyxGuardCalibrateParams::new(
        SYN_TIMELINE_PANEL_VERSION,
        vec![SynapseCalyxGuardSlotSpec {
            slot: GUARD_SLOT,
            aspect: SynapseCalyxGuardAspect::Content,
        }],
    );
    params.domain = format!("fsv1919-{name}");

    match vault.guard_calibrate(&params) {
        Ok(report) => Ok(Attempt {
            good: report.adjudicated_good,
            bad: report.adjudicated_bad,
            unadjudicated: report.unadjudicated,
            conflicting: report.conflicting,
            scanned: report.records_scanned,
            outcome: Ok(Calibrated {
                persisted: report.persisted,
                readback_calibrated: report.readback_calibrated,
                guard_cf_profile_bytes: report.guard_cf_profile_bytes,
                guard_cf_rows_after: report.guard_cf_rows_after,
                tau: report.slots.first().map_or(f32::NAN, |slot| slot.tau),
            }),
        }),
        // The corpus counts live in the refusal message when it refuses, which
        // is the point of that message carrying them.
        Err(error) => Ok(Attempt {
            good: 0,
            bad: 0,
            unadjudicated: 0,
            conflicting: 0,
            scanned: 0,
            outcome: Err(format!("{error}")),
        }),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: ward_declared_enum_adjudication_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&root)?;

    println!(
        "ward_declared_enum_adjudication_fsv: root={}",
        root.display()
    );
    println!(
        "CONSTRUCTED CORPUS: {TOTAL_ROWS} rows = {GOOD_ROWS} good + {BAD_ROWS} bad, \
         guard slot {GUARD_SLOT} (syn.timeline.hour_cyclic.v1)"
    );
    println!("  known before the vault is opened; every count below is compared against it");
    println!();

    let mut ok = true;

    // --- CLAIM 0: the unclamped-cosine defect this run uncovered -----------
    ok &= claim_0_cosine_range();

    // --- CLAIM 1: the fail-closed default is unchanged ---------------------
    println!("CLAIM 1 — an enum kind NOT in the declaration table stays unadjudicated");
    let undeclared = attempt(&root, "undeclared", AnchorPlan::UndeclaredEnum)?;
    match &undeclared.outcome {
        Err(message) => {
            let refused = message.contains("GUARD_BAD_CORPUS_ABSENT");
            let counted = message.contains(&format!("{TOTAL_ROWS} unadjudicated"));
            ok &= refused && counted;
            println!(
                "  refused={refused} reports_all_{TOTAL_ROWS}_unadjudicated={counted} {}",
                if refused && counted { "OK" } else { "FAIL" }
            );
            println!("  {}", first_line(message));
        }
        Ok(_) => {
            ok = false;
            println!(
                "  FAIL — calibrated against an undeclared enum kind. This is the fabrication the guard exists to refuse."
            );
        }
    }

    // --- CLAIM 2: the declared kind adjudicates, with the constructed split -
    println!();
    println!("CLAIM 2 — the declared kind adjudicates into the split we constructed");
    let declared = attempt(&root, "declared", AnchorPlan::DeclaredEnum)?;
    match &declared.outcome {
        Ok(calibrated) => {
            let split_exact = declared.good == GOOD_ROWS as usize
                && declared.bad == BAD_ROWS as usize
                && declared.unadjudicated == 0
                && declared.conflicting == 0
                && declared.scanned == TOTAL_ROWS as usize;
            ok &= split_exact;
            println!(
                "  scanned={} good={} bad={} unadjudicated={} conflicting={}",
                declared.scanned,
                declared.good,
                declared.bad,
                declared.unadjudicated,
                declared.conflicting
            );
            println!(
                "  expected  scanned={TOTAL_ROWS} good={GOOD_ROWS} bad={BAD_ROWS} unadjudicated=0 conflicting=0   {}",
                if split_exact { "OK" } else { "FAIL" }
            );

            // --- CLAIM 3: the source of truth, not the return value --------
            println!();
            println!("CLAIM 3 — the profile is physically in the Guard CF, and decodes");
            let persisted = calibrated.persisted
                && calibrated.readback_calibrated
                && calibrated.guard_cf_profile_bytes > 0
                && calibrated.guard_cf_rows_after >= 1;
            ok &= persisted;
            println!(
                "  persisted={} readback_calibrated={} guard_cf_profile_bytes={} guard_cf_rows_after={} tau={:.6} {}",
                calibrated.persisted,
                calibrated.readback_calibrated,
                calibrated.guard_cf_profile_bytes,
                calibrated.guard_cf_rows_after,
                calibrated.tau,
                if persisted { "OK" } else { "FAIL" }
            );
            let tau_real = calibrated.tau.is_finite() && calibrated.tau > -1.0;
            ok &= tau_real;
            println!(
                "  tau is a real certified threshold (finite, > -1)   {}",
                if tau_real { "OK" } else { "FAIL" }
            );
        }
        Err(message) => {
            ok = false;
            println!(
                "  FAIL — refused the declared corpus: {}",
                first_line(message)
            );
        }
    }

    // --- CLAIM 4: the honesty gate is intact -------------------------------
    println!();
    println!("CLAIM 4 — a declared kind with no bad value still refuses (no manufactured badness)");
    let all_good = attempt(&root, "all-good", AnchorPlan::DeclaredEnumAllGood)?;
    match &all_good.outcome {
        Err(message) => {
            let refused = message.contains("GUARD_BAD_CORPUS_ABSENT");
            let all_counted_good = message.contains(&format!("{TOTAL_ROWS} adjudicated good"));
            ok &= refused && all_counted_good;
            println!(
                "  refused={refused} counted_all_{TOTAL_ROWS}_as_good={all_counted_good} {}",
                if refused && all_counted_good {
                    "OK"
                } else {
                    "FAIL"
                }
            );
            println!("  {}", first_line(message));
        }
        Ok(_) => {
            ok = false;
            println!("  FAIL — calibrated with zero adjudicated bad cases.");
        }
    }

    // --- CLAIM 5: the Bool path is untouched -------------------------------
    println!();
    println!("CLAIM 5 — the pre-existing Bool path is unchanged");
    let bools = attempt(&root, "bool", AnchorPlan::Bool)?;
    match &bools.outcome {
        Ok(calibrated) => {
            let same = bools.good == GOOD_ROWS as usize
                && bools.bad == BAD_ROWS as usize
                && bools.unadjudicated == 0;
            ok &= same && calibrated.readback_calibrated;
            println!(
                "  good={} bad={} unadjudicated={} readback_calibrated={} {}",
                bools.good,
                bools.bad,
                bools.unadjudicated,
                calibrated.readback_calibrated,
                if same && calibrated.readback_calibrated {
                    "OK"
                } else {
                    "FAIL"
                }
            );
        }
        Err(message) => {
            ok = false;
            println!("  FAIL — the Bool path regressed: {}", first_line(message));
        }
    }

    // --- CLAIM 6: contradiction is excluded, not resolved ------------------
    println!();
    println!("CLAIM 6 — an enum and a Bool that disagree on one record are counted conflicting");
    let conflicting = attempt(&root, "conflicting", AnchorPlan::ConflictingEnumAndBool)?;
    match &conflicting.outcome {
        Err(message) => {
            // Every record carries Enum(ok)=good AND Bool(false)=bad, so every
            // record is conflicting, so the bad corpus is empty and the guard
            // refuses. Excluding a contradiction is the correct handling; the
            // alternative is picking a winner, which is inference.
            let refused = message.contains("GUARD_BAD_CORPUS_ABSENT");
            let zero_bad = message.contains("0 adjudicated bad");
            ok &= refused && zero_bad;
            println!(
                "  refused={refused} zero_adjudicated_bad={zero_bad} {}",
                if refused && zero_bad { "OK" } else { "FAIL" }
            );
            println!("  {}", first_line(message));
        }
        Ok(_) => {
            ok = false;
            println!("  FAIL — resolved a contradiction instead of excluding it.");
        }
    }

    println!();
    if ok {
        println!("ALL CLAIMS OK");
        Ok(())
    } else {
        Err("at least one claim FAILED — see above".into())
    }
}

fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).to_owned()
}

/// The raw quotient `dense_cosine` used to return, reproduced so the defect is
/// demonstrated rather than described.
fn unclamped_cosine(left: &[f32], right: &[f32]) -> f32 {
    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (a, b) in left.iter().zip(right) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

/// Ward rejects a whole calibration if any score falls outside `[-1, 1]`, and a
/// leave-one-out score compares a record against its nearest neighbour — so two
/// records sharing a guarded slot vector put the comparison exactly on that
/// boundary. This sweeps hour-cyclic vectors, which is the guarded lens here and
/// has only 24 distinct values, so duplicates are guaranteed on any real corpus.
fn claim_0_cosine_range() -> bool {
    println!("CLAIM 0 — a cosine never escapes [-1,1], and duplicates are where it tried to");
    let mut worst_excess = 0.0_f32;
    let mut escapes = 0_usize;
    let mut clamped_ok = true;
    for hour in 0..24_u32 {
        let angle = 2.0 * std::f32::consts::PI * hour as f32 / 24.0;
        let vector = [angle.sin(), angle.cos()];
        // Self-against-identical: exactly what leave-one-out does when two rows
        // share an hour.
        let raw = unclamped_cosine(&vector, &vector);
        if !(-1.0..=1.0).contains(&raw) {
            escapes += 1;
            worst_excess = worst_excess.max(raw.abs() - 1.0);
        }
        match calyx_core::dense_cosine(&vector, &vector) {
            Some(value) => clamped_ok &= (-1.0..=1.0).contains(&value),
            None => clamped_ok = false,
        }
    }
    println!(
        "  raw quotient escaped [-1,1] on {escapes}/24 identical-vector pairs, worst excess {worst_excess:.3e}"
    );
    println!(
        "  dense_cosine now inside [-1,1] on all 24   {}",
        if clamped_ok { "OK" } else { "FAIL" }
    );
    // A value far outside the range is a defect, not rounding, and must still
    // fail closed rather than clamp.
    let absurd = calyx_core::dense_cosine(&[1.0, 0.0], &[1.0, 0.0]).is_some()
        && calyx_core::dense_cosine(&[f32::MAX, f32::MAX], &[f32::MAX, -f32::MAX]).is_none();
    println!(
        "  a non-rounding out-of-range result still returns None (fails closed)   {}",
        if absurd { "OK" } else { "FAIL" }
    );
    clamped_ok && absurd
}
