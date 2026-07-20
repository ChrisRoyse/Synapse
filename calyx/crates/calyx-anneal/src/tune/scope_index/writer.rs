use calyx_core::{Clock, Result};
use calyx_ledger::LedgerCfStore;
use serde_json::{Value, json};

use super::types::{IndexPromotionRecord, index_slot_label};
use crate::{
    AnnealLedger, AnnealLedgerAction, AnnealLedgerEntry, AnnealSubstrate, BanditStorage,
    ConfigBandit, ConfigBanditStore, MetricComparison, MetricSnapshot, TripwireMetric,
};

pub trait IndexPromotionWriter {
    fn write_autotune_promote(&mut self, event: &IndexPromotionRecord) -> Result<()>;
}

#[derive(Default)]
pub struct NoopIndexPromotionWriter;

impl IndexPromotionWriter for NoopIndexPromotionWriter {
    fn write_autotune_promote(&mut self, _event: &IndexPromotionRecord) -> Result<()> {
        Ok(())
    }
}

impl<T> IndexPromotionWriter for &mut T
where
    T: IndexPromotionWriter + ?Sized,
{
    fn write_autotune_promote(&mut self, event: &IndexPromotionRecord) -> Result<()> {
        (**self).write_autotune_promote(event)
    }
}

pub trait IndexBanditPersistence {
    fn load_bandit(&self, key_hash: [u8; 32]) -> Result<Option<ConfigBandit>>;
    fn save_bandit(&self, key_hash: [u8; 32], bandit: &ConfigBandit) -> Result<()>;
}

#[derive(Default)]
pub struct NoopIndexBanditStore;

impl IndexBanditPersistence for NoopIndexBanditStore {
    fn load_bandit(&self, _key_hash: [u8; 32]) -> Result<Option<ConfigBandit>> {
        Ok(None)
    }

    fn save_bandit(&self, _key_hash: [u8; 32], _bandit: &ConfigBandit) -> Result<()> {
        Ok(())
    }
}

impl<S> IndexBanditPersistence for ConfigBanditStore<S>
where
    S: BanditStorage,
{
    fn load_bandit(&self, key_hash: [u8; 32]) -> Result<Option<ConfigBandit>> {
        self.load(key_hash)
    }

    fn save_bandit(&self, key_hash: [u8; 32], bandit: &ConfigBandit) -> Result<()> {
        self.save(key_hash, bandit)
    }
}

impl<S, C> IndexPromotionWriter for AnnealLedger<S, C>
where
    S: LedgerCfStore,
    C: Clock,
{
    fn write_autotune_promote(&mut self, event: &IndexPromotionRecord) -> Result<()> {
        self.write(autotune_ledger_entry(event)).map(|_| ())
    }
}

impl<'a, R, L, C, P> IndexPromotionWriter for AnnealSubstrate<'a, R, L, C, P>
where
    R: crate::RollbackStorage,
    L: LedgerCfStore,
    C: Clock,
    P: crate::BudgetProbe,
{
    fn write_autotune_promote(&mut self, event: &IndexPromotionRecord) -> Result<()> {
        self.write_outcome_event_with_details(
            AnnealLedgerAction::AutotunePromote,
            event.change_id,
            index_slot_label(event.panel_slot),
            event.new_config_hash,
            promotion_description(event),
            quant_promotion_details(event),
        )
    }
}

fn autotune_ledger_entry(event: &IndexPromotionRecord) -> AnnealLedgerEntry {
    AnnealLedgerEntry {
        action: AnnealLedgerAction::AutotunePromote,
        change_id: event.change_id,
        artifact_id: index_slot_label(event.panel_slot),
        prior_ptr_hash: event.old_config_hash,
        candidate_ptr_hash: event.new_config_hash,
        metrics: MetricSnapshot {
            evaluated_at: event.change_id.0,
            query_count: 1,
            metrics: vec![
                MetricComparison {
                    metric: TripwireMetric::SearchP99,
                    candidate_value: event.latency_after_ns as f64,
                    incumbent_value: event.latency_before_ns as f64,
                },
                MetricComparison {
                    metric: TripwireMetric::RecallAtK,
                    candidate_value: event.recall_after,
                    incumbent_value: event.recall_before,
                },
            ],
        },
        ts: event.change_id.0,
        description: promotion_description(event),
        fault: None,
        proposal: None,
        details: quant_promotion_details(event),
        prev_hash: None,
    }
}

fn quant_promotion_details(event: &IndexPromotionRecord) -> Option<Value> {
    let evidence = event.quant_evidence.as_ref()?;
    Some(json!({
        "tag": "quant_compression_promotion_v1",
        "scope": "index",
        "panel_version": event.panel_slot.panel_version(),
        "slot": event.panel_slot.slot_id().get(),
        "slot_hash_bytes": event.slot_key_hash,
        "level_before_bits": event.old_config.quant_bits,
        "level_after_bits": event.new_config.quant_bits,
        "bits_per_anchor_before": event.bits_before,
        "bits_per_anchor_after": event.bits_after,
        "cosine_error_before": evidence.cosine_error_before,
        "cosine_error_after": evidence.cosine_error_after,
        "max_cosine_error": evidence.max_cosine_error,
        "guard_far_before": evidence.guard_far_before,
        "guard_far_after": evidence.guard_far_after,
    }))
}

fn promotion_description(event: &IndexPromotionRecord) -> String {
    format!(
        "index autotune promote {} latency {} -> {} recall {:.6} -> {:.6} bits {:.6} -> {:.6} quant {} -> {}",
        index_slot_label(event.panel_slot),
        event.latency_before_ns,
        event.latency_after_ns,
        event.recall_before,
        event.recall_after,
        event.bits_before,
        event.bits_after,
        event.old_config.quant_bits,
        event.new_config.quant_bits
    )
}
