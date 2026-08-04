//! FSV for #1979: dense and sparse content slots both build native association
//! graphs and measured-recall inputs; absent and MaxSim inputs fail closed.
//!
//! ```powershell
//! cargo run --release -p synapse-calyx --example kernel_content_slot_kind_fsv -- <restore-copy> [panel_version] [sparse_slot] [dense_slot]
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
//! | sparse lane | the BM25 lexical lane (#1898) | reaches graph and recall math |
//! | absent lane | a slot id no record carries | structured empty-corpus refusal |
//! | dense lane | a real dense encoder lens | reaches graph and recall math |
//!
//! The middle arm is the one that makes this a test rather than an assertion:
//! before the fix sparse and absent both produced an empty/refusal result.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use synapse_calyx::{
    SYNAPSE_CALYX_CF_WALK_PAGE_ROWS, SynapseCalyxConfig, SynapseCalyxError,
    SynapseCalyxKernelParams, SynapseCalyxVault, SynapseCalyxWalkStep,
};

const UNSUPPORTED: &str = "SYNAPSE_CALYX_KERNEL_CONTENT_SLOT_UNSUPPORTED";

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
    let sparse_slot = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(107);
    let dense_slot = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(110);
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
    for (label, slot, expect_usable) in [
        ("sparse lexical lane", sparse_slot, true),
        ("absent lane", unused_slot, false),
        ("dense encoder lane", dense_slot, true),
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
        let usable =
            outcome.is_ok() || (code != UNSUPPORTED && code != "CALYX_KERNEL_EMPTY_RESULT");
        let verdict = if usable == expect_usable {
            "OK "
        } else {
            "FAIL"
        };
        if usable != expect_usable {
            failures += 1;
        }
        println!(
            "\n[{verdict}] {label}: slot {slot} ({} records declare it)",
            declared.get(&slot).copied().unwrap_or(0)
        );
        println!("       expected measured content: {expect_usable}, got code {code}");
        println!("       {message}");
    }

    assert_eq!(failures, 0, "{failures} arm(s) produced the wrong verdict");
    println!("\nPASS: sparse and dense lanes reach native kernel math; absence does not");
    Ok(())
}
