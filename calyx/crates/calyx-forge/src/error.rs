use std::error::Error;
use std::fmt;

const SEED_VERSION_REMEDIATION: &str =
    "Use the recorded rotation seed version or re-encode with the current quantizer seed";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForgeError {
    NumericalInvariant {
        op: String,
        detail: String,
        remediation: String,
    },
    DeviceUnavailable {
        device: String,
        detail: String,
        remediation: String,
    },
    GpuError {
        detail: String,
        remediation: String,
    },
    ShapeMismatch {
        expected: Vec<usize>,
        got: Vec<usize>,
        remediation: String,
    },
    Unimplemented {
        op: String,
        remediation: String,
    },
    QuantError {
        op: String,
        level: String,
        detail: String,
        remediation: String,
    },
    QuantIntelligenceLoss {
        slot: String,
        detail: String,
        remediation: String,
    },
    CacheError {
        op: String,
        path: String,
        detail: String,
        remediation: String,
    },
    LedgerError {
        op: String,
        detail: String,
        remediation: String,
    },
    /// A GPU allocation would exceed the configured soft VRAM budget, the
    /// live device free-VRAM headroom, or the budget was misconfigured.
    /// Fail-closed: any inability to *prove* an allocation is safe surfaces
    /// here (e.g. a failed `cudaMemGetInfo` is treated as over-budget).
    VramBudget {
        detail: String,
        remediation: String,
    },
    /// A host-wide GPU reservation could not be admitted atomically.
    ///
    /// Unlike [`Self::VramBudget`], this is explicitly cross-process and maps
    /// to the universal back-pressure code so an orchestrator can retry the
    /// same immutable job after another reservation releases.
    HostVramBackpressure {
        detail: String,
        remediation: String,
    },
    LensVramBudget {
        detail: String,
        remediation: String,
    },
    SeedVersionMismatch {
        expected: u8,
        got: u8,
    },
}

impl ForgeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NumericalInvariant { .. } => "CALYX_FORGE_NUMERICAL_INVARIANT",
            Self::DeviceUnavailable { .. } => "CALYX_FORGE_DEVICE_UNAVAILABLE",
            Self::GpuError { .. } => "CALYX_GPU_ERROR",
            Self::ShapeMismatch { .. } => "CALYX_FORGE_SHAPE_MISMATCH",
            Self::Unimplemented { .. } => "CALYX_FORGE_UNIMPLEMENTED",
            Self::QuantError { .. } => "CALYX_FORGE_QUANT_ERROR",
            Self::QuantIntelligenceLoss { .. } => "CALYX_QUANT_INTELLIGENCE_LOSS",
            Self::CacheError { .. } => "CALYX_FORGE_CACHE_ERROR",
            Self::LedgerError { .. } => "CALYX_FORGE_LEDGER_ERROR",
            Self::VramBudget { .. } => "CALYX_FORGE_VRAM_BUDGET",
            Self::HostVramBackpressure { .. } => "CALYX_BACKPRESSURE",
            Self::LensVramBudget { .. } => "CALYX_VRAM_BUDGET_EXCEEDED",
            Self::SeedVersionMismatch { .. } => "CALYX_FORGE_QUANT_SEED_VERSION",
        }
    }

    pub fn remediation(&self) -> &str {
        match self {
            Self::NumericalInvariant { remediation, .. }
            | Self::DeviceUnavailable { remediation, .. }
            | Self::GpuError { remediation, .. }
            | Self::ShapeMismatch { remediation, .. }
            | Self::Unimplemented { remediation, .. }
            | Self::QuantError { remediation, .. }
            | Self::QuantIntelligenceLoss { remediation, .. }
            | Self::CacheError { remediation, .. }
            | Self::LedgerError { remediation, .. }
            | Self::VramBudget { remediation, .. }
            | Self::HostVramBackpressure { remediation, .. }
            | Self::LensVramBudget { remediation, .. } => remediation,
            Self::SeedVersionMismatch { .. } => SEED_VERSION_REMEDIATION,
        }
    }
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let first_line = match self {
            Self::NumericalInvariant { op, detail, .. } => {
                format!("{} op={} detail={}", self.code(), op, detail)
            }
            Self::DeviceUnavailable { device, detail, .. } => {
                format!("{} device={} detail={}", self.code(), device, detail)
            }
            Self::GpuError { detail, .. } => format!("{} detail={}", self.code(), detail),
            Self::ShapeMismatch { expected, got, .. } => {
                format!("{} expected={expected:?} got={got:?}", self.code())
            }
            Self::Unimplemented { op, .. } => format!("{} op={op}", self.code()),
            Self::QuantError {
                op, level, detail, ..
            } => {
                format!(
                    "{} op={} level={} detail={}",
                    self.code(),
                    op,
                    level,
                    detail
                )
            }
            Self::QuantIntelligenceLoss { slot, detail, .. } => {
                format!("{} slot={} detail={}", self.code(), slot, detail)
            }
            Self::CacheError {
                op, path, detail, ..
            } => {
                format!("{} op={} path={} detail={}", self.code(), op, path, detail)
            }
            Self::LedgerError { op, detail, .. } => {
                format!("{} op={} detail={}", self.code(), op, detail)
            }
            Self::VramBudget { detail, .. } => {
                format!("{} detail={}", self.code(), detail)
            }
            Self::HostVramBackpressure { detail, .. } => {
                format!("{} detail={}", self.code(), detail)
            }
            Self::LensVramBudget { detail, .. } => {
                format!("{} detail={}", self.code(), detail)
            }
            Self::SeedVersionMismatch { expected, got } => {
                format!("{} expected={} got={}", self.code(), expected, got)
            }
        };
        if matches!(self, Self::NumericalInvariant { .. }) {
            debug_assert!(first_line.starts_with("CALYX_FORGE_NUMERICAL_INVARIANT"));
        }
        write!(f, "{first_line}\nRemediation: {}", self.remediation())
    }
}

impl Error for ForgeError {}
