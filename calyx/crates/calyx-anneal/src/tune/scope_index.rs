mod types;
mod writer;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use calyx_core::{CalyxError, PanelSlotId, Result};
use calyx_forge::AutotuneCache;

use crate::{
    AssayMetrics, BanditPolicy, ComponentHealth, ComponentKind, ConfigBandit, DegradeRegistry,
    HealthStorage, SignalSample, shape_key_hash,
};

use types::metrics_are_valid;
pub use types::{
    DEFAULT_INDEX_RECALL_TARGET, DEFAULT_INDEX_VRAM_BUDGET_BYTES, IndexConfig,
    IndexPromotionRecord, IndexTuneDecision, IndexTuneSkip, MAX_INDEX_CANDIDATES,
    MIN_BITS_PER_ANCHOR, QuantPromotionEvidence, candidate_configs, decode_index_config,
    encode_index_config, index_slot_label, quant_win_check, slot_autotune_key,
    validate_index_config, validate_quant_promotion_evidence,
};
pub use writer::{
    IndexBanditPersistence, IndexPromotionWriter, NoopIndexBanditStore, NoopIndexPromotionWriter,
};

pub const CALYX_INDEX_CACHE_WRITE_FAIL: &str = "CALYX_INDEX_CACHE_WRITE_FAIL";
pub const CALYX_INDEX_SCOPE_INVALID_CONFIG: &str = "CALYX_INDEX_SCOPE_INVALID_CONFIG";

const NEXT_CHANGE_ID_START: u64 = 414_000;
const RECALL_EPSILON: f64 = 1e-12;

struct PromotionMetrics {
    latency_before_ns: u64,
    latency_after_ns: u64,
    recall_before: f64,
    recall_after: f64,
    bits_before: f64,
    bits_after: f64,
    quant_evidence: Option<QuantPromotionEvidence>,
}

pub trait IndexSlotHealth {
    fn is_slot_parked(&self, panel_slot: PanelSlotId) -> bool;
}

#[derive(Clone, Copy, Default)]
pub struct NoopIndexSlotHealth;

impl IndexSlotHealth for NoopIndexSlotHealth {
    fn is_slot_parked(&self, _panel_slot: PanelSlotId) -> bool {
        false
    }
}

impl<S> IndexSlotHealth for DegradeRegistry<S>
where
    S: HealthStorage,
{
    fn is_slot_parked(&self, panel_slot: PanelSlotId) -> bool {
        matches!(
            self.health(&ComponentKind::ann_index(panel_slot)),
            ComponentHealth::Parked { .. }
        )
    }
}

impl<T> IndexSlotHealth for &T
where
    T: IndexSlotHealth + ?Sized,
{
    fn is_slot_parked(&self, panel_slot: PanelSlotId) -> bool {
        (**self).is_slot_parked(panel_slot)
    }
}

#[derive(Default)]
pub struct NoopIndexAssayMetrics;

impl AssayMetrics for NoopIndexAssayMetrics {
    fn signal_samples(&self) -> Result<Vec<SignalSample>> {
        Ok(Vec::new())
    }
}

pub struct IndexScopeTuner<
    W = NoopIndexPromotionWriter,
    B = NoopIndexBanditStore,
    H = NoopIndexSlotHealth,
> where
    W: IndexPromotionWriter,
    B: IndexBanditPersistence,
    H: IndexSlotHealth,
{
    pub bandits: HashMap<PanelSlotId, ConfigBandit>,
    pub assay: Arc<dyn AssayMetrics>,
    pub cache: Arc<Mutex<AutotuneCache>>,
    promotion_writer: W,
    bandit_store: B,
    health: H,
    pending_arms: HashMap<PanelSlotId, usize>,
    incumbent_latency_ns: HashMap<PanelSlotId, u64>,
    incumbent_recall: HashMap<PanelSlotId, f64>,
    incumbent_bits: HashMap<PanelSlotId, f64>,
    recall_target: f32,
    next_change_id: u64,
    promotions: Vec<IndexPromotionRecord>,
}

impl IndexScopeTuner {
    pub fn new(cache: AutotuneCache) -> Self {
        Self::with_parts(
            cache,
            NoopIndexPromotionWriter,
            NoopIndexBanditStore,
            NoopIndexSlotHealth,
        )
    }
}

impl<W, B, H> IndexScopeTuner<W, B, H>
where
    W: IndexPromotionWriter,
    B: IndexBanditPersistence,
    H: IndexSlotHealth,
{
    pub fn with_parts(
        cache: AutotuneCache,
        promotion_writer: W,
        bandit_store: B,
        health: H,
    ) -> Self {
        Self::with_assay_parts(
            cache,
            Arc::new(NoopIndexAssayMetrics),
            promotion_writer,
            bandit_store,
            health,
        )
    }

    pub fn with_assay_parts(
        cache: AutotuneCache,
        assay: Arc<dyn AssayMetrics>,
        promotion_writer: W,
        bandit_store: B,
        health: H,
    ) -> Self {
        Self {
            bandits: HashMap::new(),
            assay,
            cache: Arc::new(Mutex::new(cache)),
            promotion_writer,
            bandit_store,
            health,
            pending_arms: HashMap::new(),
            incumbent_latency_ns: HashMap::new(),
            incumbent_recall: HashMap::new(),
            incumbent_bits: HashMap::new(),
            recall_target: DEFAULT_INDEX_RECALL_TARGET,
            next_change_id: NEXT_CHANGE_ID_START,
            promotions: Vec::new(),
        }
    }

    pub fn on_search(
        &mut self,
        panel_slot: PanelSlotId,
        p99_ns: u64,
        recall_k: f64,
        bits_per_anchor: f64,
    ) -> Result<IndexTuneDecision> {
        if self.health.is_slot_parked(panel_slot) {
            return self.parked_decision(panel_slot);
        }
        self.ensure_bandit(panel_slot)?;
        let arm = self
            .pending_arms
            .remove(&panel_slot)
            .unwrap_or(self.bandits[&panel_slot].incumbent_idx);
        self.on_search_for_arm(panel_slot, arm, p99_ns, recall_k, bits_per_anchor)
    }

    pub fn on_search_for_arm(
        &mut self,
        panel_slot: PanelSlotId,
        arm_idx: usize,
        p99_ns: u64,
        recall_k: f64,
        bits_per_anchor: f64,
    ) -> Result<IndexTuneDecision> {
        self.on_search_for_arm_with_quant_evidence(
            panel_slot,
            arm_idx,
            p99_ns,
            recall_k,
            bits_per_anchor,
            None,
        )
    }

    pub fn on_search_for_arm_with_quant_evidence(
        &mut self,
        panel_slot: PanelSlotId,
        arm_idx: usize,
        p99_ns: u64,
        recall_k: f64,
        bits_per_anchor: f64,
        quant_evidence: Option<QuantPromotionEvidence>,
    ) -> Result<IndexTuneDecision> {
        if self.health.is_slot_parked(panel_slot) {
            return self.parked_decision(panel_slot);
        }
        self.ensure_bandit(panel_slot)?;
        let prior_idx = self.bandits[&panel_slot].incumbent_idx;
        let prior_config = self.config_for_arm(panel_slot, prior_idx)?;
        let won = self.arm_won(
            panel_slot,
            arm_idx,
            p99_ns,
            recall_k,
            bits_per_anchor,
            quant_evidence.as_ref(),
        )?;
        self.bandits
            .get_mut(&panel_slot)
            .expect("bandit ensured")
            .record_result(arm_idx, won)?;

        let new_idx = self.bandits[&panel_slot].incumbent_idx;
        let promoted = if new_idx != prior_idx {
            let new_config = self.config_for_arm(panel_slot, new_idx)?;
            let metrics = PromotionMetrics {
                latency_before_ns: self
                    .incumbent_latency_ns
                    .get(&panel_slot)
                    .copied()
                    .unwrap_or(p99_ns),
                latency_after_ns: p99_ns,
                recall_before: self
                    .incumbent_recall
                    .get(&panel_slot)
                    .copied()
                    .unwrap_or(recall_k),
                recall_after: recall_k,
                bits_before: self
                    .incumbent_bits
                    .get(&panel_slot)
                    .copied()
                    .unwrap_or(bits_per_anchor),
                bits_after: bits_per_anchor,
                quant_evidence,
            };
            self.record_incumbent_metrics(panel_slot, p99_ns, recall_k, bits_per_anchor);
            Some(self.promote_with_metrics(panel_slot, prior_config, new_config, metrics)?)
        } else {
            if arm_idx == prior_idx && metrics_are_valid(recall_k, bits_per_anchor) {
                self.record_incumbent_metrics(panel_slot, p99_ns, recall_k, bits_per_anchor);
            }
            None
        };

        self.save_bandit(panel_slot)?;
        let shadow_arm = self
            .bandits
            .get_mut(&panel_slot)
            .expect("bandit ensured")
            .select_arm()?;
        self.pending_arms.insert(panel_slot, shadow_arm);
        let shadow_candidate = (shadow_arm != new_idx)
            .then(|| self.config_for_arm(panel_slot, shadow_arm))
            .transpose()?;

        Ok(IndexTuneDecision {
            evaluated_arm: arm_idx,
            won,
            incumbent: self.get_incumbent_config(panel_slot)?,
            promoted,
            shadow_arm: Some(shadow_arm),
            shadow_candidate,
            skipped: None,
        })
    }

    pub fn install_candidates(
        &mut self,
        panel_slot: PanelSlotId,
        configs: Vec<IndexConfig>,
    ) -> Result<()> {
        if configs.is_empty() || configs.len() > MAX_INDEX_CANDIDATES {
            return Err(invalid_config(
                "Index candidate set must contain 1..=8 configs",
            ));
        }
        let mut bandit =
            ConfigBandit::new(BanditPolicy::Thompson, types::seed_for_slot(panel_slot));
        for config in configs {
            validate_index_config(&config)?;
            bandit.add_arm(encode_index_config(&config)?);
        }
        let key_hash = shape_key_hash(&index_slot_label(panel_slot));
        self.bandit_store.save_bandit(key_hash, &bandit)?;
        self.bandits.insert(panel_slot, bandit);
        Ok(())
    }

    pub fn get_incumbent_config(&self, panel_slot: PanelSlotId) -> Result<IndexConfig> {
        if let Some(bandit) = self.bandits.get(&panel_slot)
            && !bandit.arms.is_empty()
        {
            return decode_index_config(&bandit.incumbent()?.config);
        }
        let cache = self
            .cache
            .lock()
            .map_err(|_| invalid_config("autotune cache lock poisoned"))?;
        match cache.get(&slot_autotune_key(panel_slot, self.recall_target)) {
            Some(config) => IndexConfig::from_best_config(config),
            None => Ok(IndexConfig::default()),
        }
    }

    pub fn promotions(&self) -> &[IndexPromotionRecord] {
        &self.promotions
    }

    fn parked_decision(&self, panel_slot: PanelSlotId) -> Result<IndexTuneDecision> {
        Ok(IndexTuneDecision {
            evaluated_arm: self.pending_arms.get(&panel_slot).copied().unwrap_or(0),
            won: false,
            incumbent: self.get_incumbent_config(panel_slot)?,
            promoted: None,
            shadow_arm: None,
            shadow_candidate: None,
            skipped: Some(IndexTuneSkip::ParkedSlot),
        })
    }

    fn ensure_bandit(&mut self, panel_slot: PanelSlotId) -> Result<()> {
        if self.bandits.contains_key(&panel_slot) {
            return Ok(());
        }
        let key_hash = shape_key_hash(&index_slot_label(panel_slot));
        let bandit = match self.bandit_store.load_bandit(key_hash)? {
            Some(bandit) => bandit,
            None => {
                let mut bandit =
                    ConfigBandit::new(BanditPolicy::Thompson, types::seed_for_slot(panel_slot));
                for config in candidate_configs(panel_slot)? {
                    bandit.add_arm(encode_index_config(&config)?);
                }
                self.bandit_store.save_bandit(key_hash, &bandit)?;
                bandit
            }
        };
        self.bandits.insert(panel_slot, bandit);
        Ok(())
    }

    fn arm_won(
        &self,
        panel_slot: PanelSlotId,
        arm_idx: usize,
        p99_ns: u64,
        recall_k: f64,
        bits_per_anchor: f64,
        quant_evidence: Option<&QuantPromotionEvidence>,
    ) -> Result<bool> {
        if !metrics_are_valid(recall_k, bits_per_anchor) {
            return Ok(false);
        }
        let Some(bandit) = self.bandits.get(&panel_slot) else {
            return Ok(false);
        };
        if arm_idx == bandit.incumbent_idx {
            return Ok(true);
        }
        let candidate = self.config_for_arm(panel_slot, arm_idx)?;
        let incumbent = self.config_for_arm(panel_slot, bandit.incumbent_idx)?;
        let baseline_latency = self
            .incumbent_latency_ns
            .get(&panel_slot)
            .copied()
            .unwrap_or(p99_ns);
        let baseline_recall = self
            .incumbent_recall
            .get(&panel_slot)
            .copied()
            .unwrap_or(recall_k);
        let baseline_bits = self
            .incumbent_bits
            .get(&panel_slot)
            .copied()
            .unwrap_or(bits_per_anchor);
        let latency_ok = p99_ns < baseline_latency;
        let recall_ok = recall_k + RECALL_EPSILON >= baseline_recall;
        let bits_ok = quant_win_check(&candidate, &incumbent, baseline_bits, bits_per_anchor);
        if !(latency_ok && recall_ok && bits_ok) {
            return Ok(false);
        }
        if candidate.quant_bits != incumbent.quant_bits {
            validate_quant_promotion_evidence(quant_evidence.ok_or_else(|| {
                invalid_config("quant promotion requires measured cosine/FAR evidence")
            })?)?;
        }
        Ok(true)
    }

    fn promote_with_metrics(
        &mut self,
        panel_slot: PanelSlotId,
        old_config: IndexConfig,
        new_config: IndexConfig,
        metrics: PromotionMetrics,
    ) -> Result<IndexPromotionRecord> {
        let change_id = crate::ChangeId(self.next_change_id);
        self.next_change_id = self.next_change_id.saturating_add(1);
        let old_bytes = encode_index_config(&old_config)?;
        let new_bytes = encode_index_config(&new_config)?;
        let record = IndexPromotionRecord {
            panel_slot,
            change_id,
            old_config,
            new_config,
            latency_before_ns: metrics.latency_before_ns,
            latency_after_ns: metrics.latency_after_ns,
            recall_before: metrics.recall_before,
            recall_after: metrics.recall_after,
            bits_before: metrics.bits_before,
            bits_after: metrics.bits_after,
            slot_key_hash: shape_key_hash(&index_slot_label(panel_slot)),
            old_config_hash: *blake3::hash(&old_bytes).as_bytes(),
            new_config_hash: *blake3::hash(&new_bytes).as_bytes(),
            quant_evidence: metrics.quant_evidence,
        };
        self.write_cache(&record)?;
        self.promotion_writer.write_autotune_promote(&record)?;
        self.promotions.push(record.clone());
        Ok(record)
    }

    fn write_cache(&self, record: &IndexPromotionRecord) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| invalid_config("autotune cache lock poisoned"))?;
        cache.insert(
            slot_autotune_key(record.panel_slot, self.recall_target),
            record.new_config.to_best_config(record.panel_slot),
        );
        cache.persist().map_err(cache_write_fail)
    }

    fn save_bandit(&self, panel_slot: PanelSlotId) -> Result<()> {
        let key_hash = shape_key_hash(&index_slot_label(panel_slot));
        self.bandit_store
            .save_bandit(key_hash, &self.bandits[&panel_slot])
    }

    fn config_for_arm(&self, panel_slot: PanelSlotId, arm_idx: usize) -> Result<IndexConfig> {
        let bandit = self
            .bandits
            .get(&panel_slot)
            .ok_or_else(|| invalid_config("missing Index bandit"))?;
        let arm = bandit
            .arms
            .get(arm_idx)
            .ok_or_else(|| invalid_config(format!("Index arm {arm_idx} out of range")))?;
        decode_index_config(&arm.config)
    }

    fn record_incumbent_metrics(
        &mut self,
        panel_slot: PanelSlotId,
        p99_ns: u64,
        recall_k: f64,
        bits_per_anchor: f64,
    ) {
        self.incumbent_latency_ns
            .entry(panel_slot)
            .and_modify(|value| *value = (*value).min(p99_ns))
            .or_insert(p99_ns);
        self.incumbent_recall.entry(panel_slot).or_insert(recall_k);
        self.incumbent_bits
            .entry(panel_slot)
            .or_insert(bits_per_anchor);
    }
}

pub(super) fn invalid_config(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_INDEX_SCOPE_INVALID_CONFIG,
        message: message.into(),
        remediation: "repair Index scope autotune slot/config metadata before promotion",
    }
}

fn cache_write_fail(error: calyx_forge::ForgeError) -> CalyxError {
    CalyxError {
        code: CALYX_INDEX_CACHE_WRITE_FAIL,
        message: format!("Index autotune cache write failed: {error}"),
        remediation: "repair the PH16 autotune cache path before persisting an Index promotion",
    }
}
