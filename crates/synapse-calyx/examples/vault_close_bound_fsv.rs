//! Manual FSV for #2100: where the vault close actually spends its time, and
//! whether close intent is able to fence maintenance out of its way.
//!
//! ## The reading this harness corrects
//!
//! #2100 reads the production evidence as "the close stalled behind the durable
//! commit lock". The daemon logs falsify that directly:
//!
//! * failing deploy, `daemon-stderr-gen1-20260807173656.log` — the last
//!   `CALYX_ASTER_DURABLE_COMMIT_LOCK_SLOW` before the flush reports
//!   `wait_ms=0 hold_ms=1478`. The lock was held 1.5 s, not 87.
//! * the *successful* close one generation earlier,
//!   `daemon-stderr-gen1-20260807172913.log` — **63 seconds with not one log
//!   line** between `SYNAPSE_CALYX_VAULT_FLUSHED` (22:32:17.529) and
//!   `SYNAPSE_CALYX_VAULT_LINEAGE_CLOSE_RECORDED` (22:33:20.516).
//!
//! The only statement between those two lines was `drop(vault)`. That is the
//! region this harness measures, and the margin it eats is what #2100 is really
//! about: the daemon's own HTTP shutdown watchdog is 90 s from arming, the
//! successful close disarmed it at **79.7 s**, and the failing one was killed by
//! the drain's 90 s exit-wait 0.65 s before that watchdog would have fired.
//!
//! ## Phases
//!
//! | phase | construction | what it settles |
//! |---|---|---|
//! | A | ingest, then close with the per-step teardown report | which step of the close carries the cost |
//! | B | the same at 2x MVCC versions | whether the teardown is proportional to resident versions |
//! | C | hold the maintenance guard, then close | close intent preempts the maintenance admission instead of queueing behind it |
//!
//! Phase C is the acceptance test for #2100 ask 2. It fails the run if the
//! close's fan-out admission takes anywhere near the periodic lane's 120 s
//! budget, and it separately proves that a maintenance pass *requested after*
//! the close is refused rather than admitted.
//!
//! ```text
//! cargo run --release -p synapse-calyx --example vault_close_bound_fsv -- <empty-scratch-dir> [keys] [versions]
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::{AsterVault, VaultOptions, VaultTeardownReport};
use calyx_core::VaultId;

const DEFAULT_KEYS: usize = 100_000;
const ROWS_PER_COMMIT: usize = 500;
const VALUE_BYTES: usize = 256;

/// The periodic lane's admission budget. The close must not spend anything like
/// this; if it does, the fence is not working.
const PERIODIC_LANE_BUDGET_MS: u128 = 120_000;
/// What phase C requires of the close's fan-out admission.
const CLOSE_ADMISSION_CEILING_MS: u128 = 15_000;

struct CloseTiming {
    keys: usize,
    versions: usize,
    ingest_ms: u128,
    fanout_ms: u128,
    flush_ms: u128,
    teardown: VaultTeardownReport,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next().map(PathBuf::from) else {
        return Err("usage: vault_close_bound_fsv <empty-scratch-dir> [keys] [versions]".into());
    };
    let keys: usize = match args.next() {
        Some(value) => value.parse()?,
        None => DEFAULT_KEYS,
    };
    let versions: usize = match args.next() {
        Some(value) => value.parse()?,
        None => 2,
    };
    std::fs::create_dir_all(&root)?;
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();
    println!("vault_close_bound_fsv  (#2100)\nroot = {}", root.display());

    println!("\n=== Phase A/B: which step of the close carries the cost ===");
    let first = measure_close(&root.join("vault-a"), keys, versions, 1)?;
    let second = measure_close(&root.join("vault-b"), keys, versions * 2, 2)?;

    println!("\n--- teardown split ---");
    for timing in [&first, &second] {
        println!(
            "  keys={:<7} versions/key={:<3} ingest={:>6} ms  fanout={:>5} ms  flush={:>5} ms  \
             TEARDOWN total={:>6} ms (flush_drain={} rows={} durable={} residual={}, \
             sealed_outstanding_before={})",
            timing.keys,
            timing.versions,
            timing.ingest_ms,
            timing.fanout_ms,
            timing.flush_ms,
            timing.teardown.total_ms,
            timing.teardown.flush_drain_ms,
            timing.teardown.rows_teardown_ms,
            timing.teardown.durable_teardown_ms,
            timing.teardown.residual_teardown_ms,
            timing.teardown.sealed_outstanding_before,
        );
    }
    let ratio = if first.teardown.total_ms == 0 {
        f64::INFINITY
    } else {
        second.teardown.total_ms as f64 / first.teardown.total_ms as f64
    };
    println!(
        "  teardown scaling across a 2x MVCC-version increase at constant key count: {ratio:.2}x"
    );
    println!(
        "  (~2x means the teardown is proportional to resident VERSIONS -- in-memory row/version\n   \
         chain release -- not to I/O and not to key count. Before #2100 this whole line was one\n   \
         unlogged `drop(vault)` and none of it could be attributed at all.)"
    );

    // ---- Phase C: close intent preempts maintenance admission --------------
    println!("\n=== Phase C: close intent fences the maintenance admission (#2100 ask 2) ===");
    let admission_ms = measure_close_preemption(&root.join("vault-c"))?;
    println!(
        "  close fan-out admission took {admission_ms} ms against the periodic lane's \
         {PERIODIC_LANE_BUDGET_MS} ms budget"
    );
    if admission_ms > CLOSE_ADMISSION_CEILING_MS {
        return Err(format!(
            "the close's fan-out admission took {admission_ms} ms (ceiling \
             {CLOSE_ADMISSION_CEILING_MS} ms): close intent is NOT preempting the maintenance \
             admission and the close can still queue behind fan-out work"
        )
        .into());
    }
    println!(
        "  [PASS] the close was admitted inside its own bounded budget rather than the periodic \
         lane's"
    );
    Ok(())
}

fn measure_close(
    vault_dir: &Path,
    keys: usize,
    versions: usize,
    seed: u64,
) -> Result<CloseTiming, Box<dyn Error>> {
    println!(
        "\n--- close timing at {keys} keys x {versions} version(s): {} ---",
        vault_dir.display()
    );
    let vault = open_vault(vault_dir)?;
    let ingest_started = Instant::now();
    let written = ingest(&vault, keys, versions, seed)?;
    let ingest_ms = ingest_started.elapsed().as_millis();
    println!(
        "  ingested {written} row-versions over {keys} keys in {ingest_ms} ms (latest_seq={})",
        vault.latest_seq()
    );

    let fanout_started = Instant::now();
    let attempts = vault.compact_native_fanout_for_close("fsv_close_bound")?;
    let fanout_ms = fanout_started.elapsed().as_millis();
    println!(
        "  compact_native_fanout_for_close: {fanout_ms} ms (admitted={}, {} attempt(s))",
        attempts.is_some(),
        attempts.map_or(0, |results| results.len())
    );

    let flush_started = Instant::now();
    vault.flush()?;
    let flush_ms = flush_started.elapsed().as_millis();
    println!("  flush: {flush_ms} ms");

    let teardown = vault.close_teardown("fsv_close_bound");
    teardown.verdict()?;
    println!(
        "  close_teardown: total={} ms (flush_drain={} rows={} durable={} residual={})",
        teardown.total_ms,
        teardown.flush_drain_ms,
        teardown.rows_teardown_ms,
        teardown.durable_teardown_ms,
        teardown.residual_teardown_ms
    );

    Ok(CloseTiming {
        keys,
        versions,
        ingest_ms,
        fanout_ms,
        flush_ms,
        teardown,
    })
}

/// Holds the native-compaction maintenance guard on another thread, then closes.
///
/// Constructed so the close arrives while a pass is genuinely active — which is
/// the #2100 condition — and so a *second* maintenance request is made after the
/// close intent is declared, which must be refused rather than admitted.
fn measure_close_preemption(vault_dir: &Path) -> Result<u128, Box<dyn Error>> {
    let vault = Arc::new(open_vault(vault_dir)?);
    ingest(&vault, 20_000, 1, 3)?;

    // A real periodic-lane pass, running on its own thread and holding the
    // maintenance guard for a while.
    let holder_vault = Arc::clone(&vault);
    let (holder_started_tx, holder_started_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || -> Result<(), String> {
        let _ = holder_started_tx.send(());
        for _ in 0..8 {
            match holder_vault.compact_native_fanout_once() {
                Ok(_) => {}
                // The whole point: once the close fence is up this lane is
                // refused, and that is the expected terminal state here.
                Err(error) if error.code == "CALYX_ASTER_VAULT_CLOSING" => {
                    println!(
                        "  concurrent periodic lane refused after close intent: [{}] {}",
                        error.code, error.message
                    );
                    return Ok(());
                }
                Err(error) if error.code == "CALYX_ASTER_NATIVE_COMPACTION_BUSY" => {}
                Err(error) => return Err(format!("[{}] {}", error.code, error.message)),
            }
        }
        Ok(())
    });
    holder_started_rx.recv()?;
    std::thread::sleep(Duration::from_millis(200));

    let admission_started = Instant::now();
    let admitted = vault.compact_native_fanout_for_close("fsv_close_preemption")?;
    let admission_ms = admission_started.elapsed().as_millis();
    println!(
        "  close fan-out admission: admitted={} in {admission_ms} ms",
        admitted.is_some()
    );

    holder
        .join()
        .map_err(|_| "the concurrent maintenance thread panicked")?
        .map_err(|error| -> Box<dyn Error> {
            format!("the concurrent maintenance thread failed unexpectedly: {error}").into()
        })?;

    // A maintenance request made AFTER close intent must be refused, not queued.
    match vault.compact_native_fanout_once() {
        Err(error) if error.code == "CALYX_ASTER_VAULT_CLOSING" => {
            println!(
                "  [PASS] a post-close-intent maintenance admission was refused: {}",
                error.code
            );
        }
        Err(error) => {
            return Err(format!(
                "a post-close-intent maintenance admission failed with [{}] instead of \
                 CALYX_ASTER_VAULT_CLOSING: {}",
                error.code, error.message
            )
            .into());
        }
        Ok(_) => {
            return Err(
                "a maintenance pass was ADMITTED after close intent was declared: the fence is not \
                 in force and the close can still queue behind fan-out work"
                    .into(),
            );
        }
    }

    let vault = Arc::try_unwrap(vault)
        .map_err(|_| "the preemption harness still holds an extra vault owner")?;
    vault.flush()?;
    let teardown = vault.close_teardown("fsv_close_preemption");
    teardown.verdict()?;
    println!(
        "  close_teardown after preemption: total={} ms",
        teardown.total_ms
    );
    Ok(admission_ms)
}

fn ingest(
    vault: &AsterVault<calyx_core::SystemClock>,
    keys: usize,
    versions: usize,
    seed: u64,
) -> Result<usize, Box<dyn Error>> {
    let mut written = 0_usize;
    for version in 0..versions {
        let mut done = 0_usize;
        while done < keys {
            let batch = ROWS_PER_COMMIT.min(keys - done);
            let rows: Vec<(ColumnFamily, Vec<u8>, Vec<u8>)> = (0..batch)
                .map(|row| {
                    let index = done + row;
                    (
                        ColumnFamily::Kv,
                        format!("fsv-2100/{seed}/{index:09}").into_bytes(),
                        vec![b'A' + u8::try_from(version % 26).unwrap_or(0); VALUE_BYTES],
                    )
                })
                .collect();
            vault.write_cf_batch(rows)?;
            done += batch;
            written += batch;
        }
    }
    Ok(written)
}

fn open_vault(vault_dir: &Path) -> Result<AsterVault<calyx_core::SystemClock>, Box<dyn Error>> {
    let vault_id: VaultId = "01JZ2100BFSV2100BFSV210000"
        .parse()
        .map_err(|error| format!("fixed harness vault id is not a valid ULID: {error:?}"))?;
    Ok(AsterVault::open(
        vault_dir,
        vault_id,
        b"fsv-2100-salt".to_vec(),
        VaultOptions::default(),
    )?)
}
