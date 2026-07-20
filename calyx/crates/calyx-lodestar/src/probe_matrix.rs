use std::collections::BTreeMap;

use calyx_core::{CxId, Panel, PanelSlotId, SlotState};
use calyx_sextant::{RrfProfile, lookup};
use serde::{Deserialize, Serialize};

use crate::{LodestarError, Result};

pub const PROBE_MATRIX_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeMatrixSpec {
    pub frontier: String,
    pub panel_version: u32,
    pub active_slots: Vec<PanelSlotId>,
    pub weighted_profiles: Vec<RrfProfile>,
    pub phrasings: Vec<ProbePhrasing>,
    pub lengths: Vec<ProbeLength>,
    pub top_k: usize,
}

impl ProbeMatrixSpec {
    pub fn new(
        frontier: impl Into<String>,
        panel: &Panel,
        weighted_profiles: Vec<RrfProfile>,
    ) -> Result<Self> {
        for profile in &weighted_profiles {
            lookup(*profile, panel).map_err(|error| LodestarError::KernelInvalidParams {
                detail: format!(
                    "probe profile {profile:?} is not bound to panel {}: {}",
                    panel.version, error.message
                ),
            })?;
        }
        Ok(Self {
            frontier: frontier.into(),
            panel_version: panel.version,
            active_slots: panel
                .slots
                .iter()
                .filter(|slot| slot.state == SlotState::Active && !slot.retrieval_only)
                .map(|slot| PanelSlotId::new(panel.version, slot.slot_id))
                .collect(),
            weighted_profiles,
            phrasings: ProbePhrasing::all(),
            lengths: ProbeLength::all(),
            top_k: 20,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbePhrasing {
    Terse,
    Clinical,
    Mechanistic,
    Analogical,
    Contrast,
}

impl ProbePhrasing {
    pub fn all() -> Vec<Self> {
        vec![
            Self::Terse,
            Self::Clinical,
            Self::Mechanistic,
            Self::Analogical,
            Self::Contrast,
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeLength {
    Entity,
    Phrase,
    Paragraph,
}

impl ProbeLength {
    pub fn all() -> Vec<Self> {
        vec![Self::Entity, Self::Phrase, Self::Paragraph]
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeFusionMode {
    KernelFirst,
    Rrf,
    WeightedRrf,
    SingleLens,
    Pipeline,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum ProbeLensEmphasis {
    Balanced,
    WeightedProfile(RrfProfile),
    Slot(PanelSlotId),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeVariant {
    pub id: usize,
    pub frontier: String,
    pub query_text: String,
    pub fusion: ProbeFusionMode,
    pub phrasing: ProbePhrasing,
    pub length: ProbeLength,
    pub lens_emphasis: ProbeLensEmphasis,
    pub top_k: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeHit {
    pub cx_id: CxId,
    pub score: f32,
    pub grounded: bool,
    pub provenance: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeRefusal {
    pub code: String,
    pub reason: String,
    pub deficit_bits: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeResponse {
    pub hits: Vec<ProbeHit>,
    pub refusals: Vec<ProbeRefusal>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeRecord {
    pub variant: ProbeVariant,
    pub hits: Vec<ProbeHit>,
    pub refusals: Vec<ProbeRefusal>,
    pub accepted_hit_count: usize,
    pub unique_grounded_hits: Vec<CxId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeProductivity {
    pub variant_id: usize,
    pub fusion: ProbeFusionMode,
    pub phrasing: ProbePhrasing,
    pub length: ProbeLength,
    pub lens_emphasis: ProbeLensEmphasis,
    pub unique_hit_count: usize,
    pub accepted_hit_count: usize,
    pub refusal_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeMatrixLog {
    pub schema_version: u32,
    pub spec: ProbeMatrixSpec,
    pub records: Vec<ProbeRecord>,
    pub productive: Vec<ProbeProductivity>,
}

pub fn build_probe_matrix(spec: &ProbeMatrixSpec) -> Result<Vec<ProbeVariant>> {
    validate_spec(spec)?;
    let mut variants = Vec::new();
    for phrasing in &spec.phrasings {
        for length in &spec.lengths {
            push_variant(
                &mut variants,
                spec,
                ProbeFusionMode::KernelFirst,
                *phrasing,
                *length,
                ProbeLensEmphasis::WeightedProfile(RrfProfile::Kernel),
            );
            push_variant(
                &mut variants,
                spec,
                ProbeFusionMode::Rrf,
                *phrasing,
                *length,
                ProbeLensEmphasis::Balanced,
            );
            for profile in &spec.weighted_profiles {
                push_variant(
                    &mut variants,
                    spec,
                    ProbeFusionMode::WeightedRrf,
                    *phrasing,
                    *length,
                    ProbeLensEmphasis::WeightedProfile(*profile),
                );
            }
            for slot in &spec.active_slots {
                push_variant(
                    &mut variants,
                    spec,
                    ProbeFusionMode::SingleLens,
                    *phrasing,
                    *length,
                    ProbeLensEmphasis::Slot(*slot),
                );
            }
            push_variant(
                &mut variants,
                spec,
                ProbeFusionMode::Pipeline,
                *phrasing,
                *length,
                ProbeLensEmphasis::Balanced,
            );
        }
    }
    Ok(variants)
}

pub fn run_probe_matrix<F>(spec: &ProbeMatrixSpec, mut probe: F) -> Result<ProbeMatrixLog>
where
    F: FnMut(&ProbeVariant) -> Result<ProbeResponse>,
{
    let variants = build_probe_matrix(spec)?;
    let mut records = Vec::with_capacity(variants.len());
    for variant in variants {
        let response = probe(&variant)?;
        validate_response(&response)?;
        let accepted_hit_count = response.hits.iter().filter(|hit| hit.grounded).count();
        records.push(ProbeRecord {
            variant,
            hits: response.hits,
            refusals: response.refusals,
            accepted_hit_count,
            unique_grounded_hits: Vec::new(),
        });
    }
    attach_unique_hits(&mut records);
    let productive = productive_rows(&records);
    Ok(ProbeMatrixLog {
        schema_version: PROBE_MATRIX_SCHEMA_VERSION,
        spec: spec.clone(),
        records,
        productive,
    })
}

fn push_variant(
    out: &mut Vec<ProbeVariant>,
    spec: &ProbeMatrixSpec,
    fusion: ProbeFusionMode,
    phrasing: ProbePhrasing,
    length: ProbeLength,
    lens_emphasis: ProbeLensEmphasis,
) {
    out.push(ProbeVariant {
        id: out.len(),
        frontier: spec.frontier.clone(),
        query_text: render_query(&spec.frontier, phrasing, length),
        fusion,
        phrasing,
        length,
        lens_emphasis,
        top_k: spec.top_k,
    });
}

fn render_query(frontier: &str, phrasing: ProbePhrasing, length: ProbeLength) -> String {
    let base = match phrasing {
        ProbePhrasing::Terse => frontier.to_string(),
        ProbePhrasing::Clinical => format!("clinical associations for {frontier}"),
        ProbePhrasing::Mechanistic => format!("mechanisms and pathways related to {frontier}"),
        ProbePhrasing::Analogical => {
            format!("cross-domain analogies resembling {frontier}")
        }
        ProbePhrasing::Contrast => format!("contradictions or contrasts for {frontier}"),
    };
    match length {
        ProbeLength::Entity => base,
        ProbeLength::Phrase => base,
        ProbeLength::Paragraph => format!(
            "{base}. Return grounded biomedical associations with provenance and refusal evidence."
        ),
    }
}

fn attach_unique_hits(records: &mut [ProbeRecord]) {
    let mut counts = BTreeMap::<CxId, usize>::new();
    for record in records.iter() {
        for hit in record.hits.iter().filter(|hit| hit.grounded) {
            *counts.entry(hit.cx_id).or_default() += 1;
        }
    }
    for record in records {
        record.unique_grounded_hits = record
            .hits
            .iter()
            .filter(|hit| hit.grounded && counts.get(&hit.cx_id) == Some(&1))
            .map(|hit| hit.cx_id)
            .collect();
    }
}

fn productive_rows(records: &[ProbeRecord]) -> Vec<ProbeProductivity> {
    let mut rows: Vec<_> = records
        .iter()
        .filter(|record| record.accepted_hit_count > 0)
        .map(|record| ProbeProductivity {
            variant_id: record.variant.id,
            fusion: record.variant.fusion.clone(),
            phrasing: record.variant.phrasing,
            length: record.variant.length,
            lens_emphasis: record.variant.lens_emphasis.clone(),
            unique_hit_count: record.unique_grounded_hits.len(),
            accepted_hit_count: record.accepted_hit_count,
            refusal_count: record.refusals.len(),
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .unique_hit_count
            .cmp(&left.unique_hit_count)
            .then_with(|| right.accepted_hit_count.cmp(&left.accepted_hit_count))
            .then_with(|| left.variant_id.cmp(&right.variant_id))
    });
    rows
}

fn validate_spec(spec: &ProbeMatrixSpec) -> Result<()> {
    if spec.frontier.trim().is_empty() {
        return invalid_params("frontier must not be empty");
    }
    if spec.active_slots.is_empty() {
        return invalid_params("active_slots must include at least one slot");
    }
    if let Some(slot) = spec
        .active_slots
        .iter()
        .find(|slot| slot.panel_version() != spec.panel_version)
    {
        return invalid_params(format!(
            "active slot {slot} does not belong to probe panel {}",
            spec.panel_version
        ));
    }
    if spec.weighted_profiles.is_empty() {
        return invalid_params("weighted_profiles must not be empty");
    }
    if spec.phrasings.is_empty() {
        return invalid_params("phrasings must not be empty");
    }
    if spec.lengths.is_empty() {
        return invalid_params("lengths must not be empty");
    }
    if spec.top_k == 0 {
        return invalid_params("top_k must be greater than zero");
    }
    Ok(())
}

fn validate_response(response: &ProbeResponse) -> Result<()> {
    for hit in &response.hits {
        if !hit.score.is_finite() {
            return invalid_params("probe hit score must be finite");
        }
    }
    for refusal in &response.refusals {
        if refusal.code.trim().is_empty() {
            return invalid_params("probe refusal code must not be empty");
        }
        if refusal
            .deficit_bits
            .is_some_and(|bits| !bits.is_finite() || bits < 0.0)
        {
            return invalid_params("probe refusal deficit_bits must be finite and non-negative");
        }
    }
    Ok(())
}

fn invalid_params<T>(detail: impl Into<String>) -> Result<T> {
    Err(LodestarError::KernelInvalidParams {
        detail: detail.into(),
    })
}
