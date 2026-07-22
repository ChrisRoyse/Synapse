use std::path::{Path, PathBuf};

use calyx_core::{
    Asymmetry, CalyxError, Constellation, Modality, Panel, Placement, QuantPolicy, Result, Ts,
};
use calyx_registry::{
    AlgorithmicLens, CommissionRequest, Registry, SlotSpec, SwapController, commission_lens,
    register_commissioned,
};

use super::{
    CALYX_REGISTRY_HOT_ADD_FAIL, CandidateLens, CommissionSpec, ConversionTarget,
    ExpectedTargetCost, HotAddPlan, HotAddReceipt, LensHotAdder,
};
use crate::{ArtifactKey, ArtifactPtr};

pub struct RegistryHotAdder<'a> {
    registry: &'a mut Registry,
    artifact_dir: PathBuf,
}

impl<'a> RegistryHotAdder<'a> {
    pub fn new(registry: &'a mut Registry) -> Self {
        Self::with_artifact_dir(registry, default_artifact_dir())
    }

    pub fn with_artifact_dir(registry: &'a mut Registry, artifact_dir: impl AsRef<Path>) -> Self {
        Self {
            registry,
            artifact_dir: artifact_dir.as_ref().to_path_buf(),
        }
    }
}

impl LensHotAdder for RegistryHotAdder<'_> {
    fn plan_hot_add(
        &mut self,
        panel: &Panel,
        candidate: &CandidateLens,
        _corpus: &[Constellation],
    ) -> Result<HotAddPlan> {
        let candidate_hash = hash_candidate(candidate)?;
        let panel_hash = hash_panel(panel)?;
        Ok(HotAddPlan {
            artifact_key: ArtifactKey::ConfigCache(panel_hash),
            prior_ptr: ArtifactPtr::ConfigCacheKeyHash(panel_hash),
            candidate_ptr: ArtifactPtr::ConfigCacheKeyHash(hash_many([
                b"lens-proposal-candidate".as_slice(),
                &candidate_hash,
                &panel_hash,
            ])),
            description: format!(
                "lens_proposal hot_add candidate={}",
                hex_prefix(candidate_hash, 12)
            ),
        })
    }

    fn apply_hot_add(
        &mut self,
        controller: &mut SwapController,
        candidate: &CandidateLens,
        corpus: &[Constellation],
        now: Ts,
        next_panel_generation: u32,
    ) -> Result<HotAddReceipt> {
        match candidate {
            CandidateLens::Algorithmic { .. } => {
                let (lens, spec) = candidate_slot_spec(candidate)?;
                let contract = lens.contract().clone();
                if !self.registry.contains(spec.lens_id) {
                    self.registry.register_frozen(lens, contract)?;
                }
                add_slot(controller, self.registry, spec, now, next_panel_generation)
            }
            CandidateLens::Commission { spec } => {
                let target = primary_target(spec)?;
                let artifact = commission_lens(
                    &CommissionRequest {
                        name: commissioned_name(&target),
                        base_model: target.hf_id.clone(),
                        corpus: corpus_bytes(corpus, spec)?,
                        output_dim: target_dim(&target),
                        modality: target.modality,
                        axis: Some(target.axis.clone()),
                    },
                    &self.artifact_dir.join(safe_slug(&target.axis)),
                )?;
                let lens_id = register_commissioned(self.registry, artifact.clone())?;
                let slot_spec = SlotSpec {
                    key: format!("anneal_{}", commissioned_name(&target)),
                    lens_id,
                    shape: artifact.contract.shape(),
                    modality: artifact.contract.modality(),
                    asymmetry: Asymmetry::None,
                    quant: QuantPolicy::turboquant_default(),
                    axis: Some(target.axis),
                    retrieval_only: false,
                    excluded_from_dedup: false,
                };
                add_slot(
                    controller,
                    self.registry,
                    slot_spec,
                    now,
                    next_panel_generation,
                )
            }
        }
    }
}

fn add_slot(
    controller: &mut SwapController,
    registry: &Registry,
    spec: SlotSpec,
    now: Ts,
    next_panel_generation: u32,
) -> Result<HotAddReceipt> {
    let outcome = controller.add_lens(registry, spec, [], now, next_panel_generation)?;
    Ok(HotAddReceipt {
        lens_id: outcome.slot.lens_id,
        panel_version: outcome.panel_version,
        slot_count: controller.panel().slots.len(),
    })
}

fn candidate_slot_spec(candidate: &CandidateLens) -> Result<(AlgorithmicLens, SlotSpec)> {
    let (name, lens, modality, axis) = match candidate {
        CandidateLens::Algorithmic { kind, params } => {
            let name = format!("anneal-{}-{}", algorithmic_key(*kind), params.seed);
            let (lens, modality, axis) = match kind {
                super::AlgorithmicKind::Tfidf => (
                    AlgorithmicLens::byte_features(&name, Modality::Text),
                    Modality::Text,
                    Some("tfidf".to_string()),
                ),
                super::AlgorithmicKind::TimeLag => (
                    AlgorithmicLens::scalar(&name, Modality::Structured),
                    Modality::Structured,
                    Some("created_at".to_string()),
                ),
                super::AlgorithmicKind::FrequencyBand => (
                    AlgorithmicLens::scalar(&name, Modality::Structured),
                    Modality::Structured,
                    Some("periodic".to_string()),
                ),
                super::AlgorithmicKind::ValueDivergence => (
                    AlgorithmicLens::scalar(&name, Modality::Structured),
                    Modality::Structured,
                    Some("runtime_value".to_string()),
                ),
                super::AlgorithmicKind::ExceptionValue => (
                    AlgorithmicLens::scalar(&name, Modality::Structured),
                    Modality::Structured,
                    Some("exception_value".to_string()),
                ),
                super::AlgorithmicKind::ControlFlow => (
                    AlgorithmicLens::scalar(&name, Modality::Structured),
                    Modality::Structured,
                    Some("control_flow".to_string()),
                ),
                super::AlgorithmicKind::Pca => (
                    AlgorithmicLens::scalar(&name, Modality::Structured),
                    Modality::Structured,
                    Some("pca".to_string()),
                ),
            };
            (name, lens, modality, axis)
        }
        CandidateLens::Commission { .. } => {
            return Err(CalyxError {
                code: CALYX_REGISTRY_HOT_ADD_FAIL,
                message:
                    "commissioned candidate must be applied through RegistryHotAdder::apply_hot_add"
                        .to_string(),
                remediation: "route commissioned candidates through the conversion target hot-add path",
            });
        }
    };
    let contract = lens.contract();
    let spec = SlotSpec {
        key: format!("anneal_{name}"),
        lens_id: contract.lens_id(),
        shape: contract.shape(),
        modality,
        asymmetry: Asymmetry::None,
        quant: QuantPolicy::None,
        axis,
        retrieval_only: false,
        excluded_from_dedup: false,
    };
    Ok((lens, spec))
}

fn primary_target(spec: &CommissionSpec) -> Result<ConversionTarget> {
    if let Some(target) = spec.suggested_targets.first() {
        return Ok(target.clone());
    }
    let Some(hf_id) = spec.model_id.clone() else {
        return Err(CalyxError {
            code: CALYX_REGISTRY_HOT_ADD_FAIL,
            message: "commissioned candidate has no hf_id conversion target".to_string(),
            remediation: "synthesize a ranked conversion target before registry hot-add",
        });
    };
    Ok(ConversionTarget {
        hf_id,
        modality: spec.target_modality,
        axis: spec.axis.clone(),
        formats: vec!["adapter".to_string()],
        expected_bits: 0.0,
        expected_cost: ExpectedTargetCost {
            placement: Placement::Cpu,
            vram_mb: 0.0,
            ram_mb: 0.0,
            ms_per_input: 1.0,
        },
        expected_bits_per_vram_mb: None,
        expected_bits_per_ms: 0.0,
    })
}

fn corpus_bytes(corpus: &[Constellation], spec: &CommissionSpec) -> Result<Vec<Vec<u8>>> {
    let mut rows = Vec::new();
    for cx in corpus.iter().take(256) {
        rows.push(serde_json::to_vec(cx).map_err(|error| CalyxError {
            code: CALYX_REGISTRY_HOT_ADD_FAIL,
            message: format!("serialize constellation for commission corpus failed: {error}"),
            remediation: "repair corpus serialization before registry hot-add",
        })?);
    }
    if rows.is_empty() {
        rows.push(spec.description.as_bytes().to_vec());
    }
    Ok(rows)
}

fn commissioned_name(target: &ConversionTarget) -> String {
    format!(
        "{}-{}",
        safe_slug(&target.axis),
        hex_prefix(hash_bytes(target.hf_id.as_bytes()), 8)
    )
}

fn target_dim(target: &ConversionTarget) -> u32 {
    match target.modality {
        Modality::Text | Modality::Code | Modality::Structured | Modality::Mixed => 384,
        _ => 16,
    }
}

fn safe_slug(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn default_artifact_dir() -> PathBuf {
    std::env::var_os("CALYX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("lenses")
        .join("anneal-flywheel")
}

fn algorithmic_key(kind: super::AlgorithmicKind) -> &'static str {
    match kind {
        super::AlgorithmicKind::Pca => "pca",
        super::AlgorithmicKind::TimeLag => "time_lag",
        super::AlgorithmicKind::FrequencyBand => "frequency_band",
        super::AlgorithmicKind::ValueDivergence => "value_divergence",
        super::AlgorithmicKind::ExceptionValue => "exception_value",
        super::AlgorithmicKind::ControlFlow => "control_flow",
        super::AlgorithmicKind::Tfidf => "tfidf",
    }
}

fn hash_candidate(candidate: &CandidateLens) -> Result<[u8; 32]> {
    serde_json::to_vec(candidate)
        .map(|bytes| blake3::hash(&bytes).into())
        .map_err(|error| CalyxError {
            code: CALYX_REGISTRY_HOT_ADD_FAIL,
            message: format!("serialize candidate for hot-add hash failed: {error}"),
            remediation: "repair candidate serialization before registry hot-add",
        })
}

fn hash_panel(panel: &Panel) -> Result<[u8; 32]> {
    serde_json::to_vec(panel)
        .map(|bytes| blake3::hash(&bytes).into())
        .map_err(|error| CalyxError {
            code: CALYX_REGISTRY_HOT_ADD_FAIL,
            message: format!("serialize panel for hot-add hash failed: {error}"),
            remediation: "repair panel serialization before registry hot-add",
        })
}

fn hash_many<const N: usize>(parts: [&[u8]; N]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    blake3::hash(bytes).into()
}

fn hex32(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_prefix(bytes: [u8; 32], len: usize) -> String {
    hex32(bytes).chars().take(len).collect()
}
