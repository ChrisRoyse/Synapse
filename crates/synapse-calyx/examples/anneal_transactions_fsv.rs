//! Manual full-state verification for Synapse's native Anneal transaction path.
//!
//! The replay is intentionally minimal but real: it executes the production
//! RRF implementation over two known rankings, measures actual elapsed search
//! work, classifies a labelled guard corpus, and writes/syncs/reads canary bytes
//! on the host filesystem. Acceptance comes from the independent read-only
//! reopen of Kv, AnnealRollback, and Ledger after the writer closes.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use calyx_anneal::{
    ActionMetricSnapshot, AnnealAction, ChangeOutcome, HeldOutReplay, ReplayAnchor, ReplayQuery,
    TripwireMetric,
};
use calyx_aster::cf::ColumnFamily;
use calyx_core::{CxId, LedgerRef, Result as CalyxResult, SlotId};
use calyx_sextant::fusion::{FusionContext, FusionStrategy, fuse};
use calyx_sextant::index::IndexSearchHit;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use synapse_calyx::{
    SynapseCalyxConfig, SynapseCalyxReadOnlyVault, SynapseCalyxTuningConfig, SynapseCalyxVault,
};

const PANEL_VERSION: u32 = 1;
const BENCHMARK_PASSES: usize = 101;

#[derive(Clone)]
struct MeasuredAction(ActionMetricSnapshot);

impl AnnealAction for MeasuredAction {
    fn apply_shadow(&self, _query: &ReplayQuery) -> CalyxResult<ActionMetricSnapshot> {
        Ok(self.0.clone())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| format!("initialize tracing subscriber: {error}"))?;
    let vault_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: anneal_transactions_fsv <new-vault-dir>")?;
    if vault_dir.exists() {
        return Err(format!(
            "ANNEAL_FSV_VAULT_NOT_NEW: {} already exists; use a new empty path",
            vault_dir.display()
        )
        .into());
    }
    let canary_path = vault_dir.with_extension("anneal-fsv-canary.bin");
    if canary_path.exists() {
        return Err(format!(
            "ANNEAL_FSV_CANARY_ALREADY_EXISTS: {}",
            canary_path.display()
        )
        .into());
    }

    println!(
        "{}",
        json!({
            "state": "before",
            "vault_exists": vault_dir.exists(),
            "canary_exists": canary_path.exists(),
        })
    );

    let config = SynapseCalyxConfig::from_vault_dir(vault_dir.clone());
    let vault = SynapseCalyxVault::open(config.clone())?;
    println!("{}", json!({"phase":"vault_open_complete"}));
    let baseline = vault.anneal_status()?;
    println!("{}", json!({"phase":"baseline_status_complete"}));
    let baseline_hash = baseline.live_artifact_sha256.clone();

    println!("{}", json!({"phase":"workload_measure_begin"}));
    let measured = measure_real_workload(&canary_path)?;
    println!("{}", json!({"phase":"workload_measure_complete"}));
    let good = MeasuredAction(metric_snapshot(&measured, measured.recall));
    let bad = MeasuredAction(metric_snapshot(&measured, 0.0));
    let replay = held_out_replay();

    // Edge 1: an empty replay must be persisted as a rejected native change and
    // must not move the live pointer.
    let mut empty_tuning = baseline.effective_tuning;
    empty_tuning.fusion_k = 7;
    let empty = vault.anneal_propose_tuning(
        empty_tuning,
        HeldOutReplay {
            queries: Vec::new(),
            seed: 1681,
        },
        &good,
        &good,
        "FSV edge: empty replay must reject",
    )?;
    require_reverted(&empty.outcome, "empty replay")?;
    require_hash(
        &empty.live_artifact_sha256_after,
        &baseline_hash,
        "empty replay changed live pointer",
    )?;

    // Edge 2: byte-identical tuning is rejected before a native change is
    // prepared. The structured error is printed as evidence.
    let unchanged_error = match vault.anneal_propose_tuning(
        baseline.effective_tuning,
        replay.clone(),
        &good,
        &good,
        "FSV edge: unchanged candidate",
    ) {
        Ok(_) => return Err("ANNEAL_FSV_UNCHANGED_CANDIDATE_ACCEPTED".into()),
        Err(error) => error,
    };
    if unchanged_error.code != "SYNAPSE_CALYX_ANNEAL_CANDIDATE_UNCHANGED" {
        return Err(format!("ANNEAL_FSV_WRONG_UNCHANGED_ERROR: {}", unchanged_error.code).into());
    }

    // Edge 3: an unknown rollback id must fail without changing the live row.
    let unknown_error = match vault.anneal_rollback(u64::MAX) {
        Ok(_) => return Err("ANNEAL_FSV_UNKNOWN_ROLLBACK_ACCEPTED".into()),
        Err(error) => error,
    };
    if unknown_error.code != "SYNAPSE_CALYX_ANNEAL_CHANGE_UNKNOWN" {
        return Err(format!("ANNEAL_FSV_WRONG_UNKNOWN_ERROR: {}", unknown_error.code).into());
    }
    require_hash(
        &vault.anneal_status()?.live_artifact_sha256,
        &baseline_hash,
        "unknown rollback changed live pointer",
    )?;

    // Happy path: the only changed value is the load-bearing production RRF k.
    let mut promoted_tuning = baseline.effective_tuning;
    promoted_tuning.fusion_k = 5;
    let promoted = vault.anneal_propose_tuning(
        promoted_tuning,
        replay.clone(),
        &good,
        &good,
        "FSV measured RRF k=5 promotion",
    )?;
    let promoted_change_id = require_promoted(&promoted.outcome)?;
    require_hash(
        &promoted.live_artifact_sha256_after,
        &promoted.candidate_artifact_sha256,
        "promotion did not select candidate bytes",
    )?;

    // Forced bad candidate: identical measured host/guard metrics but recall is
    // deliberately zero, so native per-metric non-regression must reject it.
    let mut bad_tuning = baseline.effective_tuning;
    bad_tuning.fusion_k = 6;
    let rejected = vault.anneal_propose_tuning(
        bad_tuning,
        replay,
        &bad,
        &good,
        "FSV forced-bad recall candidate",
    )?;
    require_reverted(&rejected.outcome, "forced-bad recall")?;
    require_hash(
        &rejected.live_artifact_sha256_after,
        &promoted.candidate_artifact_sha256,
        "rejected candidate changed live pointer",
    )?;

    let rollback = vault.anneal_rollback(promoted_change_id)?;
    require_hash(
        &rollback.restored_artifact_sha256,
        &baseline_hash,
        "explicit rollback did not restore baseline bytes",
    )?;
    let final_status = vault.anneal_status()?;
    require_hash(
        &final_status.live_artifact_sha256,
        &baseline_hash,
        "final status does not select baseline",
    )?;

    println!(
        "{}",
        json!({
            "state": "after_transactions_before_close",
            "baseline": baseline,
            "measured_workload": measured,
            "empty_replay": empty,
            "unchanged_error": unchanged_error,
            "unknown_rollback_error": unknown_error,
            "promotion": promoted,
            "forced_bad_rejection": rejected,
            "rollback": rollback,
            "final_status": final_status,
        })
    );
    vault.close("anneal_transactions_fsv")?;

    let inspector = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        config,
        Some(vec![
            ColumnFamily::Kv,
            ColumnFamily::AnnealRollback,
            ColumnFamily::Ledger,
        ]),
    )?;
    let artifacts = inspector.scan_anneal_tuning_artifacts_latest()?;
    let rollback_rows = inspector.scan_anneal_rollback_latest()?;
    let ledger_rows = inspector.scan_cf_latest(ColumnFamily::Ledger)?;
    let baseline_bytes = artifacts
        .iter()
        .find_map(|(_, value)| {
            (hex(&tuning_artifact_hash(value)) == baseline_hash).then_some(value)
        })
        .ok_or("ANNEAL_FSV_BASELINE_ARTIFACT_NOT_FOUND")?;
    let decoded: SynapseCalyxTuningConfig = serde_json::from_slice(baseline_bytes)?;
    if decoded != baseline.effective_tuning {
        return Err("ANNEAL_FSV_BASELINE_BYTES_CHANGED".into());
    }
    let canary_bytes = fs::read(&canary_path)?;
    let canary_sha256 = hex(&Sha256::digest(&canary_bytes));
    println!(
        "{}",
        json!({
            "state": "after_independent_read",
            "latest_seq": inspector.latest_seq(),
            "artifact_rows": artifacts.len(),
            "rollback_rows": rollback_rows.len(),
            "ledger_rows": ledger_rows.len(),
            "baseline_artifact_sha256": baseline_hash,
            "baseline_artifact_bytes": baseline_bytes.len(),
            "baseline_decoded": decoded,
            "canary_path": canary_path,
            "canary_bytes": canary_bytes.len(),
            "canary_sha256": canary_sha256,
            "rollback_physical": rollback_rows.iter().map(|(key, value)| json!({
                "key_hex": hex(key), "value_hex": hex(value)
            })).collect::<Vec<_>>(),
        })
    );
    drop(inspector);
    fs::remove_file(&canary_path)?;
    println!(
        "{}",
        json!({"state":"cleanup_readback","canary_exists":canary_path.exists(),"vault_preserved":vault_dir.exists()})
    );
    Ok(())
}

#[derive(serde::Serialize)]
struct WorkloadMeasurement {
    recall: f64,
    guard_far: f64,
    guard_frr: f64,
    search_p99_ms: f64,
    ingest_p95_ms: f64,
    canary_bytes: usize,
    canary_sha256: String,
}

fn measure_real_workload(canary_path: &Path) -> Result<WorkloadMeasurement, Box<dyn Error>> {
    let (per_slot, expected) = fusion_input();
    let mut elapsed_ms = Vec::with_capacity(BENCHMARK_PASSES);
    let mut recall = 0.0;
    for _ in 0..BENCHMARK_PASSES {
        let started = Instant::now();
        let hits = run_fusion(&per_slot, 5.0)?;
        elapsed_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
        recall = f64::from(hits.first().is_some_and(|hit| hit.cx_id == expected));
    }
    elapsed_ms.sort_by(f64::total_cmp);

    let good_scores = [0.95_f64, 0.91, 0.88];
    let bad_scores = [0.22_f64, 0.13, 0.05];
    let tau = 0.70;
    let guard_far =
        bad_scores.iter().filter(|score| **score >= tau).count() as f64 / bad_scores.len() as f64;
    let guard_frr =
        good_scores.iter().filter(|score| **score < tau).count() as f64 / good_scores.len() as f64;

    let payload = (0_u16..2048).flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    let mut ingest_ms = Vec::with_capacity(21);
    for _ in 0..21 {
        let started = Instant::now();
        fs::write(canary_path, &payload)?;
        fs::File::options()
            .write(true)
            .open(canary_path)?
            .sync_all()?;
        if fs::read(canary_path)? != payload {
            return Err("ANNEAL_FSV_CANARY_READBACK_MISMATCH".into());
        }
        ingest_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    ingest_ms.sort_by(f64::total_cmp);
    Ok(WorkloadMeasurement {
        recall,
        guard_far,
        guard_frr,
        search_p99_ms: percentile(&elapsed_ms, 99),
        ingest_p95_ms: percentile(&ingest_ms, 95),
        canary_bytes: payload.len(),
        canary_sha256: hex(&Sha256::digest(&payload)),
    })
}

fn metric_snapshot(measured: &WorkloadMeasurement, recall: f64) -> ActionMetricSnapshot {
    ActionMetricSnapshot::from_values([
        (TripwireMetric::RecallAtK, recall),
        (TripwireMetric::GuardFAR, measured.guard_far),
        (TripwireMetric::GuardFRR, measured.guard_frr),
        (TripwireMetric::SearchP99, measured.search_p99_ms),
        (TripwireMetric::IngestP95, measured.ingest_p95_ms),
    ])
}

fn held_out_replay() -> HeldOutReplay {
    HeldOutReplay {
        queries: vec![ReplayQuery {
            query_id: 1,
            query_vector: vec![1.0, 0.0],
            expected_top_k: vec![ReplayAnchor {
                cx_id: CxId::from_bytes([0x02; 16]),
                similarity: 1.0,
            }],
        }],
        seed: 1681,
    }
}

fn fusion_input() -> (BTreeMap<SlotId, Vec<IndexSearchHit>>, CxId) {
    let x = CxId::from_bytes([0x01; 16]);
    let y = CxId::from_bytes([0x02; 16]);
    let w = CxId::from_bytes([0x03; 16]);
    let z = CxId::from_bytes([0x04; 16]);
    let mut per_slot = BTreeMap::new();
    per_slot.insert(SlotId::new(1), ranking(&[x, y, z]));
    per_slot.insert(SlotId::new(2), ranking(&[y, w, x]));
    (per_slot, y)
}

fn ranking(ids: &[CxId]) -> Vec<IndexSearchHit> {
    ids.iter()
        .enumerate()
        .map(|(index, cx_id)| IndexSearchHit {
            cx_id: *cx_id,
            score: 1.0 - index as f32 * 0.1,
            rank: index + 1,
        })
        .collect()
}

fn run_fusion(
    per_slot: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    rrf_k: f32,
) -> CalyxResult<Vec<calyx_sextant::Hit>> {
    let context = FusionContext {
        panel_version: PANEL_VERSION,
        k: 4,
        rrf_k,
        explain: false,
        strategy: FusionStrategy::Rrf,
        weights: BTreeMap::new(),
        stage1_slots: Vec::new(),
    };
    fuse(per_slot, &context, &|cx_id| {
        Ok(LedgerRef {
            seq: u64::from(cx_id.as_bytes()[0]),
            hash: [0; 32],
        })
    })
}

fn percentile(sorted: &[f64], percentile: usize) -> f64 {
    let index = (sorted.len() - 1) * percentile / 100;
    sorted[index]
}

fn require_promoted(outcome: &ChangeOutcome) -> Result<u64, Box<dyn Error>> {
    match outcome {
        ChangeOutcome::Promoted(change_id) => Ok(change_id.0),
        other => Err(format!("ANNEAL_FSV_EXPECTED_PROMOTION: {other:?}").into()),
    }
}

fn require_reverted(outcome: &ChangeOutcome, phase: &str) -> Result<(), Box<dyn Error>> {
    match outcome {
        ChangeOutcome::Reverted { .. } => Ok(()),
        other => Err(format!("ANNEAL_FSV_EXPECTED_REJECTION phase={phase}: {other:?}").into()),
    }
}

fn require_hash(actual: &str, expected: &str, context: &str) -> Result<(), Box<dyn Error>> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("ANNEAL_FSV_HASH_MISMATCH: {context}: {actual} != {expected}").into())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn tuning_artifact_hash(bytes: &[u8]) -> [u8; 32] {
    const TAG: &[u8] = b"synapse-anneal-tuning-v1";
    let mut hasher = Sha256::new();
    hasher.update((TAG.len() as u64).to_be_bytes());
    hasher.update(TAG);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    hasher.finalize().into()
}
