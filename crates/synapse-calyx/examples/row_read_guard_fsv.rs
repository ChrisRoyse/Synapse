//! Manual FSV for #1950 ask 1: does the row-table read-guard instrument fire,
//! and does it name the site that actually held the guard?
//!
//! ## What this has to settle
//!
//! #1950 measured commits waiting **1.0 s** for the row-table write lock while
//! nothing anywhere could say who held the read side. `TimedRowRead` is the
//! instrument added to close that. The failure mode of an instrument is that it
//! silently never fires — a guard that reports nothing looks exactly like a
//! vault with no slow readers, which is the condition #1950 is stuck in. So the
//! thing to prove is not that it compiles but that a genuinely long read
//! produces a report naming that read.
//!
//! ## Construction
//!
//! Known input, known expected output. Enough rows are written that a full-table
//! scan cannot finish inside the 25 ms budget, then exactly one such scan is
//! run. The expectation stated before the run:
//!
//! * at least one `CALYX_ASTER_ROW_READ_GUARD_SLOW` event, and
//! * its `site` field is the scan actually invoked, not some other reader, and
//! * its `held_us` is >= the 25 ms budget.
//!
//! A point read of a single key is then run as the negative control: it must
//! produce **no** event. An instrument that fires on everything is as useless as
//! one that fires on nothing, and only running both directions distinguishes
//! "it works" from "it always reports".
//!
//! Usage:
//! `cargo run --release -p synapse-calyx --example row_read_guard_fsv -- <empty-scratch-dir>`

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};

use calyx_aster::cf::ColumnFamily;
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
const PANEL: u32 = 1_950_001;
/// The budget the instrument enforces, restated here so the harness fails if
/// the two ever drift apart rather than silently measuring the wrong bar.
const BUDGET_US: u64 = 25_000;

#[derive(Debug, Clone)]
struct GuardEvent {
    site: String,
    held_us: u64,
    cpu_us: u64,
    starved: bool,
}

#[derive(Default)]
struct Collector {
    events: Mutex<Vec<GuardEvent>>,
}

impl Collector {
    fn drain(&self) -> Vec<GuardEvent> {
        std::mem::take(
            &mut *self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

#[derive(Default)]
struct Fields {
    code: String,
    site: String,
    held_us: u64,
    cpu_us: u64,
    starved: bool,
}

impl Visit for Fields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "held_us" => self.held_us = value,
            "cpu_us" => self.cpu_us = value,
            _ => {}
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "starved" {
            self.starved = value;
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "code" => self.code = value.to_owned(),
            "site" => self.site = value.to_owned(),
            _ => {}
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}").trim_matches('"').to_owned();
        match field.name() {
            "code" => self.code = rendered,
            "site" => self.site = rendered,
            _ => {}
        }
    }
}

struct Sink(Arc<Collector>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Sink {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.code == "CALYX_ASTER_ROW_READ_GUARD_SLOW" {
            self.0
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(GuardEvent {
                    site: fields.site,
                    held_us: fields.held_us,
                    cpu_us: fields.cpu_us,
                    starved: fields.starved,
                });
        }
    }
}

fn row(vault_id: VaultId, tag: u64) -> Constellation {
    let mut slots: BTreeMap<SlotId, SlotVector> = BTreeMap::new();
    slots.insert(
        SlotId::new(1),
        SlotVector::Dense {
            dim: 8,
            data: (0..8)
                .map(|i| (tag as f32).mul_add(0.001, i as f32))
                .collect(),
        },
    );
    let mut cx = [0_u8; 16];
    cx[0] = 0x19;
    cx[1] = 0x50;
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
        scalars: BTreeMap::new(),
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

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: row_read_guard_fsv <empty-scratch-dir>")?;
    std::fs::create_dir_all(&dir)?;

    let collector = Arc::new(Collector::default());
    tracing_subscriber::registry()
        .with(
            Sink(Arc::clone(&collector)).with_filter(tracing_subscriber::filter::LevelFilter::WARN),
        )
        .init();

    let vault_id = VaultId::from_str(VAULT_ID)?;
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"row-read-guard-fsv".to_vec(),
        VaultOptions::default(),
    )?;

    println!("row_read_guard_fsv  (#1950 ask 1)");
    println!("vault_dir = {}", dir.display());
    println!("instrument budget = {BUDGET_US} us");

    // Enough rows that a full-table scan cannot finish inside the budget. This
    // is the same shape of read the daemon runs on a 30 s cadence.
    //
    // Calibrated rather than guessed: 60,000 rows scanned in 14,425 us on this
    // host, i.e. *under* the 25 ms budget, so the instrument correctly stayed
    // silent and the first version of this harness failed on its own
    // construction rather than on the code under test. ~4.2 rows/us puts
    // 200,000 at roughly 48 ms, a 2x margin over the budget that survives
    // ordinary machine variance without needing a bigger corpus than the
    // question requires.
    const ROWS: u64 = 200_000;
    println!("\nwriting {ROWS} rows...");
    for tag in 0..ROWS {
        vault.put(row(vault_id, tag))?;
    }
    println!("  latest_seq = {}", vault.latest_seq());
    let _ = collector.drain();

    // --- positive: one full scan, which must report itself -----------------
    // One page big enough to cover the whole CF, so a single guard acquisition
    // holds for the length of the entire scan rather than being split across
    // many short ones.
    println!("\n=== positive: a scan that exceeds the budget must report ===");
    let mut scanned = 0_usize;
    let mut pages = 0_usize;
    let scan_started = std::time::Instant::now();
    vault.scan_cf_pages_at::<_, calyx_core::CalyxError>(
        vault.snapshot(),
        ColumnFamily::Base,
        usize::try_from(ROWS)? + 1,
        |page| {
            scanned += page.len();
            pages += 1;
            Ok(())
        },
    )?;
    let scan_us = scan_started.elapsed().as_micros();
    let events = collector.drain();
    println!(
        "  scan_cf_pages_at(Base) returned {scanned} rows in {pages} page(s), {scan_us} us wall"
    );
    println!("  guard events captured = {}", events.len());
    for event in &events {
        println!(
            "    site={:<28} held_us={:<9} cpu_us={:<9} starved={}",
            event.site, event.held_us, event.cpu_us, event.starved
        );
    }

    let expected_site = "scan_cf_range_page_at";
    let named = events.iter().any(|event| event.site == expected_site);
    let over_budget = events.iter().all(|event| event.held_us >= BUDGET_US);
    let positive_ok = !events.is_empty() && named && over_budget;
    println!(
        "  expected site '{expected_site}' present = {named}; every held_us >= budget = {over_budget}"
    );

    // --- negative control: a point read must NOT report ---------------------
    // An instrument that reports on every read names nothing. This is what
    // separates "fires correctly" from "always fires".
    println!("\n=== negative control: a point read must NOT report ===");
    let probe = row(vault_id, ROWS / 2).cx_id;
    let found = vault.get(probe, vault.snapshot()).is_ok();
    let noise = collector.drain();
    println!("  point read resolved = {found}");
    println!("  guard events captured = {} (expected 0)", noise.len());
    for event in &noise {
        println!(
            "    UNEXPECTED site={} held_us={}",
            event.site, event.held_us
        );
    }
    let negative_ok = noise.is_empty() && found;

    // --- starvation: the SAME scan, with the machine saturated --------------
    // #1955: `held_us` alone cannot distinguish a slow scan from a descheduled
    // thread, and on this host that mattered -- a `read_latest` (a point read
    // of ONE key) reported holding the guard for 743 ms while a compile ran.
    // `cpu_us` is what separates them, and the only way to prove it separates
    // them is to produce both conditions and check the verdict flips.
    //
    // Same scan, same data, same code. The only change is that every core is
    // busy.
    println!(
        "
=== starvation control: identical scan, machine saturated ==="
    );
    let stop_load = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cores = std::thread::available_parallelism().map_or(8, std::num::NonZeroUsize::get);
    let mut load = Vec::new();
    for _ in 0..cores * 2 {
        let stop = Arc::clone(&stop_load);
        load.push(std::thread::spawn(move || {
            // Memory-thrashing rather than pure ALU. The production starvation
            // came from a compile, which evicts the scanning thread's working
            // set and stalls it on memory rather than merely competing for ALU
            // slots. A spin loop shares cores politely and does not reproduce
            // it -- measured: with 24 pure-ALU spinners the scan still got
            // 62,500 us of CPU for a 69,767 us hold.
            let mut buffer = vec![0_u8; 32 << 20];
            let mut cursor = 0_usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for _ in 0..4096 {
                    cursor = cursor.wrapping_add(4099) % buffer.len();
                    buffer[cursor] = buffer[cursor].wrapping_add(1);
                }
                std::hint::black_box(&buffer);
            }
        }));
    }
    println!("  {} busy threads on {cores} logical cores", cores * 2);

    let mut scanned_loaded = 0_usize;
    vault.scan_cf_pages_at::<_, calyx_core::CalyxError>(
        vault.snapshot(),
        ColumnFamily::Base,
        usize::try_from(ROWS)? + 1,
        |page| {
            scanned_loaded += page.len();
            Ok(())
        },
    )?;
    stop_load.store(true, std::sync::atomic::Ordering::Relaxed);
    for thread in load {
        thread.join().map_err(|_| "load thread panicked")?;
    }
    let loaded_events = collector.drain();
    println!("  scan returned {scanned_loaded} rows");
    for event in &loaded_events {
        println!(
            "    site={:<28} held_us={:<9} cpu_us={:<9} starved={}",
            event.site, event.held_us, event.cpu_us, event.starved
        );
    }
    let starved_seen = loaded_events.iter().any(|event| event.starved);
    let quiet_starved = events.iter().any(|event| event.starved);
    println!("  quiet scan  starved = {quiet_starved} (expected false)");
    println!("  loaded scan starved = {starved_seen} (expected true)");
    let starvation_ok = !quiet_starved && starved_seen && !loaded_events.is_empty();

    println!("\n--- VERDICT ---");
    println!(
        "  positive (slow scan reports, names its site) : {}",
        if positive_ok { "PASS" } else { "FAIL" }
    );
    println!(
        "  starvation (cpu_us separates the two)        : {}",
        if starvation_ok { "PASS" } else { "FAIL" }
    );
    println!(
        "  negative (fast point read stays silent)      : {}",
        if negative_ok { "PASS" } else { "FAIL" }
    );
    if positive_ok && negative_ok && starvation_ok {
        println!("  the row-read guard instrument fires, and only when it should");
        Ok(())
    } else {
        Err("row-read guard instrument did not behave as specified".into())
    }
}
