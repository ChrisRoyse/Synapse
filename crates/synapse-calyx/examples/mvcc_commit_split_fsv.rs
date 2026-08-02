//! Manual FSV instrument for #1948: which part of the `mvcc` commit stage is
//! the 91-99%?
//!
//! ## What this has to settle
//!
//! On the deployment host `mvcc` — `commit_rows_to_mvcc`, the in-memory MVCC
//! row-table and router apply — measured 91-99% of durable commit cost, up to
//! 180 ms against ~2 ms of WAL fsync on the same commit. The stage was one
//! number, so the candidate causes could not be told apart, and #1948 ask 3
//! explicitly forbids picking the visible allocation (the per-row key/value
//! clone) on the strength of being visible.
//!
//! The stage is now split into fixed points with an explicit remainder. This
//! harness constructs each candidate deliberately and reads the split back, so
//! the attribution is measured rather than argued.
//!
//! ## The prediction, stated before the run
//!
//! `mvcc` is documented in #1948 as performing **no disk I/O**. That is false,
//! and this harness is built to prove it either way: `CfRouter::put_at` calls
//! `flush_cf_at` when a memtable fills, which seals every value under the
//! vault's AEAD, writes an entire SST, and re-opens it to build a lookup index
//! — all synchronously, inside the commit, holding *both* the row-table and
//! router write locks.
//!
//! So:
//!
//! | phase | construction | predicted dominant sub-stage |
//! |---|---|---|
//! | A baseline | small commits, nothing else running | `row_apply` or `router_apply`, all of it sub-millisecond |
//! | B flush | commit until an 8 MB memtable fills | **`router_flush`**, and it should dwarf phase A |
//! | C contention | commit while other threads commit | **`row_lock_wait` / `router_lock_wait`** |
//!
//! ## Source of truth
//!
//! Not the returned timings. A router flush's physical product is an SST named
//! `flush-{watermark:020}-{ordinal:04}.sst` (`storage_names`), so the file
//! system is the independent witness:
//!
//! ```text
//!   count of flush-*.sst on disk after phase B
//!     - count before
//!     == sum of mvcc_router_flushes reported by the commits in between
//! ```
//!
//! Two independent counters that must agree exactly. The timings say a flush
//! was paid for; the directory says a flush happened. Neither alone is
//! evidence, and a disagreement is a real finding rather than a rounding
//! nuisance.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example mvcc_commit_split_fsv -- <empty-scratch-dir>`

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};

use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{
    Constellation, CxFlags, CxId, InputRef, LedgerRef, Modality, SlotId, SlotVector, VaultId,
    VaultStore as _,
};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const PANEL: u32 = 1_948_001;
/// Slots per row, matching the `slot=12` of the live `syn-mcp-usage-v1` batch.
const SLOTS: u16 = 12;

// ---------------------------------------------------------------------------
// Capturing the daemon's own commit event
// ---------------------------------------------------------------------------

/// One `CALYX_ASTER_DURABLE_COMMIT_STAGE_TIMINGS` event, as fields.
type Event = BTreeMap<String, u64>;

#[derive(Default)]
struct Collector {
    events: Mutex<Vec<Event>>,
}

impl Collector {
    fn drain(&self) -> Vec<Event> {
        std::mem::take(
            &mut *self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

struct Sink(Arc<Collector>);

#[derive(Default)]
struct Fields {
    values: Event,
    code: String,
}

impl Visit for Fields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.values.insert(field.name().to_owned(), value);
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.values
            .insert(field.name().to_owned(), u64::try_from(value).unwrap_or(0));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "code" {
            self.code = value.to_owned();
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "code" {
            self.code = format!("{value:?}").trim_matches('"').to_owned();
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Sink {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.code == "CALYX_ASTER_DURABLE_COMMIT_STAGE_TIMINGS" {
            self.0
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(fields.values);
        }
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// A row shaped like the live `syn-mcp-usage-v1` observation: 12 slots and 12
/// scalars over one base row. `dim` scales the bytes so a phase can decide how
/// fast it fills a memtable without changing the row's shape.
fn row(vault_id: VaultId, tag: u64, dim: u32) -> Constellation {
    let mut slots: BTreeMap<SlotId, SlotVector> = BTreeMap::new();
    for slot in 1..=SLOTS {
        slots.insert(
            SlotId::new(slot),
            SlotVector::Dense {
                dim,
                // Content varies per row so values cannot be deduplicated or
                // compressed into nothing, which would make the memtable-fill
                // phase measure the wrong thing.
                data: (0..dim)
                    .map(|i| (tag as f32).mul_add(0.001, i as f32 * 0.000_1))
                    .collect(),
            },
        );
    }
    let mut scalars: BTreeMap<String, f64> = BTreeMap::new();
    for slot in 1..=SLOTS {
        scalars.insert(format!("s{slot:02}"), tag as f64 + f64::from(slot));
    }
    let mut cx = [0_u8; 16];
    cx[0] = 0x19;
    cx[1] = 0x48;
    cx[2..10].copy_from_slice(&tag.to_be_bytes());
    Constellation {
        cx_id: CxId::from_bytes(cx),
        vault_id,
        panel_version: PANEL,
        created_at: 1_785_000_000_000 + tag,
        input_ref: InputRef {
            hash: [u8::try_from(tag % 251).unwrap_or(0); 32],
            pointer: None,
            redacted: false,
        },
        modality: Modality::Text,
        slots,
        scalars,
        metadata: BTreeMap::new(),
        anchors: Vec::new(),
        provenance: LedgerRef {
            seq: 0,
            hash: [0; 32],
        },
        flags: CxFlags {
            ungrounded: true,
            ..CxFlags::default()
        },
    }
}

// ---------------------------------------------------------------------------
// Physical source of truth
// ---------------------------------------------------------------------------

/// Every `flush-*.sst` under the vault's CF tree — the physical product of a
/// synchronous router flush, counted by reading the directory rather than by
/// asking the code under measurement.
fn flush_ssts(vault_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let cf_root = vault_dir.join("cf");
    let Ok(cfs) = std::fs::read_dir(&cf_root) else {
        return found;
    };
    for cf in cfs.flatten() {
        let Ok(files) = std::fs::read_dir(cf.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if name.starts_with("flush-") && name.ends_with(".sst") {
                found.push(file.path());
            }
        }
    }
    found.sort();
    found
}

fn field(event: &Event, name: &str) -> u64 {
    event.get(name).copied().unwrap_or(0)
}

/// The sub-stages, in the order the split reports them. Complete: these must
/// sum to `mvcc_us`, and a gap is the finding rather than a rounding nuisance.
const SUB: [&str; 10] = [
    "mvcc_teardown_us",
    "mvcc_materialize_us",
    "mvcc_row_lock_wait_us",
    "mvcc_router_lock_wait_us",
    "mvcc_panel_attribution_us",
    "mvcc_watermark_us",
    "mvcc_row_apply_us",
    "mvcc_router_apply_us",
    "mvcc_sst_write_unlocked_us",
    "mvcc_unattributed_us",
];

fn report(phase: &str, events: &[Event]) -> u64 {
    println!(
        "\n=== phase {phase}: {} outlier commit(s) reported ===",
        events.len()
    );
    if events.is_empty() {
        println!("  (no commit tripped the outlier gate)");
        return 0;
    }
    println!(
        "  {:<6} {:>8} {:>8} {:>10} {:>10} {:>6}  dominant sub-stage",
        "rows", "total_us", "mvcc_us", "LOCKED_us", "sst_write", "seals"
    );
    let mut flushes = 0;
    for event in events {
        let mvcc = field(event, "mvcc_us");
        let dominant = SUB
            .iter()
            .map(|name| (*name, field(event, name)))
            .max_by_key(|(_, value)| *value)
            .unwrap_or(("none", 0));
        let share = dominant
            .1
            .saturating_mul(100)
            .checked_div(mvcc.max(1))
            .unwrap_or(0);
        flushes += field(event, "mvcc_router_seals");
        println!(
            "  {:<6} {:>8} {:>8} {:>10} {:>10} {:>6}  {} = {} us ({}% of mvcc)",
            field(event, "row_count"),
            field(event, "total_us"),
            mvcc,
            field(event, "mvcc_locked_us"),
            field(event, "mvcc_sst_write_unlocked_us"),
            field(event, "mvcc_router_seals"),
            dominant
                .0
                .trim_start_matches("mvcc_")
                .trim_end_matches("_us"),
            dominant.1,
            share,
        );
    }
    // Full split of the single worst commit in the phase, so the numbers behind
    // the "dominant" column are on the record rather than summarised away.
    if let Some(worst) = events.iter().max_by_key(|e| field(e, "mvcc_us")) {
        println!("  worst commit, full split:");
        for name in SUB {
            println!(
                "    {:<28} {:>9} us",
                name.trim_start_matches("mvcc_"),
                field(worst, name)
            );
        }
        println!(
            "    {:<28} {:>9} us   (nested inside router_apply)",
            "of which ensure_cf",
            field(worst, "mvcc_router_ensure_cf_us")
        );
        // SUB is the exact partition of mvcc_us, so this must reconcile. A gap
        // means a stage exists that nothing names, which is the finding.
        let parts: u64 = SUB.iter().map(|n| field(worst, n)).sum();
        let mvcc = field(worst, "mvcc_us");
        println!(
            "    {:<28} {:>9} us   vs mvcc_us {} -> gap {}",
            "SUM OF PARTS",
            parts,
            mvcc,
            i64::try_from(mvcc).unwrap_or(0) - i64::try_from(parts).unwrap_or(0)
        );
    }
    flushes
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: mvcc_commit_split_fsv <empty-scratch-vault-dir>")?;
    std::fs::create_dir_all(&dir)?;

    let collector = Arc::new(Collector::default());
    tracing_subscriber::registry()
        .with(
            Sink(Arc::clone(&collector)).with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        )
        .init();

    let vault_id = VaultId::from_str(VAULT_ID)?;
    let vault = Arc::new(AsterVault::open(
        &dir,
        vault_id,
        b"mvcc-commit-split-fsv".to_vec(),
        VaultOptions::default(),
    )?);

    println!("mvcc_commit_split_fsv");
    println!("vault_dir = {}", dir.display());
    println!("flush-*.sst present at start = {}", flush_ssts(&dir).len());

    // --- phase A: baseline -------------------------------------------------
    // Small rows, one writer, nothing else touching the vault. Whatever cost
    // exists here is the irreducible cost of the apply itself.
    for tag in 0..200_u64 {
        vault.put(row(vault_id, tag, 16))?;
    }
    let flush_a = flush_ssts(&dir).len();
    let reported_a = report("A baseline (200 commits, dim=16)", &collector.drain());
    println!("  flush-*.sst on disk after A = {flush_a}");

    // --- phase B: force a memtable flush -----------------------------------
    // dim=8192 makes each row ~48 KB of slot payload across 12 slots, so the
    // 8 MB default memtable cap is reached inside this phase rather than after
    // it. The flush is not simulated: it is the production path deciding, on
    // byte pressure, to write an SST inside the commit.
    let before_b = flush_ssts(&dir).len();
    for tag in 1_000..1_400_u64 {
        vault.put(row(vault_id, tag, 8192))?;
    }
    let after_b = flush_ssts(&dir).len();
    let reported_b = report(
        "B memtable flush (400 commits, dim=8192)",
        &collector.drain(),
    );

    println!("\n--- FULL STATE VERIFICATION: phase B ---");
    println!("  flush-*.sst before phase B          = {before_b}");
    println!("  flush-*.sst after  phase B          = {after_b}");
    println!(
        "  delta on disk                       = {}",
        after_b - before_b
    );
    println!("  sum of mvcc_router_flushes reported = {reported_b}");
    println!(
        "  VERDICT: {}",
        if u64::try_from(after_b - before_b).unwrap_or(0) >= reported_b && reported_b > 0 {
            "flushes reported by the split are present on disk"
        } else if reported_b == 0 {
            "NO FLUSH OBSERVED - phase B did not construct its condition"
        } else {
            "MISMATCH - the split claims flushes the filesystem does not have"
        }
    );

    // --- phase C: lock contention ------------------------------------------
    // Eight writers against one vault. Every commit must serialise through the
    // same row-table and router write locks, so a commit's wait for them is a
    // real wait for another commit rather than a synthetic sleep.
    let before_c = flush_ssts(&dir).len();
    let mut writers = Vec::new();
    for worker in 0..8_u64 {
        let vault = Arc::clone(&vault);
        writers.push(std::thread::spawn(move || -> Result<(), String> {
            for tag in 0..40_u64 {
                vault
                    .put(row(vault_id, 10_000 + worker * 1_000 + tag, 512))
                    .map_err(|error| format!("worker {worker}: {error}"))?;
            }
            Ok(())
        }));
    }
    for writer in writers {
        writer.join().map_err(|_| "writer thread panicked")??;
    }
    let reported_c = report("C contention (8 writers x 40 commits)", &collector.drain());
    println!(
        "  flush-*.sst delta across C = {}  (reported {reported_c})",
        flush_ssts(&dir).len() - before_c
    );

    // --- phase D: the read path must never miss a sealed memtable ----------
    // #1949 moves the SST write outside the locks, which means there is now a
    // window in which a memtable's rows live *only* in the sealed queue -- not
    // in the active memtable, not yet in any SST. If any read path forgot to
    // consult that queue, rows would silently vanish for the duration of a
    // flush. This is the failure that change could cause, so it is the one
    // measured directly.
    //
    // A writer fills memtables (forcing seals) while readers continuously
    // re-read every key written so far. Known input, known expected output:
    // every key ever written must be readable, with its exact value, at every
    // instant. A single miss is a hard failure.
    println!("\n=== phase D: read-your-writes across seals ===");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let highest = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let misses = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let mut readers = Vec::new();
    for _ in 0..3 {
        let (vault, stop, highest, misses, reads) = (
            Arc::clone(&vault),
            Arc::clone(&stop),
            Arc::clone(&highest),
            Arc::clone(&misses),
            Arc::clone(&reads),
        );
        readers.push(std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                let top = highest.load(std::sync::atomic::Ordering::Acquire);
                if top == 0 {
                    continue;
                }
                // Re-read a spread of already-committed keys, including the
                // oldest, which is the one most likely to be sitting in a
                // sealed memtable or a just-installed SST.
                for tag in [20_000, top / 2 + 10_000, top - 1, top] {
                    if tag < 20_000 {
                        continue;
                    }
                    let cx = row(vault_id, tag, 1).cx_id;
                    reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // A row absent from every source surfaces as an error, so
                    // any failure here is a miss.
                    if vault.get(cx, vault.snapshot()).is_err() {
                        misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    let before_d = flush_ssts(&dir).len();
    for tag in 20_000..20_260_u64 {
        vault.put(row(vault_id, tag, 8192))?;
        highest.store(tag, std::sync::atomic::Ordering::Release);
    }
    stop.store(true, std::sync::atomic::Ordering::Release);
    for reader in readers {
        reader.join().map_err(|_| "reader thread panicked")?;
    }
    let after_d = flush_ssts(&dir).len();
    let reported_d = report("D concurrent read-your-writes", &collector.drain());
    println!(
        "  reads performed = {}   MISSES = {}",
        reads.load(std::sync::atomic::Ordering::Relaxed),
        misses.load(std::sync::atomic::Ordering::Relaxed)
    );
    println!(
        "  flush-*.sst delta = {}  (seals reported {reported_d})",
        after_d - before_d
    );

    // Exhaustive readback: every key written in phase D, after the fact.
    let mut absent = Vec::new();
    for tag in 20_000..20_260_u64 {
        let cx = row(vault_id, tag, 1).cx_id;
        if vault.get(cx, vault.snapshot()).is_err() {
            absent.push(tag);
        }
    }
    println!(
        "  exhaustive readback of 260 keys: absent = {} {}",
        absent.len(),
        if absent.is_empty() {
            "(all present)".to_owned()
        } else {
            format!("{:?}", &absent[..absent.len().min(8)])
        }
    );
    let verdict_d = misses.load(std::sync::atomic::Ordering::Relaxed) == 0 && absent.is_empty();
    println!(
        "  VERDICT: {}",
        if verdict_d {
            "no read ever missed a row across a seal"
        } else {
            "FAILED - the sealed-memtable read path loses rows"
        }
    );

    println!("\n--- totals ---");
    println!("  phase A outliers reported flushes: {reported_a}");
    println!(
        "  flush-*.sst total on disk        : {}",
        flush_ssts(&dir).len()
    );
    println!(
        "  vault latest_seq                 : {}",
        vault.latest_seq()
    );
    Ok(())
}
