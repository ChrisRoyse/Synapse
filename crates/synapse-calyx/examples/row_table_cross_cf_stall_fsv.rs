//! Manual FSV for #1950: a commit that touches only `kv` waits on a scan of
//! `Base`, two column families that share no key and no invariant.
//!
//! ## The claim under test
//!
//! `VersionedCfStore::rows` is one vault-wide `RwLock<BTreeMap<ColumnFamily,
//! BTreeMap<Vec<u8>, VersionChain>>>`. The map is *already* partitioned by
//! column family, and a commit's per-CF sub-maps are independent — there is no
//! cross-CF invariant the lock is protecting. So a long read of one CF blocking
//! a write to another is an artefact of the lock's granularity, not of the data.
//!
//! #1950 measured a 1.4 s commit of three rows in production and attributed
//! 99.996% of it to row-table lock wait, with `scan_cf_latest(Base)` named as
//! the holder. This harness reproduces that in-process so the remedy is
//! falsifiable: a fix must move the contended number and leave the baseline
//! alone.
//!
//! ## Construction — why concurrency, not volume
//!
//! A serial burst can never show this. The stall exists only when a reader is
//! *inside* the guard while a writer asks for it, so the harness runs two
//! threads and measures the writer:
//!
//! | phase | scanner thread | writer thread | expectation |
//! |---|---|---|---|
//! | A baseline | idle | N commits to `kv` | fast; this is the floor |
//! | B contended | `scan_cf_latest(Base)` in a loop | N commits to `kv` | today: dominated by the scan's hold |
//!
//! `Base` is the ~102k-row constellation CF on a real vault, which is what
//! makes the scan long enough to be the thing measured rather than noise. The
//! writer's rows go to `kv`: a different CF, a different sub-map, no shared key.
//!
//! ## Source of truth
//!
//! Latency is a measurement, so it is reported beside two facts that are not:
//! the per-site row-guard census (which names the holder and counts every hold,
//! #1952), and a read-back of every key the writer claims to have committed,
//! pulled out of `kv` after the run. A latency win with missing rows is not a
//! win.
//!
//! ```text
//! cargo run --release -p synapse-calyx --example row_table_cross_cf_stall_fsv -- <vault-copy-dir>
//! ```
//!
//! Needs a **copy**: this writes.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::mvcc::RowGuardSite;
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;

/// Commits issued per phase. Each is one `write_cf_batch` of two `kv` rows —
/// deliberately tiny, so any time it takes is wait rather than work. 40 is
/// enough to see a distribution and short enough that the harness finishes
/// while the scanner is still running.
const COMMITS_PER_PHASE: usize = 40;
/// Rows per commit. #1950's production stall was a 2-18 row batch.
const ROWS_PER_COMMIT: usize = 2;

fn main() -> Result<(), Box<dyn Error>> {
    let Some(root) = std::env::args().nth(1).map(PathBuf::from) else {
        return Err("usage: row_table_cross_cf_stall_fsv <vault-copy-dir>".into());
    };
    let vault_dir = root.join("db-daemon");
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }

    let vault_id = read_vault_id(&vault_dir)?;
    // The real machine salt, not a synthetic one: content addresses are derived
    // from it, so opening with anything else would be opening a different vault
    // that happens to share a directory.
    let salt = std::fs::read(root.join("machine-salt.bin"))
        .map_err(|error| format!("cannot read machine-salt.bin: {error}"))?;
    let vault = Arc::new(AsterVault::open(
        &vault_dir,
        vault_id,
        salt,
        VaultOptions::default(),
    )?);
    println!("row_table_cross_cf_stall_fsv  (#1950)");
    println!("vault_dir = {}", vault_dir.display());

    // The scan has to be long enough to be the thing measured. Report it.
    let base_rows = vault.scan_cf_latest(ColumnFamily::Base)?.len();
    let kv_rows_before = vault.scan_cf_latest(ColumnFamily::Kv)?.len();
    println!("  Base rows = {base_rows}   Kv rows before = {kv_rows_before}");
    if base_rows < 10_000 {
        return Err(format!(
            "Base holds only {base_rows} rows; the scan will not be long enough for its hold to \
             be distinguishable from noise. Point this at a copy of a real vault."
        )
        .into());
    }

    let census_before = census(&vault);

    // ---- Phase A: the floor, with nothing else touching the row table ----
    println!("\n=== Phase A: {COMMITS_PER_PHASE} kv commits, no concurrent Base scan");
    let phase_a = commit_phase(&vault, 0)?;
    report("A baseline", &phase_a);

    // ---- Phase B: the same commits, with a Base scanner inside the guard ----
    println!("\n=== Phase B: the same commits, with scan_cf_latest(Base) running concurrently");
    let stop = Arc::new(AtomicBool::new(false));
    let scans = Arc::new(AtomicU64::new(0));
    let scanner = {
        let vault = Arc::clone(&vault);
        let stop = Arc::clone(&stop);
        let scans = Arc::clone(&scans);
        std::thread::spawn(move || -> Result<(), String> {
            while !stop.load(Ordering::Relaxed) {
                vault
                    .scan_cf_latest(ColumnFamily::Base)
                    .map_err(|error| format!("scanner failed: {error}"))?;
                scans.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        })
    };

    let phase_b = commit_phase(&vault, COMMITS_PER_PHASE)?;
    stop.store(true, Ordering::Relaxed);
    scanner
        .join()
        .map_err(|_| "scanner thread panicked")?
        .map_err(|error| -> Box<dyn Error> { error.into() })?;
    report("B contended", &phase_b);
    println!(
        "  concurrent Base scans completed = {}",
        scans.load(Ordering::Relaxed)
    );

    // ---- The holder, named from the census rather than inferred ----
    println!("\n=== row-guard census delta (every hold, not only over-budget ones)");
    let census_after = census(&vault);
    for (site, holds_after, total_after, max_after) in &census_after {
        let (holds_before, total_before, _) = census_before
            .iter()
            .find(|(s, ..)| s == site)
            .map(|(_, h, t, m)| (*h, *t, *m))
            .unwrap_or((0, 0, 0));
        let holds = holds_after - holds_before;
        if holds == 0 {
            continue;
        }
        let total = total_after - total_before;
        println!(
            "  {site:<32} holds={holds:<6} total_held_us={total:<12} mean_us={:<10} max_us={max_after}",
            total / holds.max(1)
        );
    }

    // ---- Source of truth: every committed key, read back off the vault ----
    println!("\n=== readback: every key the writer claims it committed");
    let kv_after = vault.scan_cf_latest(ColumnFamily::Kv)?;
    let expected = COMMITS_PER_PHASE * ROWS_PER_COMMIT * 2;
    let present = phase_a
        .keys
        .iter()
        .chain(phase_b.keys.iter())
        .filter(|key| kv_after.iter().any(|(k, _)| k == *key))
        .count();
    println!(
        "  Kv rows after = {}   keys written = {expected}   keys found on readback = {present}",
        kv_after.len()
    );
    if present != expected {
        return Err(format!(
            "{present} of {expected} committed keys are present in Kv: the latency numbers above \
             describe a write path that lost rows, so they mean nothing"
        )
        .into());
    }

    // ---- The verdict ----
    println!("\n=== verdict");
    let ratio = phase_b.p50_us as f64 / phase_a.p50_us.max(1) as f64;
    println!(
        "  p50 A={} us  p50 B={} us  ratio={ratio:.1}x     max A={} us  max B={} us",
        phase_a.p50_us, phase_b.p50_us, phase_a.max_us, phase_b.max_us
    );
    println!(
        "\n  A commit to `kv` and a scan of `Base` share no key and no invariant. Any ratio\n  \
         above ~1 is the vault-wide row-table lock, not the work."
    );
    Ok(())
}

struct Phase {
    p50_us: u64,
    max_us: u64,
    total_us: u64,
    keys: Vec<Vec<u8>>,
}

fn report(label: &str, phase: &Phase) {
    println!(
        "  {label:<12} commits={:<4} p50={:<8} us  max={:<9} us  total={} us",
        phase.keys.len() / ROWS_PER_COMMIT,
        phase.p50_us,
        phase.max_us,
        phase.total_us
    );
}

/// Issues `COMMITS_PER_PHASE` tiny `kv` batches and times each one.
///
/// `tag_base` keeps phase A's and phase B's keys disjoint so the readback can
/// account for every one of them.
fn commit_phase(
    vault: &AsterVault<calyx_core::SystemClock>,
    tag_base: usize,
) -> Result<Phase, Box<dyn Error>> {
    let mut samples = Vec::with_capacity(COMMITS_PER_PHASE);
    let mut keys = Vec::with_capacity(COMMITS_PER_PHASE * ROWS_PER_COMMIT);
    for commit in 0..COMMITS_PER_PHASE {
        let rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)> = (0..ROWS_PER_COMMIT)
            .map(|row| {
                let key = stall_key(tag_base + commit, row);
                keys.push(key.clone());
                (ColumnFamily::Kv, key, b"1950".to_vec())
            })
            .collect();
        let started = Instant::now();
        vault.write_cf_batch(rows)?;
        samples.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    let total_us = samples.iter().sum();
    samples.sort_unstable();
    Ok(Phase {
        p50_us: samples[samples.len() / 2],
        max_us: *samples.last().unwrap_or(&0),
        total_us,
        keys,
    })
}

/// A key in `kv`'s user keyspace, prefixed so a real row can never collide.
fn stall_key(commit: usize, row: usize) -> Vec<u8> {
    format!("fsv-1950/{commit:04}/{row:02}").into_bytes()
}

fn census(vault: &AsterVault<calyx_core::SystemClock>) -> Vec<(String, u64, u64, u64)> {
    let _ = RowGuardSite::COUNT;
    vault
        .row_guard_census()
        .into_iter()
        .map(|row| {
            (
                row.site.as_str().to_owned(),
                row.holds,
                row.total_held_us,
                row.max_held_us,
            )
        })
        .collect()
}

/// The vault's own id, read out of `vault-identity.json`.
///
/// `AsterVault::open` fails closed on a mismatch, so this must be the real one
/// rather than a synthetic constant.
fn read_vault_id(vault_dir: &std::path::Path) -> Result<VaultId, Box<dyn Error>> {
    let path = vault_dir.join("vault-identity.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)?;
    let id = parsed
        .get("vault_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{} has no vault_id field", path.display()))?;
    Ok(id.parse()?)
}
