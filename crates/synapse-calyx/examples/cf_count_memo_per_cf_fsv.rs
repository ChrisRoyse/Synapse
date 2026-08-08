//! Full-state verification for #2139: the CF-count memo must be invalidated by
//! commits **to its own column family**, not by any commit anywhere in the
//! vault.
//!
//! ## What was wrong
//!
//! #2114 shipped `count_cf_latest_bounded_memoized`, whose reuse condition was
//! `latest_seq() == walk.snapshot_seq_last` — the *whole vault* quiescent. The
//! proof is sound and the condition is nearly unreachable: on the deployed
//! daemon a transcript-ingest commit lands every few seconds whenever any agent
//! session is live, and each one moves `latest_seq()`. So every weave readback
//! saw a moved sequence, re-walked `XTerm` + `Graph` in full, and the 2.75 M-row
//! walks the memo existed to remove continued unchanged.
//!
//! The reason it could not be sharper is #2139: nothing in Calyx could answer
//! "did *this family* change since sequence S" more cheaply than walking the
//! family. `changed_keys_after_at` looks like that signal and is `O(family)`.
//!
//! ## The fix
//!
//! Every commit already knows exactly which families it writes and already holds
//! their row-table write guards. It now publishes a per-family
//! `last_commit_seq` there, before the rows become visible, plus an
//! `out_of_band_epoch` for the physical changes that allocate no sequence
//! (CF retire, compaction/retention-GC level swaps). `cf_change_signal(cf)` is
//! two atomic loads.
//!
//! ## What this run proves, physically
//!
//! Guard holds at the `scan_cf_range_page_latest` census site are the evidence:
//! a skipped walk shows as **zero new holds**, not merely a faster call.
//!
//! 1. **Isolation** — a commit to `XTerm` moves `XTerm`'s signal and not
//!    `Graph`'s; `Graph`'s memo survives with `unchanged_since_last_walk` and
//!    zero holds while `XTerm`'s invalidates and re-walks.
//! 2. **Busy vault** — with an unrelated family committed to between every
//!    readback, the reuse rate before the fix (evaluated as the old predicate
//!    against the same real sequences) is 0%, and after it is ~100%, with every
//!    count byte-identical to a physical walk taken at the same moment.
//! 3. **Same-family commit still invalidates** — the number changes and the
//!    physical walk agrees.
//! 4. **The drift check still fires and agrees** — `walked_drift_check` appears
//!    on cadence with no `SYNAPSE_CALYX_CF_COUNT_MEMO_DRIFT` record.
//! 5. **A change that allocates no sequence still invalidates** — a physical
//!    `Kv` compaction bumps only `Kv`'s out-of-band epoch, `Kv` re-walks, and
//!    `Graph` is undisturbed.
//! 6. **Recovery rebuilds the signals** — after close/reopen the counts match,
//!    the memo starts cold, and the per-family isolation still holds against
//!    sequences that came back from the WAL rather than from a live commit.
//!
//! ```text
//! cargo run --release -p synapse-calyx --example cf_count_memo_per_cf_fsv -- <scratch-dir> [rows]
//! ```

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use synapse_calyx::{
    SynapseCalyxCfWrite, SynapseCalyxConfig, SynapseCalyxMathBackend, SynapseCalyxTuningConfig,
    SynapseCalyxVault,
};

/// The two families the weave reads back.
const WEAVE_CFS: &[ColumnFamily] = &[ColumnFamily::XTerm, ColumnFamily::Graph];

/// The family standing in for "everything else committing on a busy vault".
/// `Kv` is the transcript/rollup-adjacent family in the real daemon and, like
/// every static family, owns its own signal cell.
const NOISE_CF: ColumnFamily = ColumnFamily::Kv;

/// Rows seeded into each weave family. Large enough that a full walk is many
/// pages and the difference between walking and not is unmistakable.
const DEFAULT_ROWS: usize = 60_000;

const SEED_BATCH: usize = 5_000;

/// Rounds in the busy-vault arm. Above the 32-reuse drift cadence so the forced
/// physical re-walk is exercised inside the busy arm rather than beside it.
const BUSY_ROUNDS: usize = 40;

/// `(holds, total_held_us)` for the paged-walk guard site at this instant.
fn walk_site_counters(vault: &SynapseCalyxVault) -> (u64, u64) {
    vault
        .row_guard_census()
        .into_iter()
        .find(|entry| entry.site == "scan_cf_range_page_latest")
        .map_or((0, 0), |entry| (entry.holds, entry.total_held_us))
}

fn fsv_key(cf: ColumnFamily, index: usize) -> Vec<u8> {
    let mut key = b"fsv2139/".to_vec();
    key.extend_from_slice(cf.name().as_bytes());
    key.push(b'/');
    key.extend_from_slice(&(index as u64).to_be_bytes());
    key
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

fn commit_one(
    vault: &SynapseCalyxVault,
    cf: ColumnFamily,
    index: usize,
) -> Result<(), Box<dyn Error>> {
    vault.write_cf_batch(vec![SynapseCalyxCfWrite {
        cf,
        key: fsv_key(cf, index),
        value: format!("{{\"fsv\":{index}}}").into_bytes(),
    }])?;
    Ok(())
}

struct Observation {
    rows: usize,
    provenance: &'static str,
    holds: u64,
    held_us: u64,
    cf_last_commit_seq: u64,
    vault_latest_seq: u64,
    /// What the #2114 whole-vault predicate would have decided at this instant,
    /// evaluated against the same real sequences: reuse iff the vault-wide
    /// latest sequence has not moved since the walk that is being reused.
    old_gate_would_reuse: bool,
}

fn observe_memoized(
    vault: &SynapseCalyxVault,
    cf: ColumnFamily,
) -> Result<Observation, Box<dyn Error>> {
    let (holds_before, held_before) = walk_site_counters(vault);
    let readback = vault.count_cf_latest_bounded_memoized(cf)?;
    let (holds_after, held_after) = walk_site_counters(vault);
    let old_gate_would_reuse = readback
        .unchanged_since_seq
        .is_some_and(|walk_seq| walk_seq == readback.vault_latest_seq);
    Ok(Observation {
        rows: readback.rows(),
        provenance: readback.provenance(),
        holds: holds_after - holds_before,
        held_us: held_after - held_before,
        cf_last_commit_seq: readback.cf_last_commit_seq,
        vault_latest_seq: readback.vault_latest_seq,
        old_gate_would_reuse,
    })
}

/// A physical walk taken right now, with the holds it cost.
fn observe_walked(
    vault: &SynapseCalyxVault,
    cf: ColumnFamily,
) -> Result<(usize, u64, u64), Box<dyn Error>> {
    let (holds_before, held_before) = walk_site_counters(vault);
    let walk = vault.count_cf_latest_bounded(cf)?;
    let (holds_after, held_after) = walk_site_counters(vault);
    Ok((
        walk.rows_visited,
        holds_after - holds_before,
        held_after - held_before,
    ))
}

#[allow(
    clippy::too_many_lines,
    reason = "one verification run: seeding, the isolation case, the busy-vault A/B and three \
              edge cases are a single ordered sequence whose steps only mean anything together"
)]
#[allow(
    clippy::cast_precision_loss,
    clippy::suboptimal_flops,
    reason = "reuse rates and hold-reduction percentages printed for a human; the exact integers \
              they are derived from are printed beside them"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: cf_count_memo_per_cf_fsv <scratch-dir> [rows]")?;
    let rows: usize = match std::env::args().nth(2) {
        Some(value) => value.parse()?,
        None => DEFAULT_ROWS,
    };
    std::fs::create_dir_all(&root)?;
    let vault_dir = root.join("vault");
    // No math backend is reached by any CF walk, so CPU is selected explicitly
    // rather than letting an uncompiled-CUDA host refuse to open a vault this
    // verification never asks to compute with.
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

    println!("cf_count_memo_per_cf_fsv  (#2139)");
    println!("vault      = {}", vault_dir.display());
    println!("rows/CF    = {rows}");
    println!();

    let mut failures: Vec<String> = Vec::new();

    for cf in WEAVE_CFS {
        seed(&vault, *cf, rows)?;
    }
    seed(&vault, NOISE_CF, 32)?;
    println!("seeded; vault latest_seq = {}", vault.latest_seq());
    for cf in WEAVE_CFS.iter().chain(std::iter::once(&NOISE_CF)) {
        let (seq, epoch) = vault.cf_change_signal(*cf);
        println!(
            "  signal {:14} last_commit_seq={seq:<8} out_of_band_epoch={epoch}",
            cf.name()
        );
    }
    println!();

    // Warm both memos with a physical walk each.
    for cf in WEAVE_CFS {
        let warm = observe_memoized(&vault, *cf)?;
        if warm.provenance != "walked" {
            failures.push(format!(
                "{}: the first memoized call must physically walk, reported {}",
                cf.name(),
                warm.provenance
            ));
        }
    }

    // ---------------------------------------------------------------- EDGE 1
    // Cross-family isolation: a commit to XTerm must move XTerm's signal and
    // leave Graph's memo standing.
    println!("--- EDGE 1: a commit to XTerm must not invalidate Graph ---");
    let (xterm_seq_before, _) = vault.cf_change_signal(ColumnFamily::XTerm);
    let (graph_seq_before, graph_epoch_before) = vault.cf_change_signal(ColumnFamily::Graph);
    commit_one(&vault, ColumnFamily::XTerm, rows)?;
    vault.flush()?;
    let (xterm_seq_after, _) = vault.cf_change_signal(ColumnFamily::XTerm);
    let (graph_seq_after, graph_epoch_after) = vault.cf_change_signal(ColumnFamily::Graph);
    println!(
        "BEFORE  xterm.last_commit_seq={xterm_seq_before}  graph.last_commit_seq={graph_seq_before}  vault.latest_seq={}",
        vault.latest_seq()
    );
    println!(
        "AFTER   xterm.last_commit_seq={xterm_seq_after}  graph.last_commit_seq={graph_seq_after}  vault.latest_seq={}",
        vault.latest_seq()
    );
    if xterm_seq_after <= xterm_seq_before {
        failures.push(format!(
            "a committed XTerm row did not advance XTerm's signal ({xterm_seq_before} -> {xterm_seq_after})"
        ));
    }
    if graph_seq_after != graph_seq_before || graph_epoch_after != graph_epoch_before {
        failures.push(format!(
            "a committed XTerm row moved Graph's signal ({graph_seq_before},{graph_epoch_before}) -> ({graph_seq_after},{graph_epoch_after})"
        ));
    }

    let graph_memo = observe_memoized(&vault, ColumnFamily::Graph)?;
    let (graph_physical, graph_walk_holds, _) = observe_walked(&vault, ColumnFamily::Graph)?;
    let xterm_memo = observe_memoized(&vault, ColumnFamily::XTerm)?;
    let (xterm_physical, _, _) = observe_walked(&vault, ColumnFamily::XTerm)?;
    println!(
        "graph  memo: rows={} provenance={} guard_holds={} (a physical walk of the same family costs {graph_walk_holds} holds and finds {graph_physical})",
        graph_memo.rows, graph_memo.provenance, graph_memo.holds
    );
    println!(
        "xterm  memo: rows={} provenance={} guard_holds={} (physical walk finds {xterm_physical})",
        xterm_memo.rows, xterm_memo.provenance, xterm_memo.holds
    );
    println!(
        "old #2114 gate at this instant: graph reuse={} (it compares vault.latest_seq={} against the walk sequence)",
        graph_memo.old_gate_would_reuse, graph_memo.vault_latest_seq
    );
    if graph_memo.provenance != "unchanged_since_last_walk" || graph_memo.holds != 0 {
        failures.push(format!(
            "Graph re-walked after a commit to XTerm: provenance={} holds={}",
            graph_memo.provenance, graph_memo.holds
        ));
    }
    if graph_memo.rows != graph_physical {
        failures.push(format!(
            "Graph's reused count {} disagrees with a physical walk taken at the same moment ({graph_physical})",
            graph_memo.rows
        ));
    }
    if graph_memo.old_gate_would_reuse {
        failures.push(
            "the #2114 whole-vault predicate would also have reused here, so this case does not \
             demonstrate the #2139 difference"
                .to_owned(),
        );
    }
    if xterm_memo.provenance == "unchanged_since_last_walk" {
        failures.push("a commit to XTerm did not invalidate XTerm's own memo".to_owned());
    }
    if xterm_memo.rows != xterm_physical || xterm_memo.rows != rows + 1 {
        failures.push(format!(
            "XTerm should hold {} rows after the commit; memo says {}, physical walk says {xterm_physical}",
            rows + 1,
            xterm_memo.rows
        ));
    }
    println!();

    // ---------------------------------------------------------------- BUSY A/B
    // The production shape: an unrelated family commits between every readback.
    println!("--- BUSY VAULT: {BUSY_ROUNDS} rounds, one Kv commit before each readback ---");

    // BEFORE arm, physically measured: the walk the old code performed on every
    // pass, because its gate could never hold under this traffic.
    let mut before_holds = 0_u64;
    let mut before_us = 0_u64;
    // First count each family produced in this arm. Every later walk must repeat
    // it exactly: the only commits in the loop go to an unrelated family, so a
    // moving number here would mean the "unrelated" traffic was not unrelated
    // and the whole A/B would be measuring something else.
    let mut before_rows: [Option<usize>; 2] = [None; 2];
    let before_started = std::time::Instant::now();
    for round in 0..BUSY_ROUNDS {
        commit_one(&vault, NOISE_CF, 1_000 + round)?;
        for (index, cf) in WEAVE_CFS.iter().enumerate() {
            let (walked_rows, holds, held_us) = observe_walked(&vault, *cf)?;
            before_holds += holds;
            before_us += held_us;
            match before_rows[index] {
                None => before_rows[index] = Some(walked_rows),
                Some(first) if first == walked_rows => {}
                Some(first) => failures.push(format!(
                    "{}: round {round} walked {walked_rows} rows, the first walk of this arm found {first}; the Kv-only traffic changed this family",
                    cf.name()
                )),
            }
        }
    }
    let before_wall_ms = before_started.elapsed().as_millis();

    // AFTER arm: identical traffic, the memoized readback.
    let mut after_holds = 0_u64;
    let mut after_us = 0_u64;
    let mut reused = 0_usize;
    let mut walked = 0_usize;
    let mut drift_checked = 0_usize;
    let mut old_gate_reuses = 0_usize;
    let after_started = std::time::Instant::now();
    for round in 0..BUSY_ROUNDS {
        commit_one(&vault, NOISE_CF, 2_000 + round)?;
        for cf in WEAVE_CFS {
            let cf = *cf;
            let observation = observe_memoized(&vault, cf)?;
            after_holds += observation.holds;
            after_us += observation.held_us;
            if observation.old_gate_would_reuse {
                old_gate_reuses += 1;
            }
            match observation.provenance {
                "walked" => walked += 1,
                "walked_drift_check" => drift_checked += 1,
                _ => {
                    reused += 1;
                    if observation.holds != 0 {
                        failures.push(format!(
                            "{}: round {round} reused a count but took {} guard hold(s)",
                            cf.name(),
                            observation.holds
                        ));
                    }
                }
            }
            // Every single answer is confronted with the disk, whatever its
            // provenance. This is what makes "reuse rate" a claim about cost
            // rather than a claim about correctness.
            let (physical, _, _) = observe_walked(&vault, cf)?;
            if observation.rows != physical {
                failures.push(format!(
                    "{}: round {round} ({}) reported {} rows, a physical walk at the same moment finds {physical}",
                    cf.name(),
                    observation.provenance,
                    observation.rows
                ));
            }
            if observation.cf_last_commit_seq >= observation.vault_latest_seq {
                failures.push(format!(
                    "{}: round {round} shows cf_last_commit_seq={} at vault_latest_seq={}; the Kv \
                     commits should have moved the vault sequence past this family's",
                    cf.name(),
                    observation.cf_last_commit_seq,
                    observation.vault_latest_seq
                ));
            }
        }
    }
    let after_wall_ms = after_started.elapsed().as_millis();
    let calls = BUSY_ROUNDS * WEAVE_CFS.len();

    println!(
        "BEFORE  {calls} readbacks: guard_holds={before_holds:6} held_us={before_us:8} wall_ms={before_wall_ms}  reuses=0/{calls} (0.0%)"
    );
    println!(
        "AFTER   {calls} readbacks: guard_holds={after_holds:6} held_us={after_us:8} wall_ms={after_wall_ms}  reuses={reused}/{calls} ({:.1}%)  walked={walked} drift_check={drift_checked}",
        (reused as f64 / calls as f64) * 100.0
    );
    println!(
        "old #2114 whole-vault predicate would have reused {old_gate_reuses}/{calls} times under this traffic"
    );
    println!(
        "guard holds removed = {:.1}%",
        100.0 - (after_holds as f64 / before_holds.max(1) as f64) * 100.0
    );
    if old_gate_reuses != 0 {
        failures.push(format!(
            "the old whole-vault predicate reused {old_gate_reuses} times; the busy-vault arm is \
             not actually busy and proves nothing"
        ));
    }
    if reused * 4 < calls * 3 {
        failures.push(format!(
            "the per-CF memo reused only {reused}/{calls} times under unrelated traffic"
        ));
    }
    if drift_checked == 0 {
        failures.push(
            "no cadence drift check fired in the busy arm; the memo was never confronted with the \
             disk on cadence"
                .to_owned(),
        );
    }
    println!();

    // ---------------------------------------------------------------- EDGE 2
    println!("--- EDGE 2: a commit to Graph itself must invalidate Graph ---");
    let graph_before = observe_memoized(&vault, ColumnFamily::Graph)?;
    commit_one(&vault, ColumnFamily::Graph, rows)?;
    vault.flush()?;
    let graph_after = observe_memoized(&vault, ColumnFamily::Graph)?;
    let (graph_physical_after, _, _) = observe_walked(&vault, ColumnFamily::Graph)?;
    println!(
        "BEFORE  rows={} provenance={} holds={}",
        graph_before.rows, graph_before.provenance, graph_before.holds
    );
    println!(
        "AFTER   rows={} provenance={} holds={}  physical_walk={graph_physical_after}",
        graph_after.rows, graph_after.provenance, graph_after.holds
    );
    if graph_after.provenance == "unchanged_since_last_walk" {
        failures.push("a commit to Graph did not invalidate Graph's own memo".to_owned());
    }
    if graph_after.rows != graph_physical_after || graph_after.rows != graph_before.rows + 1 {
        failures.push(format!(
            "after one committed Graph row the memo reports {} and the physical walk {graph_physical_after}; expected {}",
            graph_after.rows,
            graph_before.rows + 1
        ));
    }
    println!();

    // ---------------------------------------------------------------- EDGE 3
    // A physical compaction rewrites a family's served SST level and allocates
    // NO sequence. A memo keyed only on sequences would survive it; the
    // out-of-band epoch is what stops that.
    println!("--- EDGE 3: a compaction allocates no sequence and must still invalidate ---");
    // Give the compaction bridge real inputs: several flushed generations of Kv
    // rows plus a checkpoint. A vacuous "nothing to compact" would leave this
    // case proving nothing, so the corpus is built for it rather than hoped for.
    for generation in 0..4 {
        seed(&vault, NOISE_CF, 6_000 + generation)?;
        vault.checkpoint()?;
    }
    let kv_warm = observe_memoized(&vault, NOISE_CF)?;
    let (kv_seq_before, kv_epoch_before) = vault.cf_change_signal(NOISE_CF);
    let (graph_seq_before_compact, graph_epoch_before_compact) =
        vault.cf_change_signal(ColumnFamily::Graph);
    let graph_warm = observe_memoized(&vault, ColumnFamily::Graph)?;
    let compacted = vault.compact_kv_once()?;
    let (kv_seq_after, kv_epoch_after) = vault.cf_change_signal(NOISE_CF);
    let (graph_seq_after_compact, graph_epoch_after_compact) =
        vault.cf_change_signal(ColumnFamily::Graph);
    let kv_after = observe_memoized(&vault, NOISE_CF)?;
    let (kv_physical, _, _) = observe_walked(&vault, NOISE_CF)?;
    let graph_after_compact = observe_memoized(&vault, ColumnFamily::Graph)?;
    println!("compact_kv_once compacted = {compacted}");
    println!(
        "BEFORE  kv.signal=({kv_seq_before},{kv_epoch_before})  kv memo rows={} ({})",
        kv_warm.rows, kv_warm.provenance
    );
    println!(
        "AFTER   kv.signal=({kv_seq_after},{kv_epoch_after})  kv memo rows={} ({}) holds={}  physical_walk={kv_physical}",
        kv_after.rows, kv_after.provenance, kv_after.holds
    );
    println!(
        "        graph.signal ({graph_seq_before_compact},{graph_epoch_before_compact}) -> ({graph_seq_after_compact},{graph_epoch_after_compact}); graph memo {} -> {} (rows {} -> {})",
        graph_warm.provenance,
        graph_after_compact.provenance,
        graph_warm.rows,
        graph_after_compact.rows
    );
    if kv_after.rows != kv_physical {
        failures.push(format!(
            "after compaction the Kv memo reports {} rows against a physical {kv_physical}",
            kv_after.rows
        ));
    }
    if compacted {
        if kv_epoch_after == kv_epoch_before {
            failures.push(
                "a physical Kv compaction did not bump the Kv out-of-band epoch; a sequence-only \
                 memo would have survived it"
                    .to_owned(),
            );
        }
        if kv_after.provenance == "unchanged_since_last_walk" {
            failures.push("the Kv memo survived a physical compaction of Kv".to_owned());
        }
        if graph_epoch_after_compact != graph_epoch_before_compact
            || graph_after_compact.provenance != "unchanged_since_last_walk"
        {
            failures.push(
                "compacting Kv disturbed Graph's signal or memo; the out-of-band epoch is not \
                 per-family"
                    .to_owned(),
            );
        }
    } else {
        println!(
            "note: the compaction bridge found nothing to compact, so the epoch case is vacuous \
             this run; the equality above still holds"
        );
    }
    println!();

    // ---------------------------------------------------------------- EDGE 4
    // Recovery restores rows at their original sequences. Those restores must
    // publish the per-family signal too, or a family whose rows all came from
    // the WAL would report `last_commit_seq = 0` and its memo would then be
    // invalidated by nothing and validated by everything.
    println!("--- EDGE 4: after close/reopen the signals are rebuilt from recovery ---");
    let xterm_final = vault
        .count_cf_latest_bounded(ColumnFamily::XTerm)?
        .rows_visited;
    let graph_final = vault
        .count_cf_latest_bounded(ColumnFamily::Graph)?
        .rows_visited;
    vault.close("fsv2139 reopen")?;
    let reopen_tuning = SynapseCalyxTuningConfig {
        math_backend: SynapseCalyxMathBackend::Cpu,
        ..SynapseCalyxTuningConfig::default()
    };
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig {
        vault_dir,
        machine_salt_path: root.join("machine-salt.bin"),
        tuning: reopen_tuning.validate()?,
    })?;
    let (xterm_seq_reopen, _) = vault.cf_change_signal(ColumnFamily::XTerm);
    let (graph_seq_reopen, _) = vault.cf_change_signal(ColumnFamily::Graph);
    let xterm_first = observe_memoized(&vault, ColumnFamily::XTerm)?;
    let graph_first = observe_memoized(&vault, ColumnFamily::Graph)?;
    commit_one(&vault, ColumnFamily::XTerm, 5_000)?;
    vault.flush()?;
    let graph_after_reopen_write = observe_memoized(&vault, ColumnFamily::Graph)?;
    let (graph_physical_reopen, _, _) = observe_walked(&vault, ColumnFamily::Graph)?;
    println!(
        "reopened; vault.latest_seq={}  xterm.last_commit_seq={xterm_seq_reopen}  graph.last_commit_seq={graph_seq_reopen}",
        vault.latest_seq()
    );
    println!(
        "first readback after reopen: xterm rows={} ({}), graph rows={} ({})",
        xterm_first.rows, xterm_first.provenance, graph_first.rows, graph_first.provenance
    );
    println!(
        "then one XTerm commit: graph memo rows={} ({}) holds={}  physical_walk={graph_physical_reopen}",
        graph_after_reopen_write.rows,
        graph_after_reopen_write.provenance,
        graph_after_reopen_write.holds
    );
    if xterm_first.rows != xterm_final || graph_first.rows != graph_final {
        failures.push(format!(
            "the reopened vault counts xterm={} graph={} against pre-close xterm={xterm_final} graph={graph_final}",
            xterm_first.rows, graph_first.rows
        ));
    }
    if xterm_first.provenance != "walked" || graph_first.provenance != "walked" {
        failures.push(
            "the first readback after a reopen reused a memo; the memo must be process-local and \
             start cold"
                .to_owned(),
        );
    }
    if graph_after_reopen_write.provenance != "unchanged_since_last_walk"
        || graph_after_reopen_write.rows != graph_physical_reopen
    {
        failures.push(format!(
            "after a reopen, an XTerm commit disturbed Graph: provenance={} rows={} physical={graph_physical_reopen}",
            graph_after_reopen_write.provenance, graph_after_reopen_write.rows
        ));
    }
    println!();

    if failures.is_empty() {
        println!(
            "VERDICT: PASS — the per-CF signal isolates families exactly, every memoized count \
             equalled a physical walk taken at the same moment, reuses cost zero guard holds, the \
             old whole-vault predicate reused nothing under the same traffic, and the cadence \
             drift check agreed."
        );
        Ok(())
    } else {
        for failure in &failures {
            println!("FAIL: {failure}");
        }
        Err(format!(
            "cf_count_memo_per_cf_fsv found {} failure(s)",
            failures.len()
        )
        .into())
    }
}
