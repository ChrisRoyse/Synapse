//! Manual FSV for #1676: planted fused neighbor plus applied temporal boost.
//!
//! Run against an empty directory:
//! `cargo run -p synapse-storage --example find_temporal_fsv -- <dir>`

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::{ColumnFamily, SlotFamilyKind, base_key, slot_key};
use calyx_core::{CxId, SlotId, TemporalPolicy, VaultId};
use serde_json::json;
use synapse_calyx::{
    SynapseCalyxConfig, SynapseCalyxFindFusion, SynapseCalyxFindGuardMode, SynapseCalyxFindParams,
    SynapseCalyxFindQuery, SynapseCalyxFindTemporal, SynapseCalyxVault,
    VaultTemporalPanelRegistration,
};
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_TIMELINE_PANEL_NAME, SYN_TIMELINE_PANEL_VERSION,
    build_timeline_constellation, syn_active_panel_contract,
};

const DAY0_NS: u64 = 1_782_950_400_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;
const MINUTE_NS: u64 = 60_000_000_000;

fn cx(index: u8) -> CxId {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x16;
    bytes[1] = 0x76;
    bytes[15] = index;
    CxId::from_bytes(bytes)
}

fn record(index: u8) -> TimelineRecord {
    match index {
        0 => timeline(12, 0, "atlas.exe", "project atlas compile report"),
        // Planted neighbor: same app, kind, actor, hour and lexical content.
        1 => timeline(12, 1, "atlas.exe", "project atlas compile report copy"),
        2 => timeline(2, 0, "mail.exe", "quarterly invoice review"),
        3 => timeline(22, 0, "terminal.exe", "system package upgrade"),
        _ => unreachable!("fixture has exactly four rows"),
    }
}

fn timeline(hour: u64, minute: u64, app: &str, title: &str) -> TimelineRecord {
    TimelineRecord {
        record_version: 1,
        ts_ns: DAY0_NS + hour * HOUR_NS + minute * MINUTE_NS,
        kind: TimelineKind::FocusChange,
        actor: TimelineActor::Human,
        app: Some(app.to_owned()),
        payload: json!({"title": title}),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: find_temporal_fsv <empty-dir>")?;
    std::fs::create_dir_all(&dir)?;
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(dir.clone()))?;
    let vault_id: VaultId = vault.vault_id_value();

    println!(
        "SOURCE OF TRUTH BEFORE: base_rows={}",
        vault.count_cf_latest(ColumnFamily::Base)?
    );
    for index in 0..4_u8 {
        let source = record(index);
        let raw = serde_json::to_vec(&source)?;
        let constellation = build_timeline_constellation(
            NativeConstellationContext {
                vault_id,
                cx_id: cx(index),
                created_at_ms: 1_783_000_000_000 + u64::from(index),
                next_ledger_seq: u64::from(index) + 1,
            },
            format!("fsv-1676-{index}").as_bytes(),
            &raw,
            &source,
        )?;
        vault.put_observation_constellation(constellation)?;
    }
    vault.flush()?;
    vault.checkpoint()?;
    let contract = syn_active_panel_contract(SYN_TIMELINE_PANEL_VERSION, 1_783_000_000_100)?
        .ok_or("timeline panel contract missing")?;
    vault.publish_active_panel(&contract.panel, &contract.registry)?;

    let policy = TemporalPolicy {
        recurrence_boost: None,
        ..TemporalPolicy::default()
    };
    let registration = VaultTemporalPanelRegistration::new(
        SYN_TIMELINE_PANEL_VERSION,
        SYN_TIMELINE_PANEL_NAME,
        policy,
        1_783_000_000_200,
    )?;
    vault.register_temporal_panel(&registration)?;
    let registry_readback = vault
        .read_temporal_panel(SYN_TIMELINE_PANEL_NAME, SYN_TIMELINE_PANEL_VERSION)?
        .ok_or("temporal Registry row absent after registration")?;
    if registry_readback != registration {
        return Err("temporal Registry readback differs from the persisted policy".into());
    }
    let rebuilt = vault.rebuild_search_indexes(SYN_TIMELINE_PANEL_VERSION)?;
    println!(
        "SOURCE OF TRUTH AFTER: base_rows={} manifest_sha256={}",
        vault.count_cf_latest(ColumnFamily::Base)?,
        rebuilt.generation.manifest_sha256
    );

    let base_params = SynapseCalyxFindParams {
        panel_version: Some(SYN_TIMELINE_PANEL_VERSION),
        query: SynapseCalyxFindQuery::ByExample {
            cx_id: cx(0).to_string(),
        },
        k: 3,
        fusion: SynapseCalyxFindFusion::Rrf,
        filter: None,
        explain: true,
        temporal: None,
        guard: SynapseCalyxFindGuardMode::Off,
    };
    let plain = vault.find_similar(&base_params)?;
    let first = plain
        .hits
        .first()
        .ok_or("plain find returned no neighbors")?;
    if first.cx_id != cx(1).to_string() {
        return Err(format!("planted neighbor was not rank 1: got {}", first.cx_id).into());
    }

    let mut base_scores = BTreeMap::new();
    for hit in &plain.hits {
        let mut recomputed = 0.0_f64;
        for lane in &hit.per_lens {
            recomputed +=
                f64::from(lane.weight) / f64::from(plain.rrf_k + u32::try_from(lane.rank)?);
        }
        let delta = (recomputed - f64::from(hit.score)).abs();
        if delta > 1.0e-6 {
            return Err(format!(
                "RRF mismatch for {}: reported={} recomputed={recomputed}",
                hit.cx_id, hit.score
            )
            .into());
        }
        base_scores.insert(hit.cx_id.clone(), hit.score);
        let id: CxId = hit.cx_id.parse()?;
        let base = vault.read_cf_latest(ColumnFamily::Base, &base_key(id))?;
        if base.is_none() {
            return Err(format!("returned hit {} has no physical Base row", hit.cx_id).into());
        }
        for slot in &plain.consulted_slots {
            let slot = SlotId::new(*slot);
            let quantized = vault.read_cf_latest(
                ColumnFamily::Slot {
                    slot,
                    kind: SlotFamilyKind::Quantized,
                },
                &slot_key(id),
            )?;
            let raw = vault.read_cf_latest(
                ColumnFamily::Slot {
                    slot,
                    kind: SlotFamilyKind::Raw,
                },
                &slot_key(id),
            )?;
            if quantized.is_none() && raw.is_none() {
                return Err(format!(
                    "hit {} has no physical row for consulted slot {slot}",
                    hit.cx_id
                )
                .into());
            }
        }
        println!(
            "PLAIN rank={} cx={} score={:.9} rrf_delta={delta:.3e}",
            hit.rank, hit.cx_id, hit.score
        );
    }

    let boosted = vault.find_similar(&SynapseCalyxFindParams {
        temporal: Some(SynapseCalyxFindTemporal {
            query_time_secs: i64::try_from(
                (DAY0_NS + 12 * HOUR_NS + 2 * MINUTE_NS) / 1_000_000_000,
            )?,
            tz_offset_secs: 0,
        }),
        ..base_params.clone()
    })?;
    if !boosted.temporal_applied {
        return Err(
            "find reported temporal_applied=false after an explicit temporal request".into(),
        );
    }
    for hit in &boosted.hits {
        let base = *base_scores
            .get(&hit.cx_id)
            .ok_or("temporal rerank changed candidate identity")?;
        let scores = hit.temporal_scores.ok_or("temporal evidence missing")?;
        let fused = policy.fusion_weights.recency * scores.e2_recency
            + policy.fusion_weights.sequence * scores.e4_sequence
            + policy.fusion_weights.periodic * scores.e3_periodic;
        let expected = base * (1.0 + fused.clamp(0.0, 1.0) * policy.boost.post_retrieval_alpha);
        if (expected - hit.score).abs() > 1.0e-6 {
            return Err(format!(
                "temporal score mismatch for {}: reported={} expected={expected}",
                hit.cx_id, hit.score
            )
            .into());
        }
        if hit.score > base * (1.0 + policy.boost.post_retrieval_alpha) + 1.0e-6 {
            return Err(format!("temporal AP-60 bound exceeded for {}", hit.cx_id).into());
        }
        println!(
            "BOOST rank={} cx={} base={base:.9} score={:.9} e2={:.3} e3={:.3} e4={:.3}",
            hit.rank,
            hit.cx_id,
            hit.score,
            scores.e2_recency,
            scores.e3_periodic,
            scores.e4_sequence
        );
    }

    // Boundary: an unregistered panel name must refuse and leave Registry
    // unchanged. The valid request above proves the same path is reachable.
    let registry_before = vault.count_cf_latest(ColumnFamily::Registry)?;
    let absent = vault.read_temporal_panel("missing-panel", SYN_TIMELINE_PANEL_VERSION)?;
    let registry_after = vault.count_cf_latest(ColumnFamily::Registry)?;
    println!(
        "EDGE missing_registration: before={registry_before} absent={} after={registry_after} unchanged={}",
        absent.is_none(),
        registry_before == registry_after
    );
    if absent.is_some() || registry_before != registry_after {
        return Err("missing registration read mutated Registry state".into());
    }

    for (name, query) in [
        (
            "invalid_cx_format",
            SynapseCalyxFindQuery::ByExample {
                cx_id: "not-a-content-address".to_owned(),
            },
        ),
        (
            "missing_example",
            SynapseCalyxFindQuery::ByExample {
                cx_id: CxId::from_bytes([0xEE; 16]).to_string(),
            },
        ),
    ] {
        let base_before = vault.count_cf_latest(ColumnFamily::Base)?;
        let registry_before = vault.count_cf_latest(ColumnFamily::Registry)?;
        let result = vault.find_similar(&SynapseCalyxFindParams {
            query,
            ..base_params.clone()
        });
        let base_after = vault.count_cf_latest(ColumnFamily::Base)?;
        let registry_after = vault.count_cf_latest(ColumnFamily::Registry)?;
        let unchanged = base_before == base_after && registry_before == registry_after;
        println!(
            "EDGE {name}: base_before={base_before} registry_before={registry_before} refused={} base_after={base_after} registry_after={registry_after} unchanged={unchanged}",
            result.is_err()
        );
        if result.is_ok() || !unchanged {
            return Err(format!("edge {name} did not refuse without mutation").into());
        }
    }

    println!("ALL CLAIMS OK");
    Ok(())
}
