//! Full-state verification for #2114: the weave's post-write `XTerm`/`Graph`
//! readback must stay a *physical* count while it stops re-walking a column
//! family the commit sequence proves unchanged.
//!
//! ## What was wrong
//!
//! `weave_panel_intelligence` ended with two unconditional
//! `count_cf_latest_bounded` calls — complete paged walks that do nothing but
//! count. On the deployed vault that is 114,017 `XTerm` rows plus 2,640,002
//! `Graph` rows, walked after **every** weave call, three panels per
//! maintenance tick, including the passes that wove nothing: 63.2 M rows/h,
//! 639 s of wall clock across four hours to produce 483 woven records, and the
//! single largest source of `Graph` row-guard holds.
//!
//! ## The fix, and why it is not a cache
//!
//! `count_cf_latest_bounded_memoized` reuses the previous walk **only** when
//! `latest_seq()` still equals the sequence that served every page of it. A row
//! can only enter or leave a family through a committed MVCC transaction, and
//! every commit allocates a strictly greater sequence — so an unmoved sequence
//! is a proof that the row set is the identical row set, not a guess that it
//! probably is. A walk that straddled a commit (`atomic() == false`) is never
//! memoized, and every `CF_COUNT_MEMO_DRIFT_CHECK_REUSES` reuses the count is
//! re-measured against the disk anyway.
//!
//! ## What this run proves
//!
//! 1. **Equality** — every memoized answer equals the count a full walk
//!    produces at the same sequence, on every call, for both families.
//! 2. **The walk really is skipped** — the `scan_cf_range_page_latest` guard
//!    census counts holds, so a skipped walk is visible as *zero new holds*
//!    rather than merely a faster call.
//! 3. **A write invalidates it** — one committed row moves the sequence, the
//!    next memoized call walks, and the number it reports is the new one.
//! 4. **The drift check fires and agrees** — after the reuse budget the memo is
//!    confronted with the disk and reports `walked_drift_check` with the same
//!    count, with no `SYNAPSE_CALYX_CF_COUNT_MEMO_DRIFT` record emitted.
//!
//! ```text
//! cargo run --release -p synapse-calyx --example cf_count_memo_fsv -- <scratch-dir> [rows]
//! ```

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use synapse_calyx::{
    SynapseCalyxCfWrite, SynapseCalyxConfig, SynapseCalyxMathBackend, SynapseCalyxTuningConfig,
    SynapseCalyxVault,
};

/// Families the weave reads back. Both are exercised because the memo is
/// per-family and a per-family bug would hide behind a single-family run.
const CFS: &[ColumnFamily] = &[ColumnFamily::XTerm, ColumnFamily::Graph];

/// Rows seeded into each family. Large enough that a full walk is many pages
/// and its cost is unmistakable next to a skipped one.
const DEFAULT_ROWS: usize = 60_000;

/// Rows per committed batch while seeding.
const SEED_BATCH: usize = 5_000;

/// Repeats per arm. Chosen to exceed `CF_COUNT_MEMO_DRIFT_CHECK_REUSES` (32) so
/// the cadence drift check is *exercised*, not merely described.
const REPEATS: usize = 40;

/// `(holds, total_held_us)` for the paged-walk guard site at this instant.
fn walk_site_counters(vault: &SynapseCalyxVault) -> Result<(u64, u64), Box<dyn Error>> {
    vault
        .row_guard_census()
        .into_iter()
        .find(|entry| entry.site == "scan_cf_range_page_latest")
        .map_or(Ok((0, 0)), |entry| Ok((entry.holds, entry.total_held_us)))
}

fn seed(vault: &SynapseCalyxVault, cf: ColumnFamily, rows: usize) -> Result<(), Box<dyn Error>> {
    let mut batch: Vec<SynapseCalyxCfWrite> = Vec::with_capacity(SEED_BATCH);
    for index in 0..rows {
        batch.push(SynapseCalyxCfWrite {
            cf,
            key: fsv_key(cf, index),
            value: format!("{{\"fsv\":{index}}}").into_bytes(),
        });
        if batch.len() == SEED_BATCH {
            vault.write_cf_batch(std::mem::take(&mut batch))?;
        }
    }
    if !batch.is_empty() {
        vault.write_cf_batch(batch)?;
    }
    vault.flush()?;
    Ok(())
}

fn fsv_key(cf: ColumnFamily, index: usize) -> Vec<u8> {
    let mut key = b"fsv2114/".to_vec();
    key.extend_from_slice(cf.name().as_bytes());
    key.push(b'/');
    key.extend_from_slice(&(index as u64).to_be_bytes());
    key
}

struct Observation {
    rows: usize,
    provenance: &'static str,
    holds: u64,
    held_us: u64,
}

fn observe_memoized(
    vault: &SynapseCalyxVault,
    cf: ColumnFamily,
) -> Result<Observation, Box<dyn Error>> {
    let (holds_before, held_before) = walk_site_counters(vault)?;
    let readback = vault.count_cf_latest_bounded_memoized(cf)?;
    let (holds_after, held_after) = walk_site_counters(vault)?;
    Ok(Observation {
        rows: readback.rows(),
        provenance: readback.provenance(),
        holds: holds_after - holds_before,
        held_us: held_after - held_before,
    })
}

fn observe_walked(
    vault: &SynapseCalyxVault,
    cf: ColumnFamily,
) -> Result<Observation, Box<dyn Error>> {
    let (holds_before, held_before) = walk_site_counters(vault)?;
    let walk = vault.count_cf_latest_bounded(cf)?;
    let (holds_after, held_after) = walk_site_counters(vault)?;
    Ok(Observation {
        rows: walk.rows_visited,
        provenance: "walked",
        holds: holds_after - holds_before,
        held_us: held_after - held_before,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "one verification run: seed, both arms, the write-invalidation case and the drift \
              check are a single ordered sequence whose steps only make sense together"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: cf_count_memo_fsv <scratch-dir> [rows]")?;
    let rows: usize = match std::env::args().nth(2) {
        Some(value) => value.parse()?,
        None => DEFAULT_ROWS,
    };
    std::fs::create_dir_all(&root)?;
    let vault_dir = root.join("vault");
    // This run measures row-guard holds over paged CF walks. No math backend is
    // reached at all, so CPU math is selected explicitly rather than letting an
    // uncompiled-CUDA host refuse to open a vault this verification never asks
    // to compute with.
    let tuning = SynapseCalyxTuningConfig {
        math_backend: SynapseCalyxMathBackend::Cpu,
        ..SynapseCalyxTuningConfig::default()
    };
    let config = SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: tuning.validate()?,
    };
    let vault = SynapseCalyxVault::open(config)?;

    println!("cf_count_memo_fsv  (#2114)");
    println!("vault      = {}", vault_dir.display());
    println!("rows/CF    = {rows}");
    println!("repeats    = {REPEATS}  (drift-check cadence is 32 reuses)");

    for cf in CFS {
        seed(&vault, *cf, rows)?;
    }
    println!("seeded; latest_seq = {}", vault.latest_seq());
    println!();

    let mut failures: Vec<String> = Vec::new();

    for cf in CFS {
        let cf = *cf;
        // ---- BEFORE arm: what the weave did on every pass, REPEATS times.
        let mut before_holds = 0_u64;
        let mut before_us = 0_u64;
        let mut before_rows = None;
        let before_started = std::time::Instant::now();
        for _ in 0..REPEATS {
            let observation = observe_walked(&vault, cf)?;
            before_holds += observation.holds;
            before_us += observation.held_us;
            match before_rows {
                None => before_rows = Some(observation.rows),
                Some(previous) if previous == observation.rows => {}
                Some(previous) => failures.push(format!(
                    "{}: full walks disagree with each other on a quiescent vault: {previous} then {}",
                    cf.name(),
                    observation.rows
                )),
            }
        }
        let before_wall_ms = before_started.elapsed().as_millis();

        // ---- AFTER arm: the memoized readback, same count, same vault.
        let mut after_holds = 0_u64;
        let mut after_us = 0_u64;
        let mut walked = 0_usize;
        let mut reused = 0_usize;
        let mut drift_checked = 0_usize;
        let after_started = std::time::Instant::now();
        for round in 0..REPEATS {
            let observation = observe_memoized(&vault, cf)?;
            after_holds += observation.holds;
            after_us += observation.held_us;
            match observation.provenance {
                "walked" => walked += 1,
                "walked_drift_check" => drift_checked += 1,
                _ => {
                    reused += 1;
                    if observation.holds != 0 {
                        failures.push(format!(
                            "{}: round {round} reported a reused count but took {} guard hold(s); a reuse must not touch the family",
                            cf.name(),
                            observation.holds
                        ));
                    }
                }
            }
            if Some(observation.rows) != before_rows {
                failures.push(format!(
                    "{}: round {round} ({}) reported {} rows, the full walk reports {:?}",
                    cf.name(),
                    observation.provenance,
                    observation.rows,
                    before_rows
                ));
            }
        }
        let after_wall_ms = after_started.elapsed().as_millis();

        if drift_checked == 0 {
            failures.push(format!(
                "{}: {REPEATS} rounds produced no cadence drift check; the memo was never confronted with the disk",
                cf.name()
            ));
        }

        println!("--- {} ---", cf.name());
        println!("rows                     = {before_rows:?}");
        println!(
            "BEFORE  {REPEATS} full walks : guard_holds={before_holds:6}  held_us={before_us:8}  wall_ms={before_wall_ms}"
        );
        println!(
            "AFTER   {REPEATS} memoized   : guard_holds={after_holds:6}  held_us={after_us:8}  wall_ms={after_wall_ms}  (walked={walked} drift_check={drift_checked} reused={reused})"
        );
        let saved = 100.0 - (after_holds as f64 / before_holds.max(1) as f64) * 100.0;
        println!("guard holds removed      = {saved:.1}%");
        println!();
    }

    // ---- A committed write must invalidate every memo, on every family.
    println!("--- write invalidation ---");
    vault.write_cf_batch(vec![SynapseCalyxCfWrite {
        cf: ColumnFamily::Graph,
        key: fsv_key(ColumnFamily::Graph, rows),
        value: b"{\"fsv\":\"invalidator\"}".to_vec(),
    }])?;
    vault.flush()?;
    println!("wrote 1 Graph row; latest_seq = {}", vault.latest_seq());
    for cf in CFS {
        let cf = *cf;
        let memoized = observe_memoized(&vault, cf)?;
        let walked = observe_walked(&vault, cf)?;
        println!(
            "{:8}: memoized={} ({} holds, {}) full_walk={} ({} holds)",
            cf.name(),
            memoized.rows,
            memoized.holds,
            memoized.provenance,
            walked.rows,
            walked.holds
        );
        if memoized.provenance == "unchanged_since_last_walk" {
            failures.push(format!(
                "{}: a committed write did not invalidate the memo",
                cf.name()
            ));
        }
        if memoized.rows != walked.rows {
            failures.push(format!(
                "{}: after the write the memoized count is {} but the full walk finds {}",
                cf.name(),
                memoized.rows,
                walked.rows
            ));
        }
    }
    let expected_graph = rows + 1;
    let graph_now = vault
        .count_cf_latest_bounded(ColumnFamily::Graph)?
        .rows_visited;
    if graph_now != expected_graph {
        failures.push(format!(
            "Graph should hold {expected_graph} rows after the invalidating write, physical walk finds {graph_now}"
        ));
    }

    println!();
    if failures.is_empty() {
        println!(
            "VERDICT: PASS — every memoized count equals a physical walk at the same sequence, reuses take zero guard holds, a commit invalidates, and the cadence drift check agreed."
        );
        Ok(())
    } else {
        for failure in &failures {
            println!("FAIL: {failure}");
        }
        Err(format!("cf_count_memo_fsv found {} failure(s)", failures.len()).into())
    }
}
