//! Stable ledger entry kinds and wire codes.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Ledger event kind recorded in the append-only provenance chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryKind {
    Ingest,
    Measure,
    Assay,
    Kernel,
    Guard,
    Answer,
    Anneal,
    Migrate,
    Admin,
    Erase,
    Grounding,
    Admission,
    AgentForecast,
    Policy,
    Score,
    /// Checkpoint-cohort Merkle root over Aster raw-batch commitment rows.
    BatchCommitment,
}

impl EntryKind {
    /// All valid kinds in stable wire-code order.
    pub const ALL: [Self; 16] = [
        Self::Ingest,
        Self::Measure,
        Self::Assay,
        Self::Kernel,
        Self::Guard,
        Self::Answer,
        Self::Anneal,
        Self::Migrate,
        Self::Admin,
        Self::Erase,
        Self::Grounding,
        Self::Admission,
        Self::AgentForecast,
        Self::Policy,
        Self::Score,
        Self::BatchCommitment,
    ];

    /// Returns the stable one-byte discriminant used in ledger hashes/codecs.
    pub const fn wire_code(self) -> u8 {
        match self {
            Self::Ingest => 0,
            Self::Measure => 1,
            Self::Assay => 2,
            Self::Kernel => 3,
            Self::Guard => 4,
            Self::Answer => 5,
            Self::Anneal => 6,
            Self::Migrate => 7,
            Self::Admin => 8,
            Self::Erase => 9,
            Self::Grounding => 10,
            Self::Admission => 11,
            Self::AgentForecast => 12,
            Self::Policy => 13,
            Self::Score => 14,
            Self::BatchCommitment => 15,
        }
    }

    /// Parses a stable wire code.
    pub const fn from_wire_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Ingest),
            1 => Some(Self::Measure),
            2 => Some(Self::Assay),
            3 => Some(Self::Kernel),
            4 => Some(Self::Guard),
            5 => Some(Self::Answer),
            6 => Some(Self::Anneal),
            7 => Some(Self::Migrate),
            8 => Some(Self::Admin),
            9 => Some(Self::Erase),
            10 => Some(Self::Grounding),
            11 => Some(Self::Admission),
            12 => Some(Self::AgentForecast),
            13 => Some(Self::Policy),
            14 => Some(Self::Score),
            15 => Some(Self::BatchCommitment),
            _ => None,
        }
    }

    /// Stable lowercase label for logs/readbacks.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Measure => "measure",
            Self::Assay => "assay",
            Self::Kernel => "kernel",
            Self::Guard => "guard",
            Self::Answer => "answer",
            Self::Anneal => "anneal",
            Self::Migrate => "migrate",
            Self::Admin => "admin",
            Self::Erase => "erase",
            Self::Grounding => "grounding",
            Self::Admission => "admission",
            Self::AgentForecast => "agent_forecast",
            Self::Policy => "policy",
            Self::Score => "score",
            Self::BatchCommitment => "batch_commitment",
        }
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
