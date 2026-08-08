//! Manual FSV for #2122: prove the in-RAM MVCC version chains are actually
//! reclaimed, that reclamation cannot break a pinned reader, and that it cannot
//! stall commits.
//!
//! ## What was wrong
//!
//! Every vault commit appends a **full clone of its value bytes** to an in-RAM
//! `VersionChain`. The reclaimer that trims those chains — `snapshot_version_gc`
//! — was fully implemented and had **zero callers anywhere in `crates/`**. The
//! production daemon proved it: the `snapshot_gc_debt` row-guard site reported
//! `holds=0` for the entire lifetime of the process, against 193,269,889 for
//! `read_latest`. Private commit ratcheted 25,599 -> 28,206 MB over 26 minutes
//! with a maximum drawdown of 2.8 MB — not a sawtooth, a staircase.
//!
//! ## What this harness proves, in order
//!
//! 1. **The instrument agrees with production.** Before anything runs, the
//!    `snapshot_version_reclaim` census site reports `holds=0` — the same
//!    "never ran" reading production showed, from the same instrument, so a
//!    later non-zero reading here is comparable evidence rather than a
//!    different measurement.
//! 2. **Reclamation frees versions, and the debt census agrees.** The exact
//!    whole-vault debt is read before and after; the pass's `versions_reclaimed`
//!    must account for the difference.
//! 3. **A pinned reader is never violated.** A reader is pinned at a known
//!    sequence, newer versions are committed on top, and reclamation runs. The
//!    pinned reader must still read exactly the values as of its pin — and the
//!    floor the pass used must equal that pin, not the current sequence.
//! 4. **Releasing the pin lets the floor advance.** After release, the next
//!    pass reclaims what the pin was protecting, and a third pass converges:
//!    `versions_reclaimed = 0` **with `sweep_completed = true`**, which is the
//!    only pair that licenses "there is nothing left".
//! 5. **Read-your-writes survives.** Every key still reads its newest value
//!    after every version below it has been reclaimed.
//! 6. **Bounded passes page and resume.** With a deliberately tiny budget the
//!    sweep takes many passes, and the union of those passes reclaims exactly
//!    what one large pass would have — proving the shard/key cursor neither
//!    skips nor repeats.
//! 7. **Guard holds stay bounded.** The census reports the maximum single
//!    row-table shard **write** guard hold taken by reclamation. That hold is
//!    what a concurrent commit would wait behind, and it must stay under the
//!    25 ms budget every read guard is judged against.
//! 8. **The memory metric is honest.** Windows working set and committed
//!    private bytes are printed side by side across a bulk reclaim, because
//!    #2122's secondary finding is that Calyx reported working set — the number
//!    that *fell* while the process leaked.
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example snapshot_version_gc_fsv -- <empty-scratch-dir>`

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::mvcc::{Freshness, ROW_READ_GUARD_WARN_US, SnapshotVersionGcBudget};
use calyx_aster::resource::{heap_rss_bytes, process_private_bytes};
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{SystemClock, VaultId};

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETP";
const CF: ColumnFamily = ColumnFamily::Kv;

/// Distinct keys, each of which accumulates one version per round.
const KEYS: u64 = 512;
/// Rounds committed before the reader is pinned.
const ROUNDS_BEFORE_PIN: u64 = 6;
/// Rounds committed after the reader is pinned.
const ROUNDS_AFTER_PIN: u64 = 5;
/// Value size, chosen so the bulk phase moves a number of megabytes that a
/// process-level memory probe can actually resolve.
const VALUE_BYTES: usize = 4_096;
/// Reader lease long enough that it cannot expire mid-harness and hand the
/// floor back by accident — that would make step 3 pass for the wrong reason.
const LEASE_MS: u64 = 600_000;

/// Bulk phase: enough bytes that a real drop in committed private memory is
/// distinguishable from allocator noise.
const BULK_KEYS: u64 = 2_048;
const BULK_ROUNDS: u64 = 10;
const BULK_VALUE_BYTES: usize = 8_192;
/// Keys per commit in the bulk phase.
///
/// The background SST flusher caps a column family at 8 sealed memtables and
/// refuses to seal a ninth (`CALYX_ASTER_ROUTER_SEALED_MEMTABLE_BACKLOG`).
/// 16 MB committed in one batch outruns it. Chunking plus a checkpoint per
/// round keeps the harness measuring reclamation instead of measuring the
/// flusher's queue depth.
const BULK_COMMIT_CHUNK: u64 = 256;

type Vault = AsterVault<SystemClock>;

fn key_of(tag: u64) -> Vec<u8> {
    format!("gcfsv/{tag:08}").into_bytes()
}

fn value_of(round: u64, tag: u64, width: usize) -> Vec<u8> {
    let mut value = format!("r{round:04}/k{tag:08}/").into_bytes();
    value.resize(width, b'.');
    value
}

/// Commits one round: every key written once, in one batch, so the whole round
/// lands at exactly one sequence. That uniformity is what makes the expected
/// reclaim count computable in closed form instead of estimated.
fn commit_round(vault: &Vault, keys: u64, round: u64, width: usize) -> Result<u64, Box<dyn Error>> {
    let rows = (0..keys)
        .map(|tag| (CF, key_of(tag), value_of(round, tag, width)))
        .collect::<Vec<_>>();
    vault.write_cf_batch(rows)?;
    Ok(vault.latest_seq())
}

/// Commits one bulk round in chunks, checkpointing afterwards so the sealed
/// memtable backlog drains between rounds. The round spans several sequences,
/// which is fine here: the bulk phases are measured against the exact debt
/// census, not against a closed-form count.
fn commit_bulk_round(
    vault: &Vault,
    keys: u64,
    round: u64,
    width: usize,
) -> Result<(), Box<dyn Error>> {
    let mut tag = 0;
    while tag < keys {
        let end = (tag + BULK_COMMIT_CHUNK).min(keys);
        let rows = (tag..end)
            .map(|inner| (CF, key_of(inner), value_of(round, inner, width)))
            .collect::<Vec<_>>();
        vault.write_cf_batch(rows)?;
        tag = end;
    }
    vault.checkpoint()?;
    Ok(())
}

fn reclaim_site_census(vault: &Vault) -> (u64, u64, u64) {
    vault
        .row_guard_census()
        .into_iter()
        .find(|entry| entry.site.as_str() == "snapshot_version_reclaim")
        .map_or((0, 0, 0), |entry| {
            (entry.holds, entry.max_held_us, entry.over_budget_holds)
        })
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: snapshot_version_gc_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;

    let vault = AsterVault::open(
        &dir,
        VaultId::from_str(VAULT_ID)?,
        b"snapshot-version-gc-fsv".to_vec(),
        VaultOptions::default(),
    )?;

    println!("snapshot_version_gc_fsv  (#2122)");
    println!("vault_dir = {}", dir.display());
    let budget = SnapshotVersionGcBudget::default();
    println!(
        "budget: max_versions={} max_chains={} max_pass_us={} max_shard_hold_us={} \
         (row-guard warn budget = {ROW_READ_GUARD_WARN_US} us)",
        budget.max_versions,
        budget.max_chains_scanned,
        budget.max_pass_us,
        budget.max_shard_hold_us
    );

    // --- 1. the same instrument production read, at the same value -----------
    println!("\n=== 1. before any reclamation: the reclaim guard site has never run");
    let (holds, _, _) = reclaim_site_census(&vault);
    println!("   snapshot_version_reclaim holds = {holds}");
    if holds != 0 {
        return Err(format!(
            "snapshot_version_reclaim already reports {holds} holds on a fresh vault; the \
             baseline this harness compares against is not a baseline"
        )
        .into());
    }

    // --- 2. build a known version chain --------------------------------------
    println!("\n=== 2. committing {ROUNDS_BEFORE_PIN} rounds over {KEYS} keys");
    let mut pin_seq = 0;
    for round in 0..ROUNDS_BEFORE_PIN {
        pin_seq = commit_round(&vault, KEYS, round, VALUE_BYTES)?;
    }
    println!("   latest_seq after pre-pin rounds = {pin_seq}");

    // --- 3. pin a reader, then bury it under newer versions -------------------
    println!("\n=== 3. pinning a reader, then committing {ROUNDS_AFTER_PIN} more rounds");
    let pinned = vault.pin_reader(Freshness::FreshDerived, LEASE_MS);
    if pinned.seq() != pin_seq {
        return Err(format!(
            "pinned reader is at seq {} but the vault's latest committed seq is {pin_seq}",
            pinned.seq()
        )
        .into());
    }
    let pinned_round = ROUNDS_BEFORE_PIN - 1;
    for round in ROUNDS_BEFORE_PIN..(ROUNDS_BEFORE_PIN + ROUNDS_AFTER_PIN) {
        commit_round(&vault, KEYS, round, VALUE_BYTES)?;
    }
    let latest_round = ROUNDS_BEFORE_PIN + ROUNDS_AFTER_PIN - 1;
    println!(
        "   pinned at seq {pin_seq} (round {pinned_round}); latest_seq = {}",
        vault.latest_seq()
    );

    // Expected: every key holds ROUNDS_BEFORE_PIN + ROUNDS_AFTER_PIN versions.
    // The floor is the pin, so versions strictly below it are the first
    // (ROUNDS_BEFORE_PIN - 1); the version *at* the floor is the retained
    // boundary and is not a candidate.
    let expected_from_kv = KEYS * (ROUNDS_BEFORE_PIN - 1);
    let debt_before = vault.snapshot_gc_debt_exact();
    println!(
        "   exact reclaimable debt at the pinned floor = {debt_before} \
         (>= {expected_from_kv} from the {CF:?} rows this harness wrote)"
    );
    if debt_before < expected_from_kv {
        return Err(format!(
            "debt census reports {debt_before} reclaimable versions, but this harness committed \
             {expected_from_kv} reclaimable {CF:?} versions alone"
        )
        .into());
    }

    // --- 4. reclaim under the pin --------------------------------------------
    println!("\n=== 4. one reclamation pass with the pin held");
    let pass = vault.snapshot_version_gc_memory_once(budget)?;
    println!(
        "   floor_seq={} current_seq={} active_leases={} versions_reclaimed={} \
         bytes_reclaimed={} chains_compacted={} chains_scanned={} shards={}/{} \
         shard_guard_holds={} sweep_completed={} stopped_on={} elapsed_us={} \
         max_shard_hold_us={}",
        pass.floor_seq,
        pass.current_seq,
        pass.active_leases,
        pass.versions_reclaimed,
        pass.bytes_reclaimed,
        pass.chains_compacted,
        pass.chains_scanned,
        pass.shards_visited,
        pass.shards_total,
        pass.shard_guard_holds,
        pass.sweep_completed,
        pass.stopped_on.as_str(),
        pass.elapsed_us,
        pass.max_shard_hold_us,
    );
    if pass.floor_seq != pin_seq {
        return Err(format!(
            "reclamation used floor {} while a live reader is pinned at {pin_seq}; it would have \
             reclaimed versions that reader can still see",
            pass.floor_seq
        )
        .into());
    }
    if pass.active_leases == 0 {
        return Err("the pass reports no active leases while this harness holds one".into());
    }
    if pass.versions_reclaimed != debt_before {
        return Err(format!(
            "pass reclaimed {} versions but the census said {debt_before} were reclaimable, and \
             the budget was not exhausted (stopped_on={})",
            pass.versions_reclaimed,
            pass.stopped_on.as_str()
        )
        .into());
    }
    if !pass.sweep_completed {
        return Err(format!(
            "pass did not complete its sweep (stopped_on={}) with a budget of {} versions against \
             a debt of {debt_before}",
            pass.stopped_on.as_str(),
            budget.max_versions
        )
        .into());
    }

    // --- 5. the pinned reader still reads its own view ------------------------
    println!("\n=== 5. the pinned reader after reclamation");
    for tag in 0..KEYS {
        let read = vault.read_pinned_cf(pinned, CF, &key_of(tag))?;
        let expected = value_of(pinned_round, tag, VALUE_BYTES);
        if read.as_deref() != Some(expected.as_slice()) {
            return Err(format!(
                "pinned reader at seq {pin_seq} lost key {tag}: expected the round-{pinned_round} \
                 value, got {:?}",
                read.map(
                    |value| String::from_utf8_lossy(&value[..24.min(value.len())]).to_string()
                )
            )
            .into());
        }
    }
    println!("   all {KEYS} keys still read their round-{pinned_round} value through the pin");

    // Read-your-writes at the same time: latest must be the newest round.
    for tag in 0..KEYS {
        let read = vault.read_cf_latest(CF, &key_of(tag))?;
        let expected = value_of(latest_round, tag, VALUE_BYTES);
        if read.as_deref() != Some(expected.as_slice()) {
            return Err(
                format!("latest read of key {tag} is not the round-{latest_round} value").into(),
            );
        }
    }
    println!("   all {KEYS} keys read their newest value from the latest view");

    // --- 6. release the pin; the floor advances and the rest is reclaimed -----
    println!("\n=== 6. releasing the pin");
    if !vault.release_reader(pinned.lease().id()) {
        return Err("the reader lease was already gone before the harness released it".into());
    }
    let debt_after_release = vault.snapshot_gc_debt_exact();
    println!("   exact reclaimable debt once the floor is free = {debt_after_release}");
    if debt_after_release == 0 {
        return Err(
            "releasing the pin exposed no new reclaimable versions, so step 4 was not actually \
             constrained by the pin and step 5 proved nothing"
                .into(),
        );
    }

    let pass2 = vault.snapshot_version_gc_memory_once(budget)?;
    println!(
        "   pass 2: floor_seq={} versions_reclaimed={} bytes_reclaimed={} sweep_completed={} \
         stopped_on={} max_shard_hold_us={}",
        pass2.floor_seq,
        pass2.versions_reclaimed,
        pass2.bytes_reclaimed,
        pass2.sweep_completed,
        pass2.stopped_on.as_str(),
        pass2.max_shard_hold_us
    );
    if pass2.floor_seq != vault.latest_seq() {
        return Err(format!(
            "with no reader pinned the floor should be the current sequence {}, not {}",
            vault.latest_seq(),
            pass2.floor_seq
        )
        .into());
    }
    if pass2.versions_reclaimed != debt_after_release {
        return Err(format!(
            "pass 2 reclaimed {} of {debt_after_release} reclaimable versions",
            pass2.versions_reclaimed
        )
        .into());
    }

    let pass3 = vault.snapshot_version_gc_memory_once(budget)?;
    println!(
        "   pass 3 (convergence): versions_reclaimed={} sweep_completed={} debt={}",
        pass3.versions_reclaimed,
        pass3.sweep_completed,
        vault.snapshot_gc_debt_exact()
    );
    if pass3.versions_reclaimed != 0 || !pass3.sweep_completed {
        return Err(format!(
            "reclamation did not converge: a third pass over a drained vault reclaimed {} versions \
             (sweep_completed={})",
            pass3.versions_reclaimed, pass3.sweep_completed
        )
        .into());
    }
    if vault.snapshot_gc_debt_exact() != 0 {
        return Err(
            "the debt census still reports reclaimable versions after a completed sweep \
                    reclaimed none — the pass and the census disagree"
                .into(),
        );
    }

    // Read-your-writes once more, now that every superseded version is gone.
    for tag in 0..KEYS {
        let read = vault.read_cf_latest(CF, &key_of(tag))?;
        if read.as_deref() != Some(value_of(latest_round, tag, VALUE_BYTES).as_slice()) {
            return Err(format!(
                "key {tag} lost its newest value after full reclamation — the retained boundary \
                 version was dropped"
            )
            .into());
        }
    }
    println!("   read-your-writes holds for all {KEYS} keys after full reclamation");

    // --- 7. paging: a tiny budget must cover the same ground ------------------
    println!("\n=== 7. paged reclamation with a deliberately tiny budget");
    for round in 0..BULK_ROUNDS {
        commit_bulk_round(&vault, BULK_KEYS, round, BULK_VALUE_BYTES)?;
    }
    let paged_debt = vault.snapshot_gc_debt_exact();
    let tiny = SnapshotVersionGcBudget {
        max_versions: 97,
        max_chains_scanned: 313,
        max_pass_us: 1_000_000,
        max_shard_hold_us: 1_000,
    };
    println!(
        "   debt to drain = {paged_debt}; budget = {} versions / {} chains per pass",
        tiny.max_versions, tiny.max_chains_scanned
    );
    // Termination is decided by the **external** debt census, not by the pass's
    // own `sweep_completed`: a chain budget of 313 is smaller than the vault's
    // chain count, so no single pass under this budget can ever walk the whole
    // table, and asking one to say "the sweep is done" would be asking it to
    // claim something it cannot know. `sweep_completed` is checked separately
    // below, at a budget that can actually cover the table.
    let mut passes = 0u64;
    let mut paged_reclaimed = 0u64;
    let mut budget_stops = 0u64;
    let mut max_paged_hold_us = 0u64;
    while vault.snapshot_gc_debt_exact() > 0 {
        let pass = vault.snapshot_version_gc_memory_once(tiny)?;
        passes += 1;
        paged_reclaimed += pass.versions_reclaimed;
        max_paged_hold_us = max_paged_hold_us.max(pass.max_shard_hold_us);
        if !pass.sweep_completed {
            budget_stops += 1;
        }
        if passes > 100_000 {
            return Err(format!(
                "paged reclamation did not converge in {passes} passes with {paged_reclaimed} of \
                 {paged_debt} versions reclaimed; the resume cursor is not making progress"
            )
            .into());
        }
    }
    println!(
        "   drained in {passes} passes, reclaiming {paged_reclaimed} versions \
         ({budget_stops} passes stopped on a budget, max_shard_hold_us={max_paged_hold_us})"
    );
    if paged_reclaimed != paged_debt {
        return Err(format!(
            "paged reclamation freed {paged_reclaimed} versions but {paged_debt} were reclaimable; \
             the cursor skipped or double-counted"
        )
        .into());
    }
    if passes < 2 || budget_stops == 0 {
        return Err(format!(
            "the tiny budget drained in {passes} passes with {budget_stops} budget stops, so this \
             step exercised no paging at all"
        )
        .into());
    }
    // Now that the debt is drained, a pass with a budget large enough to cover
    // the table must say so — this is the claim the tiny budget cannot make.
    let sweep = vault.snapshot_version_gc_memory_once(budget)?;
    let sweep = if sweep.sweep_completed {
        sweep
    } else {
        // The first pass after paging resumes from a mid-shard cursor and is
        // therefore not a whole-table sweep by construction; the next one is.
        vault.snapshot_version_gc_memory_once(budget)?
    };
    println!(
        "   full-budget sweep: versions_reclaimed={} chains_scanned={} shards={}/{} \
         sweep_completed={} max_shard_hold_us={}",
        sweep.versions_reclaimed,
        sweep.chains_scanned,
        sweep.shards_visited,
        sweep.shards_total,
        sweep.sweep_completed,
        sweep.max_shard_hold_us
    );
    if !sweep.sweep_completed || sweep.versions_reclaimed != 0 {
        return Err(format!(
            "a full-budget pass over a drained vault reported sweep_completed={} with {} versions \
             reclaimed",
            sweep.sweep_completed, sweep.versions_reclaimed
        )
        .into());
    }
    for tag in 0..BULK_KEYS {
        let read = vault.read_cf_latest(CF, &key_of(tag))?;
        if read.as_deref() != Some(value_of(BULK_ROUNDS - 1, tag, BULK_VALUE_BYTES).as_slice()) {
            return Err(
                format!("bulk key {tag} lost its newest value across paged reclamation").into(),
            );
        }
    }
    println!("   all {BULK_KEYS} bulk keys still read their newest value");

    // --- 8. guard holds ------------------------------------------------------
    println!("\n=== 8. row-guard census for the reclaim site");
    let (holds, max_held_us, over_budget) = reclaim_site_census(&vault);
    println!(
        "   snapshot_version_reclaim holds={holds} max_held_us={max_held_us} \
         over_budget_holds={over_budget} (budget {ROW_READ_GUARD_WARN_US} us)"
    );
    if holds == 0 {
        return Err(
            "reclamation ran but the census recorded no holds; the instrument that \
                    diagnosed #2122 would not see this fix working"
                .into(),
        );
    }
    if max_held_us >= ROW_READ_GUARD_WARN_US {
        return Err(format!(
            "reclamation held a row-table shard write guard for {max_held_us} us, at or above the \
             {ROW_READ_GUARD_WARN_US} us budget every commit routed to that shard waits behind"
        )
        .into());
    }

    // --- 9. the memory metric ------------------------------------------------
    println!("\n=== 9. working set vs committed private memory across a bulk reclaim");
    for round in BULK_ROUNDS..(BULK_ROUNDS * 2) {
        commit_bulk_round(&vault, BULK_KEYS, round, BULK_VALUE_BYTES)?;
    }
    let rss_before = heap_rss_bytes()?;
    let private_before = process_private_bytes()?;
    let bulk_debt = vault.snapshot_gc_debt_exact();
    let bulk = SnapshotVersionGcBudget {
        max_versions: usize::try_from(bulk_debt).unwrap_or(usize::MAX).max(1),
        ..SnapshotVersionGcBudget::default()
    };
    let bulk_pass = vault.snapshot_version_gc_memory_once(bulk)?;
    let rss_after = heap_rss_bytes()?;
    let private_after = process_private_bytes()?;
    println!(
        "   reclaimed {} versions / {:.1} MiB of value bytes",
        bulk_pass.versions_reclaimed,
        mib(bulk_pass.bytes_reclaimed)
    );
    println!(
        "   working set   : {:.1} -> {:.1} MiB  (delta {:+.1})",
        mib(rss_before),
        mib(rss_after),
        mib(rss_after) - mib(rss_before)
    );
    println!(
        "   private commit: {:.1} -> {:.1} MiB  (delta {:+.1})",
        mib(private_before),
        mib(private_after),
        mib(private_after) - mib(private_before)
    );
    println!(
        "   returned/reclaimed ratio (private) = {:.2}",
        (private_before.saturating_sub(private_after)) as f64
            / (bulk_pass.bytes_reclaimed.max(1)) as f64
    );
    if private_before == 0 || rss_before == 0 {
        return Err("a memory probe returned zero bytes for a live process".into());
    }
    if bulk_pass.bytes_reclaimed < 64 * 1024 * 1024 {
        return Err(format!(
            "the bulk phase only reclaimed {:.1} MiB, too little for a process-level memory probe \
             to resolve; this step would report noise",
            mib(bulk_pass.bytes_reclaimed)
        )
        .into());
    }
    if private_after >= private_before {
        // Not a pass/fail on the version-chain fix — that is proven above by the
        // debt census — but a real, separate finding about allocator retention,
        // and the harness must say so rather than quietly tolerate it.
        return Err(format!(
            "committed private memory did not fall after reclaiming {:.1} MiB of value bytes \
             ({:.1} -> {:.1} MiB). The version chains ARE being freed (steps 4-7 prove it against \
             the debt census), so this is allocator retention on top of the chain leak: the \
             process freed the allocations and the allocator did not return the pages. File it \
             against the no-#[global_allocator] finding",
            mib(bulk_pass.bytes_reclaimed),
            mib(private_before),
            mib(private_after)
        )
        .into());
    }

    println!("\nPASS: version chains are reclaimed, pinned readers are intact, holds are bounded.");
    Ok(())
}
