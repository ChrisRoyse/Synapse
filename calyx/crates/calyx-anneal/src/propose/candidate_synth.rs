use std::collections::BTreeMap;

use calyx_core::{CalyxError, Constellation, Modality, Result};
use serde::{Deserialize, Serialize};

use crate::EndpointUrl;

use super::{AnchorGap, DeficitMap, ModalityId};

mod targets;
pub use targets::{ConversionTarget, ExpectedTargetCost, ranked_conversion_targets};

pub const MAX_SYNTHESIS_CORPUS_SAMPLE: usize = 1000;
pub const CALYX_ANNEAL_CANDIDATE_INVALID_DEFICIT: &str = "CALYX_ANNEAL_CANDIDATE_INVALID_DEFICIT";
const CALYX_ASTER_CF_UNAVAILABLE: &str = "CALYX_ASTER_CF_UNAVAILABLE";
const METRIC_EPSILON: f64 = 1e-12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlgorithmicKind {
    #[serde(rename = "PCA")]
    Pca,
    TimeLag,
    FrequencyBand,
    ValueDivergence,
    ExceptionValue,
    ControlFlow,
    #[serde(rename = "TFIDF")]
    Tfidf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlgParams {
    pub target_anchor: String,
    pub sample_count: usize,
    pub seed: u64,
    pub features: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommissionSpec {
    pub target_modality: ModalityId,
    pub endpoint: Option<EndpointUrl>,
    pub model_id: Option<String>,
    #[serde(default = "default_axis")]
    pub axis: String,
    #[serde(default)]
    pub suggested_targets: Vec<ConversionTarget>,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "candidate_type", rename_all = "snake_case")]
pub enum CandidateLens {
    Algorithmic {
        kind: AlgorithmicKind,
        params: AlgParams,
    },
    Commission {
        spec: CommissionSpec,
    },
}

pub trait CorpusSampleSource {
    fn read_corpus_sample(&self, max_rows: usize) -> Result<Vec<Constellation>>;
}

pub fn synthesize_algorithmic(
    deficit: &DeficitMap,
    corpus_sample: &[Constellation],
) -> Option<CandidateLens> {
    let top = top_positive_gap(deficit)?;
    let sample = capped_sample(corpus_sample);
    if sample.is_empty() {
        return None;
    }
    let mut kinds = Vec::new();
    if has_value_target(deficit, sample) {
        kinds.push(AlgorithmicKind::ValueDivergence);
    }
    if has_exception_target(deficit, sample) {
        kinds.push(AlgorithmicKind::ExceptionValue);
    }
    if has_control_flow_target(deficit, sample) {
        kinds.push(AlgorithmicKind::ControlFlow);
    }
    if has_frequency_target(deficit, sample) {
        kinds.push(AlgorithmicKind::FrequencyBand);
    }
    if has_temporal_target(deficit, sample) {
        kinds.push(AlgorithmicKind::TimeLag);
    }
    if has_text_target(deficit) {
        kinds.push(AlgorithmicKind::Tfidf);
    }
    if deficit.underrepresented_modalities.is_empty()
        || deficit
            .underrepresented_modalities
            .iter()
            .any(|modality| matches!(modality, Modality::Structured | Modality::Mixed))
    {
        kinds.push(AlgorithmicKind::Pca);
    }
    kinds.sort_by(|left, right| {
        right
            .channel_prior_rank()
            .cmp(&left.channel_prior_rank())
            .then_with(|| algorithmic_name(*left).cmp(algorithmic_name(*right)))
    });
    kinds
        .first()
        .map(|kind| algorithmic_candidate(*kind, deficit, top, sample))
}

pub fn build_commission_spec(deficit: &DeficitMap) -> CandidateLens {
    let target_modality = deficit
        .underrepresented_modalities
        .first()
        .copied()
        .unwrap_or(Modality::Mixed);
    let suggested_targets = ranked_conversion_targets(deficit);
    let top_target = suggested_targets.first();
    let axis = top_target
        .map(|target| target.axis.clone())
        .unwrap_or_else(default_axis);
    let model_id = top_target.map(|target| target.hf_id.clone());
    let description = match top_gap(deficit) {
        Some(top) => format!(
            "commission frozen lens for '{}' modality axis '{}' targeting anchor '{}' gap {:.3} bits via {}",
            modality_name(target_modality),
            axis,
            top.anchor_class,
            top.gap,
            model_id.as_deref().unwrap_or("unresolved LensForge target")
        ),
        None => format!(
            "commission frozen lens for '{}' modality axis '{}'; no localized anchor gap was provided",
            modality_name(target_modality),
            axis
        ),
    };
    CandidateLens::Commission {
        spec: CommissionSpec {
            target_modality,
            endpoint: None,
            model_id,
            axis,
            suggested_targets,
            description,
        },
    }
}

pub fn synthesize(deficit: &DeficitMap, corpus_sample: &[Constellation]) -> Result<CandidateLens> {
    validate_deficit(deficit)?;
    Ok(synthesize_algorithmic(deficit, corpus_sample)
        .unwrap_or_else(|| build_commission_spec(deficit)))
}

pub fn synthesize_from_source(
    deficit: &DeficitMap,
    source: &dyn CorpusSampleSource,
) -> Result<CandidateLens> {
    let sample = source
        .read_corpus_sample(MAX_SYNTHESIS_CORPUS_SAMPLE)
        .map_err(corpus_unavailable)?;
    synthesize(deficit, &sample)
}

pub fn describe(candidate: &CandidateLens) -> String {
    match candidate {
        CandidateLens::Algorithmic { kind, params } => format!(
            "Algorithmic {} lens for anchor '{}' over {} corpus rows (seed {})",
            algorithmic_name(*kind),
            params.target_anchor,
            params.sample_count,
            params.seed
        ),
        CandidateLens::Commission { spec } => spec.description.clone(),
    }
}

fn algorithmic_candidate(
    kind: AlgorithmicKind,
    deficit: &DeficitMap,
    top: &AnchorGap,
    sample: &[Constellation],
) -> CandidateLens {
    CandidateLens::Algorithmic {
        kind,
        params: AlgParams {
            target_anchor: top.anchor_class.clone(),
            sample_count: sample.len(),
            seed: synthesis_seed(kind, top, sample),
            features: algorithmic_features(kind, deficit, top, sample),
        },
    }
}

fn algorithmic_features(
    kind: AlgorithmicKind,
    deficit: &DeficitMap,
    top: &AnchorGap,
    sample: &[Constellation],
) -> BTreeMap<String, String> {
    let mut features = BTreeMap::new();
    features.insert("gap_bits".to_string(), format!("{:.6}", top.gap));
    features.insert(
        "total_bits_deficit".to_string(),
        format!("{:.6}", deficit.total_bits_deficit),
    );
    features.insert(
        "modalities".to_string(),
        modality_list(&deficit.underrepresented_modalities),
    );
    features.insert("sample_count".to_string(), sample.len().to_string());
    features.insert(
        "created_at_span".to_string(),
        created_at_span(sample).to_string(),
    );
    features.insert(
        "channel_prior".to_string(),
        kind.channel_family().to_string(),
    );
    features.insert(
        "channel_prior_weight".to_string(),
        format!("{:.3}", kind.channel_prior_weight()),
    );
    features.insert(
        "channel_prior_source".to_string(),
        "issue774_value_density_prior".to_string(),
    );
    features.insert(
        "independence_contract".to_string(),
        "max_pairwise_corr<=0.600000".to_string(),
    );
    match kind {
        AlgorithmicKind::ValueDivergence => {
            features.insert(
                "value_axis".to_string(),
                "runtime-return-intermediate".to_string(),
            );
        }
        AlgorithmicKind::ExceptionValue => {
            features.insert(
                "exception_axis".to_string(),
                "exception-error-value".to_string(),
            );
            features.insert(
                "complementary_channel".to_string(),
                "value_divergence".to_string(),
            );
        }
        AlgorithmicKind::ControlFlow => {
            features.insert(
                "flow_axis".to_string(),
                "branch-path-control-flow".to_string(),
            );
        }
        AlgorithmicKind::Pca => {
            features.insert("basis".to_string(), "scalar-slot-covariance".to_string());
        }
        AlgorithmicKind::TimeLag => {
            features.insert("lag_axis".to_string(), "created_at".to_string());
        }
        AlgorithmicKind::FrequencyBand => {
            features.insert("band_axis".to_string(), "periodic-scalar".to_string());
        }
        AlgorithmicKind::Tfidf => {
            features.insert(
                "token_axis".to_string(),
                "metadata-and-text-modality".to_string(),
            );
        }
    }
    features
}

fn validate_deficit(deficit: &DeficitMap) -> Result<()> {
    validate_metric("total_bits_deficit", deficit.total_bits_deficit)?;
    for gap in &deficit.top_gaps {
        validate_metric("entropy_h", gap.entropy_h)?;
        validate_metric("mutual_info_i", gap.mutual_info_i)?;
        validate_metric("gap", gap.gap)?;
        if super::deficit_localize::violates_dpi(gap.mutual_info_i, gap.entropy_h) {
            return Err(invalid_deficit(format!(
                "anchor '{}' violates DPI: I={} > H={}",
                gap.anchor_class, gap.mutual_info_i, gap.entropy_h
            )));
        }
    }
    Ok(())
}

fn validate_metric(name: &'static str, value: f64) -> Result<()> {
    if !value.is_finite() || value < -METRIC_EPSILON {
        return Err(invalid_deficit(format!(
            "{name} must be finite and non-negative, got {value}"
        )));
    }
    Ok(())
}

fn top_positive_gap(deficit: &DeficitMap) -> Option<&AnchorGap> {
    top_gap(deficit).filter(|gap| gap.gap > 0.0)
}

fn top_gap(deficit: &DeficitMap) -> Option<&AnchorGap> {
    deficit
        .top_gaps
        .iter()
        .max_by(|left, right| left.gap.total_cmp(&right.gap))
}

fn default_axis() -> String {
    "unspecified".to_string()
}

fn capped_sample(corpus_sample: &[Constellation]) -> &[Constellation] {
    let end = corpus_sample.len().min(MAX_SYNTHESIS_CORPUS_SAMPLE);
    &corpus_sample[..end]
}

fn has_temporal_target(deficit: &DeficitMap, sample: &[Constellation]) -> bool {
    target_contains(
        deficit,
        &["temporal", "time", "lag", "recurrence", "periodic"],
    ) || sample_has_key(sample, &["temporal", "timestamp", "time_lag", "period"])
}

fn has_frequency_target(deficit: &DeficitMap, sample: &[Constellation]) -> bool {
    target_contains(deficit, &["frequency", "band", "spectrum", "periodic_band"])
        || sample_has_key(sample, &["frequency", "band", "spectrum"])
}

fn has_value_target(deficit: &DeficitMap, sample: &[Constellation]) -> bool {
    target_contains(
        deficit,
        &["value", "return", "runtime", "state", "intermediate"],
    ) || sample_has_key(sample, &["value", "return_value", "runtime", "state"])
}

fn has_exception_target(deficit: &DeficitMap, sample: &[Constellation]) -> bool {
    target_contains(deficit, &["exception", "error", "panic", "throw"])
        || sample_has_key(sample, &["exception", "error", "panic"])
}

fn has_control_flow_target(deficit: &DeficitMap, sample: &[Constellation]) -> bool {
    target_contains(deficit, &["control", "branch", "cfg", "path", "trace"])
        || sample_has_key(sample, &["control_flow", "branch", "cfg", "path"])
}

fn has_text_target(deficit: &DeficitMap) -> bool {
    deficit
        .underrepresented_modalities
        .iter()
        .any(|modality| matches!(modality, Modality::Text | Modality::Code))
        || target_contains(deficit, &["text", "token", "keyword", "tfidf"])
}

fn target_contains(deficit: &DeficitMap, needles: &[&str]) -> bool {
    top_gap(deficit)
        .map(|gap| {
            let anchor = gap.anchor_class.to_ascii_lowercase();
            needles.iter().any(|needle| anchor.contains(needle))
        })
        .unwrap_or(false)
}

fn sample_has_key(sample: &[Constellation], needles: &[&str]) -> bool {
    sample.iter().any(|cx| {
        cx.scalars.keys().any(|key| key_matches(key, needles))
            || cx
                .metadata
                .iter()
                .any(|(key, value)| key_matches(key, needles) || key_matches(value, needles))
    })
}

fn key_matches(value: &str, needles: &[&str]) -> bool {
    let value = value.to_ascii_lowercase();
    needles.iter().any(|needle| value.contains(needle))
}

fn created_at_span(sample: &[Constellation]) -> u64 {
    let min = sample.iter().map(|cx| cx.created_at).min().unwrap_or(0);
    let max = sample.iter().map(|cx| cx.created_at).max().unwrap_or(min);
    max.saturating_sub(min)
}

fn synthesis_seed(kind: AlgorithmicKind, top: &AnchorGap, sample: &[Constellation]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(algorithmic_name(kind).as_bytes());
    hasher.update(top.anchor_class.as_bytes());
    hasher.update(&sample.len().to_le_bytes());
    if let Some(first) = sample.first() {
        hasher.update(first.cx_id.as_bytes());
    }
    if let Some(last) = sample.last() {
        hasher.update(last.cx_id.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(bytes)
}

fn modality_list(modalities: &[ModalityId]) -> String {
    if modalities.is_empty() {
        return "none".to_string();
    }
    modalities
        .iter()
        .map(|modality| modality_name(*modality))
        .collect::<Vec<_>>()
        .join(",")
}

fn algorithmic_name(kind: AlgorithmicKind) -> &'static str {
    match kind {
        AlgorithmicKind::Pca => "PCA",
        AlgorithmicKind::TimeLag => "TimeLag",
        AlgorithmicKind::FrequencyBand => "FrequencyBand",
        AlgorithmicKind::ValueDivergence => "ValueDivergence",
        AlgorithmicKind::ExceptionValue => "ExceptionValue",
        AlgorithmicKind::ControlFlow => "ControlFlow",
        AlgorithmicKind::Tfidf => "TFIDF",
    }
}

impl AlgorithmicKind {
    fn channel_family(self) -> &'static str {
        match self {
            Self::ValueDivergence => "runtime_value",
            Self::ExceptionValue => "exception_value",
            Self::ControlFlow => "control_flow",
            Self::FrequencyBand => "frequency_value",
            Self::TimeLag => "temporal_value",
            Self::Tfidf => "semantic_structure",
            Self::Pca => "state_value",
        }
    }

    fn channel_prior_rank(self) -> u8 {
        match self {
            Self::ValueDivergence => 80,
            Self::ExceptionValue => 70,
            Self::FrequencyBand | Self::TimeLag => 60,
            Self::ControlFlow => 50,
            Self::Tfidf => 40,
            Self::Pca => 30,
        }
    }

    fn channel_prior_weight(self) -> f64 {
        f64::from(self.channel_prior_rank()) / 50.0
    }
}

fn modality_name(modality: ModalityId) -> &'static str {
    match modality {
        Modality::Text => "text",
        Modality::Code => "code",
        Modality::Image => "image",
        Modality::Audio => "audio",
        Modality::Video => "video",
        Modality::Protein => "protein",
        Modality::Dna => "dna",
        Modality::Molecule => "molecule",
        Modality::Structured => "structured",
        Modality::Mixed => "mixed",
    }
}

fn invalid_deficit(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_CANDIDATE_INVALID_DEFICIT,
        message: message.into(),
        remediation: "recompute the DeficitMap before synthesizing a candidate lens",
    }
}

fn corpus_unavailable(error: CalyxError) -> CalyxError {
    CalyxError {
        code: CALYX_ASTER_CF_UNAVAILABLE,
        message: format!(
            "corpus sample unavailable while synthesizing candidate lens: {}: {}",
            error.code, error.message
        ),
        remediation: "restore the Aster corpus sample before proposing a candidate lens",
    }
}
