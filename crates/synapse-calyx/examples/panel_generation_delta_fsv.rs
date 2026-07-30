//! Manual FSV instrument for #1905: does one live panel generation's
//! reconciliation delta charge itself for another generation's ingest?
//!
//! ## The construction the issue asks for
//!
//! On the live vault only one generation of the timeline panel ingests, so the
//! defect is latent and cannot be observed there. This builds the missing
//! condition deliberately: **two generations of the same panel, both holding
//! live rows, both measuring the same slot id.**
//!
//! ```text
//!   panel OLD (1905001)  rows measuring slot 3  ->  cf/slot_03
//!   panel NEW (1905002)  rows measuring slot 3  ->  cf/slot_03   (same CF)
//! ```
//!
//! Slot ids are allocated per panel *name*, not per panel *version* (#1776), so
//! both generations share one physical `cf/slot_03`. A delta that scans that CF
//! unscoped sees both generations' keys.
//!
//! ## The known answer
//!
//! After pinning a base sequence, **exactly one row is written, and it belongs
//! to the OLD generation.** So:
//!
//! | measured for | `distinct_changed` must be | because |
//! |---|---|---|
//! | OLD | 1 | its own row changed |
//! | NEW | **0** | nothing of its own changed |
//!
//! The raw `slot_03=` count is printed alongside, and it is `1` for both — that
//! number is what the pre-#1905 delta unioned into `changed`, so seeing
//! `slot_03=1` next to `distinct_changed=0` is the before-and-after in a single
//! line: the scan still observes the key, and the delta no longer charges for
//! it.
//!
//! Source of Truth is the vault's own MVCC changed-key journal read through the
//! production `calyx_search::measure_panel_delta` — the same function the query
//! path and the unattended maintainer both call.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example panel_generation_delta_fsv -- <scratch-dir>`

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;

use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{
    Constellation, CxFlags, CxId, InputRef, LedgerRef, Modality, SlotId, SlotVector, SparseEntry,
    VaultId, VaultStore as _,
};

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const PANEL_OLD: u32 = 1_905_001;
const PANEL_NEW: u32 = 1_905_002;
const SHARED_SLOT: u16 = 3;

fn row(vault_id: VaultId, tag: u8, panel_version: u32) -> Constellation {
    let mut slots: BTreeMap<SlotId, SlotVector> = BTreeMap::new();
    slots.insert(
        SlotId::new(SHARED_SLOT),
        SlotVector::Sparse {
            dim: 2048,
            entries: vec![SparseEntry {
                idx: u32::from(tag),
                val: 1.0,
            }],
        },
    );
    Constellation {
        // Distinct per (tag, panel) so the two generations hold distinct rows,
        // exactly as a re-measure into a new panel version would produce.
        cx_id: CxId::from_bytes([
            0x19,
            0x05,
            tag,
            u8::try_from(panel_version % 251).unwrap_or(0),
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]),
        vault_id,
        panel_version,
        created_at: 1_785_000_000_000 + u64::from(tag),
        input_ref: InputRef {
            hash: [tag; 32],
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

fn report(label: &str, composition: &calyx_search::PanelDeltaComposition) {
    println!("  {label:<28} {}", composition.composition());
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: panel_generation_delta_fsv <empty-scratch-vault-dir>")?;
    std::fs::create_dir_all(&dir)?;
    let vault_id = VaultId::from_str(VAULT_ID)?;
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"panel-generation-delta-fsv".to_vec(),
        VaultOptions::default(),
    )?;

    println!("panel_generation_delta_fsv: vault_dir={}", dir.display());
    println!("two live generations of one panel, both measuring slot {SHARED_SLOT}");

    // --- both generations hold live rows -----------------------------------
    for tag in 1..=3_u8 {
        vault.put(row(vault_id, tag, PANEL_OLD))?;
    }
    for tag in 1..=2_u8 {
        vault.put(row(vault_id, tag, PANEL_NEW))?;
    }
    let base_seq = vault.latest_seq();
    println!("\nboth generations populated; pinned base_seq={base_seq}");

    // Baseline: nothing has changed after base_seq yet, for either generation.
    let slots = [SlotId::new(SHARED_SLOT)];
    let snapshot = vault.pin_reader(calyx_aster::mvcc::Freshness::FreshDerived, SEARCH_LEASE_MS);
    let before_old =
        calyx_search::measure_panel_delta(&vault, snapshot, PANEL_OLD, base_seq, slots)?;
    let before_new =
        calyx_search::measure_panel_delta(&vault, snapshot, PANEL_NEW, base_seq, slots)?;
    let _ = vault.release_reader(snapshot.lease().id());
    println!("BEFORE (no writes after base_seq)");
    report("measured_for_OLD", &before_old);
    report("measured_for_NEW", &before_new);

    // --- the trigger: ONE write, to the OLD generation only ----------------
    let written = row(vault_id, 9, PANEL_OLD);
    let written_id = written.cx_id;
    vault.put(written)?;
    println!("\nTRIGGER wrote 1 row cx_id={written_id} panel={PANEL_OLD} (slot {SHARED_SLOT})");
    println!("        latest_seq now {}", vault.latest_seq());

    let snapshot = vault.pin_reader(calyx_aster::mvcc::Freshness::FreshDerived, SEARCH_LEASE_MS);
    let after_old =
        calyx_search::measure_panel_delta(&vault, snapshot, PANEL_OLD, base_seq, slots)?;
    let after_new =
        calyx_search::measure_panel_delta(&vault, snapshot, PANEL_NEW, base_seq, slots)?;
    let _ = vault.release_reader(snapshot.lease().id());
    println!("AFTER");
    report("measured_for_OLD", &after_old);
    report("measured_for_NEW", &after_new);

    println!("\nEXPECTED  OLD distinct_changed=1   NEW distinct_changed=0 slot_other_generation=1");
    println!(
        "OBSERVED  OLD distinct_changed={}   NEW distinct_changed={} slot_other_generation={}",
        after_old.changed_len(),
        after_new.changed_len(),
        after_new.slot_keys_other_generation
    );
    let pass = after_old.changed_len() == 1
        && after_new.changed_len() == 0
        && after_new.slot_keys_other_generation == 1;
    println!("VERDICT   newer_generation_unmoved_by_older_ingest={pass}");
    Ok(())
}

const SEARCH_LEASE_MS: u64 = 30_000;
