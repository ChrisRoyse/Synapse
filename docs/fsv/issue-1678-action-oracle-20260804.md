# Issue #1678: grounded action Oracle FSV (2026-08-04)

## Scope and source of truth

The trigger is a terminal action audit publication followed by native Calyx
Oracle prediction or reverse query. The source of truth is the isolated Calyx
vault's physical `Base`, `Anchors`, `Recurrence`, `Assay`, and `Ledger` column
families, plus `CF_ACTION_LOG`. A returned prediction is not acceptance
evidence.

Final vault:
`C:\Users\hotra\AppData\Local\Temp\synapse-action-oracle-final-1785869571613`.

## Diagnosis

1. `syn.action.kind_onehot.v1` included terminal `status`, leaking the outcome
   into the feature used to establish sufficiency.
2. The action panel had no graded dense lens, so panel admission correctly
   refused neighbourhood analysis.
3. Synapse persisted point estimates under `synapse-intelligence`/Label while
   Oracle consumes calibrated `(synapse.action, Reward)` Assay evidence.
4. The capability card persisted only its summary, leaving its calibrated
   panel, entropy, and per-lens measurements unavailable to Oracle.
5. Anchored action rows lacked Oracle domain/action metadata, and recurrence
   context lacked a grounded consequence edge, so reverse traversal had no
   grounded cause evidence.

## Research

`scripts/check-research-lane.ps1` reported `exa_mcp live`; its real
`tools/call web_search_exa` returned content. Built-in web research used primary
sources:

- W3C PROV-O models provenance as associations among entities, activities, and
  agents, supporting co-located action, outcome, and derivation evidence:
  https://www.w3.org/TR/prov-o/
- OpenTelemetry event semantics require distinct outcomes to carry their own
  timestamp and structured attributes:
  https://opentelemetry.io/docs/specs/semconv/general/events/
- NIST TEVV guidance treats reliable measurement and evaluation as necessary
  for trustworthy behavior:
  https://www.nist.gov/ai-test-evaluation-validation-and-verification-tevv

## Implementation

- Issued frozen action panel generation `2006001` and
  `syn.action.kind_onehot.v2`, sourced only from `row_kind`, `tool`, and `verb`.
- Added a deterministic graded action record-vector lens.
- Published terminal outcomes as grounded Reward anchors and recurrence
  evidence with explicit ground truth and grounded consequence edges.
- Persisted the calibrated ensemble panel estimate, outcome entropy, and lens
  estimates under the exact Oracle domain key.
- Added typed `oracle_predict` and `oracle_reverse` storage-intelligence facade
  operations, strict panel/input validation, and structured errors.
- Raised the validated recurrence context ceiling from 256 to 512 bytes so the
  complete hash-committed evidence fits; 513 bytes remains a hard refusal.

## Manual execution and readback

The fixture publishes 200 known failures and 200 known successes. Outcome
classes occupy separated time regions while retaining within-class variation,
so the calibrated panel has known signal without encoding status as a feature.

```text
BEFORE seq=10 action_rows=0
INGEST_READBACK seq=412 action_rows=400 expected=400
HAPPY_PREDICT ... I_panel_oracle=1.0 anchor_entropy_bits=1.0
  outcome=true confidence=0.75 consequence=terminal_outcome
HAPPY_REVERSE ... action_or_event=fsv_fails confidence=0.9950494766
  provisional=false support=200
PHYSICAL_SOT snapshot=419 Base=402 Anchors=400 Recurrence=400
  Assay=7 Ledger=409 panel=2006001
```

The final counts came from a separately opened read-only vault handle after the
writer was dropped.

## Boundary audit

```text
empty action: before_seq=10 after_seq=10 code=STORAGE_WRITE_FAILED
empty corpus: before_seq=10 after_seq=10
  code=SYNAPSE_CALYX_ENSEMBLE_NO_ANCHORED_RECORDS
unknown action after grounding: before_seq=417 after_seq=419
  code=CALYX_ORACLE_NO_RECURRENCE
```

The unknown-action query persists a fresh calibrated Assay measurement before
the recurrence refusal, hence the two-sequence increase; it does not fabricate
an answer. The empty and malformed cases leave the vault unchanged.

## Gates

`pwsh -File scripts/lint.ps1 -Fix` passed all seven gates in both workspaces,
including format, dependency policy, all-target Clippy, and the public-API
ratchet.
