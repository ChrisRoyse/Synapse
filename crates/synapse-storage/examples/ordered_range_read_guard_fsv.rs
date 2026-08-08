//! Manual FSV for the #2060 residual: the ordinary ordered-range read
//! (`scan_cf`) no longer takes a wide MVCC row-table guard.
//!
//! # What this settles, and why the previous FSVs could not
//!
//! #2060's earlier rounds fixed four maintenance sites and the panel-coverage
//! census, and measured each of them decisively. Both rounds then closed on the
//! *same* residual: `scan_cf_range_latest` still could not reach zero
//! over-budget holds, because the public `scan_cf` trait read went straight to
//! `scan_kv_range_latest` — one guard hold across a whole namespace, at 25-30 ms
//! per hold under ordinary MCP tool load, with a 224 ms worst hold. The second
//! FSV chased all 38 of that window's over-budget holds and found every one of
//! them on this path. Until it is rehomed the site's census cannot serve as the
//! acceptance meter the issue defined.
//!
//! This harness measures exactly that site, in isolation, with nothing else
//! touching the vault:
//!
//! | phase | construction | what it settles |
//! |---|---|---|
//! | A | seed `--rows` rows into one CF, then `scan_cf` it | the read's holds land on the PAGED site, and the wide site gains none |
//! | B | compare the rows returned against the rows written | the paged fold returns the same answer, key for key and byte for byte |
//! | C | scan a prefix sub-range | `scan_cf_prefix` rides the same pager |
//!
//! Phase B is not decoration. The cheapest way to make a scan fast is to return
//! less of it, so a hold-time improvement with no output check is not evidence.
//!
//! ```text
//! cargo run --release -p synapse-storage --example ordered_range_read_guard_fsv -- <empty-scratch-dir> [rows]
//! ```

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxMathBackend};
use synapse_storage::{Db, StorageBackendKind, cf};

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_ROWS: usize = 60_000;
const VALUE_BYTES: usize = 192;

/// The row guard's own budget. Not raised, not consulted for control flow — this
/// is only the number the census reports `over_budget_holds` against, restated
/// here so the verdict below is readable without opening the census.
const ROW_GUARD_BUDGET_US: u64 = 25_000;

/// The wide site every maintenance path was moved off. It must gain **no** holds
/// from an ordered range read.
const WIDE_SITE: &str = "scan_cf_range_latest";
/// The paged site those holds must appear on instead.
const PAGED_SITE: &str = "scan_cf_range_page_latest";

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct SiteSample {
    holds: u64,
    max_held_us: u64,
    over_budget_holds: u64,
}

fn census(db: &Db) -> Result<BTreeMap<String, SiteSample>, Box<dyn Error>> {
    let status = db.calyx_vault_status()?;
    Ok(status
        .row_guard_census
        .iter()
        .map(|entry| {
            (
                entry.site.clone(),
                SiteSample {
                    holds: entry.holds,
                    max_held_us: entry.max_held_us,
                    over_budget_holds: entry.over_budget_holds,
                },
            )
        })
        .collect())
}

fn delta(
    before: &BTreeMap<String, SiteSample>,
    after: &BTreeMap<String, SiteSample>,
    site: &str,
) -> SiteSample {
    let before = before.get(site).copied().unwrap_or_default();
    let after = after.get(site).copied().unwrap_or_default();
    SiteSample {
        holds: after.holds.saturating_sub(before.holds),
        max_held_us: after.max_held_us,
        over_budget_holds: after
            .over_budget_holds
            .saturating_sub(before.over_budget_holds),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: ordered_range_read_guard_fsv <empty-scratch-dir> [rows]")?;
    let rows: usize = match args.next() {
        Some(value) => value.parse()?,
        None => DEFAULT_ROWS,
    };
    std::fs::create_dir_all(&root)?;
    println!("ordered_range_read_guard_fsv  (#2060 residual)");
    println!("  root = {}  rows = {rows}", root.display());

    // A scratch FSV vault on a CUDA host must say which math it wants: `Auto`
    // refuses rather than silently running CPU kernels on a GPU box. Nothing
    // measured here touches math.
    let mut calyx_config = SynapseCalyxConfig::from_vault_dir(root.clone());
    calyx_config.tuning.math_backend = SynapseCalyxMathBackend::Cpu;
    let db = Db::open_with_resolved_calyx_config(
        &root,
        SCHEMA_VERSION,
        StorageBackendKind::default(),
        calyx_config,
    )?;

    // ---- seed -----------------------------------------------------------
    println!("\n=== seeding {rows} rows into {} ===", cf::CF_KV);
    let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(500);
    for index in 0..rows {
        let key = format!("fsv-2060/{index:09}").into_bytes();
        let mut value = vec![0_u8; VALUE_BYTES];
        value[..8].copy_from_slice(&(index as u64).to_be_bytes());
        batch.push((key.clone(), value.clone()));
        expected.insert(key, value);
        if batch.len() == 500 {
            db.put_batch(cf::CF_KV, batch.drain(..))?;
        }
    }
    if !batch.is_empty() {
        db.put_batch(cf::CF_KV, batch.drain(..))?;
    }
    println!("  seeded {} distinct keys", expected.len());

    // ---- Phase A: where the holds land ----------------------------------
    println!("\n=== Phase A: one whole-namespace scan_cf, measured on the guard census ===");
    let before = census(&db)?;
    let scan_started = std::time::Instant::now();
    let scanned = db.scan_cf(cf::CF_KV)?;
    let scan_ms = scan_started.elapsed().as_millis();
    let after = census(&db)?;

    let wide = delta(&before, &after, WIDE_SITE);
    let paged = delta(&before, &after, PAGED_SITE);
    println!(
        "  {WIDE_SITE:<26} holds +{:<7} over_budget +{:<5} lifetime max_held_us {}",
        wide.holds, wide.over_budget_holds, wide.max_held_us
    );
    println!(
        "  {PAGED_SITE:<26} holds +{:<7} over_budget +{:<5} lifetime max_held_us {}",
        paged.holds, paged.over_budget_holds, paged.max_held_us
    );
    println!("  scan wall clock: {scan_ms} ms for {} rows", scanned.len());

    if wide.holds != 0 {
        return Err(format!(
            "the ordered range read still took {} hold(s) on the wide site {WIDE_SITE}: it has not \
             been rehomed onto the pager",
            wide.holds
        )
        .into());
    }
    if paged.holds == 0 {
        return Err(format!(
            "the scan took no holds on {PAGED_SITE} either, so this run measured nothing; the read \
             did not go through the pager at all"
        )
        .into());
    }
    if paged.over_budget_holds != 0 {
        return Err(format!(
            "{} paged hold(s) exceeded the {ROW_GUARD_BUDGET_US} us row-guard budget; the pager is \
             not bounding the critical section",
            paged.over_budget_holds
        )
        .into());
    }
    println!(
        "  [PASS] every hold landed on the paged site, none on the wide one, none over budget"
    );

    // ---- Phase B: the answer is unchanged --------------------------------
    println!("\n=== Phase B: the paged fold returns the same rows ===");
    if scanned.len() != expected.len() {
        return Err(format!(
            "scan_cf returned {} rows for {} written: the paged fold lost or duplicated rows",
            scanned.len(),
            expected.len()
        )
        .into());
    }
    let mut previous: Option<&[u8]> = None;
    for (key, value) in &scanned {
        if let Some(previous) = previous
            && previous >= key.as_slice()
        {
            return Err("scan_cf returned keys that are not strictly increasing".into());
        }
        previous = Some(key);
        match expected.get(key) {
            Some(written) if written == value => {}
            Some(_) => {
                return Err(format!(
                    "scan_cf returned a different value for key {}",
                    String::from_utf8_lossy(key)
                )
                .into());
            }
            None => {
                return Err(format!(
                    "scan_cf returned a key that was never written: {}",
                    String::from_utf8_lossy(key)
                )
                .into());
            }
        }
    }
    println!(
        "  [PASS] {} keys, strictly increasing, every value byte-identical to what was written",
        scanned.len()
    );

    // ---- Phase C: the prefix sub-range rides the same pager ---------------
    println!("\n=== Phase C: scan_cf_prefix over a sub-range ===");
    let before = census(&db)?;
    let prefix = b"fsv-2060/00000".to_vec();
    let prefixed = db.scan_cf_prefix(cf::CF_KV, &prefix)?;
    let after = census(&db)?;
    let wide = delta(&before, &after, WIDE_SITE);
    let paged = delta(&before, &after, PAGED_SITE);
    let expected_prefixed = expected
        .keys()
        .filter(|key| key.starts_with(&prefix))
        .count();
    println!(
        "  prefix rows: {} (expected {expected_prefixed});  {WIDE_SITE} holds +{}  {PAGED_SITE} holds +{}",
        prefixed.len(),
        wide.holds,
        paged.holds
    );
    if prefixed.len() != expected_prefixed {
        return Err(format!(
            "scan_cf_prefix returned {} rows, expected {expected_prefixed}",
            prefixed.len()
        )
        .into());
    }
    if wide.holds != 0 {
        return Err(format!(
            "scan_cf_prefix still took {} hold(s) on {WIDE_SITE}",
            wide.holds
        )
        .into());
    }
    println!("  [PASS] the prefix read is paged too, and returns the same sub-range");

    // ---- Phase D: what the narrowing is actually for ---------------------
    //
    // Guard-hold numbers are a proxy. The thing #2060 is about is that every
    // vault commit serialises behind the durable commit lock, and a wide row
    // guard held across a whole namespace stalls all of them. So the payload
    // measurement is commit latency with a whole-CF scan interleaved against
    // commit latency without one: if the scan still held a wide guard, the
    // writer's tail would move when the scans start, and it must not.
    println!("\n=== Phase D: commit latency with and without a CONCURRENT whole-CF scan ===");
    let db = std::sync::Arc::new(db);
    let quiet = commit_latency_samples(&db, 60, "quiet")?;
    let keep_scanning = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let scanner_db = std::sync::Arc::clone(&db);
    let scanner_flag = std::sync::Arc::clone(&keep_scanning);
    let scanner = std::thread::spawn(move || -> Result<usize, String> {
        let mut scans = 0_usize;
        while scanner_flag.load(std::sync::atomic::Ordering::Relaxed) {
            scanner_db
                .scan_cf(cf::CF_KV)
                .map_err(|error| error.to_string())?;
            scans += 1;
        }
        Ok(scans)
    });
    let under_scan = commit_latency_samples(&db, 60, "under_scan")?;
    keep_scanning.store(false, std::sync::atomic::Ordering::Relaxed);
    let scans = scanner
        .join()
        .map_err(|_| "the concurrent scanner thread panicked")?
        .map_err(|error| -> Box<dyn Error> { error.into() })?;
    println!(
        "  commit latency, no scanner running                : p50={} us  p99={} us  max={} us",
        quiet.0, quiet.1, quiet.2
    );
    println!(
        "  commit latency, {scans} concurrent whole-CF scans : p50={} us  p99={} us  max={} us",
        under_scan.0, under_scan.1, under_scan.2
    );
    println!(
        "  (before this change each of those scans took ONE row-guard hold across the whole\n   \
         namespace -- 25-30 ms typical and 224 ms worst on the live daemon -- and every commit in\n   \
         the window waited behind it.)"
    );

    println!("\n=== verdict ===");
    println!(
        "  scan_cf: 0 wide holds, {} paged holds, 0 over budget",
        paged.holds
    );
    println!("  output parity: exact, key for key and byte for byte");
    Ok(())
}

/// Times `samples` single-row commits and returns (p50, p99, max) microseconds.
fn commit_latency_samples(
    db: &Db,
    samples: usize,
    label: &str,
) -> Result<(u64, u64, u64), Box<dyn Error>> {
    let mut measured = Vec::with_capacity(samples);
    for index in 0..samples {
        let key = format!("fsv-2060-commit/{label}/{index:06}").into_bytes();
        let started = std::time::Instant::now();
        db.put_batch(cf::CF_KV, [(key, vec![7_u8; 64])])?;
        measured.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    measured.sort_unstable();
    let p50 = measured[measured.len() / 2];
    let p99 = measured[(measured.len() * 99).div_ceil(100).saturating_sub(1)];
    let max = *measured.last().unwrap_or(&0);
    Ok((p50, p99, max))
}
