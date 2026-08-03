//! FSV for #1978: on a `full_mvcc_restore` vault, does the CF router hold a
//! single row the MVCC row table does not?
//!
//! The fix gates the router off every **latest**-view read when
//! `router_latest_readback == false`, on the ground that a
//! `restore_mvcc_rows: true` open materialises the whole corpus into the row
//! table. If that ground were false the gate would silently drop rows, so it is
//! checked here against the real corpus rather than taken on the recovery
//! code's word.
//!
//! Two handles over **identical bytes**, each with exactly one source:
//!
//! | handle | open mode | its only source |
//! |---|---|---|
//! | R | `open_latest_readback` (`restore_mvcc_rows: false`) | the CF router |
//! | F | `open` (`full_mvcc_restore`) | the MVCC row table (after the gate) |
//!
//! Both are walked page by page over the whole of `Base`, and every row is
//! reduced to `blake3(value)`. The claim is proven only if the two key sets are
//! equal **and** every shared key's digest agrees — a count match alone would
//! pass with two rows swapped.
//!
//! `F.count_cf_latest_table_only(Base)` is read as well: it excludes the router
//! unconditionally, so it distinguishes "the table holds the corpus" from "the
//! walk quietly read the router anyway".
//!
//! ```powershell
//! cargo run -p synapse-calyx --example page_walk_router_parity_fsv -- <router-copy> <restore-copy>
//! ```
//!
//! The two directories must be byte-identical copies of one live vault taken at
//! the same instant (`vault-copy-fsv-recipe`); handle F replays the WAL and
//! writes, so it must not share a directory with handle R.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault, SynapseCalyxVault};

const CF: ColumnFamily = ColumnFamily::Base;
const PAGE_ROWS: usize = 256;

type Digests = BTreeMap<Vec<u8>, [u8; 32]>;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let router_root = PathBuf::from(args.next().ok_or("arg 1: router-copy root")?);
    let restore_root = PathBuf::from(args.next().ok_or("arg 2: restore-copy root")?);

    // ---- handle R: the CF router alone ------------------------------------
    let started = Instant::now();
    let router_vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(router_root.join("db-daemon")),
        Some(vec![CF, ColumnFamily::Kv]),
    )?;
    println!(
        "R open: seq {} in {:?}",
        router_vault.latest_seq(),
        started.elapsed()
    );

    let (router_digests, router_ms, router_pages) = walk_read_only(&router_vault)?;
    println!(
        "R walk: {} rows / {} pages / {router_ms:.0} ms  (source: CF router)",
        router_digests.len(),
        router_pages
    );
    drop(router_vault);

    // ---- handle F: the MVCC row table alone -------------------------------
    let started = Instant::now();
    let restore_vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(
        restore_root.join("db-daemon"),
    ))?;
    println!(
        "F open: seq {} in {:?}",
        restore_vault.latest_seq(),
        started.elapsed()
    );

    // Fail closed on the mode, not on the flag we asked for. A handle that
    // silently opened latest-only would make the comparison below vacuous:
    // both arms would be reading the router (#1954, #1957).
    let readback = restore_vault.router_latest_readback();
    println!("F router_latest_readback = {readback} (expected false)");
    assert!(
        !readback,
        "handle F opened latest-only, so it reads the router too and this comparison proves nothing"
    );

    let table_only = restore_vault.count_cf_latest_table_only(CF);
    println!("F count_cf_latest_table_only({}) = {table_only}", CF.name());

    let (restore_digests, restore_ms, restore_pages) = walk_writable(&restore_vault)?;
    println!(
        "F walk: {} rows / {} pages / {restore_ms:.0} ms  (source: MVCC row table)",
        restore_digests.len(),
        restore_pages
    );

    // ---- the comparison ---------------------------------------------------
    let router_only = router_digests
        .keys()
        .filter(|key| !restore_digests.contains_key(*key))
        .collect::<Vec<_>>();
    let table_only_keys = restore_digests
        .keys()
        .filter(|key| !router_digests.contains_key(*key))
        .collect::<Vec<_>>();
    let disagreeing = router_digests
        .iter()
        .filter_map(|(key, digest)| {
            restore_digests
                .get(key)
                .filter(|other| *other != digest)
                .map(|_| key)
        })
        .collect::<Vec<_>>();

    println!("\n=== parity over {} ===", CF.name());
    println!("rows in the router only : {}", router_only.len());
    println!("rows in the table only  : {}", table_only_keys.len());
    println!("values disagreeing      : {}", disagreeing.len());
    for key in router_only.iter().take(5) {
        println!("  router-only key {}", hex(key));
    }
    for key in table_only_keys.iter().take(5) {
        println!("  table-only  key {}", hex(key));
    }
    for key in disagreeing.iter().take(5) {
        println!("  disagreeing key {}", hex(key));
    }

    println!(
        "\nwalk cost: router {router_ms:.0} ms vs row table {restore_ms:.0} ms  ({:.1}x)",
        router_ms / restore_ms.max(0.001)
    );

    assert!(
        router_only.is_empty(),
        "the CF router holds {} row(s) the MVCC row table does not: gating the router off latest reads would DROP them",
        router_only.len()
    );
    assert!(
        disagreeing.is_empty(),
        "{} key(s) resolve to a different value through the router than through the row table",
        disagreeing.len()
    );
    assert_eq!(
        table_only,
        restore_digests.len(),
        "the router-excluded census disagrees with the paged walk on the same handle, so the walk is not reading the table alone"
    );
    println!(
        "\nPASS: the row table is a complete superset of the router on {}",
        CF.name()
    );
    Ok(())
}

fn walk_read_only(
    vault: &SynapseCalyxReadOnlyVault,
) -> Result<(Digests, f64, usize), Box<dyn Error>> {
    let range = KeyRange::all();
    let mut cursor: Option<Vec<u8>> = None;
    let mut digests = Digests::new();
    let mut pages = 0;
    let started = Instant::now();
    loop {
        let page = vault.scan_cf_range_page_latest(CF, &range, cursor.as_deref(), PAGE_ROWS)?;
        pages += 1;
        for (key, value) in &page.rows {
            digests.insert(key.clone(), digest(value));
        }
        if !page.more {
            break;
        }
        cursor = Some(page.resume_after.clone().ok_or("more implies a cursor")?);
    }
    Ok((digests, started.elapsed().as_secs_f64() * 1000.0, pages))
}

fn walk_writable(vault: &SynapseCalyxVault) -> Result<(Digests, f64, usize), Box<dyn Error>> {
    let range = KeyRange::all();
    let mut cursor: Option<Vec<u8>> = None;
    let mut digests = Digests::new();
    let mut pages = 0;
    let started = Instant::now();
    loop {
        let page = vault.scan_cf_range_page_latest(CF, &range, cursor.as_deref(), PAGE_ROWS)?;
        pages += 1;
        for (key, value) in &page.rows {
            digests.insert(key.clone(), digest(value));
        }
        if !page.more {
            break;
        }
        cursor = Some(page.resume_after.clone().ok_or("more implies a cursor")?);
    }
    Ok((digests, started.elapsed().as_secs_f64() * 1000.0, pages))
}

fn digest(value: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(value);
    hasher.finalize().into()
}

fn hex(key: &[u8]) -> String {
    key.iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
