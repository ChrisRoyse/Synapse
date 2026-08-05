//! Manual Full State Verification for persisted dense-index quantization.
//!
//! Usage: `cargo run -p synapse-storage --example quantized_search_fsv -- <empty-dir>`

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::{CxId, SlotId, SlotVector, VaultId, VaultStore as _};
use calyx_registry::VaultPanelState;
use calyx_search::{PersistedDenseIndexConfig, PersistedSearchIndexes};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_TIMELINE_PANEL_VERSION, build_timeline_constellation,
    syn_active_panel_contract,
};

const VAULT_ID: &str = "01KYN9878AFNR5ESEDB1S5AETQ";
const CREATED_AT_MS: u64 = 1_785_000_000_000;
const ROWS: u8 = 24;
const QUANTIZED_SLOT: u16 = 104;

fn sha256(path: &Path) -> Result<String, Box<dyn Error>> {
    Ok(hex_digest(&fs::read(path)?))
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn row(vault_id: VaultId, tag: u8) -> Result<calyx_core::Constellation, Box<dyn Error>> {
    let record = TimelineRecord {
        record_version: 1,
        ts_ns: 1_785_000_000_000_000_000 + u64::from(tag) * 3_600_000_000_000,
        kind: TimelineKind::TitleChange,
        actor: TimelineActor::Human,
        app: Some(format!("fsv-app-{tag}.exe")),
        payload: json!({
            "title": format!("quantized search proof row {tag} {}", "length".repeat(usize::from(tag)))
        }),
    };
    let raw = serde_json::to_vec(&record)?;
    let context = NativeConstellationContext {
        vault_id,
        cx_id: CxId::from_bytes([0x16, 0x81, tag, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        created_at_ms: CREATED_AT_MS + u64::from(tag),
        next_ledger_seq: u64::from(tag) + 1,
    };
    Ok(build_timeline_constellation(
        context,
        format!("fsv-1681/timeline-{tag}").as_bytes(),
        &raw,
        &record,
    )?)
}

fn find_quantized_slot(manifest: &Value) -> Result<(SlotId, &Value), Box<dyn Error>> {
    let entry = manifest["slots"]
        .as_array()
        .ok_or("manifest slots is not an array")?
        .iter()
        .find(|entry| entry.get("dense_quantization").is_some())
        .ok_or("manifest contains no quantized dense slot")?;
    let slot = u16::try_from(entry["slot"].as_u64().ok_or("slot is not an integer")?)?;
    Ok((SlotId::new(slot), &entry["dense_quantization"]))
}

fn query(
    vault_dir: &Path,
    slot: SlotId,
    vector: &SlotVector,
) -> Result<Vec<calyx_sextant::index::IndexSearchHit>, Box<dyn Error>> {
    Ok(
        PersistedSearchIndexes::open(vault_dir, SYN_TIMELINE_PANEL_VERSION)?
            .search(slot, vector, 3)?,
    )
}

fn dense(vector: SlotVector) -> Result<Vec<f32>, Box<dyn Error>> {
    match vector {
        SlotVector::Dense { data, .. } => Ok(data),
        other => Err(format!("expected dense vector, observed {other:?}").into()),
    }
}

fn cosine(left: &[f32], right: &[f32]) -> f32 {
    let (mut dot, mut left_norm, mut right_norm) = (0.0_f32, 0.0_f32, 0.0_f32);
    for (left, right) in left.iter().zip(right) {
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: quantized_search_fsv <empty-dir>")?;
    fs::create_dir_all(&dir)?;
    if fs::read_dir(&dir)?.next().is_some() {
        return Err(format!("scratch directory is not empty: {}", dir.display()).into());
    }

    let vault_id: VaultId = VAULT_ID.parse()?;
    let vault = AsterVault::open(
        &dir,
        vault_id,
        b"quantized-search-fsv".to_vec(),
        VaultOptions::default(),
    )?;
    let contract = syn_active_panel_contract(SYN_TIMELINE_PANEL_VERSION, CREATED_AT_MS)?
        .ok_or("timeline panel contract missing")?;
    let state = VaultPanelState {
        panel: contract.panel,
        registry: contract.registry,
        registry_snapshot: None,
    };
    let mut ids = Vec::new();
    for tag in 1..=ROWS {
        let constellation = row(vault_id, tag)?;
        ids.push(constellation.cx_id);
        vault.put(constellation)?;
    }
    println!(
        "HAPPY before seq={} rows={ROWS} manifest_exists=false",
        vault.latest_seq()
    );

    let config = PersistedDenseIndexConfig {
        ef_search: usize::from(ROWS),
        quant_bits_by_slot: BTreeMap::from([(QUANTIZED_SLOT, 4)]),
        ..PersistedDenseIndexConfig::default()
    };
    calyx_search::rebuild_for_vault_with_panel_state_and_dense_config(
        &dir,
        &vault,
        &state,
        config.clone(),
    )?;
    let manifest_path = calyx_search::manifest_path(&dir, SYN_TIMELINE_PANEL_VERSION);
    let manifest_bytes = fs::read(&manifest_path)?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes)?;
    let (slot, quantization) = find_quantized_slot(&manifest)?;
    let pq_path = dir.join(quantization["pq_rel"].as_str().ok_or("pq_rel missing")?);
    let raw_path = dir.join(quantization["raw_rel"].as_str().ok_or("raw_rel missing")?);
    let manifest_sha = hex_digest(&manifest_bytes);
    let pq_sha = sha256(&pq_path)?;
    let raw_sha = sha256(&raw_path)?;
    println!(
        "HAPPY disk manifest={} sha256={manifest_sha} slot={} bits={} pq={} bytes={} sha256={pq_sha} raw={} bytes={} sha256={raw_sha}",
        manifest_path.display(),
        slot,
        quantization["bits"],
        pq_path.display(),
        fs::metadata(&pq_path)?.len(),
        raw_path.display(),
        fs::metadata(&raw_path)?.len(),
    );
    if quantization["pq_sha256"] != pq_sha || quantization["raw_sha256"] != raw_sha {
        return Err("physical sidecar hashes disagree with the manifest".into());
    }
    let expected = ids[7];
    let vector = vault
        .read_slot_vector_at(vault.latest_seq(), expected, slot)?
        .ok_or("known row has no vector for quantized slot")?;
    let hits = query(&dir, slot, &vector)?;
    println!("HAPPY readback expected={expected} hits={hits:?}");
    if hits.first().map(|hit| hit.cx_id) != Some(expected) {
        return Err("exact rerank did not return the known self-match first".into());
    }

    let mut corpus = Vec::new();
    for id in &ids {
        let stored = vault
            .read_slot_vector_at(vault.latest_seq(), *id, slot)?
            .ok_or("corpus row lost its quantized slot")?;
        corpus.push((*id, dense(stored)?));
    }
    let mut recovered = 0_usize;
    let mut expected_neighbors = 0_usize;
    for (query_id, query_data) in &corpus {
        let mut exact = corpus
            .iter()
            .map(|(id, data)| (*id, cosine(query_data, data)))
            .collect::<Vec<_>>();
        exact.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        let exact_ids = exact
            .iter()
            .take(3)
            .map(|(id, _)| *id)
            .collect::<BTreeSet<_>>();
        let production_ids = query(
            &dir,
            slot,
            &SlotVector::Dense {
                dim: u32::try_from(query_data.len())?,
                data: query_data.clone(),
            },
        )?
        .into_iter()
        .map(|hit| hit.cx_id)
        .collect::<BTreeSet<_>>();
        let matched = exact_ids.intersection(&production_ids).count();
        println!(
            "RECALL query={query_id} exact={exact_ids:?} production={production_ids:?} matched={matched}/3"
        );
        recovered += matched;
        expected_neighbors += exact_ids.len();
    }
    let recall = recovered as f64 / expected_neighbors as f64;
    println!("RECALL aggregate={recovered}/{expected_neighbors} recall={recall:.6}");
    if recovered != expected_neighbors {
        return Err(format!("quantized candidate path recall {recall:.6} is below 1.0").into());
    }

    println!("EDGE invalid-bits before manifest_sha256={manifest_sha}");
    let invalid = PersistedDenseIndexConfig {
        quant_bits_by_slot: BTreeMap::from([(QUANTIZED_SLOT, 5)]),
        ..config
    };
    let invalid_error = match calyx_search::rebuild_for_vault_with_panel_state_and_dense_config(
        &dir, &vault, &state, invalid,
    ) {
        Ok(()) => return Err("5-bit quantization unexpectedly succeeded".into()),
        Err(error) => error,
    };
    let after_invalid_sha = sha256(&manifest_path)?;
    println!("EDGE invalid-bits error={invalid_error} after manifest_sha256={after_invalid_sha}");
    if after_invalid_sha != manifest_sha {
        return Err("invalid config mutated manifest".into());
    }

    let original_pq = fs::read(&pq_path)?;
    println!("EDGE corrupt-pq before sha256={pq_sha}");
    let mut corrupted = original_pq.clone();
    corrupted[0] ^= 0xff;
    fs::write(&pq_path, &corrupted)?;
    let corrupt_error = match query(&dir, slot, &vector) {
        Ok(_) => return Err("corrupt PQ unexpectedly produced search results".into()),
        Err(error) => error,
    };
    println!(
        "EDGE corrupt-pq error={corrupt_error} after sha256={}",
        sha256(&pq_path)?
    );
    fs::write(&pq_path, &original_pq)?;

    let held_raw = raw_path.with_extension("raw.held-for-fsv");
    println!(
        "EDGE missing-raw before exists={} sha256={raw_sha}",
        raw_path.is_file()
    );
    fs::rename(&raw_path, &held_raw)?;
    let missing_error = match query(&dir, slot, &vector) {
        Ok(_) => return Err("missing raw sidecar unexpectedly produced search results".into()),
        Err(error) => error,
    };
    println!(
        "EDGE missing-raw error={missing_error} after exists={}",
        raw_path.is_file()
    );
    fs::rename(&held_raw, &raw_path)?;

    let final_hits = query(&dir, slot, &vector)?;
    println!(
        "FINAL manifest_sha256={} pq_sha256={} raw_sha256={} expected={} first={:?}",
        sha256(&manifest_path)?,
        sha256(&pq_path)?,
        sha256(&raw_path)?,
        expected,
        final_hits.first().map(|hit| hit.cx_id)
    );
    if sha256(&manifest_path)? != manifest_sha
        || sha256(&pq_path)? != pq_sha
        || sha256(&raw_path)? != raw_sha
        || final_hits.first().map(|hit| hit.cx_id) != Some(expected)
    {
        return Err("final independently-read state did not restore the proven generation".into());
    }
    Ok(())
}
