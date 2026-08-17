# ADR: Measure bounded semantic atoms beside exact action requests

Date: 2026-08-17  
Status: accepted for implementation; production FSV pending

## Context

Production action panel `2_185_003` made exact request identity, byte-size class,
and structural-shape class independently measurable. After 396 grounded real
outcomes, the panel carried `0.850772` of `0.984470` outcome-entropy bits. The
remaining `0.133698`-bit deficit was not safely filled by status, error,
response, or after-state: those values exist only after treatment and would be
target leakage.

The physical corpus exposed the real missing atom. Slot 118 hashes each scalar
request value as one exact identity. That is correct for audit and Guard
similarity, but two distinct executable paths or arguments have no shared
features even when their field, value kind, path class, and lexical components
are the common pre-trigger cause. Size and JSON shape cannot recover those
semantics.

The design follows point-in-time feature discipline from
[BigQuery feature serving](https://docs.cloud.google.com/bigquery/docs/feature-serving)
and [Azure ML point-in-time joins](https://learn.microsoft.com/en-us/azure/machine-learning/offline-retrieval-point-in-time-join-concepts?view=azureml-api-2):
only information present in the authenticated request before execution is
eligible. It also follows feature-hashing practice by retaining a bounded dense
projection while preserving the exact value separately.

## Decision

Issue immutable action panel `2_185_004`. Keep slots 118–120 byte-for-byte
unchanged and add slot 121 `syn.action.request_atoms.v1`, a 512-dimensional
signed feature-hash projection over:

- ordered JSON field/path identity and value kind;
- bounded string/path/URL, boolean, and numeric magnitude classes;
- lower-cased lexical and field-local lexical components;
- normalized numeric and long-hex identifiers, so instance ids do not dominate;
- source/tool/verb/channel envelope atoms; and
- explicit node/atom overflow.

The lens visits at most 64 request nodes and admits at most 128 distinct semantic
atoms. Raw scalar values never appear in feature names; short SHA-256 digests do.
The exact full request remains independently bound by slot 118 and its
writer-sealed length/digest. Historical rows without authenticated pre-action
state are `Absent`, never zero-filled or reconstructed.

The lens reads no outcome, status, error, response, after-state, timestamp, or
source key. Its source-field declaration and the reward-determining field
declaration therefore keep structural target-leakage checks binding.

## Consequences

- `2_185_003` remains immutable readable history and becomes a named
  superseded generation.
- Ten active typed slots produce `10 + C(10,2) + 1 = 56` base signals per
  complete action record, ten more than generation `2_185_003`; the DPI ceiling
  still bounds any information claim.
- The semantic lens automatically enters bits, sufficiency, redundancy,
  synergy, exhaustive typed causal maps, kernel composition, and optional Ward
  calibration without flattening the constellation.
- Guard, kernel, validation, readiness, search, and causal-map artifacts must be
  rebuilt from physical `2_185_004` rows. No prior derived state is inherited.
- Production acceptance requires strict-client MCP triggers and separate
  Base/slot/anchor/Assay/Guard/Kernel/Anneal/lowered-artifact readbacks; build
  and lint results are structural evidence only.
