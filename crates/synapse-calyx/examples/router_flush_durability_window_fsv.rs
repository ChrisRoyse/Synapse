//! Manual FSV for #1951's **blocking question**: does deferring the router's
//! SST write past commit return open a durable-coverage window?
//!
//! ## The question #1951 says must be settled before the flusher is written
//!
//! > Whether that weakens the invariant, or whether the WAL plus checkpoint
//! > staging already covers the window, has to be established **before** the
//! > change rather than assumed — a wrong answer here is silent data loss on
//! > recovery.
//!
//! Today the SST write completes before `commit_batch_timed` returns, so no
//! observer can see a key whose router-flush SST does not exist. A background
//! flusher moves that moment. The failure it could cause is: the process dies
//! between commit return and the deferred write, the sealed memtable is lost
//! with it, and the rows are gone because the only copy that would have
//! survived was the SST that never landed.
//!
//! ## The experiment
//!
//! Rather than argue it from the code, construct the exact post-crash state and
//! read the vault back.
//!
//! A sealed memtable that never reached disk leaves the vault in a state that
//! is **physically identical** to one where its `flush-*.sst` was written and
//! then removed: in both cases the rows have no router-flush SST, and every
//! other artefact (WAL records, durable-batch SSTs, manifest, row table on
//! disk) is whatever the commit already produced. So:
//!
//! 1. Commit enough rows to force real router flushes, recording every key.
//! 2. Close the vault cleanly. Read every key back after a reopen — the
//!    control, which must be 100% present or the harness proves nothing.
//! 3. Close again, **delete every `flush-*.sst` on disk**, and reopen.
//! 4. Read every key back.
//!
//! If step 4 returns every key, a router-flush SST is not the durable home of
//! anything: the WAL and the checkpoint's durable-batch SSTs already cover the
//! window, and deferring the write cannot lose data. If any key is missing, the
//! window is real and a background flusher must hold the relevant watermark
//! back until its SST lands.
//!
//! This is the strictly harsher form of the crash: a crash loses only the
//! *unwritten* seals, while this deletes **every** flush SST the vault has ever
//! written, including ones from long-completed commits.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example router_flush_durability_window_fsv -- <empty-scratch-dir>`

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{
    Constellation, CxFlags, CxId, InputRef, LedgerRef, Modality, SlotId, SlotVector, VaultId,
    VaultStore as _,
};

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const PANEL: u32 = 1_951_001;
/// Slots per row, matching the `slot=12` of the live `syn-mcp-usage-v1` batch.
const SLOTS: u16 = 12;
/// Rows committed at a dimension large enough to fill the 8 MB memtable cap
/// several times over, so the flushes are the production path deciding on byte
/// pressure rather than anything this harness forces.
const ROWS: u64 = 400;
const DIM: u32 = 8_192;
/// A second, small batch written *after* the large one, so the vault also holds
/// keys whose rows are still in an unsealed memtable at close. Those exercise
/// the clean-close path rather than the flush path, and must survive too.
const TAIL_ROWS: u64 = 60;

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: router_flush_durability_window_fsv <empty-scratch-dir>")?;
    if dir.exists() && std::fs::read_dir(&dir)?.next().is_some() {
        return Err(format!(
            "{} is not empty; this harness must start from a fresh vault",
            dir.display()
        )
        .into());
    }
    std::fs::create_dir_all(&dir)?;

    let vault_id = VaultId::from_str(VAULT_ID)?;
    let mut failures: Vec<String> = Vec::new();

    println!("router_flush_durability_window_fsv");
    println!("  vault_dir = {}", dir.display());

    // ------------------------------------------------------------------
    // Step 1 — write, forcing real router flushes
    // ------------------------------------------------------------------
    println!("\n== Step 1: commit {ROWS} rows at dim={DIM}, then {TAIL_ROWS} small rows ==");
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"router-flush-durability-window-fsv".to_vec(),
        VaultOptions::default(),
    )?;
    for tag in 0..ROWS {
        vault.put(row(vault_id, tag, DIM))?;
    }
    for tag in ROWS..ROWS + TAIL_ROWS {
        vault.put(row(vault_id, tag, 16))?;
    }
    let seq_after_write = vault.latest_seq();
    drop(vault);

    let flush_ssts_written = flush_ssts(&dir);
    println!("  latest_seq after write   = {seq_after_write}");
    println!("  flush-*.sst on disk      = {}", flush_ssts_written.len());
    println!("  durable-batch/compacted  = {}", commit_domain_ssts(&dir).len());
    println!("  wal segments             = {}", wal_files(&dir).len());
    if flush_ssts_written.is_empty() {
        failures.push(
            "no flush-*.sst was produced; the harness never exercised the router flush path"
                .to_owned(),
        );
    }

    // ------------------------------------------------------------------
    // Step 2 — control readback
    // ------------------------------------------------------------------
    println!("\n== Step 2: control — reopen with every artefact intact ==");
    let control_missing = readback(&dir, vault_id)?;
    println!(
        "  {} of {} keys present",
        (ROWS + TAIL_ROWS) as usize - control_missing.len(),
        ROWS + TAIL_ROWS
    );
    if !control_missing.is_empty() {
        println!(
            "  first missing: {:?}",
            &control_missing[..control_missing.len().min(8)]
        );
        failures.push(format!(
            "the control readback lost {} key(s); nothing after this proves anything",
            control_missing.len()
        ));
    }

    // ------------------------------------------------------------------
    // Step 3 — construct the post-crash state
    // ------------------------------------------------------------------
    println!("\n== Step 3: delete every flush-*.sst (the state a lost seal leaves) ==");
    let commit_domain_before = commit_domain_ssts(&dir).len();
    let mut deleted = 0usize;
    let mut deleted_bytes = 0u64;
    for path in flush_ssts(&dir) {
        deleted_bytes += std::fs::metadata(&path)?.len();
        println!("  rm {}", path.display());
        std::fs::remove_file(&path)?;
        deleted += 1;
    }
    println!("  deleted {deleted} file(s), {deleted_bytes} bytes");
    println!("  flush-*.sst remaining    = {}", flush_ssts(&dir).len());
    println!("  durable-batch/compacted  = {commit_domain_before} (untouched)");

    // ------------------------------------------------------------------
    // Step 4 — the decisive readback
    // ------------------------------------------------------------------
    println!("\n== Step 4: reopen with no router-flush SST anywhere on disk ==");
    let after_missing = match readback(&dir, vault_id) {
        Ok(missing) => missing,
        Err(error) => {
            println!("  the reopen itself refused: {error}");
            failures.push(format!(
                "the vault could not be reopened with its flush SSTs removed: {error}"
            ));
            return finish(&failures);
        }
    };
    println!(
        "  {} of {} keys present",
        (ROWS + TAIL_ROWS) as usize - after_missing.len(),
        ROWS + TAIL_ROWS
    );
    if after_missing.is_empty() {
        println!(
            "\n  VERDICT: a router-flush SST is NOT the durable home of any committed row.\n\
             \x20 Every key survived with all {deleted} flush SST(s) removed, so the WAL and the\n\
             \x20 checkpoint's durable-batch SSTs already cover the window. Deferring the write\n\
             \x20 past commit return cannot lose data, and a background flusher needs no\n\
             \x20 watermark holdback for durability."
        );
    } else {
        println!(
            "  first missing: {:?}",
            &after_missing[..after_missing.len().min(8)]
        );
        println!(
            "\n  VERDICT: the window is REAL. {} key(s) live only in router-flush SSTs, so a\n\
             \x20 background flusher must hold the durable watermark back until each SST lands.",
            after_missing.len()
        );
        failures.push(format!(
            "{} key(s) were recoverable only from a router-flush SST",
            after_missing.len()
        ));
    }

    finish(&failures)
}

/// Reopens the vault and returns the tags that could not be read back.
///
/// The vault is opened and dropped inside this function so no in-memory row
/// table can answer a read the disk could not.
fn readback(dir: &Path, vault_id: VaultId) -> Result<Vec<u64>, Box<dyn Error>> {
    let vault = AsterVault::open(
        dir,
        vault_id,
        b"router-flush-durability-window-fsv".to_vec(),
        VaultOptions::default(),
    )?;
    let snapshot = vault.snapshot();
    let mut missing = Vec::new();
    for tag in 0..ROWS + TAIL_ROWS {
        let cx = row(vault_id, tag, 1).cx_id;
        if vault.get(cx, snapshot).is_err() {
            missing.push(tag);
        }
    }
    drop(vault);
    Ok(missing)
}

/// The physical product of a router memtable flush.
fn flush_ssts(vault_dir: &Path) -> Vec<PathBuf> {
    ssts_matching(vault_dir, |name| {
        name.starts_with("flush-") && name.ends_with(".sst")
    })
}

/// The commit-domain SSTs: the checkpoint's durable batches and compaction
/// output. These are the files the manifest floor and the recovery readback
/// actually vouch for, and this harness leaves every one of them in place.
fn commit_domain_ssts(vault_dir: &Path) -> Vec<PathBuf> {
    ssts_matching(vault_dir, |name| {
        name.ends_with(".sst") && !name.starts_with("flush-")
    })
}

fn ssts_matching(vault_dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(cfs) = std::fs::read_dir(vault_dir.join("cf")) else {
        return found;
    };
    for cf in cfs.flatten() {
        let Ok(files) = std::fs::read_dir(cf.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if keep(&name) {
                found.push(file.path());
            }
        }
    }
    found.sort();
    found
}

fn wal_files(vault_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(vault_dir.join("wal")) else {
        return found;
    };
    for entry in entries.flatten() {
        found.push(entry.path());
    }
    found.sort();
    found
}

fn row(vault_id: VaultId, tag: u64, dim: u32) -> Constellation {
    let mut slots = BTreeMap::new();
    for slot in 1..=SLOTS {
        slots.insert(
            SlotId::new(slot),
            SlotVector::Dense {
                dim,
                // Content varies per row so values cannot be deduplicated away,
                // which would stop the memtable filling.
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
    cx[1] = 0x51;
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

fn finish(failures: &[String]) -> Result<(), Box<dyn Error>> {
    println!("\n================================================================");
    if failures.is_empty() {
        println!("ALL CHECKS PASSED");
        Ok(())
    } else {
        println!("{} CHECK(S) FAILED:", failures.len());
        for failure in failures {
            println!("  - {failure}");
        }
        Err("router_flush_durability_window_fsv failed".into())
    }
}
