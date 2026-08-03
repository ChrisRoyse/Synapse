//! FSV for #1979: naming a non-dense lane as the grounding kernel's
//! `content_slot` must fail closed naming the slot and the kind it found, not
//! silently contribute zero concepts.
//!
//! ```powershell
//! cargo run --release -p synapse-calyx --example kernel_content_slot_kind_fsv -- <restore-copy> [panel_version]
//! ```
//!
//! Run against a copy of the live vault (`vault-copy-fsv-recipe`) so the arms
//! read the real corpus — the whole point of the defect is that it is invisible
//! on synthetic data where every lane happens to be dense.
//!
//! Three arms over the same panel, each with an outcome known in advance:
//!
//! | arm | content_slot | expected |
//! |---|---|---|
//! | sparse lane | the BM25 lexical lane (#1898) | `SYNAPSE_CALYX_KERNEL_CONTENT_SLOT_NOT_DENSE`, naming counts |
//! | absent lane | a slot id no record carries | *not* the not-dense code — nothing is the wrong kind, there is simply nothing |
//! | dense lane | a real dense encoder lens | anything but a kind refusal; a recall/anchor refusal is a legitimate outcome |
//!
//! The middle arm is the one that makes this a test rather than an assertion:
//! before the fix all three produced the same shrug, and a refusal that fires on
//! *every* empty corpus would be no more informative than the silence it
//! replaces.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use synapse_calyx::{
    SYNAPSE_CALYX_CF_WALK_PAGE_ROWS, SynapseCalyxConfig, SynapseCalyxError,
    SynapseCalyxKernelParams, SynapseCalyxVault, SynapseCalyxWalkStep,
};

const NOT_DENSE: &str = "SYNAPSE_CALYX_KERNEL_CONTENT_SLOT_NOT_DENSE";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().ok_or("pass the copy root")?);
    let vault =
        SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(root.join("db-daemon")))?;

    let panel_version = args
        .next()
        .and_then(|value| value.parse().ok())
        .or_else(|| vault.active_panel_version().ok().flatten())
        .ok_or("no active panel version and none given")?;
    println!(
        "panel_version = {panel_version}, seq = {}",
        vault.latest_seq()
    );

    // Which slots does this panel's corpus actually carry, and how often? The
    // arms are chosen from physical truth rather than from a declaration, so a
    // panel whose slots moved cannot make this harness vacuously pass.
    let mut declared: BTreeMap<u16, usize> = BTreeMap::new();
    vault.walk_cf_latest(
        ColumnFamily::Base,
        SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
        |_key, value| {
            let base = decode_constellation_base(value)
                .map_err(|error| SynapseCalyxError::from_calyx("decode Base row", &error))?;
            if base.panel_version == panel_version {
                for slot in base.slots.keys() {
                    *declared.entry(slot.get()).or_default() += 1;
                }
            }
            Ok(SynapseCalyxWalkStep::Continue)
        },
    )?;
    println!("slots carried by this panel: {declared:?}");

    let unused_slot = (1_u16..=511)
        .find(|slot| !declared.contains_key(slot))
        .ok_or("every slot id is in use")?;

    let mut failures = 0;
    for (label, slot, expect_not_dense) in [
        ("sparse lexical lane", lexical_slot(&declared)?, true),
        ("absent lane", unused_slot, false),
        ("dense encoder lane", dense_slot(&declared)?, false),
    ] {
        let params = SynapseCalyxKernelParams::new(panel_version, slot);
        let outcome = vault.build_domain_kernel_inputs(&params);
        let (code, message) = match &outcome {
            Ok(inputs) => (
                "<built>".to_owned(),
                format!(
                    "{} concepts, recall {:.4}",
                    inputs.corpus_size, inputs.recall_ratio
                ),
            ),
            Err(error) => (error.code.to_string(), error.message.clone()),
        };
        let hit = code == NOT_DENSE;
        let verdict = if hit == expect_not_dense {
            "OK "
        } else {
            "FAIL"
        };
        if hit != expect_not_dense {
            failures += 1;
        }
        println!(
            "\n[{verdict}] {label}: slot {slot} ({} records declare it)",
            declared.get(&slot).copied().unwrap_or(0)
        );
        println!("       expected not-dense refusal: {expect_not_dense}, got code {code}");
        println!("       {message}");
    }

    assert_eq!(failures, 0, "{failures} arm(s) produced the wrong verdict");
    println!("\nPASS: the kind refusal fires on the sparse lane and on nothing else");
    Ok(())
}

/// The most-carried slot below 100 that is not the timeline text lane, used as
/// the dense arm. Panel slot layout differs per panel, so it is derived.
fn dense_slot(declared: &BTreeMap<u16, usize>) -> Result<u16, Box<dyn Error>> {
    declared
        .iter()
        .filter(|(slot, _)| **slot < 100)
        .max_by_key(|(_, count)| **count)
        .map(|(slot, _)| *slot)
        .ok_or_else(|| "the panel carries no slot below 100".into())
}

/// Slot 103 is the BM25 lexical lane (#1898); fall back to the most-carried
/// slot at or above 100 if this panel numbers it differently.
fn lexical_slot(declared: &BTreeMap<u16, usize>) -> Result<u16, Box<dyn Error>> {
    if declared.contains_key(&103) {
        return Ok(103);
    }
    declared
        .iter()
        .filter(|(slot, _)| **slot >= 100)
        .max_by_key(|(_, count)| **count)
        .map(|(slot, _)| *slot)
        .ok_or_else(|| "the panel carries no slot at or above 100".into())
}
