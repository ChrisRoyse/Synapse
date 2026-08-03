//! FSV harness for #1978: what does one bounded page of a `Base` walk cost, and
//! which of its two sources is paying?
//!
//! Run against *copies* of the live vault so the daemon keeps its writer lock
//! and the corpus cannot drift between arms (`vault-copy-fsv-recipe`).
//!
//! ```powershell
//! cargo run --release -p synapse-calyx --example page_walk_cost_fsv -- <router-copy> <restore-copy>
//! ```
//!
//! `latest_range_page_from_view` reads its two sources **sequentially in one
//! function** — the CF router page, then the MVCC row-table page — so the cost
//! of the merged read is the sum of the two arms below, and #1978's gate
//! removes exactly the router arm on a `full_mvcc_restore` handle. Measuring
//! them separately on byte-identical copies in one process is therefore a
//! same-resource control phase (#1950) rather than a before/after across two
//! machine states (#1960).
//!
//! Arm 1 (router) additionally sweeps page size. If per-page setup dominated,
//! total wall time would scale as `1/page_rows`; if the per-row value read
//! dominates, it is flat. That answers #1978's ask 3 without instrumentation.

use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use synapse_calyx::{
    SynapseCalyxConfig, SynapseCalyxError, SynapseCalyxReadOnlyVault, SynapseCalyxVault,
};

const CF: ColumnFamily = ColumnFamily::Base;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let router_root = PathBuf::from(args.next().ok_or("arg 1: router-copy root")?);
    let restore_root = PathBuf::from(args.next().ok_or("arg 2: restore-copy root")?);

    // ---- arm 1: the CF router alone, swept over page size ------------------
    let router_vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(router_root.join("db-daemon")),
        Some(vec![CF, ColumnFamily::Kv]),
    )?;
    println!("R open: seq {}", router_vault.latest_seq());

    let warm = walk(
        |range, cursor, limit| {
            router_vault
                .scan_cf_range_page_latest(CF, range, cursor, limit)
                .map(|page| (page.rows, page.more, page.resume_after))
        },
        256,
    )?;
    println!(
        "R warm-up: {} rows / {} pages / {:.0} ms",
        warm.rows, warm.pages, warm.wall_ms
    );

    println!("\n--- arm 1: CF router source (the work #1978's gate removes) ---");
    println!("page_rows |  pages |  wall_ms |  us/page |  us/row |  max_page_us");
    for page_rows in [64_usize, 256, 1024, 4096] {
        let arm = walk(
            |range, cursor, limit| {
                router_vault
                    .scan_cf_range_page_latest(CF, range, cursor, limit)
                    .map(|page| (page.rows, page.more, page.resume_after))
            },
            page_rows,
        )?;
        report(page_rows, &arm);
        assert_eq!(
            arm.rows, warm.rows,
            "page size changed the router row count"
        );
    }
    drop(router_vault);

    // ---- arm 2: the MVCC row table alone -----------------------------------
    let restore_vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(
        restore_root.join("db-daemon"),
    ))?;
    assert!(
        !restore_vault.router_latest_readback(),
        "handle F opened latest-only, so arm 2 would measure the router again"
    );
    println!(
        "\nF open: seq {}, table-only census = {}",
        restore_vault.latest_seq(),
        restore_vault.count_cf_latest_table_only(CF)
    );

    let warm_f = walk(
        |range, cursor, limit| {
            restore_vault
                .scan_cf_range_page_latest(CF, range, cursor, limit)
                .map(|page| (page.rows, page.more, page.resume_after))
        },
        256,
    )?;
    println!(
        "F warm-up: {} rows / {} pages / {:.0} ms",
        warm_f.rows, warm_f.pages, warm_f.wall_ms
    );

    println!("\n--- arm 2: MVCC row-table source (what remains after the gate) ---");
    println!("page_rows |  pages |  wall_ms |  us/page |  us/row |  max_page_us");
    for page_rows in [64_usize, 256, 1024, 4096] {
        let arm = walk(
            |range, cursor, limit| {
                restore_vault
                    .scan_cf_range_page_latest(CF, range, cursor, limit)
                    .map(|page| (page.rows, page.more, page.resume_after))
            },
            page_rows,
        )?;
        report(page_rows, &arm);
        assert_eq!(
            arm.rows, warm_f.rows,
            "page size changed the table row count"
        );
    }

    println!("\n--- per-site row-guard census on the full-restore handle ---");
    for entry in restore_vault.row_guard_census() {
        if entry.holds == 0 {
            continue;
        }
        println!(
            "{:<40} holds={:<7} mean_us={:<10.1} max_us={:<9} over_budget={} starved={}",
            entry.site,
            entry.holds,
            entry.mean_held_us.unwrap_or(0.0),
            entry.max_held_us,
            entry.over_budget_holds,
            entry.starved_holds
        );
    }
    Ok(())
}

struct Arm {
    pages: usize,
    rows: usize,
    wall_ms: f64,
    max_page_us: u128,
}

fn report(page_rows: usize, arm: &Arm) {
    println!(
        "{page_rows:9} | {:6} | {:8.1} | {:8.0} | {:7.1} | {:12}",
        arm.pages,
        arm.wall_ms,
        arm.wall_ms * 1000.0 / arm.pages as f64,
        arm.wall_ms * 1000.0 / arm.rows.max(1) as f64,
        arm.max_page_us,
    );
}

type Page = Result<(Vec<(Vec<u8>, Vec<u8>)>, bool, Option<Vec<u8>>), SynapseCalyxError>;

fn walk(
    mut read: impl FnMut(&KeyRange, Option<&[u8]>, usize) -> Page,
    page_rows: usize,
) -> Result<Arm, Box<dyn Error>> {
    let range = KeyRange::all();
    let mut cursor: Option<Vec<u8>> = None;
    let mut arm = Arm {
        pages: 0,
        rows: 0,
        wall_ms: 0.0,
        max_page_us: 0,
    };
    let started = Instant::now();
    loop {
        let page_started = Instant::now();
        let (rows, more, resume_after) = read(&range, cursor.as_deref(), page_rows)?;
        arm.max_page_us = arm.max_page_us.max(page_started.elapsed().as_micros());
        arm.pages += 1;
        arm.rows += rows.len();
        if !more {
            break;
        }
        cursor = Some(resume_after.ok_or("more implies a cursor")?);
    }
    arm.wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(arm)
}
