# Shared ledger Base-row coverage

Date: 2026-08-17

Status: implemented; strict-client production verification pending

Issue: #2257

## Physical finding

Four unrelated production constellations carried distinct input hashes but the
same valid ledger reference at sequence `1530739`. `audit reproduce` reported
the entry present and self-verifying yet returned `subject_matches=false` and
`reproduced=false` for every record.

The shared sequence is intentional. Aster batch ingest stages one ledger entry
under the durable commit lock, declares every accepted constellation in the
versioned `batch_members` payload, overwrites each Base row with that exact
`LedgerRef`, and commits the Base and Ledger rows together. Only the first
member can be the entry's scalar subject.

The reproduction reader had drifted from this writer contract: it required
literal `SubjectId::Cx` equality and ignored the membership declaration and the
same coverage table already used by search.

## Decision

`calyx-ledger::base_stamp` owns one exhaustive `entry_cx_coverage` decision for
all Base-row readers. It distinguishes direct subject, legacy enumerated
member, declared batch member, historical/truncated batch scope, and decided
negative. A malformed declaration or an unregistered entry shape returns a
structured error; no reader may assume coverage.

Search and Aster reproduction both call this shared decision. Audit output
keeps literal `subject_matches` for compatibility and separately publishes
`coverage`, `coverage_matches`, and the overall reproduction verdict. A valid
non-first batch member therefore reports `subject_matches=false`,
`coverage=batch_member`, `coverage_matches=true`, and `reproduced=true` when
the physical entry self-verifies and its exact hash matches the Base row.

## Consequences

Future changes to a Base-stamping writer must update the shared writer gate and
read-side coverage function in the same crate, so compilation and fail-closed
runtime checks prevent another search/audit semantic split. Historical entries
without a post-#2096 declaration retain the documented hash-and-chain binding;
truncated declarations never turn a bounded list into a false negative.

The Source of Truth remains the constellation Base row's stored `LedgerRef`
plus the exact referenced Ledger CF bytes. Tool return values are diagnostics,
not the verdict; manual FSV must read both physical rows separately.
