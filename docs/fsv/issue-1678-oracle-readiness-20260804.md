# Issue #1678: Oracle readiness predicate FSV (2026-08-04)

## Diagnosis and design

The Calyx six-tier predicate existed but Synapse had no persisted measurement,
no public operation, and no health surface. Calling the existing Oracle
self-consistency function from health would also append a ledger row, making a
read mutate its source of truth.

The implementation separates read-only consistency measurement from its
audited writer, measures readiness explicitly through
`oracle_readiness`, stores the report in `AnnealReport`, and makes health read
that one revisioned row without recomputation. The tiers are Oracle clean,
panel sufficient, kernel exists, calibrated, Goodhart defended, and mistakes
closed. Missing evidence is a failed tier with a named remediation.

Research used the live Exa MCP lane plus primary NIST AI RMF, W3C PROV-O, and
OpenTelemetry sources already recorded in the companion completion FSV. The
design follows NIST's objective, repeatable TEVV requirement and preserves
activity/evidence provenance without observational side effects.

## Isolated physical FSV

Source of truth:
`C:\Users\hotra\AppData\Local\Temp\synapse-oracle-readiness-fsv-final-1785872122969`.

Trigger: the real storage facade methods used by MCP, over 400 synthetic action
records with known balanced outcomes. Independent verification reopened the
vault read-only at snapshot 426.

```text
READINESS_ABSENT before_seq=10 after_seq=10 present=false
HAPPY_READINESS before_seq=421 after_seq=424 measured_at_seq=423 persisted_at_seq=424
oracle_clean passed value=1.0 threshold=0.7
panel_sufficient passed value=1.0 threshold=1.0
kernel_exists failed value=0.0 threshold=0.95
calibrated failed value=0.0 threshold=0.05
goodhart_defended failed value=0.0 threshold=0.9
mistake_closed failed value=1.0 threshold=0.0
READINESS_READBACK before_seq=424 after_seq=424 row_revision_sha256=331bafa537beb3cd3fbec63197c7e31f8ab51682c2b1e411cb8a8d8194ccb4f3
PHYSICAL_SOT snapshot=426 Base=402 Anchors=400 Recurrence=400 Assay=7 Ledger=413 AnnealReport=1
```

The initially measured full panel falsely failed sufficiency because Oracle
prediction admits only capability-card-measured lenses. The final run uses the
same calibrated panel contract for both prediction and readiness; the two
grounded tiers therefore pass and the first genuinely missing prerequisite,
the action-domain kernel, is named. Re-reading readiness did not advance the
sequence and reproduced the exact physical row revision.

Completion edge cases remained zero-mutation in the same run:

```text
EDGE_COMPLETE_EMPTY_FREE before_seq=424 after_seq=424 code=SYNAPSE_CALYX_ORACLE_COMPLETION_FREE_EMPTY
EDGE_COMPLETE_UNKNOWN_SLOT before_seq=424 after_seq=424 code=SYNAPSE_CALYX_ORACLE_COMPLETION_SLOT_UNKNOWN
EDGE_COMPLETE_INVALID_CX before_seq=424 after_seq=424 code=SYNAPSE_CALYX_CX_ID_INVALID
```

The installed-daemon MCP/health evidence is appended after deployment of the
repo-built release binary.
