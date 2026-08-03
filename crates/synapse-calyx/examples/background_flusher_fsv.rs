//! Manual FSV for #1951: the SST write comes off the committing thread, and
//! every SST it deferred still lands.
//!
//! ## The claim under test
//!
//! After #1950 removed the last lock term, `sst_write_unlocked` was 89-98% of
//! the whole `mvcc` commit stage — 20-67 ms paid by the committing thread, and
//! every MCP tool call pays a durable commit. #1951 hands the sealed memtable
//! to a background flusher instead.
//!
//! A latency number alone cannot establish that, because the cheapest way to
//! make a write fast is not to do it. So this harness measures the latency
//! **and** two independent witnesses that the write happened anyway:
//!
//! | witness | source of truth |
//! |---|---|
//! | `flush_status().written` | the flusher's own counter, in-process |
//! | `flush-*.sst` file count | the filesystem, independent of the process |
//! | reopen readback | the rows, after the vault is dropped and reopened |
//!
//! The first two must agree exactly. The third is the one that matters: a
//! background flusher is precisely the change that can lose a sealed memtable
//! across a restart, and only a reopen can prove it did not.
//!
//! ## Phases
//!
//! | phase | construction | what it settles |
//! |---|---|---|
//! | A | commit until memtables seal, timing each commit | `sst_write_us` on the caller goes to ~0 |
//! | B | drain, then count SSTs on disk against the counter | the deferred writes actually happened |
//! | C | reopen and read back every key | nothing was lost across a restart |
//! | D | **crash-during-deferral**: abandon the vault with seals in flight, reopen | the WAL covers an undrained queue |
//! | E | back-pressure: submit far past the bound | the throttle is a bounded wait, not an error |
//!
//! Phase D is the case #1951's own analysis predicted safe but nobody had
//! observed. Predicting is not observing.
//!
//! ```text
//! cargo run --release -p synapse-calyx --example background_flusher_fsv -- <empty-scratch-dir>
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

/// `mvcc_sst_write_unlocked_us`, scraped from the commit's own event.
///
/// This is the acceptance criterion, and total commit latency is not a
/// substitute for it: these commits write 2.3 MB each, so their wall clock is
/// dominated by the WAL fsync no matter what the flusher does. Only the stage
/// field can say whether the SST write left the committing thread.
#[derive(Default)]
struct SstWrite {
    total_us: AtomicU64,
    max_us: AtomicU64,
    events: AtomicU64,
}

static SST_WRITE: std::sync::LazyLock<SstWrite> = std::sync::LazyLock::new(SstWrite::default);

impl SstWrite {
    fn reset(&self) {
        self.total_us.store(0, Ordering::Relaxed);
        self.max_us.store(0, Ordering::Relaxed);
        self.events.store(0, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.total_us.load(Ordering::Relaxed),
            self.max_us.load(Ordering::Relaxed),
            self.events.load(Ordering::Relaxed),
        )
    }
}

#[derive(Default)]
struct CommitVisitor {
    sst_write_us: Option<u64>,
}

impl Visit for CommitVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "mvcc_sst_write_unlocked_us" {
            self.sst_write_us = Some(value);
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

struct CommitLayer;

impl<S: tracing::Subscriber> Layer<S> for CommitLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = CommitVisitor::default();
        event.record(&mut visitor);
        let Some(sst_write_us) = visitor.sst_write_us else {
            return;
        };
        SST_WRITE
            .total_us
            .fetch_add(sst_write_us, Ordering::Relaxed);
        SST_WRITE.max_us.fetch_max(sst_write_us, Ordering::Relaxed);
        SST_WRITE.events.fetch_add(1, Ordering::Relaxed);
    }
}

/// Rows per commit, and value size, chosen so a handful of commits crosses the
/// 8 MB memtable cap and actually seals. A harness that never seals would report
/// a perfect result while testing nothing.
const ROWS_PER_COMMIT: usize = 24;
const VALUE_BYTES: usize = 96 * 1024;
const COMMITS: usize = 40;

fn main() -> Result<(), Box<dyn Error>> {
    let Some(root) = std::env::args().nth(1).map(PathBuf::from) else {
        return Err("usage: background_flusher_fsv <empty-scratch-dir>".into());
    };
    tracing_subscriber::registry().with(CommitLayer).init();
    std::fs::create_dir_all(&root)?;
    let vault_dir = root.join("vault");
    println!(
        "background_flusher_fsv  (#1951)\nvault_dir = {}",
        vault_dir.display()
    );

    // A fixed synthetic id: this vault is created by the harness, so the id is
    // ours to choose, and a constant one lets phases C/D/E reopen the same vault
    // rather than a fresh one.
    let vault_id: VaultId = "01JZ1951BFSV1951BFSV195100"
        .parse()
        .map_err(|error| format!("fixed harness vault id is not a valid ULID: {error:?}"))?;

    // ---- Phase A: commit, and time what the caller pays -------------------
    println!(
        "\n=== Phase A: {COMMITS} commits of {ROWS_PER_COMMIT} x {VALUE_BYTES}B, timed on the caller"
    );
    let flush_ssts_before;
    let status_after_commits;
    let mut samples = Vec::with_capacity(COMMITS);
    let mut all_keys = Vec::new();
    {
        let vault = Arc::new(open_vault(&vault_dir, vault_id)?);
        flush_ssts_before = count_flush_ssts(&vault_dir)?;
        SST_WRITE.reset();
        for commit in 0..COMMITS {
            let rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)> = (0..ROWS_PER_COMMIT)
                .map(|row| {
                    let key = flush_key(commit, row);
                    all_keys.push(key.clone());
                    (ColumnFamily::Kv, key, vec![b'F'; VALUE_BYTES])
                })
                .collect();
            let started = Instant::now();
            vault.write_cf_batch(rows)?;
            samples.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
        }
        let mid = vault.flush_status();
        println!(
            "  in-flight right after the last commit: outstanding={} written={} failed={}",
            mid.outstanding, mid.written, mid.failed
        );
        if mid.written == 0 && mid.outstanding == 0 {
            return Err(
                "no memtable ever sealed, so this run exercised nothing; raise VALUE_BYTES or COMMITS"
                    .into(),
            );
        }

        // ---- Phase B: drain, then two counters that must agree ------------
        println!("\n=== Phase B: drain, then the counter against the filesystem");
        let drain_started = Instant::now();
        vault.drain_pending_flushes()?;
        status_after_commits = vault.flush_status();
        println!("  drain took {} ms", drain_started.elapsed().as_millis());
        println!(
            "  flush_status: written={} failed={} outstanding={} max_depth={} capacity_waits={} capacity_wait_us={}",
            status_after_commits.written,
            status_after_commits.failed,
            status_after_commits.outstanding,
            status_after_commits.max_depth,
            status_after_commits.capacity_waits,
            status_after_commits.capacity_wait_us
        );
        vault.checkpoint()?;
    }
    let flush_ssts_after = count_flush_ssts(&vault_dir)?;
    let sst_delta = flush_ssts_after - flush_ssts_before;

    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let max = *samples.last().unwrap_or(&0);
    println!("  caller-visible commit latency: p50={p50} us  max={max} us  over {COMMITS} commits");
    // The acceptance criterion, read off the commit's own event rather than
    // inferred from the wall clock above.
    let (sst_total, sst_max, sst_events) = SST_WRITE.snapshot();
    println!(
        "  mvcc_sst_write_unlocked_us ON THE COMMITTING THREAD: total={sst_total} us  max={sst_max} us  over {sst_events} reported commit(s)"
    );
    // Before #1951 this field was 20,000-67,000 us on a single commit; it is
    // now an enqueue. A generous ceiling: the point is the order of magnitude,
    // not a micro-benchmark.
    const SST_WRITE_CEILING_US: u64 = 2_000;
    if sst_events == 0 {
        // The commit stage event is gated on being an outlier against this
        // vault's own EWMA, and these batches are deliberately uniform, so none
        // qualifies. Said plainly rather than reported as a pass: this harness
        // does not measure the stage field. `mvcc_commit_split_fsv`, whose
        // dense-vector commits do trip that gate, is where that measurement
        // lives.
        println!(
            "  [NOT MEASURED HERE] no commit was an outlier against its own EWMA, so the stage"
        );
        println!(
            "                      event never fired. See mvcc_commit_split_fsv for the stage split;"
        );
        println!(
            "                      this harness establishes the two-counter agreement below instead."
        );
    } else if sst_max > SST_WRITE_CEILING_US {
        return Err(format!(
            "a commit still paid {sst_max} us of SST write on its own thread (ceiling {SST_WRITE_CEILING_US} us): the handoff is not happening"
        )
        .into());
    } else {
        println!(
            "  [PASS] the SST write is off the committing thread (max {sst_max} us, was 20,000-67,000 us)"
        );
    }
    println!(
        "  flush-*.sst on disk: before={flush_ssts_before} after={flush_ssts_after} delta={sst_delta}"
    );
    println!("  flusher written    : {}", status_after_commits.written);
    if status_after_commits.failed != 0 {
        return Err(format!(
            "{} background flush(es) failed; every latency number above describes a broken writer",
            status_after_commits.failed
        )
        .into());
    }
    if u64::try_from(sst_delta).unwrap_or(u64::MAX) != status_after_commits.written {
        return Err(format!(
            "the flusher claims {} SST(s) written but the filesystem grew by {sst_delta}: a \
             deferred write was dropped",
            status_after_commits.written
        )
        .into());
    }
    println!("  [PASS] the flusher's counter and the filesystem agree exactly");

    // ---- Phase C: reopen and read back every key --------------------------
    println!("\n=== Phase C: reopen (the router is the source of truth) ===");
    {
        let vault = open_vault(&vault_dir, vault_id)?;
        let mut absent = Vec::new();
        for key in &all_keys {
            match vault.read_cf_latest(ColumnFamily::Kv, key)? {
                Some(value) if value.len() == VALUE_BYTES => {}
                _ => absent.push(String::from_utf8_lossy(key).into_owned()),
            }
        }
        println!(
            "  post-reopen readback of {} keys: absent={} {}",
            all_keys.len(),
            absent.len(),
            if absent.is_empty() {
                "(all present at full length)".to_owned()
            } else {
                format!("{:?}", &absent[..absent.len().min(6)])
            }
        );
        if !absent.is_empty() {
            return Err(format!(
                "{} key(s) did not survive the reopen: the background flusher lost rows",
                absent.len()
            )
            .into());
        }
        println!("  [PASS] every deferred row survived a restart");
    }

    // ---- Phase D: crash during deferral -----------------------------------
    //
    // The scenario #1951's analysis predicted safe and nobody had run: seals
    // outstanding, and the process goes away without draining. Constructed by
    // committing and then leaking the vault handle, so no Drop, no drain, no
    // checkpoint -- strictly harsher than a clean shutdown and the same
    // physical state a kill leaves.
    println!("\n=== Phase D: seals outstanding, vault abandoned without a drain ===");
    let mut crash_keys = Vec::new();
    let outstanding_at_abandon;
    {
        let vault = open_vault(&vault_dir, vault_id)?;
        for commit in 0..COMMITS {
            let rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)> = (0..ROWS_PER_COMMIT)
                .map(|row| {
                    let key = crash_key(commit, row);
                    crash_keys.push(key.clone());
                    (ColumnFamily::Kv, key, vec![b'D'; VALUE_BYTES])
                })
                .collect();
            vault.write_cf_batch(rows)?;
        }
        outstanding_at_abandon = vault.flush_status().outstanding;
        println!("  outstanding seals at the moment of abandonment = {outstanding_at_abandon}");
        // Leak: no Drop runs, so the flush thread is never joined and the queue
        // is never drained. This is the crash.
        std::mem::forget(vault);
    }
    {
        let vault = open_vault(&vault_dir, vault_id)?;
        let mut absent = 0_usize;
        for key in &crash_keys {
            if vault.read_cf_latest(ColumnFamily::Kv, key)?.is_none() {
                absent += 1;
            }
        }
        println!(
            "  post-crash reopen: {} of {} keys present, absent={absent}",
            crash_keys.len() - absent,
            crash_keys.len()
        );
        if absent != 0 {
            return Err(format!(
                "{absent} key(s) were lost when the vault was abandoned with seals in flight: the \
                 WAL does NOT cover the deferral window, and #1951's analysis was wrong"
            )
            .into());
        }
        println!("  [PASS] the WAL covered every undrained seal");
    }

    // ---- Phase E: back-pressure is a wait, not an error --------------------
    println!("\n=== Phase E: back-pressure past the bound ===");
    {
        let vault = Arc::new(open_vault(&vault_dir, vault_id)?);
        let started = Instant::now();
        // A single writer can never reach the bound: each commit is WAL-bound
        // at ~16 ms while the flusher writes an SST in ~3 ms, so the producer
        // is slower than the consumer by construction. Saturating it needs
        // CONCURRENCY, the same lesson #1936 recorded — a serial burst cannot
        // queue behind a single writer thread.
        //
        // If the bound errored instead of waiting, one of these threads gets
        // CALYX_ASTER_ROUTER_SEALED_MEMTABLE_BACKLOG and the run fails.
        const PRESSURE_THREADS: usize = 8;
        let mut writers = Vec::with_capacity(PRESSURE_THREADS);
        for thread in 0..PRESSURE_THREADS {
            let vault = Arc::clone(&vault);
            writers.push(std::thread::spawn(move || -> Result<Vec<Vec<u8>>, String> {
                let mut keys = Vec::new();
                for commit in 0..COMMITS {
                    let rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)> = (0..ROWS_PER_COMMIT)
                        .map(|row| {
                            let key = pressure_key(thread * COMMITS + commit, row);
                            keys.push(key.clone());
                            (ColumnFamily::Kv, key, vec![b'P'; VALUE_BYTES])
                        })
                        .collect();
                    vault.write_cf_batch(rows).map_err(|error| {
                        format!(
                            "writer {thread} commit {commit} failed under back-pressure with [{}]: \
                             the bound is still an error rather than a bounded wait (#1951 ask 3): {error}",
                            error.code
                        )
                    })?;
                }
                Ok(keys)
            }));
        }
        let mut pressure_keys = Vec::new();
        for writer in writers {
            let keys = writer
                .join()
                .map_err(|_| "a back-pressure writer thread panicked")?
                .map_err(|error| -> Box<dyn Error> { error.into() })?;
            pressure_keys.extend(keys);
        }
        vault.drain_pending_flushes()?;
        let status = vault.flush_status();
        println!(
            "  {} concurrent commits in {} ms; written={} failed={} max_depth={} capacity_waits={} capacity_wait_us={}",
            PRESSURE_THREADS * COMMITS,
            started.elapsed().as_millis(),
            status.written,
            status.failed,
            status.max_depth,
            status.capacity_waits,
            status.capacity_wait_us
        );
        if status.failed != 0 {
            return Err(format!("{} flush(es) failed under back-pressure", status.failed).into());
        }
        // Say what was actually exercised. A phase that reports PASS for a
        // throttle that never engaged is worse than one that reports nothing,
        // because it reads as evidence.
        if status.max_depth == 0 {
            return Err(
                "the queue never had depth, so nothing about the flusher was exercised".into(),
            );
        }
        let mut absent = 0_usize;
        for key in &pressure_keys {
            if vault.read_cf_latest(ColumnFamily::Kv, key)?.is_none() {
                absent += 1;
            }
        }
        println!(
            "  readback under pressure: {} of {} present",
            pressure_keys.len() - absent,
            pressure_keys.len()
        );
        if absent != 0 {
            return Err(format!("{absent} key(s) lost under back-pressure").into());
        }
        if status.capacity_waits > 0 {
            println!(
                "  [PASS] the bound engaged {} time(s) for {} us total and threw no error: it is a \
                 bounded wait, not a failed commit",
                status.capacity_waits, status.capacity_wait_us
            );
        } else {
            println!(
                "  [NOT EXERCISED] the queue peaked at depth {} against a bound of 8, so the wait \
                 path never ran. What this phase DOES establish: {} concurrent commits across {} \
                 threads produced {} SSTs with 0 failures and 0 lost rows.",
                status.max_depth,
                PRESSURE_THREADS * COMMITS,
                PRESSURE_THREADS,
                status.written
            );
        }
    }

    println!("\n=== verdict ===");
    println!(
        "  mvcc_sst_write_unlocked_us on the caller: max {sst_max} us over {sst_events} reported commit(s)"
    );
    println!("  caller-visible commit p50 = {p50} us, max = {max} us (WAL-bound, not SST-bound)");
    println!("  every deferred SST written, counted twice and agreeing");
    println!("  every row survived a clean reopen AND an abandoned one");
    Ok(())
}

fn open_vault(
    vault_dir: &Path,
    vault_id: VaultId,
) -> Result<AsterVault<calyx_core::SystemClock>, Box<dyn Error>> {
    Ok(AsterVault::open(
        vault_dir,
        vault_id,
        b"fsv-1951-salt".to_vec(),
        VaultOptions::default(),
    )?)
}

/// Counts `flush-*.sst` files under every CF directory: the filesystem's own
/// account of how many router flushes actually happened, independent of any
/// counter the process keeps.
fn count_flush_ssts(vault_dir: &Path) -> Result<usize, Box<dyn Error>> {
    let cf_root = vault_dir.join("cf");
    if !cf_root.exists() {
        return Ok(0);
    }
    let mut total = 0;
    for entry in std::fs::read_dir(&cf_root)? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&dir)? {
            let path = file?.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if name.starts_with("flush-") && name.ends_with(".sst") {
                total += 1;
            }
        }
    }
    Ok(total)
}

fn flush_key(commit: usize, row: usize) -> Vec<u8> {
    format!("fsv-1951/a/{commit:04}/{row:02}").into_bytes()
}

fn crash_key(commit: usize, row: usize) -> Vec<u8> {
    format!("fsv-1951/d/{commit:04}/{row:02}").into_bytes()
}

fn pressure_key(commit: usize, row: usize) -> Vec<u8> {
    format!("fsv-1951/e/{commit:04}/{row:02}").into_bytes()
}
