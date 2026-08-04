//! Manual known-answer FSV for #1674: plant one cross-lens contradiction,
//! detect it, then recompute the reported evidence from physical slot CF rows.

use std::{collections::BTreeMap, error::Error, path::PathBuf};

use calyx_aster::{
    cf::{ColumnFamily, slot_key},
    vault::encode::decode_slot_vector,
};
use calyx_core::{
    Constellation, CxFlags, CxId, InputRef, LedgerRef, Modality, SlotId, SlotVector, VaultId,
};
use synapse_calyx::{
    SynapseCalyxBlindSpotParams, SynapseCalyxConfig, SynapseCalyxReadOnlyVault, SynapseCalyxVault,
};

const PANEL: u32 = 1_674_001;
const ROWS: u16 = 200;
const ANOMALY: u16 = 100;
const SLOT_A: u16 = 1;
const SLOT_B: u16 = 2;

fn cx_id(index: u16) -> CxId {
    CxId::from_bytes([
        0x16,
        0x74,
        (index >> 8) as u8,
        index as u8,
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
    ])
}

fn angle(index: u16) -> f32 {
    // Irregular cumulative spacing makes nearest-neighbour confidence genuinely
    // discriminative instead of a constant produced by the encoding.
    (0..index)
        .map(|i| 0.01 + f32::from((i * 7) % 19) * 0.002)
        .sum()
}

fn unit(theta: f32) -> Vec<f32> {
    vec![theta.cos(), theta.sin()]
}

fn input_hash(index: u16) -> [u8; 32] {
    let mut hash = [0u8; 32];
    hash[..2].copy_from_slice(&index.to_be_bytes());
    hash
}

fn row(vault_id: VaultId, index: u16) -> Constellation {
    let a = unit(angle(index));
    let b = if index == ANOMALY {
        // The exact opposite of the preceding record under lens B. Under lens
        // A those records are near-identical, so the expected delta is near 2.
        unit(angle(index - 1) + std::f32::consts::PI)
    } else {
        a.clone()
    };
    let slots = BTreeMap::from([
        (SlotId::new(SLOT_A), SlotVector::Dense { dim: 2, data: a }),
        (SlotId::new(SLOT_B), SlotVector::Dense { dim: 2, data: b }),
    ]);
    Constellation {
        cx_id: cx_id(index),
        vault_id,
        panel_version: PANEL,
        created_at: 1_785_000_000_000 + u64::from(index),
        input_ref: InputRef {
            hash: input_hash(index),
            pointer: Some(format!("fsv-1674/{index}")),
            redacted: false,
        },
        modality: Modality::Structured,
        slots,
        scalars: BTreeMap::new(),
        metadata: BTreeMap::from([("fixture_index".to_owned(), index.to_string())]),
        anchors: Vec::new(),
        provenance: LedgerRef {
            seq: u64::from(index) + 1,
            hash: [0; 32],
        },
        flags: CxFlags::default(),
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let an: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let bn: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (an * bn)
}

fn dense(vector: SlotVector) -> Result<Vec<f32>, Box<dyn Error>> {
    match vector {
        SlotVector::Dense { data, .. } => Ok(data),
        other => Err(format!("expected physical dense slot, found {other:?}").into()),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: blind_spot_planted_anomaly_fsv <new-scratch-dir>")?;
    std::fs::create_dir(&dir).map_err(|error| {
        format!(
            "SYNAPSE_FSV_SCRATCH_CREATE_FAILED: cannot create fresh fixture vault {}: {error}; remediation=pass a new child path whose parent exists",
            dir.display()
        )
    })?;
    println!("BEFORE new_vault={} rows=0", dir.display());

    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(dir.clone()))?;
    let vault_id = vault.vault_id_value();
    for index in 0..ROWS {
        vault.put_observation_constellation(row(vault_id, index))?;
    }
    let mut params = SynapseCalyxBlindSpotParams::new(PANEL);
    params.max_records = usize::from(ROWS);
    params.min_samples = 50;
    params.alpha = 0.05;
    params.max_alerts = 64;
    let report = vault.blind_spot_scan(&params)?;
    let anomaly_id = cx_id(ANOMALY).to_string();
    let alert = report
        .alerts
        .iter()
        .find(|alert| alert.cx_id == anomaly_id && alert.slot_a == SLOT_A && alert.slot_b == SLOT_B)
        .ok_or_else(|| {
            format!(
                "planted anomaly was not named: evaluated={} uncalibrated={} nondiscriminative={} alerts_total={} diagnostics={:?} alerts={:?}",
                report.slot_pairs_evaluated,
                report.slot_pairs_uncalibrated,
                report.slot_pairs_nondiscriminative,
                report.alerts_total,
                report.nondiscriminative_pairs,
                report
                    .alerts
                    .iter()
                    .map(|alert| (&alert.cx_id, alert.slot_a, alert.slot_b, alert.delta))
                    .collect::<Vec<_>>()
            )
        })?;
    let healthy_false_positives = report
        .alerts
        .iter()
        .filter(|candidate| candidate.cx_id != anomaly_id)
        .count();
    let false_positive_bound = (params.alpha * f32::from(ROWS)).ceil() as usize;
    if healthy_false_positives > false_positive_bound {
        return Err(format!(
            "healthy false positives {healthy_false_positives} exceed the conformal alpha bound {false_positive_bound}"
        )
        .into());
    }
    println!(
        "DETECTED cx_id={} slots={}->{} delta={} p={} severity={} evaluated={} alerts_total={} healthy_false_positives={} bound={} nondiscriminative={}",
        alert.cx_id,
        alert.slot_a,
        alert.slot_b,
        alert.delta,
        alert.calibration_p_value,
        alert.severity,
        report.slot_pairs_evaluated,
        report.alerts_total,
        healthy_false_positives,
        false_positive_bound,
        report.slot_pairs_nondiscriminative,
    );
    vault.checkpoint()?;

    // Independent physical proof: a new read-only handle reads every slot-A
    // row, recomputes the anomaly's nearest neighbour, then reads both records'
    // slot-B bytes and reconstructs the reported delta.
    let readback = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(dir),
        None,
    )?;
    let snapshot = readback.latest_seq();
    let anomaly_a = dense(decode_slot_vector(
        &readback
            .read_cf_at(
                snapshot,
                ColumnFamily::slot(SlotId::new(SLOT_A)),
                &slot_key(cx_id(ANOMALY)),
            )?
            .ok_or("physical anomaly slot A missing")?,
    )?)?;
    let mut nearest: Option<(u16, f32)> = None;
    for index in 0..ROWS {
        if index == ANOMALY {
            continue;
        }
        let value = readback
            .read_cf_at(
                snapshot,
                ColumnFamily::slot(SlotId::new(SLOT_A)),
                &slot_key(cx_id(index)),
            )?
            .ok_or("physical candidate slot A missing")?;
        let similarity = cosine(&anomaly_a, &dense(decode_slot_vector(&value)?)?);
        if nearest.is_none_or(|(_, best)| similarity > best) {
            nearest = Some((index, similarity));
        }
    }
    let (neighbor, a_similarity) = nearest.ok_or("no physical nearest neighbour")?;
    let read_b = |index| -> Result<Vec<f32>, Box<dyn Error>> {
        let bytes = readback
            .read_cf_at(
                snapshot,
                ColumnFamily::slot(SlotId::new(SLOT_B)),
                &slot_key(cx_id(index)),
            )?
            .ok_or("physical slot B missing")?;
        dense(decode_slot_vector(&bytes)?)
    };
    let b_similarity = cosine(&read_b(ANOMALY)?, &read_b(neighbor)?);
    let recomputed_delta = a_similarity - b_similarity;
    let matches = (a_similarity - alert.lens_a_similarity).abs() <= 1.0e-6
        && (b_similarity - alert.lens_b_neighbor_mean).abs() <= 1.0e-6
        && (recomputed_delta - alert.delta).abs() <= 1.0e-6;
    let base_rows = readback.scan_cf_at(snapshot, ColumnFamily::Base)?.len();
    println!(
        "READBACK snapshot={snapshot} Base={base_rows} anomaly={} nearest={} a_similarity={} b_similarity={} delta={} matches_alert={matches}",
        ANOMALY, neighbor, a_similarity, b_similarity, recomputed_delta
    );
    if base_rows != usize::from(ROWS) || !matches {
        return Err("physical readback did not reproduce the alert".into());
    }
    println!("PASS: planted anomaly detected and reproduced from physical slot bytes");
    Ok(())
}
