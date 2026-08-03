//! FSV for #1977: does replacing the whole-family `scan_cf_at` with a bounded
//! paged walk change the answer, and what does it do to the row-guard hold?
//!
//! The migrated caller that was actually firing on the live daemon is
//! `synapse-storage`'s `collect_derived_source_references` — the GC tick's index
//! of which source rows a live derived constellation still points at (#1882).
//! Its Source of Truth is that map, so this harness computes **the map itself**
//! both ways over the same frozen corpus and compares it, rather than comparing
//! row counts and inferring the rest.
//!
//! ```powershell
//! cargo run --release -p synapse-calyx --example whole_family_hold_fsv -- <restore-copy>
//! ```
//!
//! Run against a copy of the live vault (`vault-copy-fsv-recipe`), opened in the
//! daemon's own `full_mvcc_restore` mode — the paged and whole-family paths take
//! different branches under `router_latest_readback`, so a harness in the other
//! mode would compare two things neither of which the daemon runs.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use synapse_calyx::{
    METADATA_SOURCE_CF, METADATA_SOURCE_KEY_HEX, SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
    SynapseCalyxConfig, SynapseCalyxError, SynapseCalyxVault, SynapseCalyxWalkStep,
};

/// The GC tick's Source of Truth: source CF -> the source keys still pointed at.
type References = BTreeMap<String, BTreeSet<Vec<u8>>>;

fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("pass the directory holding db-daemon/ and machine-salt.bin")?,
    );
    let vault =
        SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(root.join("db-daemon")))?;
    assert!(
        !vault.router_latest_readback(),
        "this harness must run in the daemon's own full_mvcc_restore mode"
    );
    println!(
        "opened at seq {}, Base holds {} rows",
        vault.latest_seq(),
        vault.count_cf_latest_table_only(ColumnFamily::Base)
    );

    // ---- control: the whole-family scan this issue removes ------------------
    let before = census(&vault, "scan_cf_at_overlay");
    let started = Instant::now();
    let mut control = References::new();
    for (_key, value) in vault.scan_cf_at(vault.latest_seq(), ColumnFamily::Base)? {
        index(&value, &mut control)?;
    }
    let control_ms = started.elapsed().as_secs_f64() * 1000.0;
    let control_hold = census(&vault, "scan_cf_at_overlay") - before;
    println!(
        "\ncontrol  scan_cf_at(Base)      : {control_ms:8.1} ms wall, {} guard-us in 1 hold, {} source CF(s), {} key(s)",
        control_hold,
        control.len(),
        control.values().map(BTreeSet::len).sum::<usize>()
    );

    // ---- migrated: the bounded paged walk -----------------------------------
    let before = census(&vault, "scan_cf_range_page_latest");
    let started = Instant::now();
    let mut paged = References::new();
    let walk = vault.walk_cf_latest(
        ColumnFamily::Base,
        SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
        |_key, value| {
            index(value, &mut paged).map_err(|error| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_FSV_SOURCE_REFERENCE_UNDECODABLE",
                    error.to_string(),
                    "repair the Base row this harness could not fold",
                )
            })?;
            Ok(SynapseCalyxWalkStep::Continue)
        },
    )?;
    let paged_ms = started.elapsed().as_secs_f64() * 1000.0;
    let paged_hold = census(&vault, "scan_cf_range_page_latest") - before;
    println!(
        "migrated walk_cf_latest(Base)   : {paged_ms:8.1} ms wall, {paged_hold} guard-us in {} holds, {} source CF(s), {} key(s)",
        walk.pages,
        paged.len(),
        paged.values().map(BTreeSet::len).sum::<usize>()
    );
    println!(
        "         atomic={} (seq {} -> {}), rows_visited={}",
        walk.atomic(),
        walk.snapshot_seq_first,
        walk.snapshot_seq_last,
        walk.rows_visited
    );

    // ---- the comparison: the map itself, not a proxy for it -----------------
    assert_eq!(
        control.keys().collect::<Vec<_>>(),
        paged.keys().collect::<Vec<_>>(),
        "the two walks disagree about which source column families are referenced"
    );
    for (cf, keys) in &control {
        assert_eq!(
            Some(keys),
            paged.get(cf),
            "the two walks disagree about the referenced key set of {cf}"
        );
    }

    println!("\n--- worst single hold, per site ---");
    for entry in vault.row_guard_census() {
        if entry.holds == 0 {
            continue;
        }
        println!(
            "{:<32} holds={:<6} max_us={:<9} over_budget={}",
            entry.site, entry.holds, entry.max_held_us, entry.over_budget_holds
        );
    }

    println!(
        "\nPASS: identical reference index; worst hold {} us -> {} us across {} bounded holds",
        control_hold,
        paged_hold / u64::try_from(walk.pages).unwrap_or(1).max(1),
        walk.pages
    );
    Ok(())
}

/// Exactly the fold `collect_derived_source_references` performs.
fn index(value: &[u8], referenced: &mut References) -> Result<(), Box<dyn Error>> {
    let constellation = decode_constellation_base(value)?;
    let (Some(source_cf), Some(source_key_hex)) = (
        constellation.metadata.get(METADATA_SOURCE_CF),
        constellation.metadata.get(METADATA_SOURCE_KEY_HEX),
    ) else {
        return Ok(());
    };
    let key = (0..source_key_hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&source_key_hex[index..index + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    referenced.entry(source_cf.clone()).or_default().insert(key);
    Ok(())
}

/// Total microseconds this site has held the row-table read guard so far.
fn census(vault: &SynapseCalyxVault, site: &str) -> u64 {
    vault
        .row_guard_census()
        .into_iter()
        .find(|entry| entry.site == site)
        .map_or(0, |entry| entry.total_held_us)
}
