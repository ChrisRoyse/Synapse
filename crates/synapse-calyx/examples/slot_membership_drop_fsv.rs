//! Manual FSV instrument for #1903: does the observation ingest path silently
//! drop a re-measured constellation whose slot membership changed?
//!
//! The Source of Truth is the `Base` column-family row on disk, read back
//! through an independent vault handle after the write, not the `PutOutcome`
//! the write returned. `Base` carries the row's slot **membership** plus a
//! per-slot integrity hash (`prepared::PreparedConstellationEncoding`), so the
//! declared slot set of a stored row is directly observable.
//!
//! Four cases are exercised against one real durable vault in a scratch
//! directory:
//!
//! 0/1. re-put byte-identical    -> `ExistingIdentical`, membership {1,2}
//! 2.   re-put with slot 3 ADDED -> refused, naming `slots_added=[3]`
//! 3.   re-put with slot 2 GONE  -> refused, naming `slots_removed=[2]`
//! 4.   re-put with slot 2's vector CHANGED, same membership
//!                               -> refused, naming `slots_remeasured=[2]`
//!
//! For each case the Base membership is read before and after the call, so the
//! outcome is proven from stored bytes rather than from the return value. Case
//! 4 additionally reads slot 2's stored vector back, because a refusal that
//! half-wrote the vector would be worse than the silent drop it replaces.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example slot_membership_drop_fsv <scratch-dir>`

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr as _;

use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{
    Constellation, CxFlags, CxId, InputRef, LedgerRef, Modality, SlotId, SlotVector, SparseEntry,
    VaultId, VaultStore,
};

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETN";
const PANEL_VERSION: u32 = 1_903_001;

fn dense(seed: f32) -> SlotVector {
    SlotVector::Dense {
        dim: 4,
        data: vec![seed, seed + 1.0, seed + 2.0, seed + 3.0],
    }
}

fn sparse(dim: u32, cells: &[(u32, f32)]) -> SlotVector {
    SlotVector::Sparse {
        dim,
        entries: cells
            .iter()
            .map(|(idx, val)| SparseEntry {
                idx: *idx,
                val: *val,
            })
            .collect(),
    }
}

fn constellation(
    vault_id: VaultId,
    cx_id: CxId,
    slots: BTreeMap<SlotId, SlotVector>,
) -> Constellation {
    Constellation {
        cx_id,
        vault_id,
        panel_version: PANEL_VERSION,
        created_at: 1_785_000_000_000,
        input_ref: InputRef {
            hash: [9_u8; 32],
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

/// Reads the declared slot membership of a stored row straight off disk
/// through a freshly opened handle, so nothing in the writer's memory can
/// stand in for the persisted bytes.
fn membership_on_disk(dir: &PathBuf, cx_id: CxId) -> Result<Option<Vec<SlotId>>, Box<dyn Error>> {
    let vault_id = VaultId::from_str(VAULT_ID)?;
    let vault = AsterVault::open(
        dir,
        vault_id,
        b"slot-membership-drop-fsv".to_vec(),
        VaultOptions {
            read_only: true,
            ..VaultOptions::default()
        },
    )?;
    let latest = vault.latest_seq();
    match vault.get(cx_id, latest) {
        Ok(base) => Ok(Some(base.slots.keys().copied().collect())),
        Err(error) if error.to_string().contains("constellation missing") => Ok(None),
        Err(error) => Err(Box::new(error)),
    }
}

fn ids(slots: Option<&Vec<SlotId>>) -> String {
    slots.map_or_else(
        || "<absent>".to_owned(),
        |slots| {
            slots
                .iter()
                .map(SlotId::to_string)
                .collect::<Vec<_>>()
                .join(",")
        },
    )
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: slot_membership_drop_fsv <empty-scratch-vault-dir>")?;
    std::fs::create_dir_all(&dir)?;
    let vault_id = VaultId::from_str(VAULT_ID)?;
    let cx_id = CxId::from_bytes([
        0x19, 0x03, 0x33, 0x44, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    ]);

    let mut original: BTreeMap<SlotId, SlotVector> = BTreeMap::new();
    original.insert(SlotId::new(1), dense(1.0));
    original.insert(SlotId::new(2), sparse(64, &[(3, 1.0), (11, 2.0)]));

    let mut added = original.clone();
    added.insert(SlotId::new(3), sparse(2048, &[(7, 3.0)]));

    let mut removed = original.clone();
    removed.remove(&SlotId::new(2));

    println!("slot_membership_drop_fsv: vault_dir={}", dir.display());
    println!("cx_id={cx_id} panel_version={PANEL_VERSION}");

    // --- write the original row -------------------------------------------
    {
        let vault = AsterVault::open(
            &dir,
            vault_id,
            b"slot-membership-drop-fsv".to_vec(),
            VaultOptions::default(),
        )?;
        let before = membership_on_disk(&dir, cx_id);
        drop(before); // handle exclusivity: read after the writer closes
        let outcome =
            vault.put_observation_with_outcome(constellation(vault_id, cx_id, original.clone()))?;
        println!("CASE0 first_put disposition={:?}", outcome.disposition);
    }
    let after_insert = membership_on_disk(&dir, cx_id)?;
    println!(
        "CASE0 base_membership_on_disk={}",
        ids(after_insert.as_ref())
    );

    // --- case 1: byte-identical re-measure ---------------------------------
    let before = membership_on_disk(&dir, cx_id)?;
    let case1 = {
        let vault = AsterVault::open(
            &dir,
            vault_id,
            b"slot-membership-drop-fsv".to_vec(),
            VaultOptions::default(),
        )?;
        vault.put_observation_with_outcome(constellation(vault_id, cx_id, original.clone()))
    };
    let after = membership_on_disk(&dir, cx_id)?;
    println!(
        "CASE1 identical  before={} result={} after={}",
        ids(before.as_ref()),
        match &case1 {
            Ok(outcome) => format!("Ok({:?})", outcome.disposition),
            Err(error) => format!("Err({error})"),
        },
        ids(after.as_ref())
    );

    // --- case 2: slot 3 added ----------------------------------------------
    let before = membership_on_disk(&dir, cx_id)?;
    let case2 = {
        let vault = AsterVault::open(
            &dir,
            vault_id,
            b"slot-membership-drop-fsv".to_vec(),
            VaultOptions::default(),
        )?;
        vault.put_observation_with_outcome(constellation(vault_id, cx_id, added.clone()))
    };
    let after = membership_on_disk(&dir, cx_id)?;
    println!(
        "CASE2 slot_added before={} result={} after={}",
        ids(before.as_ref()),
        match &case2 {
            Ok(outcome) => format!("Ok({:?})", outcome.disposition),
            Err(error) => format!("Err({error})"),
        },
        ids(after.as_ref())
    );

    // --- case 3: slot 2 removed --------------------------------------------
    let before = membership_on_disk(&dir, cx_id)?;
    let case3 = {
        let vault = AsterVault::open(
            &dir,
            vault_id,
            b"slot-membership-drop-fsv".to_vec(),
            VaultOptions::default(),
        )?;
        vault.put_observation_with_outcome(constellation(vault_id, cx_id, removed.clone()))
    };
    let after = membership_on_disk(&dir, cx_id)?;
    println!(
        "CASE3 slot_removed before={} result={} after={}",
        ids(before.as_ref()),
        match &case3 {
            Ok(outcome) => format!("Ok({:?})", outcome.disposition),
            Err(error) => format!("Err({error})"),
        },
        ids(after.as_ref())
    );

    // --- case 4: same membership, one slot re-measured to a new vector ------
    // Membership alone is not the whole contract: the Base row also carries a
    // per-slot integrity hash, so a row whose declared slots match but whose
    // measured vectors differ is also a re-measurement that cannot land.
    let mut revalued = original.clone();
    revalued.insert(SlotId::new(2), sparse(64, &[(3, 9.0), (11, 2.0)]));
    let before = membership_on_disk(&dir, cx_id)?;
    let before_vector = stored_vector(&dir, cx_id, SlotId::new(2))?;
    let case4 = {
        let vault = AsterVault::open(
            &dir,
            vault_id,
            b"slot-membership-drop-fsv".to_vec(),
            VaultOptions::default(),
        )?;
        vault.put_observation_with_outcome(constellation(vault_id, cx_id, revalued.clone()))
    };
    let after = membership_on_disk(&dir, cx_id)?;
    let after_vector = stored_vector(&dir, cx_id, SlotId::new(2))?;
    println!(
        "CASE4 slot_revalued before={} result={} after={}",
        ids(before.as_ref()),
        match &case4 {
            Ok(outcome) => format!("Ok({:?})", outcome.disposition),
            Err(error) => format!("Err({error})"),
        },
        ids(after.as_ref())
    );
    println!("CASE4 slot_2_vector_before={before_vector}");
    println!("CASE4 slot_2_vector_after ={after_vector}");

    let _ = (&case1, &case2, &case3, &case4);
    Ok(())
}

/// Reads slot 2's stored vector back off disk so a refused re-measurement can
/// be shown not to have half-written the vector either.
fn stored_vector(dir: &PathBuf, cx_id: CxId, slot: SlotId) -> Result<String, Box<dyn Error>> {
    let vault_id = VaultId::from_str(VAULT_ID)?;
    let vault = AsterVault::open(
        dir,
        vault_id,
        b"slot-membership-drop-fsv".to_vec(),
        VaultOptions {
            read_only: true,
            ..VaultOptions::default()
        },
    )?;
    let latest = vault.latest_seq();
    Ok(match vault.read_slot_vector_at(latest, cx_id, slot)? {
        Some(SlotVector::Sparse { dim, entries }) => format!(
            "Sparse dim={dim} entries=[{}]",
            entries
                .iter()
                .map(|entry| format!("{}:{}", entry.idx, entry.val))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Some(other) => format!("{other:?}"),
        None => "<absent>".to_owned(),
    })
}
