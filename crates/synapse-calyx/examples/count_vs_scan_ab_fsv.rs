//! Full-state verification for #1960: measure `count_cf_latest` against
//! `scan_cf_latest(cf)?.len()` as an **in-process A/B** over the real corpus.
//!
//! ## Why this shape and not the production comparison
//!
//! #1952 ask 3 asked for a before/after on the live daemon. It was attempted
//! twice and failed twice: the first window was contaminated by release builds
//! in the direction that flatters the change, and the second was clean but had
//! no recoverable pre-deploy baseline. The census then settled the remaining
//! ambiguity unflatteringly — `count_cf_latest` shows `holds=0` over a 6,266 s
//! generation carrying 814,041 guard holds, so the converted sites have never
//! executed in production and there is nothing there to measure.
//!
//! Both arms here run **in one process, over the same CF, at the same
//! sequence**, under whatever load the machine happens to be under — because
//! that load is then identical for both. Build contamination inflates both arms
//! equally, so the *ratio* survives a dirty machine, which the production
//! comparison never did.
//!
//! ## What is being claimed, and what is not
//!
//! The honest description of the win is **"two fewer copies of every value"**,
//! not "fewer bytes read". Tombstone status is only knowable from the value, so
//! `router.iter_cf` still produces every value on both paths; `count_cf_latest`
//! drops them instead of cloning them into a `Vec`. A keys-only router path
//! would read less and would be **wrong** — `range_keys_until` cannot see a
//! flushed tombstone (#1954).
//!
//! ## The instrument
//!
//! `row_guard_census()` counts every hold, not only over-budget ones, so a
//! single call can be bracketed and its exact hold read back. `held_us` starts
//! on **acquisition**, so a run that waited behind a writer is not charged for
//! the wait — which is what makes the two arms comparable at all.
//!
//! Both arms are run `REPEATS` times and the **minimum** hold is reported
//! alongside the mean: a minimum is the closest thing to an uncontended
//! measurement this host can produce, and on a machine that is never quiet the
//! mean carries the scheduler as well as the work.
//!
//! ## Correctness first
//!
//! `n_scan == n_count` on the real corpus is #1952 ask 2 re-verified against
//! production data rather than the synthetic vault `count_cf_latest_fsv` uses.
//! A disagreement fails the run regardless of any timing result: a cheaper
//! number that is silently different would turn evidence into decoration.
//!
//! Run against a **frozen copy** of the vault:
//!
//! ```text
//! cargo run --release -p synapse-calyx --example count_vs_scan_ab_fsv -- <vault-copy-dir>
//! ```

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxTuningConfig, SynapseCalyxVault};

/// The CFs to compare. `Assay` is deliberately included even though it is small
/// — the floor case matters as much as the large one, because a per-call
/// constant that dominates at small n would make the ratio at `Base` unusable
/// as a general claim.
const CFS: &[ColumnFamily] = &[
    ColumnFamily::Base,
    ColumnFamily::XTerm,
    ColumnFamily::Graph,
    ColumnFamily::Assay,
];

/// Repeats per arm. Enough that a single descheduled run cannot decide the
/// answer; small enough that the whole sweep stays under a minute.
const REPEATS: usize = 5;

#[derive(Clone, Copy)]
struct Arm {
    min_us: u64,
    mean_us: f64,
    total_us: u64,
    holds: u64,
}

/// One site's `(holds, total_held_us)` at this instant.
fn site_counters(vault: &SynapseCalyxVault, site: &str) -> Result<(u64, u64), Box<dyn Error>> {
    vault
        .row_guard_census()
        .into_iter()
        .find(|entry| entry.site == site)
        .map(|entry| (entry.holds, entry.total_held_us))
        .ok_or_else(|| format!("the census has no site named '{site}'").into())
}

/// Accumulates one arm's holds across the interleaved rounds.
#[derive(Default)]
struct ArmAccumulator {
    min_us: u64,
    total_us: u64,
    holds: u64,
    rows: usize,
}

impl ArmAccumulator {
    fn new() -> Self {
        Self {
            min_us: u64::MAX,
            ..Self::default()
        }
    }

    fn finish(&self) -> Arm {
        Arm {
            min_us: self.min_us,
            mean_us: self.total_us as f64 / self.holds.max(1) as f64,
            total_us: self.total_us,
            holds: self.holds,
        }
    }
}

/// Runs one call, bracketing it with the census so the hold is read rather than
/// timed from outside — an outside timer would include the work the call does
/// after the guard is released, which is exactly the part that differs between
/// the two arms and is not what stalls a committer.
fn one_round<F>(
    vault: &SynapseCalyxVault,
    site: &str,
    arm: &mut ArmAccumulator,
    mut call: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(&SynapseCalyxVault) -> Result<usize, Box<dyn Error>>,
{
    let (holds_before, held_before) = site_counters(vault, site)?;
    arm.rows = call(vault)?;
    let (holds_after, held_after) = site_counters(vault, site)?;
    let delta_holds = holds_after - holds_before;
    if delta_holds != 1 {
        return Err(format!(
            "one call to '{site}' produced {delta_holds} guard holds; the arms are not \
             comparable if a call is not exactly one hold"
        )
        .into());
    }
    let held = held_after - held_before;
    arm.min_us = arm.min_us.min(held);
    arm.total_us += held;
    arm.holds += delta_holds;
    Ok(())
}

/// Runs both arms **interleaved**, `REPEATS` rounds of `scan` then `count`.
///
/// The first version ran all of one arm and then all of the other, which hands
/// the second arm a warm page cache and any drift in machine load over the
/// sweep. Interleaving does not make the machine quiet; it makes both arms
/// experience the *same* machine, which is the only property this comparison
/// needs and the one the production before/after could never have.
fn measure_both(
    vault: &SynapseCalyxVault,
    cf: ColumnFamily,
) -> Result<(Arm, usize, Arm, usize), Box<dyn Error>> {
    let mut scan = ArmAccumulator::new();
    let mut count = ArmAccumulator::new();
    for _ in 0..REPEATS {
        one_round(vault, "scan_cf_latest", &mut scan, |vault| {
            Ok(vault.scan_cf_latest(cf)?.len())
        })?;
        one_round(vault, "count_cf_latest", &mut count, |vault| {
            Ok(vault.count_cf_latest(cf)?)
        })?;
    }
    Ok((scan.finish(), scan.rows, count.finish(), count.rows))
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: count_vs_scan_ab_fsv <vault-copy-dir>")?;
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

    println!("count_vs_scan_ab_fsv  (#1960)");
    println!("frozen vault copy = {}", vault_dir.display());
    println!("latest_seq        = {}", vault.latest_seq());
    println!("repeats per arm   = {REPEATS}");

    // The starting census is the evidence that the converted site really has
    // never run on this vault, which is the premise this issue was split over.
    let (count_holds_at_open, _) = site_counters(&vault, "count_cf_latest")?;
    let (scan_holds_at_open, _) = site_counters(&vault, "scan_cf_latest")?;
    println!(
        "census at open    : count_cf_latest holds={count_holds_at_open}  \
         scan_cf_latest holds={scan_holds_at_open}"
    );

    let mut disagreements = 0usize;
    println!(
        "\n{:<10} {:>9} {:>9} {:>11} {:>11} {:>11} {:>11} {:>8}",
        "cf", "n_scan", "n_count", "scan_min_us", "cnt_min_us", "scan_mean", "cnt_mean", "ratio"
    );
    for cf in CFS {
        let (scan_arm, n_scan, count_arm, n_count) = measure_both(&vault, *cf)?;

        // #1952 ask 2, on the real corpus rather than a synthetic vault.
        let agree = n_scan == n_count;
        if !agree {
            disagreements += 1;
        }
        let ratio = if count_arm.min_us == 0 {
            f64::INFINITY
        } else {
            scan_arm.min_us as f64 / count_arm.min_us as f64
        };
        println!(
            "{:<10} {:>9} {:>9} {:>11} {:>11} {:>11.1} {:>11.1} {:>8.2}{}",
            format!("{cf:?}"),
            n_scan,
            n_count,
            scan_arm.min_us,
            count_arm.min_us,
            scan_arm.mean_us,
            count_arm.mean_us,
            ratio,
            if agree { "" } else { "   <-- DISAGREE" }
        );
        if scan_arm.holds != REPEATS as u64 || count_arm.holds != REPEATS as u64 {
            return Err(format!(
                "{cf:?}: expected {REPEATS} holds per arm, saw scan={} count={}",
                scan_arm.holds, count_arm.holds
            )
            .into());
        }
        let _ = (scan_arm.total_us, count_arm.total_us);
    }

    // --- the floor case: an empty CF -----------------------------------------
    //
    // A per-call constant that dominates at n=0 would make the ratio at Base a
    // statement about the constant rather than about the copies, so the floor
    // is measured rather than assumed. `AnnealSoak` is chosen because the
    // anneal soak loop is not enabled on this vault; if it turns out non-empty
    // the harness says so rather than silently reporting a non-floor number.
    println!("\n--- floor case: a CF with no rows ---");
    let empty_cf = ColumnFamily::AnnealSoak;
    let (scan_arm, n_scan, count_arm, n_count) = measure_both(&vault, empty_cf)?;
    println!(
        "{empty_cf:?}: n_scan={n_scan} n_count={n_count} scan_min_us={} count_min_us={}",
        scan_arm.min_us, count_arm.min_us
    );
    if n_scan != n_count {
        disagreements += 1;
    }
    if n_scan != 0 {
        println!(
            "   NOTE: {empty_cf:?} holds {n_scan} rows on this vault, so this is not a \
             zero-row floor; read the line above as another sized case."
        );
    }

    // --- the census the whole issue turns on ---------------------------------
    println!("\n--- census after the sweep ---");
    for entry in vault.row_guard_census() {
        if entry.site != "scan_cf_latest" && entry.site != "count_cf_latest" {
            continue;
        }
        println!(
            "   {:<18} holds={:<5} total_held_us={:<10} max_held_us={:<9} mean_us={:?}",
            entry.site,
            entry.holds,
            entry.total_held_us,
            entry.max_held_us,
            entry.mean_held_us.map(|mean| (mean * 10.0).round() / 10.0)
        );
    }

    if disagreements > 0 {
        return Err(format!(
            "{disagreements} CF(s) where count_cf_latest disagreed with scan_cf_latest().len(); \
             the conversion's correctness contract (#1952 ask 2) is broken and no timing \
             result matters"
        )
        .into());
    }

    println!(
        "\nPASS: count_cf_latest == scan_cf_latest().len() on every CF measured, and both \
         arms' guard holds were read from the census rather than timed from outside."
    );
    println!(
        "Read the ratio, not the delta. If it is close to 1.0 the honest conclusion is \
         'the allocation saving is not measurable at this corpus size', which leaves the \
         correctness contract standing and is a legitimate outcome (#1960 ask 3)."
    );
    Ok(())
}
