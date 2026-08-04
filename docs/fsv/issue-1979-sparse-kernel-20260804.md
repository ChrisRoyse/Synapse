# Issue #1979: sparse grounding-kernel content

Date: 2026-08-04

## Source of truth

The source is a fresh robocopy of the live `%LOCALAPPDATA%\synapse\db-daemon`
vault, opened through the daemon's `full_mvcc_restore` path. The immutable copy
contained 1,657 files / 2,250,243,496 bytes and opened at durable sequence
492542. `Base` and slot column-family bytes are read independently by
`kernel_content_slot_kind_fsv`; no generated corpus stands in for them.

Research lanes: Exa MCP `web_search_exa` (live, topical query served) and the
built-in web lane. Primary reference: Stanford's *Introduction to Information
Retrieval*, whose exact cosine scorer traverses postings for query terms,
accumulates document scores, normalizes by document length, then selects top-k.

## Before

The production panel census identified `syn-agent-transcript-v1@1965002` as an
outcome-bearing corpus with 50,973 active records and 13,250 grounded records.
Every Base row declared sparse raw-TF slots 107/109 and dense record slot 110.
Before the fix, slot 107 returned
`SYNAPSE_CALYX_KERNEL_CONTENT_SLOT_NOT_DENSE` and contributed zero concepts.

## Trigger and physical readback

Release command:

```powershell
target\release\examples\kernel_content_slot_kind_fsv.exe `
  $env:TEMP\synapse-fsv-1979-20260804 1965002 107 110
```

Observed Base-CF census:

```text
panel_version = 1965002, seq = 492542
slot 107 declared by 50973 records
slot 110 declared by 50973 records
```

Observed outcomes:

```text
sparse slot 107: built 4237 nonempty concepts, measured recall 0.3149
absent slot 1: CALYX_KERNEL_EMPTY_RESULT, 0 concepts out of 50973
dense slot 110: built 20000 concepts (the configured cap), recall 0.0975
PASS: sparse and dense lanes reach native kernel math; absence does not
```

The sparse population is smaller than its declared-row count because real
transcript rows with no lexical content carry empty sparse vectors. They are
explicitly excluded, never interpreted as zero vectors. The 0.3149 result is an
honest below-gate quality measurement tracked by #1675, not a decode failure.

## Boundary audit

1. Empty sparse measurements: 46,736 physically declared rows were excluded;
   the remaining 4,237 reached graph selection and recall measurement.
2. Missing slot: slot 1 was absent before and after and returned the structured
   empty-corpus error with the physical 50,973-row denominator.
3. Maximum bound: dense slot 110 stopped at exactly 20,000 concepts and still
   completed graph and recall math.

During the dense control, Forge produced cosine `1.0000001`. The graph correctly
refused it. This was filed as #1995 and fixed by rejecting non-finite values and
clamping finite computed cosine to the graph's mathematical `[0,1]` domain. The
same live-vault trigger then built successfully.
