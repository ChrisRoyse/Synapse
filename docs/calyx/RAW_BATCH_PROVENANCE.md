# Raw-batch provenance commitments

Status: accepted and implemented for issue #1804.

## Decision

Aster uses checkpoint-cohort commitments for commits that do not already
contain a Ledger row.

Per-row Ledger entries were rejected because Synapse's operational KV,
timeline, cursor, and outbox traffic often arrives as one-row batches. A
"one Ledger entry per batch" design therefore has the same write volume as
per-row chaining. Merely hashing the in-memory checkpoint batch was also
rejected: checkpoint coalescing intentionally collapses many sequences into
one newest-wins SST, so the original batch boundary cannot be reconstructed
from those SSTs after WAL recycling.

The implemented boundary has two levels:

1. Before a non-Ledger commit crosses the WAL boundary, Aster hashes its exact
   ordered rows (including the derived time-index row) with SHA-256 using
   explicit domain and length framing. It adds one fixed-size, sequence-keyed
   row to the reserved `raw_commitment` CF in the same WAL/MVCC transaction.
   Caller-supplied rows and revision guards for this CF fail with
   `CALYX_ASTER_RAW_COMMITMENT_WRITE_FORBIDDEN`.
2. Before checkpoint publication, Aster computes an RFC 9162-style,
   domain-separated Merkle root over all staged commitment rows and appends one
   `batch_commitment` entry to the existing Ledger chain. That Ledger entry is
   a hard checkpoint boundary: commits with a newer sequence remain staged for
   the next cohort.

The commitment CF uses unique sequence keys, so coalescing and ordinary
newest-wins compaction preserve every commitment without duplicating the raw
values or multiplying Ledger entries by the operational write rate.

## Crash and concurrency invariants

- The commitment row and its source rows share one WAL record and MVCC
  sequence; neither can commit alone.
- At most one cohort seal is outstanding in checkpoint staging. Recovery sees
  a WAL-resident seal and does not append a duplicate.
- A large cohort may require several bounded SST materialization chunks. Its
  seal remains above the manifest replay floor in WAL until the seal's own
  sequence is manifested, so an intermediate crash cannot strand it.
- The checkpoint publisher never drains a sequence newer than an outstanding
  seal. Later commits form the next cohort.
- The Ledger append uses the normal persistent hook and post-WAL
  reconciliation path; commitment maintenance has no bypass writer.

## Verification Source of Truth

`audit(operation="verify_chain")` reads two independent physical sources:

- `CF_LEDGER`, including each `batch_commitment` entry;
- the `raw_commitment` CF, including every sequence-keyed commitment row.

The verifier re-hashes the Ledger, partitions the physical commitment rows by
the ordered seal counts, recomputes each Merkle root, and fails closed on a
missing row, malformed codec, range mismatch, reordering, or root mismatch.
It reports total, sealed, and pending commitment counts. Pending rows are
atomically committed evidence awaiting the next periodic checkpoint seal, not
a silently accepted verification gap.

Coverage starts at `raw_commitment_coverage_from_seq`, the first commit made by
the implementation. The system does not invent retrospective operation
history for older raw writes whose original WAL batches have already been
coalesced and recycled.

## Cryptographic construction

The Merkle construction follows the current Certificate Transparency model:
leaves and interior nodes use distinct prefixes, ordered tree size is part of
the sealed payload, and SHA-256 produces a 32-byte root. Domain strings and
all variable-width batch fields are explicitly framed so concatenation is
unambiguous. See [RFC 9162](https://www.rfc-editor.org/rfc/rfc9162.html), which
specifies append-only Merkle logs and domain-separated leaf/node hashing.

