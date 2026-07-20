use thiserror::Error;

pub type Result<T> = std::result::Result<T, LodestarError>;

#[derive(Clone, Debug, PartialEq, Error)]
pub enum LodestarError {
    #[error("CALYX_KERNEL_EMPTY_GRAPH: kernel graph selection requires at least one node")]
    KernelEmptyGraph,
    #[error("CALYX_KERNEL_INVALID_PARAMS: {detail}")]
    KernelInvalidParams { detail: String },
    #[error("CALYX_KERNEL_LP_UNAVAILABLE: {detail}")]
    KernelLpUnavailable { detail: String },
    #[error("CALYX_KERNEL_LP_INFEASIBLE: {detail}")]
    KernelLpInfeasible { detail: String },
    #[error("CALYX_KERNEL_EMPTY_RESULT: kernel selection returned no nodes")]
    KernelEmptyResult,
    #[error("CALYX_KERNEL_INDEX_NOT_FOUND: kernel index {kernel_id} was not found")]
    KernelIndexNotFound { kernel_id: calyx_core::CxId },
    #[error("CALYX_KERNEL_NOT_FOUND: kernel {kernel_id} was not found")]
    KernelNotFound { kernel_id: calyx_core::CxId },
    #[error("CALYX_KERNEL_ARTIFACT_CODEC: {detail}")]
    KernelArtifactCodec { detail: String },
    #[error("CALYX_KERNEL_DIM_MISMATCH: expected dim {expected}, got {actual}")]
    KernelDimMismatch { expected: usize, actual: usize },
    #[error("CALYX_KERNEL_EMBEDDING_MISSING: missing embedding for {cx_id}")]
    KernelEmbeddingMissing { cx_id: calyx_core::CxId },
    #[error("CALYX_KERNEL_INDEX_IO: {detail}")]
    KernelIndexIo { detail: String },
    #[error("CALYX_KERNEL_INDEX_CODEC: {detail}")]
    KernelIndexCodec { detail: String },
    #[error("CALYX_KERNEL_INDEX_BUILD: {detail}")]
    KernelIndexBuild { detail: String },
    #[error("CALYX_KERNEL_NO_ANCHORED_NODE: no anchored kernel node found")]
    KernelNoAnchoredNode,
    #[error("CALYX_KERNEL_ANSWER_NO_PATH: no path from {from} to {to}")]
    KernelAnswerNoPath {
        from: calyx_core::CxId,
        to: calyx_core::CxId,
    },
    #[error("CALYX_KERNEL_ANSWER_LEDGER_REQUIRED: {detail}")]
    KernelAnswerLedgerRequired { detail: String },
    #[error("CALYX_KERNEL_ANSWER_LEDGER_MISMATCH: {detail}")]
    KernelAnswerLedgerMismatch { detail: String },
    #[error("CALYX_KERNEL_PROVENANCE_PAYLOAD_CODEC: {detail}")]
    KernelProvenancePayloadCodec { detail: String },
    #[error("CALYX_KERNEL_SCORE_INVALID: {detail}")]
    KernelScoreInvalid { detail: String },
    #[error("CALYX_KERNEL_LOOM_SLOT_MAPPING_MISSING: no CxId mapping for {xterm_cx}/{slot}")]
    KernelLoomSlotMappingMissing {
        xterm_cx: calyx_core::CxId,
        slot: calyx_core::PanelSlotId,
    },
    #[error(
        "CALYX_KERNEL_LOOM_DIRECTIONAL_CONFIDENCE_MISSING: no directional confidence for {xterm_cx}/{a}->{b}"
    )]
    KernelLoomDirectionalConfidenceMissing {
        xterm_cx: calyx_core::CxId,
        a: calyx_core::PanelSlotId,
        b: calyx_core::PanelSlotId,
    },
    #[error("CALYX_KERNEL_LOOM_AGREEMENT_MISSING: no agreement xterm for {xterm_cx}/{a}<->{b}")]
    KernelLoomAgreementMissing {
        xterm_cx: calyx_core::CxId,
        a: calyx_core::PanelSlotId,
        b: calyx_core::PanelSlotId,
    },
    #[error("CALYX_KERNEL_LOOM_AGREEMENT_INVALID: {detail}")]
    KernelLoomAgreementInvalid { detail: String },
    #[error("CALYX_RECALL_EMPTY_CORPUS: recall evaluation has no held-out queries")]
    RecallEmptyCorpus,
    #[error("CALYX_RECALL_INVALID_PARAMS: {detail}")]
    RecallInvalidParams { detail: String },
    #[error("CALYX_KERNEL_RECALL_BELOW_GATE: ratio={ratio:.6} min={min:.6}")]
    RecallBelowGate { ratio: f32, min: f32 },
    #[error("CALYX_COLLECTION_NOT_FOUND: collection {id} was not found")]
    CollectionNotFound { id: String },
    #[error("CALYX_SCOPE_TEMPORAL_NOT_READY: time-window scope metadata is not initialized")]
    ScopeTemporalNotReady,
    #[error("CALYX_SCOPE_DEPTH_EXCEEDED: depth {depth} exceeds max {max}")]
    ScopeDepthExceeded { depth: usize, max: usize },
    #[error("CALYX_SCOPE_TENANT_NOT_FOUND: tenant {id} was not found")]
    ScopeTenantNotFound { id: String },
    #[error("CALYX_DFVS_VERIFICATION_FAILED: {detail}")]
    DfvsVerificationFailed { detail: String },
    #[error("CALYX_DFVS_GENUS_TOO_LARGE: genus {genus} exceeds supported bound")]
    DfvsGenusTooLarge { genus: usize },
    #[error("CALYX_DISCOVERY_NO_SUFFICIENCY_ASSAY: {detail}")]
    DiscoveryNoSufficiencyAssay { detail: String },
    #[error("CALYX_DISCOVERY_RUN_MANIFEST_INVALID: {detail}")]
    DiscoveryRunManifestInvalid { detail: String },
    #[error(
        "CALYX_DISCOVERY_RUN_MANIFEST_CHAIN_BROKEN: stage {stage} expected input {expected}, found {found}"
    )]
    DiscoveryRunManifestChainBroken {
        stage: String,
        expected: String,
        found: String,
    },
    #[error(
        "CALYX_DISCOVERY_RUN_MANIFEST_MISSING_UPSTREAM: stage {stage} references missing upstream {upstream}"
    )]
    DiscoveryRunManifestMissingUpstream { stage: String, upstream: String },
    #[error("CALYX_DISCOVERY_RUN_MANIFEST_DRIFT: {detail}")]
    DiscoveryRunManifestDrift { detail: String },
    #[error("CALYX_MOLECULAR_KERNEL_MISSING: {detail}")]
    MolecularKernelMissing { detail: String },
    #[error("CALYX_MOLECULAR_KERNEL_UNGROUNDED: {detail}")]
    MolecularKernelUngrounded { detail: String },
    #[error("CALYX_HYPOTHESIS_EVIDENCE_MISSING_PROVENANCE: no evidence provenance for {cx_id}")]
    HypothesisEvidenceMissingProvenance { cx_id: calyx_core::CxId },
    #[error("CALYX_HYPOTHESIS_EVIDENCE_EMPTY_ABSTRACT: empty evidence text for {cx_id}")]
    HypothesisEvidenceEmptyAbstract { cx_id: calyx_core::CxId },
    #[error("CALYX_HYPOTHESIS_EVIDENCE_INVALID: {detail}")]
    HypothesisEvidenceInvalid { detail: String },
    #[error("{code}: {message}")]
    TemporalKernel { code: &'static str, message: String },
    #[error("{code}: {message}")]
    Ledger { code: &'static str, message: String },
    #[error("{code}: {message}")]
    Graph { code: &'static str, message: String },
}

impl LodestarError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::KernelEmptyGraph => "CALYX_KERNEL_EMPTY_GRAPH",
            Self::KernelInvalidParams { .. } => "CALYX_KERNEL_INVALID_PARAMS",
            Self::KernelLpUnavailable { .. } => "CALYX_KERNEL_LP_UNAVAILABLE",
            Self::KernelLpInfeasible { .. } => "CALYX_KERNEL_LP_INFEASIBLE",
            Self::KernelEmptyResult => "CALYX_KERNEL_EMPTY_RESULT",
            Self::KernelIndexNotFound { .. } => "CALYX_KERNEL_INDEX_NOT_FOUND",
            Self::KernelNotFound { .. } => "CALYX_KERNEL_NOT_FOUND",
            Self::KernelArtifactCodec { .. } => "CALYX_KERNEL_ARTIFACT_CODEC",
            Self::KernelDimMismatch { .. } => "CALYX_KERNEL_DIM_MISMATCH",
            Self::KernelEmbeddingMissing { .. } => "CALYX_KERNEL_EMBEDDING_MISSING",
            Self::KernelIndexIo { .. } => "CALYX_KERNEL_INDEX_IO",
            Self::KernelIndexCodec { .. } => "CALYX_KERNEL_INDEX_CODEC",
            Self::KernelIndexBuild { .. } => "CALYX_KERNEL_INDEX_BUILD",
            Self::KernelNoAnchoredNode => "CALYX_KERNEL_NO_ANCHORED_NODE",
            Self::KernelAnswerNoPath { .. } => "CALYX_KERNEL_ANSWER_NO_PATH",
            Self::KernelAnswerLedgerRequired { .. } => "CALYX_KERNEL_ANSWER_LEDGER_REQUIRED",
            Self::KernelAnswerLedgerMismatch { .. } => "CALYX_KERNEL_ANSWER_LEDGER_MISMATCH",
            Self::KernelProvenancePayloadCodec { .. } => "CALYX_KERNEL_PROVENANCE_PAYLOAD_CODEC",
            Self::KernelScoreInvalid { .. } => "CALYX_KERNEL_SCORE_INVALID",
            Self::KernelLoomSlotMappingMissing { .. } => "CALYX_KERNEL_LOOM_SLOT_MAPPING_MISSING",
            Self::KernelLoomDirectionalConfidenceMissing { .. } => {
                "CALYX_KERNEL_LOOM_DIRECTIONAL_CONFIDENCE_MISSING"
            }
            Self::KernelLoomAgreementMissing { .. } => "CALYX_KERNEL_LOOM_AGREEMENT_MISSING",
            Self::KernelLoomAgreementInvalid { .. } => "CALYX_KERNEL_LOOM_AGREEMENT_INVALID",
            Self::RecallEmptyCorpus => "CALYX_RECALL_EMPTY_CORPUS",
            Self::RecallInvalidParams { .. } => "CALYX_RECALL_INVALID_PARAMS",
            Self::RecallBelowGate { .. } => "CALYX_KERNEL_RECALL_BELOW_GATE",
            Self::CollectionNotFound { .. } => "CALYX_COLLECTION_NOT_FOUND",
            Self::ScopeTemporalNotReady => "CALYX_SCOPE_TEMPORAL_NOT_READY",
            Self::ScopeDepthExceeded { .. } => "CALYX_SCOPE_DEPTH_EXCEEDED",
            Self::ScopeTenantNotFound { .. } => "CALYX_SCOPE_TENANT_NOT_FOUND",
            Self::DfvsVerificationFailed { .. } => "CALYX_DFVS_VERIFICATION_FAILED",
            Self::DfvsGenusTooLarge { .. } => "CALYX_DFVS_GENUS_TOO_LARGE",
            Self::DiscoveryNoSufficiencyAssay { .. } => "CALYX_DISCOVERY_NO_SUFFICIENCY_ASSAY",
            Self::DiscoveryRunManifestInvalid { .. } => "CALYX_DISCOVERY_RUN_MANIFEST_INVALID",
            Self::DiscoveryRunManifestChainBroken { .. } => {
                "CALYX_DISCOVERY_RUN_MANIFEST_CHAIN_BROKEN"
            }
            Self::DiscoveryRunManifestMissingUpstream { .. } => {
                "CALYX_DISCOVERY_RUN_MANIFEST_MISSING_UPSTREAM"
            }
            Self::DiscoveryRunManifestDrift { .. } => "CALYX_DISCOVERY_RUN_MANIFEST_DRIFT",
            Self::MolecularKernelMissing { .. } => "CALYX_MOLECULAR_KERNEL_MISSING",
            Self::MolecularKernelUngrounded { .. } => "CALYX_MOLECULAR_KERNEL_UNGROUNDED",
            Self::HypothesisEvidenceMissingProvenance { .. } => {
                "CALYX_HYPOTHESIS_EVIDENCE_MISSING_PROVENANCE"
            }
            Self::HypothesisEvidenceEmptyAbstract { .. } => {
                "CALYX_HYPOTHESIS_EVIDENCE_EMPTY_ABSTRACT"
            }
            Self::HypothesisEvidenceInvalid { .. } => "CALYX_HYPOTHESIS_EVIDENCE_INVALID",
            Self::TemporalKernel { code, .. } => code,
            Self::Ledger { code, .. } => code,
            Self::Graph { code, .. } => code,
        }
    }
}

impl From<calyx_core::CalyxError> for LodestarError {
    fn from(value: calyx_core::CalyxError) -> Self {
        Self::Ledger {
            code: value.code,
            message: value.message,
        }
    }
}

impl From<calyx_paths::PathsError> for LodestarError {
    fn from(value: calyx_paths::PathsError) -> Self {
        Self::Graph {
            code: value.code(),
            message: value.to_string(),
        }
    }
}

impl From<calyx_mincut::MincutError> for LodestarError {
    fn from(value: calyx_mincut::MincutError) -> Self {
        Self::Graph {
            code: value.code(),
            message: value.to_string(),
        }
    }
}

impl From<calyx_mincut::SpectralError> for LodestarError {
    fn from(value: calyx_mincut::SpectralError) -> Self {
        Self::Graph {
            code: value.code(),
            message: value.to_string(),
        }
    }
}
