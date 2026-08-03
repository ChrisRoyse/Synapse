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
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

/// Commits issued per phase. Each is one `write_cf_batch` of two `kv` rows —
/// deliberately tiny, so any time it takes is wait rather than work. 40 is
/// enough to see a distribution and short enough that the harness finishes
/// while the scanner is still running.
const COMMITS_PER_PHASE: usize = 40;
/// Rows per commit. #1950's production stall was a 2-18 row batch.
const ROWS_PER_COMMIT: usize = 2;

/// Per-commit lock-wait totals, scraped from the commit's own tracing event.
///
/// The commit path already measures which lock it waited on
/// (`mvcc_row_lock_wait_us` vs `mvcc_router_lock_wait_us`) and emits both. A
/// harness that only times the call from outside can say a commit was slow but
/// not which of the vault's two global locks made it slow — and #1950 turns
/// entirely on that distinction.
#[derive(Default)]
struct LockWaits {
    row_us: AtomicU64,
    router_us: AtomicU64,
    row_max_us: AtomicU64,
    router_max_us: AtomicU64,
    commits: AtomicU64,
}

impl LockWaits {
    fn reset(&self) {
        self.row_us.store(0, Ordering::Relaxed);
        self.router_us.store(0, Ordering::Relaxed);
        self.row_max_us.store(0, Ordering::Relaxed);
        self.router_max_us.store(0, Ordering::Relaxed);
        self.commits.store(0, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, u64, u64, u64, u64) {
        (
            self.row_us.load(Ordering::Relaxed),
            self.router_us.load(Ordering::Relaxed),
            self.row_max_us.load(Ordering::Relaxed),
            self.router_max_us.load(Ordering::Relaxed),
            self.commits.load(Ordering::Relaxed),
        )
    }
}

static LOCK_WAITS: std::sync::LazyLock<LockWaits> = std::sync::LazyLock::new(LockWaits::default);

#[derive(Default)]
struct CommitVisitor {
    row_us: Option<u64>,
    router_us: Option<u64>,
}

impl Visit for CommitVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "mvcc_row_lock_wait_us" => self.row_us = Some(value),
            "mvcc_router_lock_wait_us" => self.router_us = Some(value),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

struct CommitLayer;

impl<S: tracing::Subscriber> Layer<S> for CommitLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = CommitVisitor::default();
        event.record(&mut visitor);
        let (Some(row), Some(router)) = (visitor.row_us, visitor.router_us) else {
            return;
        };
        LOCK_WAITS.row_us.fetch_add(row, Ordering::Relaxed);
        LOCK_WAITS.router_us.fetch_add(router, Ordering::Relaxed);
        LOCK_WAITS.row_max_us.fetch_max(row, Ordering::Relaxed);
        LOCK_WAITS
            .router_max_us
            .fetch_max(router, Ordering::Relaxed);
        LOCK_WAITS.commits.fetch_add(1, Ordering::Relaxed);
    }
}

/// Which of the vault's two global locks the commits actually waited on.
fn report_waits(label: &str, waits: (u64, u64, u64, u64, u64)) {
    let (row, router, row_max, router_max, commits) = waits;
    println!(
        "  {label:<12} events={commits:<4} row_lock_wait total={row:<9} us max={row_max:<9} us   router_lock_wait total={router:<9} us max={router_max} us"
    );
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::registry().with(CommitLayer).init();
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
    LOCK_WAITS.reset();
    let phase_a = commit_phase(&vault, 0)?;
    let waits_a = LOCK_WAITS.snapshot();
    report("A baseline", &phase_a);
    report_waits("A baseline", waits_a);

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

    LOCK_WAITS.reset();
    let phase_b = commit_phase(&vault, COMMITS_PER_PHASE)?;
    let waits_b = LOCK_WAITS.snapshot();
    stop.store(true, Ordering::Relaxed);
    scanner
        .join()
        .map_err(|_| "scanner thread panicked")?
        .map_err(|error| -> Box<dyn Error> { error.into() })?;
    report("B contended", &phase_b);
    report_waits("B contended", waits_b);
    println!(
        "  concurrent Base scans completed = {}",
        scans.load(Ordering::Relaxed)
    );

    // ---- Phase C: the control. Commit into the CF the scanner is scanning --
    //
    // Phase B alone cannot tell "the shard map works" from "the lock was
    // removed". If sharding is real, a commit to the *same* family as the scan
    // must still wait, because that is genuine contention over one shard rather
    // than the vault-wide lock #1950 removed. A phase C that came back as fast
    // as phase A would mean commits and scans of one column family no longer
    // exclude each other at all, which is a correctness bug wearing a
    // performance win's clothes.
    println!(
        "\n=== Phase C (control): {COMMITS_PER_PHASE} commits into Kv, with scan_cf_latest(Kv) concurrent"
    );
    let stop = Arc::new(AtomicBool::new(false));
    let same_cf_scans = Arc::new(AtomicU64::new(0));
    let scanner = {
        let vault = Arc::clone(&vault);
        let stop = Arc::clone(&stop);
        let scans = Arc::clone(&same_cf_scans);
        std::thread::spawn(move || -> Result<(), String> {
            while !stop.load(Ordering::Relaxed) {
                vault
                    .scan_cf_latest(ColumnFamily::Kv)
                    .map_err(|error| format!("same-CF scanner failed: {error}"))?;
                scans.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        })
    };
    LOCK_WAITS.reset();
    let phase_c = commit_phase_into(&vault, ColumnFamily::Kv, COMMITS_PER_PHASE * 2)?;
    let waits_c = LOCK_WAITS.snapshot();
    stop.store(true, Ordering::Relaxed);
    scanner
        .join()
        .map_err(|_| "same-CF scanner thread panicked")?
        .map_err(|error| -> Box<dyn Error> { error.into() })?;
    report("C same-CF", &phase_c);
    report_waits("C same-CF", waits_c);
    println!(
        "  concurrent Kv scans completed = {}",
        same_cf_scans.load(Ordering::Relaxed)
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
    let expected = COMMITS_PER_PHASE * ROWS_PER_COMMIT * 3;
    let present = phase_a
        .keys
        .iter()
        .chain(phase_b.keys.iter())
        .chain(phase_c.keys.iter())
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
    let cross_ratio = phase_b.p50_us as f64 / phase_a.p50_us.max(1) as f64;
    let same_ratio = phase_c.p50_us as f64 / phase_a.p50_us.max(1) as f64;
    println!(
        "  p50 A={} us   p50 B={} us (cross-CF, {cross_ratio:.1}x)   p50 C={} us (same-CF, {same_ratio:.1}x)",
        phase_a.p50_us, phase_b.p50_us, phase_c.p50_us
    );
    println!(
        "  max A={} us   max B={} us                     max C={} us",
        phase_a.max_us, phase_b.max_us, phase_c.max_us
    );
    let (row_b, router_b, row_max_b, router_max_b, _) = waits_b;
    let (row_c, router_c, row_max_c, router_max_c, _) = waits_c;
    println!(
        "\n  cross-CF (B) lock wait: row total={row_b} us max={row_max_b} us   router total={router_b} us max={router_max_b} us"
    );
    println!(
        "  same-CF  (C) lock wait: row total={row_c} us max={row_max_c} us   router total={router_c} us max={router_max_c} us"
    );

    // Two claims, each falsifiable, and the second is what stops the first from
    // being satisfiable by simply not locking.
    let cross_ok = cross_ratio < 2.0;
    let same_contends = phase_c.max_us > phase_a.max_us;
    println!(
        "\n  [{}] B: a commit to `kv` and a scan of `Base` share no key and no invariant, so\n       \
         the cross-CF ratio must be ~1. Measured {cross_ratio:.2}x.",
        if cross_ok { "PASS" } else { "FAIL" }
    );
    println!(
        "  [{}] C: a commit to `kv` and a scan of `kv` share one shard, so they MUST still\n       \
         exclude each other. A phase C as fast as phase A would mean the lock was removed\n       \
         rather than split. Measured max C={} us against max A={} us.",
        if same_contends { "PASS" } else { "FAIL" },
        phase_c.max_us,
        phase_a.max_us
    );
    if !cross_ok || !same_contends {
        return Err(format!(
            "sharding verdict failed: cross_cf_ratio={cross_ratio:.2} (want <2.0), \
             same_cf_contends={same_contends} (want true)"
        )
        .into());
    }
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

/// Issues `COMMITS_PER_PHASE` tiny batches into `cf` and times each one.
///
/// `tag_base` keeps each phase's keys disjoint so the readback can account for
/// every one of them.
fn commit_phase(
    vault: &AsterVault<calyx_core::SystemClock>,
    tag_base: usize,
) -> Result<Phase, Box<dyn Error>> {
    commit_phase_into(vault, ColumnFamily::Kv, tag_base)
}

fn commit_phase_into(
    vault: &AsterVault<calyx_core::SystemClock>,
    cf: ColumnFamily,
    tag_base: usize,
) -> Result<Phase, Box<dyn Error>> {
    let mut samples = Vec::with_capacity(COMMITS_PER_PHASE);
    let mut keys = Vec::with_capacity(COMMITS_PER_PHASE * ROWS_PER_COMMIT);
    for commit in 0..COMMITS_PER_PHASE {
        let rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)> = (0..ROWS_PER_COMMIT)
            .map(|row| {
                let key = stall_key(tag_base + commit, row);
                keys.push(key.clone());
                (cf, key, b"1950".to_vec())
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
