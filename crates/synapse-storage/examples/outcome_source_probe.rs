//! Read-only source census for the outcome panel migration (#1965).

use std::{collections::BTreeMap, error::Error, path::PathBuf};

use calyx_aster::{cf::ColumnFamily, vault::encode::decode_constellation_base};
use calyx_core::{SlotId, SlotVector};
use serde_json::Value;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault};
use synapse_storage::{
    StorageBackendKind, cf,
    constellations::{
        META_SOURCE_CF, META_SOURCE_KEY_HEX, NativeConstellationContext, SYN_OUTCOME_KEY_PREFIX,
        SYN_OUTCOME_PANEL_NAME, build_outcome_constellation, outcome_constellation_input_bytes,
    },
    scan_cf_read_only,
};

fn main() -> Result<(), Box<dyn Error>> {
    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: outcome_source_probe <vault>")?,
    );
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(path.clone()),
        None,
    )?;
    let snapshot = vault.latest_seq();
    let mut by_cf_version = BTreeMap::<(String, u32), u64>::new();
    let mut missing_identity = 0_u64;
    let mut key_prefixes = BTreeMap::<String, u64>::new();
    for (_, bytes) in vault.scan_cf_at(snapshot, ColumnFamily::Base)? {
        let cx = decode_constellation_base(&bytes)?;
        if cx.metadata.get("synapse_panel_name").map(String::as_str) != Some(SYN_OUTCOME_PANEL_NAME)
        {
            continue;
        }
        let Some(source_cf) = cx.metadata.get(META_SOURCE_CF) else {
            missing_identity += 1;
            continue;
        };
        let Some(key_hex) = cx.metadata.get(META_SOURCE_KEY_HEX) else {
            missing_identity += 1;
            continue;
        };
        let key = decode_hex(key_hex)?;
        let prefix_len = key
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'/')
            .nth(2)
            .map_or(key.len(), |(at, _)| at + 1);
        let prefix = String::from_utf8_lossy(&key[..prefix_len]).into_owned();
        *key_prefixes.entry(prefix).or_default() += 1;
        *by_cf_version
            .entry((source_cf.clone(), cx.panel_version))
            .or_default() += 1;
    }
    println!(
        "source_of_truth={} snapshot={snapshot} missing_identity={missing_identity}",
        path.display()
    );
    for ((source_cf, version), rows) in by_cf_version {
        println!("source_cf={source_cf} panel_version={version} base_rows={rows}");
    }
    for (prefix, rows) in key_prefixes {
        println!("key_prefix={prefix:?} base_rows={rows}");
    }
    let rows = scan_cf_read_only(
        &path,
        synapse_core::SCHEMA_VERSION,
        StorageBackendKind::Calyx,
        cf::CF_KV,
    )?;
    let mut distinct = std::collections::BTreeSet::new();
    let mut measured = 0_u64;
    for (key, raw) in rows
        .into_iter()
        .filter(|(key, _)| key.starts_with(SYN_OUTCOME_KEY_PREFIX))
    {
        let record: Value = serde_json::from_slice(&raw)?;
        let input = outcome_constellation_input_bytes(cf::CF_KV, &key, &raw);
        let cx = build_outcome_constellation(context()?, cf::CF_KV, &key, &raw, &input, &record)?;
        let Some(SlotVector::Dense { data, .. }) = cx.slots.get(&SlotId::new(116)) else {
            return Err("candidate slot 116 is not dense".into());
        };
        let bytes: Vec<u8> = data
            .iter()
            .flat_map(|value| value.to_bits().to_be_bytes())
            .collect();
        distinct.insert(bytes);
        measured += 1;
    }
    println!(
        "candidate_slot=116 measured={measured} distinct_vectors={}",
        distinct.len()
    );
    if measured != 18 || distinct.len() <= 1 {
        return Err("candidate outcome vector is incomplete or constant".into());
    }
    Ok(())
}

fn context() -> Result<NativeConstellationContext, Box<dyn Error>> {
    Ok(NativeConstellationContext {
        vault_id: "00000000000000000000000003".parse()?,
        cx_id: calyx_core::CxId::from_bytes([4; 16]),
        created_at_ms: 1_760_000_000_000,
        next_ledger_seq: 1,
    })
}

fn decode_hex(value: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    if !value.len().is_multiple_of(2) {
        return Err("odd-length source key hex".into());
    }
    (0..value.len())
        .step_by(2)
        .map(|at| Ok(u8::from_str_radix(&value[at..at + 2], 16)?))
        .collect()
}
