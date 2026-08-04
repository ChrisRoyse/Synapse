use std::{error::Error, path::PathBuf};

use calyx_aster::{
    cf::ColumnFamily,
    vault::encode::{decode_constellation_base, decode_slot_vector},
};
use calyx_core::{Asymmetry, Modality, QuantPolicy, SlotId, SlotShape};
use calyx_registry::{
    LensRuntime, LensSpec, NormPolicy, derive_runtime_contract_from_spec,
    lens_spec_with_frozen_contract,
};
use sha2::{Digest, Sha256};
use synapse_core::types::{EpisodeBoundary, EpisodeRecord, TimelineActor};
use synapse_storage::{Db, SYN_EPISODE_PANEL_VERSION, cf};

const SCHEMA_VERSION: u32 = 1;
const ADD_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn main() -> Result<(), Box<dyn Error>> {
    let vault_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: panel_lifecycle_end_to_end_fsv <absent-vault-dir>")?;
    if vault_dir.exists() {
        return Err(format!("vault path must be absent: {}", vault_dir.display()).into());
    }
    let parent = vault_dir
        .parent()
        .ok_or("vault path has no parent")?
        .to_path_buf();
    println!("SOURCE OF TRUTH: {}", vault_dir.display());
    let db = Db::open(&vault_dir, SCHEMA_VERSION)?;

    let historical = [
        episode("fsv-old-1", 1_000_000_000),
        episode("fsv-old-2", 2_000_000_000),
    ];
    for record in &historical {
        persist_episode(&db, record)?;
    }
    println!(
        "BEFORE add: source_rows=2 lifecycle={:?}",
        db.read_panel_lifecycle(SYN_EPISODE_PANEL_VERSION)?
    );

    let add = db.add_panel_lens(
        SYN_EPISODE_PANEL_VERSION,
        ADD_ID,
        "fsv.byte_features",
        byte_spec()?,
        synapse_calyx::panel_lifecycle::SynapseCalyxSourceProjection::RawSourceBytes,
    )?;
    println!(
        "AFTER add: target_panel={} slot={} queued={} seq={} sha256={}",
        add.state.controller.panel().version,
        add.slot_id,
        add.queued,
        add.committed_seq,
        add.value_sha256
    );
    ensure(
        add.queued == 2,
        "add did not queue the two physical historical Base rows",
    )?;

    let page1 = db.run_panel_backfill(SYN_EPISODE_PANEL_VERSION, 1, false)?;
    let page2 = db.run_panel_backfill(SYN_EPISODE_PANEL_VERSION, 1, false)?;
    println!(
        "BACKFILL page1: claimed={} completed={} pending={} verified={:?}",
        page1.claimed, page1.completed, page1.pending, page1.verified_cx_ids
    );
    println!(
        "BACKFILL page2: claimed={} completed={} pending={} verified={:?}",
        page2.claimed, page2.completed, page2.pending, page2.verified_cx_ids
    );
    ensure(
        page2.pending == 0 && page2.completed_total == 2,
        "durable queue did not complete",
    )?;

    let live = episode("fsv-live-3", 3_000_000_000);
    persist_episode(&db, &live)?;
    println!("AFTER live ingest: source_rows=3 (dual generation write required)");

    let rebuild = db.rebuild_calyx_search_indexes(add.state.controller.panel().version)?;
    ensure(
        rebuild
            .generation
            .slots
            .iter()
            .any(|entry| entry.panel_slot.slot_id() == SlotId::new(add.slot_id)),
        "search generation omitted the dynamically added slot",
    )?;
    let find = db.find_similar(&synapse_calyx::SynapseCalyxFindParams {
        query: synapse_calyx::SynapseCalyxFindQuery::ByExample {
            cx_id: page1.verified_cx_ids[0].clone(),
        },
        k: 3,
        fusion: synapse_calyx::SynapseCalyxFindFusion::SingleSlot { slot: add.slot_id },
        filter: None,
        explain: true,
        temporal: None,
        panel_version: Some(add.state.controller.panel().version),
        guard: synapse_calyx::SynapseCalyxFindGuardMode::Off,
    })?;
    println!(
        "SEARCH READBACK: manifest_sha256={} hits={} consulted_slots={:?}",
        rebuild.generation.manifest_sha256,
        find.hits.len(),
        find.consulted_slots
    );
    ensure(
        !find.hits.is_empty(),
        "new lifecycle slot was not searchable",
    )?;

    let before_edges = serde_json::to_vec(&db.read_panel_lifecycle(SYN_EPISODE_PANEL_VERSION)?)?;
    println!(
        "EDGE 1 BEFORE invalid-limit: registry_sha256={}",
        hash(&before_edges)
    );
    let edge1 = expected_error(
        db.run_panel_backfill(SYN_EPISODE_PANEL_VERSION, 0, false),
        "invalid backfill limit",
    )?;
    let after_edge1 = serde_json::to_vec(&db.read_panel_lifecycle(SYN_EPISODE_PANEL_VERSION)?)?;
    println!(
        "EDGE 1 AFTER invalid-limit: error={} registry_sha256={}",
        edge1,
        hash(&after_edge1)
    );
    ensure(
        before_edges == after_edge1,
        "invalid limit mutated lifecycle state",
    )?;

    println!(
        "EDGE 2 BEFORE unknown-panel: registry_sha256={}",
        hash(&after_edge1)
    );
    let edge2 = expected_error(
        db.read_panel_lifecycle(u32::MAX),
        "unknown panel lifecycle read",
    )?;
    let after_edge2 = serde_json::to_vec(&db.read_panel_lifecycle(SYN_EPISODE_PANEL_VERSION)?)?;
    println!(
        "EDGE 2 AFTER unknown-panel: error={} registry_sha256={}",
        edge2,
        hash(&after_edge2)
    );
    ensure(
        after_edge1 == after_edge2,
        "unknown panel read mutated lifecycle state",
    )?;

    println!(
        "EDGE 3 BEFORE invalid-operation-id: registry_sha256={}",
        hash(&after_edge2)
    );
    let edge3 = expected_error(
        db.add_panel_lens(
            SYN_EPISODE_PANEL_VERSION,
            "BAD",
            "fsv.invalid",
            byte_spec()?,
            synapse_calyx::panel_lifecycle::SynapseCalyxSourceProjection::RawSourceBytes,
        ),
        "invalid lifecycle operation id",
    )?;
    let after_edge3 = serde_json::to_vec(&db.read_panel_lifecycle(SYN_EPISODE_PANEL_VERSION)?)?;
    println!(
        "EDGE 3 AFTER invalid-operation-id: error={} registry_sha256={}",
        edge3,
        hash(&after_edge3)
    );
    ensure(
        after_edge2 == after_edge3,
        "invalid operation id mutated lifecycle state",
    )?;

    let target_panel = add.state.controller.panel().version;
    let slot = SlotId::new(add.slot_id);
    db.close_calyx_vault("panel lifecycle end-to-end FSV physical read")?;
    drop(db);

    let readonly = synapse_calyx::SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        synapse_calyx::SynapseCalyxConfig {
            vault_dir,
            machine_salt_path: parent.join("machine-salt.bin"),
            tuning: synapse_calyx::SynapseCalyxTuningConfig::default().validate()?,
        },
        Some(vec![
            ColumnFamily::Registry,
            ColumnFamily::Base,
            ColumnFamily::slot(slot),
        ]),
    )?;
    let registry = readonly.scan_cf_latest(ColumnFamily::Registry)?;
    let base = readonly.scan_cf_latest(ColumnFamily::Base)?;
    let slot_rows = readonly.scan_cf_latest(ColumnFamily::slot(slot))?;
    let target_base = base
        .iter()
        .filter_map(|(_, bytes)| decode_constellation_base(bytes).ok())
        .filter(|cx| cx.panel_version == target_panel)
        .count();
    let decoded_slot = slot_rows
        .iter()
        .filter(|(_, bytes)| decode_slot_vector(bytes).is_ok())
        .count();
    println!(
        "PHYSICAL READBACK: Registry_rows={} Base_target_panel_rows={} Slot_{}_rows={} decoded_slot_rows={}",
        registry.len(),
        target_base,
        slot.get(),
        slot_rows.len(),
        decoded_slot
    );
    for (key, bytes) in &slot_rows {
        println!(
            "PHYSICAL SLOT key={} value_sha256={}",
            hex(key),
            hash(bytes)
        );
    }
    ensure(
        target_base == 3,
        "new lifecycle generation does not contain all three records",
    )?;
    ensure(
        slot_rows.len() == 3 && decoded_slot == 3,
        "new Slot CF does not contain three valid vectors",
    )?;
    println!("VERDICT: PASS");
    Ok(())
}

fn persist_episode(db: &Db, record: &EpisodeRecord) -> Result<(), Box<dyn Error>> {
    let key = record.episode_id.as_bytes().to_vec();
    let raw = serde_json::to_vec(record)?;
    db.put_batch(cf::CF_EPISODES, [(key.clone(), raw.clone())])?;
    db.put_episode_constellation(&key, &raw, record)?;
    Ok(())
}

fn episode(id: &str, start: u64) -> EpisodeRecord {
    EpisodeRecord {
        record_version: 1,
        ts_ns: start,
        episode_id: id.to_owned(),
        start_ts_ns: start,
        end_ts_ns: start + 500_000_000,
        actor: TimelineActor::Human,
        app: Some("notepad.exe".to_owned()),
        document: Some(format!("{id}.txt")),
        url: None,
        title_first: Some(format!("{id} first")),
        title_last: Some(format!("{id} last")),
        distinct_title_count: 2,
        row_count: 2,
        keystroke_count: 4,
        click_count: 1,
        interruption_count: 0,
        interrupted_ms: 0,
        started_because: EpisodeBoundary::RangeEdge,
        ended_because: EpisodeBoundary::RangeEdge,
    }
}

fn byte_spec() -> Result<LensSpec, Box<dyn Error>> {
    let provisional = LensSpec {
        name: "syn.fsv.episode.byte_features.v1".to_owned(),
        runtime: LensRuntime::Algorithmic {
            kind: "byte_features".to_owned(),
        },
        output: SlotShape::Dense(16),
        modality: Modality::Structured,
        weights_sha256: [0; 32],
        corpus_hash: [0; 32],
        norm_policy: NormPolicy::finite_only(),
        max_batch: None,
        axis: Some("fsv".to_owned()),
        asymmetry: Asymmetry::None,
        quant_default: QuantPolicy::None,
        truncate_dim: None,
        recall_delta: 0.0,
        retrieval_only: false,
        excluded_from_dedup: false,
    };
    let contract = derive_runtime_contract_from_spec(&provisional)?;
    Ok(lens_spec_with_frozen_contract(provisional, &contract))
}

fn expected_error<T, E>(result: Result<T, E>, context: &str) -> Result<E, Box<dyn Error>> {
    match result {
        Ok(_) => Err(format!("{context} unexpectedly succeeded").into()),
        Err(error) => Ok(error),
    }
}

fn ensure(value: bool, message: &str) -> Result<(), Box<dyn Error>> {
    value.then_some(()).ok_or_else(|| message.to_owned().into())
}

fn hash(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_ref())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
