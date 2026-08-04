//! Manual FSV for #1675/#1999: build a recall-gated kernel, answer through it,
//! then reproduce the persisted artifact and every hop from physical CF bytes.

use std::{collections::BTreeMap, error::Error, path::PathBuf};

use calyx_aster::{
    cf::{ColumnFamily, base_key, slot_key},
    vault::encode::decode_slot_vector,
};
use calyx_core::{
    Anchor, AnchorKind, AnchorValue, Constellation, CxFlags, CxId, InputRef, LedgerRef, Modality,
    SlotId, SlotVector, VaultId,
};
use synapse_calyx::{
    SynapseCalyxConfig, SynapseCalyxKernelParams, SynapseCalyxReadOnlyVault, SynapseCalyxVault,
};

const PANEL: u32 = 1_675_001;
const SLOT: u16 = 1;
const ROWS: u16 = 120;

fn id(index: u16) -> CxId {
    CxId::from_bytes([
        0x16,
        0x75,
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

fn vector(index: u16) -> Vec<f32> {
    // Three related dimensions form a nontrivial but locally smooth corpus.
    let theta = f32::from(index) * 0.041 + f32::from(index % 7) * 0.003;
    vec![theta.cos(), theta.sin(), (theta * 0.37).cos()]
}

fn row(vault_id: VaultId, index: u16) -> Constellation {
    let mut hash = [0u8; 32];
    hash[..2].copy_from_slice(&index.to_be_bytes());
    Constellation {
        cx_id: id(index),
        vault_id,
        panel_version: PANEL,
        created_at: 1_785_100_000_000 + u64::from(index),
        input_ref: InputRef {
            hash,
            pointer: Some(format!("fsv-1675/{index}")),
            redacted: false,
        },
        modality: Modality::Structured,
        slots: BTreeMap::from([(
            SlotId::new(SLOT),
            SlotVector::Dense {
                dim: 3,
                data: vector(index),
            },
        )]),
        scalars: BTreeMap::new(),
        metadata: BTreeMap::from([("fixture_index".to_owned(), index.to_string())]),
        anchors: vec![Anchor {
            kind: AnchorKind::Label("synfsv1675:grounded".to_owned()),
            value: AnchorValue::Bool(true),
            source: "kernel-answer-physical-fsv".to_owned(),
            observed_at: 1_785_100_000_000 + u64::from(index),
            confidence: 1.0,
        }],
        provenance: LedgerRef {
            seq: u64::from(index) + 1,
            hash: [0; 32],
        },
        flags: CxFlags::default(),
    }
}

fn dense(bytes: &[u8]) -> Result<Vec<f32>, Box<dyn Error>> {
    match decode_slot_vector(bytes)? {
        SlotVector::Dense { data, .. } => Ok(data),
        other => Err(format!("expected dense physical slot, found {other:?}").into()),
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot = a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    let an = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let bn = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (an * bn)
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: kernel_answer_physical_fsv <new-scratch-dir>")?;
    std::fs::create_dir(&dir).map_err(|error| {
        format!(
            "SYNAPSE_FSV_SCRATCH_CREATE_FAILED: cannot create fresh kernel fixture {}: {error}; remediation=pass a new child path whose parent exists",
            dir.display()
        )
    })?;
    println!("BEFORE path={} Base=0 Kernel=0", dir.display());

    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(dir.clone()))?;
    let vault_id = vault.vault_id_value();
    for index in 0..ROWS {
        vault.put_observation_constellation(row(vault_id, index))?;
    }
    let mut params = SynapseCalyxKernelParams::new(PANEL, SLOT);
    params.max_records = usize::from(ROWS);
    params.knn = 8;
    params.edge_cos_threshold = 0.0;
    params.min_recall_ratio = 0.95;
    let built = vault.build_domain_kernel(&params)?;
    println!(
        "BUILT id={} corpus={} members={} recall={} gate={} anchored={} Kernel_rows={}",
        built.kernel_id,
        built.corpus_size,
        built.members,
        built.recall_ratio,
        built.min_recall_ratio,
        built.anchored_members,
        built.kernel_cf_rows_after
    );
    if built.recall_ratio < params.min_recall_ratio || !built.grounded {
        return Err("kernel did not clear its binding recall/grounding gate".into());
    }
    let members = built
        .member_cx_ids
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    let query = (0..ROWS)
        .map(id)
        .find(|candidate| !members.contains(&candidate.to_string()))
        .ok_or("refinement selected every record; no non-member query remains")?;
    let answer = vault.kernel_answer(&params, &query.to_string(), 16)?;
    println!(
        "ANSWER query={} anchor={} hops={} score={} grounded={} recall={}",
        answer.query_cx_id,
        answer.anchor_kernel_node,
        answer.hop_count,
        answer.total_score,
        answer.grounded,
        answer.recall_ratio
    );
    if !answer.grounded || answer.hops.is_empty() {
        return Err("kernel answer did not return a grounded evidence path".into());
    }
    vault.checkpoint()?;

    let readback = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(dir),
        None,
    )?;
    let snapshot = readback.latest_seq();
    let base_rows = readback.scan_cf_at(snapshot, ColumnFamily::Base)?.len();
    let kernel_rows = readback.scan_cf_at(snapshot, ColumnFamily::Kernel)?;
    let persisted_report = kernel_rows.iter().find_map(|(_key, bytes)| {
        let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        (value.get("kernel_id")?.as_str()? == built.kernel_id).then_some(value)
    });
    let persisted_report = persisted_report.ok_or("persisted Kernel index row not found")?;
    let persisted_recall = persisted_report
        .get("recall_ratio")
        .and_then(serde_json::Value::as_f64)
        .ok_or("persisted Kernel row has no recall_ratio")? as f32;
    let mut physical_score = 0.0_f32;
    for hop in &answer.hops {
        let from: CxId = hop.from.parse()?;
        let to: CxId = hop.to.parse()?;
        for cx_id in [from, to] {
            if readback
                .read_cf_at(snapshot, ColumnFamily::Base, &base_key(cx_id))?
                .is_none()
            {
                return Err(format!("evidence CxId {cx_id} is absent from physical Base").into());
            }
        }
        let read_slot = |cx_id| -> Result<Vec<f32>, Box<dyn Error>> {
            let bytes = readback
                .read_cf_at(
                    snapshot,
                    ColumnFamily::slot(SlotId::new(SLOT)),
                    &slot_key(cx_id),
                )?
                .ok_or("evidence slot row missing")?;
            dense(&bytes)
        };
        let weight = cosine(&read_slot(from)?, &read_slot(to)?).clamp(0.0, 1.0);
        let expected_hop_score = weight * 0.9_f32.powi(i32::try_from(hop.hop_index)?);
        if (weight - hop.edge_weight).abs() > 1.0e-6
            || (expected_hop_score - hop.hop_score).abs() > 1.0e-6
        {
            return Err(format!(
                "physical hop mismatch {}->{}: weight {weight}/{} score {expected_hop_score}/{}",
                hop.from, hop.to, hop.edge_weight, hop.hop_score
            )
            .into());
        }
        physical_score += expected_hop_score;
        println!(
            "HOP {} {}->{} physical_weight={} physical_score={} MATCH",
            hop.hop_index, hop.from, hop.to, weight, expected_hop_score
        );
    }
    let physical_ok = base_rows == usize::from(ROWS)
        && kernel_rows.len() == built.kernel_cf_rows_after
        && (persisted_recall - built.recall_ratio).abs() <= f32::EPSILON
        && (physical_score - answer.total_score).abs() <= 1.0e-5;
    println!(
        "READBACK snapshot={snapshot} Base={base_rows} Kernel={} persisted_recall={persisted_recall} physical_total={physical_score} matches={physical_ok}",
        kernel_rows.len()
    );
    if !physical_ok {
        return Err("physical Kernel/Base/slot readback did not reproduce the result".into());
    }
    println!("PASS: recall-gated kernel and grounded answer reproduced from physical bytes");
    Ok(())
}
