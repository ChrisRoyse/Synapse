//! The declared contract for stamping a stored constellation's Base-row
//! provenance with a ledger entry.
//!
//! # Why this lives in `calyx-ledger`
//!
//! Two sides need the same answer to one question — *may a ledger entry of this
//! `(EntryKind, SubjectId)` shape stand as the provenance of a Base row, and if
//! so how does it name the constellations it covers?*
//!
//! * the WRITE side (`calyx-aster`) rewrites `Constellation::provenance` on
//!   every `Base` row in a batch, and must refuse a shape the reader cannot
//!   serve **at write time**, where the caller can still fix it (#2095);
//! * the READ side (`calyx-search`) decides whether a hit's stored provenance
//!   actually attests that hit, and must be total over every shape a Base row
//!   can point at (#2084).
//!
//! Before #2095 the table existed only on the read side, so the writer could
//! mint a shape the reader had never been taught and the first symptom was a
//! refused query on an intact vault. The table now lives here — the crate both
//! sides already depend on — so the two agree *by construction*: a shape added
//! for a new writer is simultaneously a shape the reader recognises, and a shape
//! removed from the table is simultaneously refused at both ends.
//!
//! `calyx-aster` cannot host it (`calyx-search` would still be free to drift)
//! and `calyx-search` cannot host it (`calyx-aster` does not depend on
//! `calyx-search`, and must not — the writer would then need the reader).

use calyx_core::{CalyxError, Result};

use crate::entry::SubjectId;
use crate::kind::EntryKind;

/// A writer asked to stamp a Base row's provenance with a `(kind, subject)`
/// shape the declared coverage table cannot serve.
///
/// This is neither corruption nor a query-time fault: nothing has been written.
/// It is a contract violation caught at the only place it can still be repaired
/// cheaply — inside the writer, before the row exists (#2095).
pub const CALYX_LEDGER_BASE_STAMP_UNDECLARED: &str = "CALYX_LEDGER_BASE_STAMP_UNDECLARED";

const BASE_STAMP_UNDECLARED_REMEDIATION: &str = "declare this (entry kind, subject) shape in calyx_ledger::base_stamp::coverage_rule and \
     state how it names the constellations it covers, or mint the entry under a shape that is \
     already declared. Nothing was written and no vault state is damaged; do NOT restore from \
     backup";

/// The `SubjectId` variant, stripped of its identity payload so the coverage
/// table can dispatch on shape.
///
/// The conversion is an exhaustive `match` on purpose: a subject variant added
/// to [`SubjectId`] fails this build instead of silently collapsing into an
/// existing arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SubjectShape {
    Cx,
    Lens,
    Kernel,
    Guard,
    Query,
}

impl SubjectShape {
    /// Reduces a subject to its shape.
    pub fn of(subject: &SubjectId) -> Self {
        match subject {
            SubjectId::Cx(_) => Self::Cx,
            SubjectId::Lens(_) => Self::Lens,
            SubjectId::Kernel(_) => Self::Kernel,
            SubjectId::Guard(_) => Self::Guard,
            SubjectId::Query(_) => Self::Query,
        }
    }

    /// Stable label carried in evidence so a refusal names the shape it saw.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Cx => "cx",
            Self::Lens => "lens",
            Self::Kernel => "kernel",
            Self::Guard => "guard",
            Self::Query => "query",
        }
    }
}

/// How one declared ledger-entry shape names the constellations it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageRule {
    /// The entry covers exactly the constellation named by its `Cx` subject, so
    /// a `Cx` subject naming a different constellation is a decided negative.
    SubjectCx,
    /// A batch entry that enumerates its members in the payload under `cx_id`.
    /// A payload that cannot be read as that shape is malformed — a genuine
    /// corruption signal, not a negative.
    EnumeratedCxList,
    /// A declared batch entry that stamps one `LedgerRef` onto every Base row it
    /// commits. Entries minted after #2096 carry a
    /// [`crate::batch_members`] declaration that makes membership decidable;
    /// entries minted before it name no member at all, and the Base row's
    /// binding to them rests on the entry-hash and chain-link checks the reader
    /// runs before coverage.
    BatchScoped,
    /// No writer in this workspace stamps a Base row's provenance with this
    /// shape. Fail closed naming the shape rather than guess at its membership.
    Unregistered,
}

impl CoverageRule {
    /// Whether a writer may stamp a Base row's provenance under this rule.
    ///
    /// Exactly the complement of [`CoverageRule::Unregistered`], written as a
    /// method so the write-side gate and the read-side dispatch cannot disagree
    /// about which rules are servable.
    pub const fn permits_base_stamp(self) -> bool {
        match self {
            Self::SubjectCx | Self::EnumeratedCxList | Self::BatchScoped => true,
            Self::Unregistered => false,
        }
    }
}

/// The declared coverage rule for one `(EntryKind, SubjectShape)` shape.
///
/// #2084: coverage recognition used to be two hard-coded special cases — a `Cx`
/// subject naming the hit, or an `Ingest` entry whose payload listed it — and
/// every other legitimately minted shape fell through to a bare `false`, which
/// the caller reported as `CALYX_LEDGER_CORRUPT` with a "restore from restic"
/// remediation, on rows whose entry hash had just matched their Base provenance
/// byte for byte. The concrete shape that tripped it is the multi-constellation
/// grounding anchor batch (`Grounding` / `Query`), which stamps one entry onto
/// every constellation in the batch.
///
/// Recognition is this declared table, and both matches below are exhaustive
/// — over `EntryKind` and over `SubjectShape` — so a kind or subject variant
/// added to this crate breaks this build rather than degrading into a false
/// corruption verdict at query time. Each arm cites the writer that mints it.
///
/// #2095 made the table binding on writers too: see
/// [`require_base_stamp_declared`].
pub const fn coverage_rule(kind: EntryKind, subject: SubjectShape) -> CoverageRule {
    match kind {
        // Single ingest stamps `Cx(cx)` (calyx-aster vault/store.rs,
        // vault/dedup_commit.rs, vault/ledger_append.rs
        // `stage_raw_ingest_ledger_locked`) and is decided by the subject.
        // Batch ingest names only the FIRST accepted constellation as subject
        // and enumerates the whole batch in the payload `cx_id` array
        // (calyx-aster vault/batch_ingest.rs `batch_payload`), so a `Cx` subject
        // that is not ours must enumerate. Every other subject variant is a
        // batch marker minted by a caller supplying its own subject and payload:
        // the derived snapshot publication (`Query(operation_id)`, synapse-calyx
        // panel_lifecycle.rs), the cross-model transaction (`Query(batch
        // digest)`, calyx-aster txn/cross_model.rs), the precondition-gated
        // ingest (`Guard(vault_id)`, calyx-aster vault/ingest_precondition.rs)
        // and the KV/document/relational/timeseries/blob layer commits. Since
        // #2096 the caller-supplied ones carry a `batch_members` declaration.
        EntryKind::Ingest => match subject {
            SubjectShape::Cx => CoverageRule::EnumeratedCxList,
            SubjectShape::Lens
            | SubjectShape::Kernel
            | SubjectShape::Guard
            | SubjectShape::Query => CoverageRule::BatchScoped,
        },
        // The single-anchor grounding writer stamps `Cx(cx)` (synapse-calyx
        // `put_grounding_anchors`). The multi-constellation writer stamps ONE
        // `Query(b"synapse.grounding_anchor.multi.v1")` entry onto every
        // constellation in the batch (synapse-calyx
        // `put_grounding_anchors_for_many` -> calyx-aster
        // vault/ledger_anchor_batch.rs `multi_cx_anchor_rows_with_ledger_ref`).
        // That is the shape #2084 reported as vault corruption; since #2096 it
        // enumerates its members in a `batch_members` declaration, so the reader
        // can positively verify it instead of merely accepting it.
        EntryKind::Grounding => match subject {
            SubjectShape::Cx => CoverageRule::SubjectCx,
            SubjectShape::Query => CoverageRule::BatchScoped,
            SubjectShape::Lens | SubjectShape::Kernel | SubjectShape::Guard => {
                CoverageRule::Unregistered
            }
        },
        // Single-subject rewrites of one stored constellation:
        //   Migrate  — retained-input-pointer and temporal-metadata backfills
        //              (calyx-aster vault/input_pointer.rs, vault/temporal_metadata.rs)
        //   Guard    — reactive trigger persistence and Ward verdicts
        //              (calyx-loom reactive/durable.rs, calyx-ward ledger.rs)
        //   Admin    — GC orphan Base repair (calyx-aster gc/orphan_reconciler)
        //   Erase    — per-constellation tombstones (calyx-aster erase.rs)
        // Each names its constellation in the subject and never covers a second
        // one, so a different `Cx` subject is a decided negative. Their non-`Cx`
        // forms (vault-wide erase `Guard(scope digest)`, residency/retention
        // `Guard(..)`, reproduce and checkpoint `Query(..)`, subscription
        // `Guard(..)`) commit no Base rows at all.
        EntryKind::Migrate | EntryKind::Guard | EntryKind::Admin | EntryKind::Erase => {
            match subject {
                SubjectShape::Cx => CoverageRule::SubjectCx,
                SubjectShape::Lens
                | SubjectShape::Kernel
                | SubjectShape::Guard
                | SubjectShape::Query => CoverageRule::Unregistered,
            }
        }
        // Kinds with no Base-stamping writer in the workspace.
        //
        // `Measure`, `Score`, `Admission` and `AgentForecast` have no writer at
        // all — they are [`EntryKind::WriterStatus::ReservedUnwritten`], which
        // `kind.rs` declares rather than leaving to folklore (#2094). The rest
        // (`Assay`, `Kernel`, `Answer`, `Anneal`, `Policy`, `BatchCommitment`)
        // are minted, but write evidence rows (Registry/Graph/AnnealReport/
        // checkpoint cohorts) and never rewrite a Base row's provenance.
        //
        // For the four unwritten kinds `Unregistered` is the only honest entry:
        // a table cannot declare how a shape names its members when no writer
        // exists to decide that. If one is ever wired, the writer must add its
        // arm here — the write-side gate below refuses until it does, so the
        // shape cannot reach a Base row ahead of its declaration.
        //
        // A `Cx` subject naming the hit is still accepted by the reader before
        // this table is consulted, so reaching here means the entry neither
        // names the hit nor belongs to a declared batch shape.
        EntryKind::Measure
        | EntryKind::Assay
        | EntryKind::Kernel
        | EntryKind::Answer
        | EntryKind::Anneal
        | EntryKind::Admission
        | EntryKind::AgentForecast
        | EntryKind::Policy
        | EntryKind::Score
        | EntryKind::BatchCommitment => match subject {
            SubjectShape::Cx
            | SubjectShape::Lens
            | SubjectShape::Kernel
            | SubjectShape::Guard
            | SubjectShape::Query => CoverageRule::Unregistered,
        },
    }
}

/// Refuses a `(kind, subject)` pair that may not stamp a Base row's provenance.
///
/// Call this from every writer that rewrites `Constellation::provenance` on a
/// caller-supplied row batch, *before* anything is staged. A pair the coverage
/// table cannot serve is refused here, naming the shape and the operation, so
/// the failure lands on the writer that chose it. Without this gate the write
/// succeeds and the first symptom is a refused query weeks later, on a vault an
/// operator then suspects of corruption (#2084, #2095).
///
/// # Errors
///
/// [`CALYX_LEDGER_BASE_STAMP_UNDECLARED`] when the shape's declared rule is
/// [`CoverageRule::Unregistered`].
pub fn require_base_stamp_declared(
    operation: &str,
    kind: EntryKind,
    subject: &SubjectId,
) -> Result<()> {
    let shape = SubjectShape::of(subject);
    let rule = coverage_rule(kind, shape);
    if rule.permits_base_stamp() {
        return Ok(());
    }
    Err(CalyxError {
        code: CALYX_LEDGER_BASE_STAMP_UNDECLARED,
        message: format!(
            "{operation} would stamp Base-row provenance with the {kind}/{} ledger entry shape, \
             which calyx_ledger::base_stamp::coverage_rule declares as \
             {rule:?} — no writer is declared to mint it onto a constellation and the search \
             provenance verifier could not decide which constellations it covers. Nothing was \
             written",
            shape.tag()
        ),
        remediation: BASE_STAMP_UNDECLARED_REMEDIATION,
    })
}
