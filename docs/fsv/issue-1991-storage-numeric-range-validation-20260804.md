# Issue #1991: storage numeric range validation

Date: 2026-08-04

## Root cause

`#[schemars(range(...))]` described numeric limits in the generated MCP JSON
Schema, but Serde deserialization did not enforce those limits. Several storage
operations therefore accepted values outside their published contract. The
corpus histogram and intelligence record limit then silently clamped the value,
turning an invalid request into a plausible success.

The fix validates every numeric range published by the affected find,
corpus-histogram, temporal-backfill, and intelligence request types at the
operation boundary, before any storage read or mutation. Failures use
`TOOL_PARAMS_INVALID`, name the operation, field, received value and accepted
range, and state that no storage operation was attempted. Silent clamps were
removed.

## Research

The Exa lane was probed after diagnosis with
`scripts/check-research-lane.ps1`: `exa-search-server` 3.4.0 initialized,
advertised both tools, and a real `web_search_exa` call returned content. The
structured readback was written to
`%TEMP%\synapse-research-lane-readback.json`.

The built-in research lane read primary upstream documentation:

- Schemars generates JSON Schema and aims to describe Serde's wire shape; it is
  not a runtime request validator: <https://docs.rs/crate/schemars/latest/source/README.md>
- Serde documents custom deserialization and structured deserialization errors:
  <https://serde.rs/custom-serialization.html> and
  <https://serde.rs/error-handling.html>
- JSON Schema defines inclusive `minimum` and `maximum` numeric validation:
  <https://json-schema.org/understanding-json-schema/reference/numeric>

The chosen application-boundary validator preserves the facade's structured
error/remediation envelope and guarantees validation precedes storage access.

## Build and deployment

- Commit: `5f7ac9b8` (`enforce storage numeric parameter ranges`)
- `cargo check -p synapse-mcp`: pass
- `pwsh -File scripts/lint.ps1`: all seven gates passed in both workspaces
- Deployment: `scripts/synapse-setup.ps1 -SourceDir C:\code\synapse`
- Installed process: PID `6816`,
  `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`
- Installed SHA-256:
  `4992F4FE97463E82E1E358AE15D0EA76028E3CAF57F57ED652DCD3D78BC7F3C3`

An independent live `storage inspect` read vault
`01KYJPGWATPD4XNMZY3ERGTKQW`, sequence `467351`, and `126590` live rows.

## Full State Verification

### Source of truth

For a rejected mutating intelligence request, the authoritative result is the
physical Calyx XTerm and Graph CF row counts, read independently through the
abundance operation before and after the trigger. The live vault's `CURRENT`
and selected manifest bytes were also hashed to expose concurrent whole-vault
activity.

Pre-trigger whole-vault readback:

- CURRENT: `manifest-00000000000000030899.json`
- CURRENT SHA-256:
  `16D6BE452FA2F7EF853E72CEF2F924874867E438E40DEB08CD2DC854C0ED339D`
- manifest SHA-256:
  `A9C017BEC970E0E701EA3AA1A3F24DD4DE2840E7F8A5CD812DBEEEF4F92478A4`

The daemon's independent background ingestion advanced CURRENT by two
sequences during the invalid read-only batch. That concurrent change is
reported rather than misrepresented as an operation mutation. The isolated
derived-CF proof below is therefore the acceptance source of truth.

### Happy path and boundaries

1. Corpus histogram minimum: `max_rows=1`, `max_buckets=1` scanned and decoded
   exactly one authoritative `CF_AGENT_EVENTS` row, with zero decode failures.
2. Corpus histogram maximum: `max_rows=200000`, `max_buckets=1000` completed the
   real corpus scan: `9129` rows and zero decode failures.
3. Find minimum: `k=1` returned a real persisted generation (panel `1963001`,
   base sequence `425868`) and consulted BM25 slot `103`.
4. Intelligence minimum: `max_records=1` reported one constellation from panel
   `1965001`.
5. Intelligence maximum: `max_records=20000` reported all `9129` available
   constellations from panel `1965001` without clamping.

### Invalid and edge cases

Every case failed with structured `TOOL_PARAMS_INVALID`, including the exact
field/value, accepted range, and remediation:

- find: `k=0`, `k=1001`, `exact_slot=65536`
- histogram: `max_rows=0`, `max_rows=200001`, `max_buckets=0`,
  `max_buckets=1001`
- intelligence: `max_records=0/20001`, `knn_k=0`, `min_gate_lenses=1`,
  `ksg_k=33`, `bin_seconds=0`, `max_lag=65`, `min_recall_ratio=1.1`,
  `max_hops=0`

### Physical mutation proof

Before trigger (`operation=weave`, `max_records=0`):

```text
XTerm CF rows: 2271
Graph CF rows: 81236
derived rows: 2271
```

Trigger result:

```text
TOOL_PARAMS_INVALID
storage operation=intelligence field max_records=0 is outside the accepted
range (an integer in 1..=20000)
remediation: set max_records to an integer in 1..=20000; validation stopped
before any storage operation was attempted
```

Independent post-trigger read:

```text
XTerm CF rows: 2271
Graph CF rows: 81236
derived rows: 2271
```

The affected physical state was byte-logically unchanged: no XTerm or Graph row
was added, removed, or replaced by the invalid mutating request.

## Verdict

PASS. Published storage numeric ranges are enforced at runtime, invalid values
fail closed before storage access, valid lower and upper boundaries operate on
real persisted data, and the invalid mutating edge leaves the affected Calyx
source-of-truth CFs unchanged.
