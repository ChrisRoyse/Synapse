//! Stable ledger entry kinds and wire codes.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Whether any writer in this workspace mints a kind.
///
/// #2094: four variants of [`EntryKind`] have never had a writer, and a reader
/// (`reproduce.rs`) was gating on one of them. A gate that cannot be satisfied
/// is indistinguishable from a gate that is never reached, so "nothing mints
/// this" stopped being folklore in a comment and became a declared, exhaustive
/// property of the enum that readers can consult and that a new variant cannot
/// silently join.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WriterStatus {
    /// At least one writer in the workspace mints this kind. The arm in
    /// [`EntryKind::writer_status`] names it.
    Minted,
    /// The variant holds a stable wire code and nothing mints it.
    ///
    /// The code must NOT be reused by a future shape — historical readers and
    /// the codec both key on it — but no reader may treat an entry of this kind
    /// as reachable evidence. A reader that needs one is reading a contract with
    /// no producer, and must say so rather than fall through a branch that can
    /// only ever be skipped.
    ReservedUnwritten,
}

/// Ledger event kind recorded in the append-only provenance chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryKind {
    Ingest,
    /// A per-slot frozen-lens measurement record: the evidence
    /// [`crate::reproduce::RecordedSlot`] describes and the reproduce path
    /// consumes.
    ///
    /// **Reserved-unwritten (#2094).** No writer in the workspace mints one, and
    /// no writer mints the `measure_refs` / `recorded_slots` answer-payload
    /// fields that would point at one, so the whole reproduce context contract
    /// is currently producer-less. `build_reproduce_context` therefore refuses
    /// with evidence instead of quietly returning an empty context.
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
    /// **Reserved-unwritten (#2094).** No writer mints it and no reader consumes
    /// it. Holds wire code 11 so a future shape cannot claim it.
    Admission,
    /// **Reserved-unwritten (#2094).** No writer mints it and no reader consumes
    /// it. Holds wire code 12 so a future shape cannot claim it.
    AgentForecast,
    Policy,
    /// **Reserved-unwritten (#2094).** No writer mints it and no reader consumes
    /// it. Holds wire code 14 so a future shape cannot claim it.
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

    /// Declares whether any writer in this workspace mints this kind.
    ///
    /// The match is exhaustive so a variant added later must state its answer
    /// here, and each `Minted` arm cites a writer so the claim is checkable
    /// rather than asserted. Verified by sweeping every `EntryKind::` reference
    /// in `calyx-*` and `crates/synapse-*` (#2094).
    pub const fn writer_status(self) -> WriterStatus {
        match self {
            // vault/store.rs, vault/dedup_commit.rs, vault/batch_ingest.rs,
            // vault/ledger_append.rs, the KV/document/relational/timeseries/blob
            // layer commits, txn/cross_model.rs, vault/ingest_precondition.rs.
            Self::Ingest => WriterStatus::Minted,
            // calyx-assay store.rs.
            Self::Assay => WriterStatus::Minted,
            // calyx-lodestar provenance.rs `append_kernel_build_entry`.
            Self::Kernel => WriterStatus::Minted,
            // calyx-loom reactive/durable.rs, calyx-ward ledger.rs, the
            // residency/retention and subscription writers.
            Self::Guard => WriterStatus::Minted,
            // calyx-lodestar provenance.rs / kernel_answer.rs, calyx-oracle
            // butterfly.rs, complete.rs, predict.rs, reverse_query.rs.
            Self::Answer => WriterStatus::Minted,
            // calyx-anneal report/rollback writers.
            Self::Anneal => WriterStatus::Minted,
            // vault/input_pointer.rs, vault/temporal_metadata.rs backfills.
            Self::Migrate => WriterStatus::Minted,
            // gc/orphan_reconciler Base repair, reproduce::append_reproduce_entry,
            // checkpoint publication.
            Self::Admin => WriterStatus::Minted,
            // vault/erase.rs per-constellation and vault-wide tombstones.
            Self::Erase => WriterStatus::Minted,
            // synapse-calyx `put_grounding_anchors` (single) and
            // `put_grounding_anchors_for_many` (multi-constellation batch).
            Self::Grounding => WriterStatus::Minted,
            // calyx-ward policy persistence.
            Self::Policy => WriterStatus::Minted,
            // vault/raw_commitment.rs checkpoint-cohort Merkle roots.
            Self::BatchCommitment => WriterStatus::Minted,
            // #2094: no writer anywhere in the workspace. See the variant docs.
            Self::Measure | Self::Admission | Self::AgentForecast | Self::Score => {
                WriterStatus::ReservedUnwritten
            }
        }
    }

    /// Whether an entry of this kind may carry a
    /// [`crate::reproduce::RecordedSlot`] — one slot's frozen-lens measurement.
    ///
    /// The reproduce path follows `measure_refs` from an answer entry to the
    /// entries holding its slot evidence, and must check that a referenced seq
    /// resolves to a measurement record rather than to an unrelated entry whose
    /// payload happens to parse. This predicate is that check's single
    /// declaration; `Measure` is the only kind whose payload is defined to be a
    /// recorded slot, and it is presently
    /// [`WriterStatus::ReservedUnwritten`] (#2094).
    pub const fn carries_recorded_slot(self) -> bool {
        matches!(self, Self::Measure)
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
