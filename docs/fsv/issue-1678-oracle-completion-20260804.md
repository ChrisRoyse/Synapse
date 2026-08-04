# Issue #1678: Oracle completion FSV (2026-08-04)

## Root cause

Synapse published grounded action evidence and exposed Calyx forward/reverse
Oracle operations, but it had no completion facade. Calyx's completion primitive
already existed; the missing bridge was a bounded adapter from persisted action
constellations to Ward trusted regions, plus public storage/MCP routing.

The first physical run found a second defect: validation after capability-card
calibration meant refused empty/unknown slot requests advanced the vault. All
request and target validation now precedes calibration, so invalid input is a
zero-mutation refusal.

## Research

Research lane: Exa MCP (`exa-search-server` 3.4.0) was verified live by
`scripts/check-research-lane.ps1`, including a real `tools/call`. Built-in web
research used primary sources:

- NIST AI RMF Measure requires objective, repeatable, documented TEVV and
  operationally representative evaluation: <https://airc.nist.gov/airmf-resources/airmf/5-sec-core/>
- W3C PROV-O models provenance through entities, activities, use, generation,
  and derivation: <https://www.w3.org/TR/prov-o/>
- OpenTelemetry's stable log data model requires unambiguous typed records and
  structured error attributes: <https://opentelemetry.io/docs/specs/otel/logs/data-model/>

Applied design: explicit clamp/free partition, persisted anchored peers as the
only attractors, measured sufficiency and self-consistency gates, structured
refusals, and append-only completion provenance.

## Full State Verification

Source of truth: the Calyx vault at
`C:\Users\hotra\AppData\Local\Temp\synapse-oracle-completion-fsv-final-1785870893277`.
The trigger was `cargo run -p synapse-storage --example
action_oracle_prediction_fsv -- <vault>`. Verification reopened the vault
read-only and scanned physical `Base`, `Anchors`, `Recurrence`, `Assay`, and
`Ledger` column families at snapshot 423.

Happy path:

```text
INGEST_READBACK seq=412 action_rows=400 expected=400
HAPPY_COMPLETE before_seq=417 after_seq=421 cx_id=2d9eb1f46de8eca1d6be5e08a05323d7
result: converged=true energy_score=1.0; slot 50 lens tagged inferred; remaining calibrated lenses tagged measured
PHYSICAL_SOT snapshot=423 Base=402 Anchors=400 Recurrence=400 Assay=7 Ledger=412 completion_ledger_rows=1 panel=2006001
```

Boundary audit, with durable state before and after:

```text
EDGE_COMPLETE_EMPTY_FREE before_seq=421 after_seq=421 code=SYNAPSE_CALYX_ORACLE_COMPLETION_FREE_EMPTY
EDGE_COMPLETE_UNKNOWN_SLOT before_seq=421 after_seq=421 code=SYNAPSE_CALYX_ORACLE_COMPLETION_SLOT_UNKNOWN
EDGE_COMPLETE_INVALID_CX before_seq=421 after_seq=421 code=SYNAPSE_CALYX_CX_ID_INVALID
```

The physical ledger scan searched stored row bytes for
`oracle_completion_v1` and found exactly one row. Refused edge cases added no
sequence and therefore no row.
