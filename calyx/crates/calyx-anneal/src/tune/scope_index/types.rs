use std::collections::HashMap;

use calyx_core::{PanelSlotId, Result};
use calyx_forge::{AutotuneKey, BackendKind, BestConfig};
use serde::{Deserialize, Serialize};

use super::invalid_config;
use crate::shape_key_hash;

pub const MAX_INDEX_CANDIDATES: usize = 8;
pub const DEFAULT_INDEX_RECALL_TARGET: f32 = 0.99;
pub const DEFAULT_INDEX_VRAM_BUDGET_BYTES: u64 = 1 << 30;
pub const MIN_BITS_PER_ANCHOR: f64 = 0.05;
const QUANT_EPSILON: f64 = 1e-6;
const GUARD_FAR_EPSILON: f64 = 1e-12;
const VALID_QUANT_BITS: [u8; 4] = [4, 8, 16, 32];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IndexConfig {
    pub hnsw_ef: u32,
    pub hnsw_m: u32,
    pub diskann_beamwidth: u32,
    pub spann_cutoff: u32,
    pub quant_bits: u8,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            hnsw_ef: 64,
            hnsw_m: 16,
            diskann_beamwidth: 32,
            spann_cutoff: 1024,
            quant_bits: 16,
        }
    }
}

impl IndexConfig {
    pub fn to_best_config(&self, panel_slot: PanelSlotId) -> BestConfig {
        let slot = index_slot_label(panel_slot);
        BestConfig {
            backend: BackendKind::Cpu,
            tile_m: self.hnsw_ef as usize,
            tile_n: self.hnsw_m as usize,
            tile_k: self.diskann_beamwidth as usize,
            extra: HashMap::from([
                ("scope".to_string(), "index".to_string()),
                (
                    "panel_version".to_string(),
                    panel_slot.panel_version().to_string(),
                ),
                ("slot".to_string(), panel_slot.slot_id().get().to_string()),
                ("slot_key".to_string(), slot),
                ("hnsw_ef".to_string(), self.hnsw_ef.to_string()),
                ("hnsw_m".to_string(), self.hnsw_m.to_string()),
                (
                    "diskann_beamwidth".to_string(),
                    self.diskann_beamwidth.to_string(),
                ),
                ("spann_cutoff".to_string(), self.spann_cutoff.to_string()),
                ("quant_bits".to_string(), self.quant_bits.to_string()),
                ("source".to_string(), "anneal-index-scope".to_string()),
            ]),
        }
    }

    pub fn from_best_config(config: &BestConfig) -> Result<Self> {
        let parsed = Self {
            hnsw_ef: parse_u32(config, "hnsw_ef").unwrap_or(config.tile_m as u32),
            hnsw_m: parse_u32(config, "hnsw_m").unwrap_or(config.tile_n as u32),
            diskann_beamwidth: parse_u32(config, "diskann_beamwidth")
                .unwrap_or(config.tile_k as u32),
            spann_cutoff: parse_u32(config, "spann_cutoff").unwrap_or(1024),
            quant_bits: parse_u8(config, "quant_bits").unwrap_or(16),
        };
        validate_index_config(&parsed)?;
        Ok(parsed)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexPromotionRecord {
    pub panel_slot: PanelSlotId,
    pub change_id: crate::ChangeId,
    pub old_config: IndexConfig,
    pub new_config: IndexConfig,
    pub latency_before_ns: u64,
    pub latency_after_ns: u64,
    pub recall_before: f64,
    pub recall_after: f64,
    pub bits_before: f64,
    pub bits_after: f64,
    pub slot_key_hash: [u8; 32],
    pub old_config_hash: [u8; 32],
    pub new_config_hash: [u8; 32],
    pub quant_evidence: Option<QuantPromotionEvidence>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuantPromotionEvidence {
    pub cosine_error_before: f64,
    pub cosine_error_after: f64,
    pub max_cosine_error: f64,
    pub guard_far_before: f64,
    pub guard_far_after: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexTuneSkip {
    ParkedSlot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexTuneDecision {
    pub evaluated_arm: usize,
    pub won: bool,
    pub incumbent: IndexConfig,
    pub promoted: Option<IndexPromotionRecord>,
    pub shadow_arm: Option<usize>,
    pub shadow_candidate: Option<IndexConfig>,
    pub skipped: Option<IndexTuneSkip>,
}

pub fn candidate_configs(panel_slot: PanelSlotId) -> Result<Vec<IndexConfig>> {
    let base = IndexConfig::default();
    let candidates = [
        base.clone(),
        IndexConfig {
            hnsw_ef: 128,
            quant_bits: 8,
            ..base.clone()
        },
        IndexConfig {
            hnsw_ef: 256,
            ..base.clone()
        },
        IndexConfig {
            hnsw_m: 8,
            ..base.clone()
        },
        IndexConfig {
            hnsw_ef: 128,
            hnsw_m: 32,
            ..base.clone()
        },
        IndexConfig {
            diskann_beamwidth: 64,
            ..base.clone()
        },
        IndexConfig {
            spann_cutoff: 2048,
            ..base.clone()
        },
        IndexConfig {
            hnsw_ef: 128,
            hnsw_m: 32,
            quant_bits: 4,
            ..base
        },
    ];
    let mut configs = Vec::with_capacity(MAX_INDEX_CANDIDATES);
    for config in candidates {
        validate_index_config(&config)?;
        if estimate_vram_bytes(panel_slot, &config) <= DEFAULT_INDEX_VRAM_BUDGET_BYTES {
            push_unique(&mut configs, config);
        }
    }
    if configs.is_empty() {
        return Err(invalid_config("Index candidate pruning removed every arm"));
    }
    Ok(configs)
}

pub fn quant_win_check(
    candidate: &IndexConfig,
    incumbent: &IndexConfig,
    bits_before: f64,
    bits_after: f64,
) -> bool {
    if !bits_before.is_finite() || !bits_after.is_finite() {
        return false;
    }
    candidate.quant_bits >= incumbent.quant_bits || bits_after + QUANT_EPSILON >= bits_before
}

pub(super) fn metrics_are_valid(recall_k: f64, bits_per_anchor: f64) -> bool {
    recall_k.is_finite() && bits_per_anchor.is_finite() && bits_per_anchor >= 0.0
}

pub fn validate_quant_promotion_evidence(evidence: &QuantPromotionEvidence) -> Result<()> {
    let values = [
        evidence.cosine_error_before,
        evidence.cosine_error_after,
        evidence.max_cosine_error,
        evidence.guard_far_before,
        evidence.guard_far_after,
    ];
    if values
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(invalid_config(
            "quant promotion evidence must be finite and nonnegative",
        ));
    }
    if evidence.max_cosine_error == 0.0 {
        return Err(invalid_config("quant cosine error bound must be positive"));
    }
    if evidence.cosine_error_after > evidence.max_cosine_error + QUANT_EPSILON {
        return Err(invalid_config("quant cosine error exceeds accepted bound"));
    }
    if evidence.guard_far_after > evidence.guard_far_before + GUARD_FAR_EPSILON {
        return Err(invalid_config("quant guard FAR regressed"));
    }
    Ok(())
}

pub fn validate_index_config(config: &IndexConfig) -> Result<()> {
    if config.hnsw_ef == 0
        || config.hnsw_m == 0
        || config.diskann_beamwidth == 0
        || config.spann_cutoff == 0
    {
        return Err(invalid_config("Index tuning parameters must be nonzero"));
    }
    if !VALID_QUANT_BITS.contains(&config.quant_bits) {
        return Err(invalid_config(format!(
            "quant_bits {} must be one of 4, 8, 16, 32",
            config.quant_bits
        )));
    }
    Ok(())
}

pub fn encode_index_config(config: &IndexConfig) -> Result<Vec<u8>> {
    validate_index_config(config)?;
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(config, &mut bytes)
        .map_err(|error| invalid_config(error.to_string()))?;
    Ok(bytes)
}

pub fn decode_index_config(bytes: &[u8]) -> Result<IndexConfig> {
    let config: IndexConfig =
        ciborium::de::from_reader(bytes).map_err(|error| invalid_config(error.to_string()))?;
    validate_index_config(&config)?;
    Ok(config)
}

pub fn slot_autotune_key(panel_slot: PanelSlotId, recall_target: f32) -> AutotuneKey {
    AutotuneKey {
        op: "index".to_string(),
        shape: vec![
            panel_slot.panel_version() as usize,
            panel_slot.slot_id().get() as usize,
        ],
        dtype: "ann".to_string(),
        device: index_slot_label(panel_slot),
        recall_tgt: recall_target,
    }
}

pub fn index_slot_label(panel_slot: PanelSlotId) -> String {
    format!(
        "index:panel_{:010}:slot_{:04}",
        panel_slot.panel_version(),
        panel_slot.slot_id().get()
    )
}

pub(super) fn seed_for_slot(panel_slot: PanelSlotId) -> u64 {
    let hash = shape_key_hash(&index_slot_label(panel_slot));
    u64::from_le_bytes(hash[0..8].try_into().expect("hash slice has 8 bytes"))
}

fn estimate_vram_bytes(panel_slot: PanelSlotId, config: &IndexConfig) -> u64 {
    let slot_factor = u64::from(panel_slot.slot_id().get()) + 1;
    let graph_bytes = u64::from(config.hnsw_ef) * u64::from(config.hnsw_m) * 16;
    let diskann_bytes = u64::from(config.diskann_beamwidth) * 4096;
    let spann_bytes = u64::from(config.spann_cutoff) * 8;
    let quant_bytes = slot_factor * 4096 * u64::from(config.quant_bits);
    graph_bytes + diskann_bytes + spann_bytes + quant_bytes
}

fn push_unique(configs: &mut Vec<IndexConfig>, config: IndexConfig) {
    if !configs.contains(&config) {
        configs.push(config);
    }
}

fn parse_u32(config: &BestConfig, field: &str) -> Option<u32> {
    config.extra.get(field).and_then(|value| value.parse().ok())
}

fn parse_u8(config: &BestConfig, field: &str) -> Option<u8> {
    config.extra.get(field).and_then(|value| value.parse().ok())
}
