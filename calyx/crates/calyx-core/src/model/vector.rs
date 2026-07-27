//! Slot vector representations.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{AbsentReason, Result};

use super::validation::record_schema_error;

/// Sparse vector entry.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SparseEntry {
    /// Ambient vector index.
    pub idx: u32,
    /// Entry value.
    pub val: f32,
}

/// Per-slot vector payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotVector {
    /// Dense f32 payload.
    Dense { dim: u32, data: Vec<f32> },
    /// Sparse payload.
    Sparse { dim: u32, entries: Vec<SparseEntry> },
    /// Multi-vector token payload.
    Multi {
        token_dim: u32,
        tokens: Vec<Vec<f32>>,
    },
    /// Explicit absence; this must never be interpreted as a zero vector.
    Absent { reason: AbsentReason },
}

impl SlotVector {
    /// Returns true when the vector is explicitly absent.
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent { .. })
    }

    /// Returns dense data only for a real dense vector.
    pub fn as_dense(&self) -> Option<&[f32]> {
        match self {
            Self::Dense { data, .. } => Some(data.as_slice()),
            Self::Sparse { .. } | Self::Multi { .. } | Self::Absent { .. } => None,
        }
    }

    /// Returns true when the payload carries a concrete, index-consumable shape.
    ///
    /// This exhaustive match is the authoritative index-admission predicate.
    /// Adding a vector shape therefore requires an explicit decision here
    /// instead of silently diverging across search consumers.
    pub const fn is_indexable(&self) -> bool {
        match self {
            Self::Dense { .. } | Self::Sparse { .. } | Self::Multi { .. } => true,
            Self::Absent { .. } => false,
        }
    }

    /// Validates a stored vector payload against the Calyx record schema.
    pub fn validate_schema(&self) -> Result<()> {
        match self {
            Self::Dense { dim, data } => validate_dense(*dim, data),
            Self::Sparse { dim, entries } => validate_sparse(*dim, entries),
            Self::Multi { token_dim, tokens } => validate_multi(*token_dim, tokens),
            Self::Absent { .. } => Ok(()),
        }
    }
}

fn validate_dense(dim: u32, data: &[f32]) -> Result<()> {
    if dim == 0 {
        return Err(record_schema_error(
            "dense slot dim must be greater than zero",
        ));
    }
    if data.len() != dim as usize {
        return Err(record_schema_error(format!(
            "dense slot dim {dim} does not match {} values",
            data.len()
        )));
    }
    ensure_finite("dense slot", data)
}

fn validate_sparse(dim: u32, entries: &[SparseEntry]) -> Result<()> {
    if dim == 0 {
        return Err(record_schema_error(
            "sparse slot dim must be greater than zero",
        ));
    }
    let mut previous = None;
    let mut strictly_increasing = true;
    for entry in entries {
        if entry.idx >= dim {
            return Err(record_schema_error(format!(
                "sparse slot index {} outside dim {dim}",
                entry.idx
            )));
        }
        if previous == Some(entry.idx) {
            return Err(record_schema_error(format!(
                "sparse slot index {} is duplicated",
                entry.idx
            )));
        }
        if previous.is_some_and(|index| index > entry.idx) {
            strictly_increasing = false;
        }
        previous = Some(entry.idx);
        if !entry.val.is_finite() {
            return Err(record_schema_error(format!(
                "sparse slot index {} is non-finite",
                entry.idx
            )));
        }
    }
    if strictly_increasing {
        return Ok(());
    }
    let mut seen = HashSet::with_capacity(entries.len());
    for entry in entries {
        if !seen.insert(entry.idx) {
            return Err(record_schema_error(format!(
                "sparse slot index {} is duplicated",
                entry.idx
            )));
        }
    }
    Ok(())
}

fn validate_multi(token_dim: u32, tokens: &[Vec<f32>]) -> Result<()> {
    if token_dim == 0 {
        return Err(record_schema_error(
            "multi-vector token_dim must be greater than zero",
        ));
    }
    if tokens.is_empty() {
        return Err(record_schema_error(
            "multi-vector payload must contain at least one token",
        ));
    }
    for (idx, token) in tokens.iter().enumerate() {
        if token.len() != token_dim as usize {
            return Err(record_schema_error(format!(
                "multi-vector token {idx} length {} does not match token_dim {token_dim}",
                token.len()
            )));
        }
        ensure_finite("multi-vector token", token)?;
    }
    Ok(())
}

fn ensure_finite(field: &str, values: &[f32]) -> Result<()> {
    if values.iter().all(|value| value.is_finite()) {
        return Ok(());
    }
    Err(record_schema_error(format!("{field} contains NaN or Inf")))
}
