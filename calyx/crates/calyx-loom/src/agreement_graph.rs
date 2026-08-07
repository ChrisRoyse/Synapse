//! In-memory xterm CF and agreement graph readbacks.

use std::collections::BTreeMap;

use calyx_aster::cf::{CfRouter, ColumnFamily};
use calyx_core::{CalyxError, CxId, PanelSlotId, Result, SlotId};
use serde::{Deserialize, Serialize};

use crate::cross_term::{
    AgreementOutcome, CrossTermKey, CrossTermKind, CrossTermValue, SignalProvenanceTag,
    ZeroNormSide, agreement_scalar, agreement_scalar_classified, agreement_weight, canonical_pair,
    concat_vec, delta_vec, interaction_vec,
};
use crate::error::{
    CALYX_LOOM_PANEL_SCOPE_REQUIRED, CALYX_LOOM_SLOT_MISSING, CALYX_LOOM_XTERM_SCHEMA_UNSUPPORTED,
    loom_error,
};
use crate::lru_cache::LruCache;
use crate::materialization::{MaterializationAction, MaterializationPlan};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct XtermRow {
    pub key: CrossTermKey,
    pub value: CrossTermValue,
    pub tag: SignalProvenanceTag,
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

#[derive(Clone, Debug)]
pub struct LoomStore {
    xterm_cf: BTreeMap<CrossTermKey, XtermRow>,
    measured_tags: BTreeMap<(CxId, PanelSlotId), SignalProvenanceTag>,
    cache: LruCache<CrossTermKey, CrossTermValue>,
    zero_norm_skips: Vec<ZeroNormAgreementSkip>,
    zero_norm_skip_total: usize,
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
        panel_version: u32,
        cx: CxId,
        slots: &BTreeMap<SlotId, Vec<f32>>,
    ) -> Result<usize> {
        let mut inserted = 0;
        for slot in slots.keys() {
            self.tag_measured(cx, PanelSlotId::new(panel_version, *slot));
        }
        let ids: Vec<_> = slots.keys().copied().collect();
        for i in 0..ids.len() {
            for j in i + 1..ids.len() {
                let a = ids[i];
                let b = ids[j];
                let key = CrossTermKey {
                    cx_id: cx,
                    a: PanelSlotId::new(panel_version, a),
                    b: PanelSlotId::new(panel_version, b),
                    kind: CrossTermKind::Agreement,
                };
                let value = match agreement_scalar_classified(&slots[&a], &slots[&b])? {
                    AgreementOutcome::Scored(value) => value,
                    AgreementOutcome::ZeroNorm(zero_side) => {
                        self.record_zero_norm_skip(ZeroNormAgreementSkip {
                            cx_id: cx,
                            a: key.a,
                            b: key.b,
                            zero_side,
                        });
                        continue;
                    }
                };
                self.xterm_cf.insert(
                    key,
                    XtermRow {
                        key,
                        value: CrossTermValue::Scalar(value),
                        tag: SignalProvenanceTag::Derived,
                    },
                );
                inserted += 1;
            }
        }
        Ok(inserted)
    }

    pub fn materialize_plan(
        &mut self,
        panel_version: u32,
        cx: CxId,
        slots: &BTreeMap<SlotId, Vec<f32>>,
        plan: &MaterializationPlan,
    ) -> Result<usize> {
        let mut inserted = 0;
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
            // #2076: a zero-norm operand is a valid measurement with no
            // direction, not a write failure. Skip the one pair it makes
            // undefined, name it, and let the rest of the record — and the rest
            // of the corpus — weave.
            let value = match compute_cross_term_classified(a, b, entry.kind, slots)? {
                CrossTermOutcome::Value(value) => value,
                CrossTermOutcome::ZeroNormAgreement(zero_side) => {
                    self.record_zero_norm_skip(ZeroNormAgreementSkip {
                        cx_id: cx,
                        a: key.a,
                        b: key.b,
                        zero_side,
                    });
                    continue;
                }
            };
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
        Ok(inserted)
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
            let row: XtermRow = serde_json::from_slice(&entry.value).map_err(|error| {
                loom_error(
                    CALYX_LOOM_XTERM_SCHEMA_UNSUPPORTED,
                    format!("decode panel-qualified xterm row: {error}"),
                )
            })?;
            validate_panel_pair(&row.key)?;
            if entry.key != xterm_key(&row.key) {
                return Err(CalyxError::aster_corrupt_shard(
                    "xterm CF key does not match row key",
                ));
            }
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

/// A materialized cross-term, or the reason there is none to materialize.
enum CrossTermOutcome {
    Value(CrossTermValue),
    /// Agreement only: one operand is a directionless exact measurement.
    /// `Delta`, `Interaction` and `Concat` are all defined at zero and are
    /// never reported here.
    ZeroNormAgreement(ZeroNormSide),
}

fn compute_cross_term_classified(
    a: SlotId,
    b: SlotId,
    kind: CrossTermKind,
    slots: &BTreeMap<SlotId, Vec<f32>>,
) -> Result<CrossTermOutcome> {
    let (left, right) = slot_pair(a, b, slots)?;
    match kind {
        CrossTermKind::Agreement => match agreement_scalar_classified(left, right)? {
            AgreementOutcome::Scored(value) => {
                Ok(CrossTermOutcome::Value(CrossTermValue::Scalar(value)))
            }
            AgreementOutcome::ZeroNorm(side) => Ok(CrossTermOutcome::ZeroNormAgreement(side)),
        },
        CrossTermKind::Delta => Ok(CrossTermOutcome::Value(CrossTermValue::Vector(delta_vec(
            left, right,
        )?))),
        CrossTermKind::Interaction => Ok(CrossTermOutcome::Value(CrossTermValue::Vector(
            interaction_vec(left, right)?,
        ))),
        CrossTermKind::Concat => Ok(CrossTermOutcome::Value(CrossTermValue::Vector(concat_vec(
            left, right,
        )?))),
    }
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
