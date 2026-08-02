//! Full-state verification for issue #1957: a latest-only open must never be
//! silently granted in the opposite mode.
//!
//! ## What was wrong
//!
//! `VaultOptions::restore_mvcc_rows: false` asks for a handle that serves reads
//! from the CF router and does *not* materialise the MVCC row table. Two of the
//! three recovery branches honoured it. The third — WAL-only recovery, taken
//! when the vault has no published `CURRENT` manifest — hardcoded
//! `router_latest_readback: false` and returned success, so the caller got the
//! row-table path while believing it got the router path, and got exactly the
//! memory profile it opened the handle to avoid. The `CALYX_ASTER_RECOVERY_DONE`
//! event logged `router_latest_readback = false` as a *literal*, so it reported
//! the value it forced rather than the divergence from what was asked.
//!
//! ## What this proves, against the bytes
//!
//! The source of truth is `AsterVault::router_latest_readback()`, which reads
//! the `AtomicBool` the recovery planner actually installed on
//! `VersionedCfStore` — not the value the caller passed in. Every phase reads it
//! back after the open and compares it to what was requested.
//!
//! | phase | vault state | requested | must happen |
//! |---|---|---|---|
//! | A | rows written, **no** manifest | latest-only | **refused**, `CALYX_ASTER_LATEST_READBACK_REQUIRES_MANIFEST` |
//! | B | same vault, still no manifest | full MVCC | granted, readback `false` == requested |
//! | C | same vault, `checkpoint()` published | latest-only | granted, readback `true` == requested |
//! | D | fresh empty vault, no manifest | latest-only | granted, readback `true` — nothing was materialised, so the mode is truthful |
//! | E | phase C handle | — | rows actually readable through the router path |
//!
//! Phase A is the regression: before the fix it *succeeded* and returned
//! `false`. Phase D is the boundary that keeps the fix from being a blanket
//! refusal — an empty vault has nothing to materialise, so the request is
//! trivially satisfiable and must still be granted.
//!
//! `cargo run --release -p synapse-calyx --example latest_readback_mode_fsv`

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;
use std::error::Error;
use std::path::Path;
use std::str::FromStr as _;

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const CF: ColumnFamily = ColumnFamily::Kv;
const KEY_PREFIX: &[u8] = b"fsv1957/";
const ROWS: u32 = 64;
const SALT: &[u8] = b"latest-readback-mode-fsv";

fn key_of(tag: u32) -> Vec<u8> {
    let mut key = KEY_PREFIX.to_vec();
    key.extend_from_slice(format!("{tag:06}").as_bytes());
    key
}

fn latest_only() -> VaultOptions {
    VaultOptions {
        restore_mvcc_rows: false,
        ..VaultOptions::default()
    }
}

fn manifest_published(dir: &Path) -> bool {
    dir.join("CURRENT").exists()
}

/// Reads the mode the handle is actually in and compares it to the request.
///
/// This is the readback that makes the run evidence rather than an assertion:
/// it comes from the store the recovery planner built, so a branch that forced
/// a value cannot report the value it was asked for.
fn assert_mode(
    vault: &AsterVault<calyx_core::SystemClock>,
    label: &str,
    requested_latest: bool,
) -> Result<(), Box<dyn Error>> {
    let effective = vault.router_latest_readback();
    println!(
        "   {label}: requested router_latest_readback = {requested_latest}, \
         effective = {effective}"
    );
    if effective == requested_latest {
        return Ok(());
    }
    Err(format!(
        "{label}: the handle opened in the OPPOSITE mode to the one requested \
         (requested {requested_latest}, got {effective}) — this is #1957"
    )
    .into())
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::temp_dir().join(format!("calyx-1957-fsv-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    let populated = root.join("populated");
    let empty = root.join("empty");
    std::fs::create_dir_all(&populated)?;
    std::fs::create_dir_all(&empty)?;
    let vault_id = VaultId::from_str(VAULT_ID)?;

    println!("latest_readback_mode_fsv  (#1957)");
    println!("root = {}", root.display());

    // --- setup: a vault with real rows and deliberately no manifest ----------
    {
        let vault = AsterVault::open(&populated, vault_id, SALT.to_vec(), VaultOptions::default())?;
        let rows = (0..ROWS)
            .map(|tag| (CF, key_of(tag), format!("value-{tag}").into_bytes()))
            .collect::<Vec<_>>();
        vault.write_cf_batch(rows)?;
        let flushed = vault.flush_all_cfs()?.len();
        println!("\n=== setup: wrote {ROWS} rows, flushed {flushed} SST(s), no checkpoint");
    }
    println!(
        "   CURRENT published = {} (expected false)",
        manifest_published(&populated)
    );
    if manifest_published(&populated) {
        return Err(
            "setup published a manifest; phase A would not reach the WAL-only \
                    branch and would prove nothing"
                .into(),
        );
    }

    // --- phase A: the regression --------------------------------------------
    println!("\n=== A. latest-only open of an un-checkpointed vault with rows");
    match AsterVault::open(&populated, vault_id, SALT.to_vec(), latest_only()) {
        Ok(vault) => {
            let effective = vault.router_latest_readback();
            return Err(format!(
                "the open SUCCEEDED with router_latest_readback = {effective}. Requested \
                 true. This is exactly #1957: the request was answered as though it had \
                 been honoured"
            )
            .into());
        }
        Err(error) => {
            println!("   refused: code = {}", error.code);
            println!("   message = {}", error.message);
            println!("   remediation = {}", error.remediation);
            if error.code != "CALYX_ASTER_LATEST_READBACK_REQUIRES_MANIFEST" {
                return Err(format!(
                    "refused with the wrong code {} — a caller cannot act on an \
                     unnamed refusal",
                    error.code
                )
                .into());
            }
            if !error.message.contains("CURRENT") {
                return Err(
                    "the refusal does not name the missing manifest, so it does \
                            not say what is wrong"
                        .into(),
                );
            }
            if !error.remediation.contains("checkpoint") {
                return Err(
                    "the refusal does not name checkpoint() as the remediation, \
                            so it does not say how to fix it"
                        .into(),
                );
            }
        }
    }

    // --- phase B: the same vault, asking for the mode it can serve ----------
    println!("\n=== B. same vault, full-MVCC open (the mode the branch can serve)");
    {
        let vault = AsterVault::open(&populated, vault_id, SALT.to_vec(), VaultOptions::default())?;
        assert_mode(&vault, "B", false)?;
        let live = vault.scan_cf_latest(CF)?;
        let mine = live
            .iter()
            .filter(|(key, _)| key.starts_with(KEY_PREFIX))
            .count();
        println!("   scan_cf_latest own rows = {mine} (expected {ROWS})");
        if mine != ROWS as usize {
            return Err(format!("phase B lost rows: expected {ROWS}, got {mine}").into());
        }
        vault.checkpoint()?;
        println!("   checkpoint() published a manifest");
    }
    println!(
        "   CURRENT published = {} (expected true)",
        manifest_published(&populated)
    );
    if !manifest_published(&populated) {
        return Err(
            "checkpoint() did not publish CURRENT; phase C would re-enter the \
                    WAL-only branch and would be testing phase A again"
                .into(),
        );
    }

    // --- phase C: the remediation the refusal named actually works ----------
    println!("\n=== C. same vault after checkpoint(), latest-only open");
    let vault = AsterVault::open(&populated, vault_id, SALT.to_vec(), latest_only())?;
    assert_mode(&vault, "C", true)?;

    // --- phase E: the granted mode serves real reads ------------------------
    println!("\n=== E. the granted latest-only handle actually serves the rows");
    let live = vault.scan_cf_latest(CF)?;
    let mine = live
        .iter()
        .filter(|(key, _)| key.starts_with(KEY_PREFIX))
        .count();
    let probe = key_of(ROWS / 2);
    let point = vault.read_cf_latest(CF, &probe)?;
    let expected = format!("value-{}", ROWS / 2).into_bytes();
    println!("   scan_cf_latest own rows = {mine} (expected {ROWS})");
    println!(
        "   read_latest({}) = {:?} (expected {:?})",
        String::from_utf8_lossy(&probe),
        point.as_deref().map(String::from_utf8_lossy),
        String::from_utf8_lossy(&expected)
    );
    if mine != ROWS as usize {
        return Err(
            format!("the latest-only handle lost rows: expected {ROWS}, got {mine}").into(),
        );
    }
    if point.as_deref() != Some(expected.as_slice()) {
        return Err("the latest-only handle did not serve the point read".into());
    }
    drop(vault);

    // --- phase D: the boundary — an empty vault is trivially satisfiable ----
    println!("\n=== D. fresh empty vault, no manifest, latest-only open");
    let vault = AsterVault::open(&empty, vault_id, SALT.to_vec(), latest_only())?;
    assert_mode(&vault, "D", true)?;
    let rows = vault.scan_cf_latest(CF)?.len();
    println!("   scan_cf_latest rows = {rows} (expected 0)");
    if rows != 0 {
        return Err(format!("the empty vault is not empty: {rows} rows").into());
    }
    drop(vault);

    std::fs::remove_dir_all(&root)?;
    println!("\nPASS: every open reports the mode it was asked for, or refuses by name.");
    Ok(())
}
