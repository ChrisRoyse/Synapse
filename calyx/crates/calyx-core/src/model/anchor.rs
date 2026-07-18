//! Grounded outcome anchors.

use serde::{Deserialize, Serialize};

use crate::{AnchorKind, Result};

use super::{Ts, validation::record_schema_error};

/// A grounded real-outcome observation attached to a constellation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    /// Outcome axis.
    pub kind: AnchorKind,
    /// Observed value on the axis.
    pub value: AnchorValue,
    /// Oracle, human labeler, reward source, or external reality source.
    pub source: String,
    /// Server-observed timestamp.
    pub observed_at: Ts,
    /// Confidence in `(0, 1]`; deterministic oracles use `1.0`.
    pub confidence: f32,
}

/// Value carried by a grounded anchor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorValue {
    /// Boolean outcome.
    Bool(bool),
    /// Named categorical outcome.
    Enum(String),
    /// Numeric outcome or reward.
    Number(f64),
    /// One-hot categorical support.
    OneHot(Vec<String>),
    /// Textual label when the source cannot reduce to a category yet.
    Text(String),
    /// Dense anchor vector for identity/style comparisons.
    Vector(Vec<f32>),
}

impl Anchor {
    /// Validates a grounded anchor before it is written to a record boundary.
    pub fn validate_schema(&self) -> Result<()> {
        if self.source.trim().is_empty() {
            return Err(record_schema_error("anchor source must not be blank"));
        }
        if !self.confidence.is_finite() || self.confidence <= 0.0 || self.confidence > 1.0 {
            return Err(record_schema_error(
                "anchor confidence must be finite and within (0, 1]",
            ));
        }
        self.value.validate_schema()
    }
}

impl AnchorValue {
    /// Validates numeric anchor payloads against the record schema.
    pub fn validate_schema(&self) -> Result<()> {
        match self {
            Self::Number(value) if !value.is_finite() => {
                Err(record_schema_error("anchor number is NaN or Inf"))
            }
            Self::Vector(values) if values.is_empty() => {
                Err(record_schema_error("anchor vector must not be empty"))
            }
            Self::Vector(values) if values.iter().any(|value| !value.is_finite()) => {
                Err(record_schema_error("anchor vector contains NaN or Inf"))
            }
            Self::Bool(_)
            | Self::Enum(_)
            | Self::Number(_)
            | Self::OneHot(_)
            | Self::Text(_)
            | Self::Vector(_) => Ok(()),
        }
    }
}
