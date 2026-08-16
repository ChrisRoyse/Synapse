//! In-memory xterm CF and agreement graph readbacks.

use std::collections::{BTreeMap, BTreeSet};

use calyx_aster::cf::{CfRouter, ColumnFamily};
use calyx_core::{CalyxError, CxId, PanelSlotId, Result, SlotId};
use calyx_forge::Backend;
use serde::{Deserialize, Serialize};

use crate::cross_term::{
    AgreementOutcome, CrossTermKey, CrossTermKind, CrossTermValue, SignalProvenanceTag,
    ZeroNormSide, agreement_batch_prevalidated, agreement_scalar, agreement_scalar_classified,
    agreement_weight, canonical_pair, concat_vec, delta_vec, ensure_same_dim, interaction_vec,
};
use crate::error::{
    CALYX_LOOM_DIM_MISMATCH, CALYX_LOOM_PANEL_SCOPE_REQUIRED, CALYX_LOOM_SLOT_MISSING,
    CALYX_LOOM_XTERM_SCHEMA_UNSUPPORTED, loom_error,
};
use crate::lru_cache::LruCache;
use crate::materialization::{MaterializationAction, MaterializationEntry, MaterializationPlan};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct XtermRow {
    pub key: CrossTermKey,
    pub value: CrossTermValue,
    pub tag: SignalProvenanceTag,
}

/// Decodes and key-verifies one physical Aster XTerm row.
///
/// Consumers that aggregate persisted cross-terms must prove the value's
/// embedded identity matches the physical key.  Decoding JSON alone would let
/// a misplaced/corrupt row silently contribute to the wrong panel or pair.
pub fn decode_xterm_kv_row(key: &[u8], value: &[u8]) -> Result<XtermRow> {
    let row: XtermRow = serde_json::from_slice(value).map_err(|error| {
        loom_error(
            CALYX_LOOM_XTERM_SCHEMA_UNSUPPORTED,
            format!("decode panel-qualified xterm row: {error}"),
        )
    })?;
    validate_panel_pair(&row.key)?;
    if key != xterm_key(&row.key) {
        return Err(CalyxError::aster_corrupt_shard(
            "xterm CF key does not match the identity embedded in its row",
        ));
    }
    Ok(row)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgreementEdge {
    pub a: PanelSlotId,
    pub b: PanelSlotId,
    pub raw_mean_agreement: f32,
    pub mean_agreement: f32,
    pub agreement_weight: f32,
    pub n: usize,
}

/// One within-record agreement cross-term that had no cosine to compute
/// because a slot vector measured to exactly zero (#2076).
///
/// Named at the exact `(cx_id, slot_a, slot_b)` the issue asks for, so an
/// operator reading a weave report can go straight to the source row rather
/// than being told only that "a vector" was zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZeroNormAgreementSkip {
    pub cx_id: CxId,
    pub a: PanelSlotId,
    pub b: PanelSlotId,
    /// Which of the pair had no direction. When both do, `A` is reported: the
    /// cosine primitive classifies the left operand first and stops there.
    pub zero_side: ZeroNormSide,
}

/// Cap on individually named skips retained per store. The *total* is always
/// counted separately, so truncation is visible and never silent.
pub const MAX_RECORDED_ZERO_NORM_SKIPS: usize = 1_024;

/// Maximum measured host/device payload submitted by one agreement dispatch.
///
/// A panel weave groups pairs by dimension and then submits bounded slabs. This
/// keeps the GPU fed without turning every record into its own reservation,
/// allocation, transfer, launch, synchronization, and telemetry transaction.
/// A single pair whose two vectors exceed the bound is still submitted whole;
/// splitting a cosine vector would change the reduction contract.
pub const MAX_AGREEMENT_DISPATCH_BYTES: usize = 16 * 1024 * 1024;

/// Readback from one panel-wide Loom materialization pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaterializationBatchReport {
    /// Newly materialized XTerm rows.
    pub inserted: usize,
    /// Actual Forge backend calls after dimension grouping and bounded slicing.
    pub backend_dispatches: usize,
    /// Distinct records with at least one zero-norm agreement pair.
    pub zero_norm_records: usize,
}

#[derive(Clone, Debug)]
pub struct LoomStore {
    xterm_cf: BTreeMap<CrossTermKey, XtermRow>,
    measured_tags: BTreeMap<(CxId, PanelSlotId), SignalProvenanceTag>,
    cache: LruCache<CrossTermKey, CrossTermValue>,
    zero_norm_skips: Vec<ZeroNormAgreementSkip>,
    zero_norm_skip_total: usize,
}

struct PendingAgreement<'a> {
    key: CrossTermKey,
    left: &'a [f32],
    right: &'a [f32],
}

impl LoomStore {
    pub fn new(cache_capacity: usize) -> Self {
        Self {
            xterm_cf: BTreeMap::new(),
            measured_tags: BTreeMap::new(),
            cache: LruCache::new(cache_capacity),
            zero_norm_skips: Vec::new(),
            zero_norm_skip_total: 0,
        }
    }

    /// Agreement cross-terms this store skipped for a zero-norm operand, up to
    /// [`MAX_RECORDED_ZERO_NORM_SKIPS`] of them.
    pub fn zero_norm_agreement_skips(&self) -> &[ZeroNormAgreementSkip] {
        &self.zero_norm_skips
    }

    /// How many were skipped in total, including any beyond the retained cap.
    pub const fn zero_norm_agreement_skip_total(&self) -> usize {
        self.zero_norm_skip_total
    }

    fn record_zero_norm_skip(&mut self, skip: ZeroNormAgreementSkip) {
        self.zero_norm_skip_total += 1;
        if self.zero_norm_skips.len() < MAX_RECORDED_ZERO_NORM_SKIPS {
            self.zero_norm_skips.push(skip);
        }
    }

    pub fn tag_measured(&mut self, cx: CxId, panel_slot: PanelSlotId) {
        self.measured_tags
            .insert((cx, panel_slot), SignalProvenanceTag::Measured);
    }

    pub fn measured_count(&self) -> usize {
        self.measured_tags.len()
    }

    pub fn xterm_count(&self) -> usize {
        self.xterm_cf.len()
    }

    pub fn cache_count(&self) -> usize {
        self.cache.len()
    }

    pub fn weave(
        &mut self,
        backend: &dyn Backend,
        panel_version: u32,
        cx: CxId,
        slots: &BTreeMap<SlotId, Vec<f32>>,
    ) -> Result<usize> {
        let ids: Vec<_> = slots.keys().copied().collect();
        let slot_count = ids.len();
        let pair_count = if slot_count.is_multiple_of(2) {
            (slot_count / 2).checked_mul(slot_count.saturating_sub(1))
        } else {
            slot_count.checked_mul(slot_count.saturating_sub(1) / 2)
        }
        .ok_or_else(|| {
            loom_error(
                CALYX_LOOM_DIM_MISMATCH,
                "Loom agreement pair count overflows usize",
            )
        })?;
        let mut entries = Vec::with_capacity(pair_count);
        for i in 0..ids.len() {
            for j in i + 1..ids.len() {
                entries.push(MaterializationEntry {
                    a: ids[i],
                    b: ids[j],
                    kind: CrossTermKind::Agreement,
                    action: MaterializationAction::EagerStore,
                });
            }
        }
        self.materialize_plan(
            backend,
            panel_version,
            cx,
            slots,
            &MaterializationPlan { entries },
        )
    }

    pub fn materialize_plan(
        &mut self,
        backend: &dyn Backend,
        panel_version: u32,
        cx: CxId,
        slots: &BTreeMap<SlotId, Vec<f32>>,
        plan: &MaterializationPlan,
    ) -> Result<usize> {
        self.materialize_plans(backend, panel_version, std::iter::once((cx, slots, plan)))
            .map(|report| report.inserted)
    }

    /// Materialize many record plans with one bounded batch per dimension slab.
    ///
    /// All validation and zero-norm accounting remain record-qualified. Only
    /// the independent Forge calls are coalesced, so results are identical to
    /// repeated [`Self::materialize_plan`] calls while avoiding thousands of
    /// tiny GPU control-plane transactions for a complete panel.
    pub fn materialize_plans<'a>(
        &mut self,
        backend: &dyn Backend,
        panel_version: u32,
        records: impl IntoIterator<
            Item = (
                CxId,
                &'a BTreeMap<SlotId, Vec<f32>>,
                &'a MaterializationPlan,
            ),
        >,
    ) -> Result<MaterializationBatchReport> {
        let mut inserted = 0;
        let mut queued_agreement_keys = BTreeSet::<CrossTermKey>::new();
        let mut zero_norm_records = BTreeSet::<CxId>::new();
        let mut agreement_by_dim = BTreeMap::<usize, Vec<PendingAgreement<'a>>>::new();
        for (cx, slots, plan) in records {
            let mut zero_norm_by_slot = BTreeMap::<SlotId, bool>::new();
            for slot in slots.keys() {
                self.tag_measured(cx, PanelSlotId::new(panel_version, *slot));
            }
            for entry in plan
                .entries
                .iter()
                .filter(|entry| entry.action == MaterializationAction::EagerStore)
            {
                let (a, b) = canonical_pair(entry.a, entry.b);
                let key = CrossTermKey {
                    cx_id: cx,
                    a: PanelSlotId::new(panel_version, a),
                    b: PanelSlotId::new(panel_version, b),
                    kind: entry.kind,
                };
                if self.xterm_cf.contains_key(&key) {
                    continue;
                }
                if entry.kind == CrossTermKind::Agreement {
                    let (left, right) = slot_pair(a, b, slots)?;
                    ensure_same_dim(left, right)?;
                    let left_zero = agreement_operand_zero_norm(a, slots, &mut zero_norm_by_slot)?;
                    let right_zero = agreement_operand_zero_norm(b, slots, &mut zero_norm_by_slot)?;
                    let zero_side = if left_zero {
                        Some(ZeroNormSide::A)
                    } else if right_zero {
                        Some(ZeroNormSide::B)
                    } else {
                        None
                    };
                    if let Some(zero_side) = zero_side {
                        self.record_zero_norm_skip(ZeroNormAgreementSkip {
                            cx_id: cx,
                            a: key.a,
                            b: key.b,
                            zero_side,
                        });
                        zero_norm_records.insert(cx);
                        continue;
                    }
                    if !queued_agreement_keys.insert(key) {
                        continue;
                    }
                    agreement_by_dim
                        .entry(left.len())
                        .or_default()
                        .push(PendingAgreement { key, left, right });
                    continue;
                }
                let value = compute_cross_term(a, b, entry.kind, slots)?;
                self.xterm_cf.insert(
                    key,
                    XtermRow {
                        key,
                        value,
                        tag: SignalProvenanceTag::Derived,
                    },
                );
                inserted += 1;
            }
        }

        let mut backend_dispatches = 0usize;
        for (dim, pending) in agreement_by_dim {
            let measured_values_per_pair = dim
                .checked_mul(2)
                .and_then(|values| values.checked_add(1))
                .ok_or_else(|| {
                    loom_error(
                        CALYX_LOOM_DIM_MISMATCH,
                        format!("Loom agreement measured payload overflows usize: dim={dim}"),
                    )
                })?;
            let measured_bytes_per_pair = measured_values_per_pair
                .checked_mul(std::mem::size_of::<f32>())
                .ok_or_else(|| {
                    loom_error(
                        CALYX_LOOM_DIM_MISMATCH,
                        format!("Loom agreement measured byte count overflows usize: dim={dim}"),
                    )
                })?;
            let pairs_per_dispatch =
                (MAX_AGREEMENT_DISPATCH_BYTES / measured_bytes_per_pair).max(1);
            for batch in pending.chunks(pairs_per_dispatch) {
                let pair_count = batch.len();
                let row_values = pair_count.checked_mul(dim).ok_or_else(|| {
                    loom_error(
                        CALYX_LOOM_DIM_MISMATCH,
                        format!(
                            "Loom agreement batch shape overflows usize: pairs={pair_count} dim={dim}"
                        ),
                    )
                })?;
                let mut left_rows = Vec::with_capacity(row_values);
                let mut right_rows = Vec::with_capacity(row_values);
                for pair in batch {
                    left_rows.extend_from_slice(pair.left);
                    right_rows.extend_from_slice(pair.right);
                }
                let scores = agreement_batch_prevalidated(
                    backend,
                    &left_rows,
                    &right_rows,
                    pair_count,
                    dim,
                )?;
                backend_dispatches = backend_dispatches.checked_add(1).ok_or_else(|| {
                    loom_error(
                        CALYX_LOOM_DIM_MISMATCH,
                        "Loom agreement backend dispatch count overflows usize",
                    )
                })?;
                for (pair, value) in batch.iter().zip(scores) {
                    self.xterm_cf.insert(
                        pair.key,
                        XtermRow {
                            key: pair.key,
                            value: CrossTermValue::Scalar(value),
                            tag: SignalProvenanceTag::Derived,
                        },
                    );
                    inserted += 1;
                }
            }
        }
        Ok(MaterializationBatchReport {
            inserted,
            backend_dispatches,
            zero_norm_records: zero_norm_records.len(),
        })
    }

    pub fn cross_term(
        &mut self,
        panel_version: u32,
        cx: CxId,
        a: SlotId,
        b: SlotId,
        kind: CrossTermKind,
        slots: &BTreeMap<SlotId, Vec<f32>>,
    ) -> Result<CrossTermValue> {
        let (a, b) = canonical_pair(a, b);
        let key = CrossTermKey {
            cx_id: cx,
            a: PanelSlotId::new(panel_version, a),
            b: PanelSlotId::new(panel_version, b),
            kind,
        };
        if let Some(row) = self.xterm_cf.get(&key) {
            return Ok(row.value.clone());
        }
        if let Some(value) = self.cache.get(&key) {
            return Ok(value);
        }
        let value = compute_cross_term(a, b, kind, slots)?;
        self.cache.put(key, value.clone());
        Ok(value)
    }

    pub fn agreement_graph(&self) -> Result<Vec<AgreementEdge>> {
        let mut edges = BTreeMap::<(PanelSlotId, PanelSlotId), (f32, usize)>::new();
        for row in self.xterm_cf.values() {
            if let CrossTermValue::Scalar(value) = row.value {
                let entry = edges.entry((row.key.a, row.key.b)).or_default();
                entry.0 += value;
                entry.1 += 1;
            }
        }
        let mut out = Vec::new();
        for ((a, b), (sum, n)) in edges {
            let raw = sum / n.max(1) as f32;
            out.push(AgreementEdge {
                a,
                b,
                raw_mean_agreement: raw,
                mean_agreement: raw,
                agreement_weight: agreement_weight(raw)?,
                n,
            });
        }
        Ok(out)
    }

    pub fn xterm_rows(&self) -> Vec<XtermRow> {
        self.xterm_cf.values().cloned().collect()
    }

    pub fn persist_xterms_to_aster(&self, router: &mut CfRouter) -> Result<usize> {
        for row in self.xterm_cf.values() {
            let key = xterm_key(&row.key);
            let value = serde_json::to_vec(row)
                .map_err(|error| CalyxError::disk_pressure(format!("encode xterm row: {error}")))?;
            router.put(ColumnFamily::XTerm, &key, &value)?;
        }
        router.flush_cf(ColumnFamily::XTerm)?;
        Ok(self.xterm_cf.len())
    }

    /// Encode and consume all in-memory XTerm rows as `(key, value)` byte pairs.
    ///
    /// Consuming the store releases each decoded row while its encoded write is
    /// built, avoiding two complete representations of a large panel in memory.
    pub fn into_xterm_kv_rows(self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::with_capacity(self.xterm_cf.len());
        for (_, row) in self.xterm_cf {
            let key = xterm_key(&row.key);
            let value = serde_json::to_vec(&row)
                .map_err(|error| CalyxError::disk_pressure(format!("encode xterm row: {error}")))?;
            out.push((key, value));
        }
        Ok(out)
    }

    /// Encode all in-memory XTerm rows as `(key, value)` byte pairs using the
    /// exact same key/value encoding as [`Self::persist_xterms_to_aster`].
    ///
    /// This lets callers persist the XTerm CF through a higher-level write path
    /// (e.g. an `AsterVault`'s WAL/MVCC `write_cf_batch`) instead of a raw
    /// `CfRouter`, keeping the on-disk encoding identical so
    /// [`Self::load_xterms_from_aster`] round-trips either way. Returns the rows
    /// in `CrossTermKey` order (the `BTreeMap` iteration order).
    pub fn xterm_kv_rows(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::with_capacity(self.xterm_cf.len());
        for row in self.xterm_cf.values() {
            let key = xterm_key(&row.key);
            let value = serde_json::to_vec(row)
                .map_err(|error| CalyxError::disk_pressure(format!("encode xterm row: {error}")))?;
            out.push((key, value));
        }
        Ok(out)
    }

    pub fn load_xterms_from_aster(router: &CfRouter, cache_capacity: usize) -> Result<Self> {
        let mut store = Self::new(cache_capacity);
        for entry in router.iter_cf(ColumnFamily::XTerm)? {
            let row = decode_xterm_kv_row(&entry.key, &entry.value)?;
            store.xterm_cf.insert(row.key, row);
        }
        Ok(store)
    }
}

fn xterm_key(key: &CrossTermKey) -> Vec<u8> {
    let mut out = Vec::with_capacity(33);
    out.extend_from_slice(b"CXTX2");
    out.extend_from_slice(key.cx_id.as_bytes());
    out.extend_from_slice(&key.a.panel_version().to_be_bytes());
    out.extend_from_slice(&key.a.slot_id().get().to_be_bytes());
    out.extend_from_slice(&key.b.panel_version().to_be_bytes());
    out.extend_from_slice(&key.b.slot_id().get().to_be_bytes());
    out.push(match key.kind {
        CrossTermKind::Concat => 0,
        CrossTermKind::Interaction => 1,
        CrossTermKind::Agreement => 2,
        CrossTermKind::Delta => 3,
    });
    out
}

fn validate_panel_pair(key: &CrossTermKey) -> Result<()> {
    if key.a.panel_version() != key.b.panel_version() {
        return Err(loom_error(
            CALYX_LOOM_PANEL_SCOPE_REQUIRED,
            format!("xterm pair spans panels {} and {}", key.a, key.b),
        ));
    }
    Ok(())
}

fn compute_cross_term(
    a: SlotId,
    b: SlotId,
    kind: CrossTermKind,
    slots: &BTreeMap<SlotId, Vec<f32>>,
) -> Result<CrossTermValue> {
    let (left, right) = slot_pair(a, b, slots)?;
    match kind {
        CrossTermKind::Agreement => Ok(CrossTermValue::Scalar(agreement_scalar(left, right)?)),
        CrossTermKind::Delta => Ok(CrossTermValue::Vector(delta_vec(left, right)?)),
        CrossTermKind::Interaction => Ok(CrossTermValue::Vector(interaction_vec(left, right)?)),
        CrossTermKind::Concat => Ok(CrossTermValue::Vector(concat_vec(left, right)?)),
    }
}

fn agreement_operand_zero_norm(
    slot: SlotId,
    slots: &BTreeMap<SlotId, Vec<f32>>,
    classified: &mut BTreeMap<SlotId, bool>,
) -> Result<bool> {
    if let Some(zero_norm) = classified.get(&slot) {
        return Ok(*zero_norm);
    }
    let values = slots.get(&slot).ok_or_else(|| {
        loom_error(
            CALYX_LOOM_SLOT_MISSING,
            format!("slot {} missing", slot.get()),
        )
    })?;
    let outcome = agreement_scalar_classified(values, values).map_err(|error| CalyxError {
        code: error.code,
        message: format!(
            "agreement operand slot {} failed one-time preclassification: {}",
            slot.get(),
            error.message
        ),
        remediation: error.remediation,
    })?;
    let zero_norm = matches!(outcome, AgreementOutcome::ZeroNorm(_));
    classified.insert(slot, zero_norm);
    Ok(zero_norm)
}

fn slot_pair(a: SlotId, b: SlotId, slots: &BTreeMap<SlotId, Vec<f32>>) -> Result<(&[f32], &[f32])> {
    let left = slots
        .get(&a)
        .ok_or_else(|| loom_error(CALYX_LOOM_SLOT_MISSING, format!("slot {} missing", a.get())))?;
    let right = slots
        .get(&b)
        .ok_or_else(|| loom_error(CALYX_LOOM_SLOT_MISSING, format!("slot {} missing", b.get())))?;
    Ok((left, right))
}
