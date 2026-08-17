# Action causal units and immutable preconditions

Date: 2026-08-17

Status: accepted

Issue: #1690

## Physical finding

The production `syn-action-v1` generation `2_185_004` had 397 grounded
`reward` anchors, but its exact and semantic request lenses each had only 244
paired measurements. Reading `CF_ACTION_LOG` established why: one MCP action
produces a command-audit final row and a separate action-audit terminal row.
Both were labelled `reward`, although the latter intentionally omits the
pre-trigger request. The readiness assay was therefore fitting one label to two
different observational units. It was honestly insufficient at 0.85231769 bits
against 0.98806268 bits of outcome entropy.

This is an event-model defect, not an estimator or threshold defect. Lowering
the honesty gate would conceal it.

## Decision

Publish immutable action generation `2_185_005`.

- Command-audit final rows retain `AnchorKind::Reward`; this is the readiness
  population whose authenticated request and `before` state are present.
- Action-audit terminal rows retain their grounded outcome under the distinct
  `AnchorKind::Label("action_execution_reward")` axis. No observation is
  discarded or counted twice on one axis.
- Add slot 122, `syn.action.precondition_atoms.v1`, a bounded 512-dimensional
  projection of the immutable command `before` object. It reads no `after`,
  `outcome`, `status`, or error field.
- Shell command writers add a value-free precondition snapshot to `before`,
  derived from the same child-environment construction and durable-host
  diagnostics execution uses. Only counts, missing required variable names,
  diagnostic codes/severity, and source identities are persisted; environment
  values and credentials are not.
- Superseded anchors are not carried. The TTL-managed authoritative action log
  is re-measured, which re-adjudicates each row onto its correct current axis.

## Why

Causal inference requires the treatment/covariates and outcome to refer to the
same unit and to precede the outcome. Hernán and Robins' causal-inference text
formalizes the need to define the observational unit, treatment strategies,
and temporally prior covariates before estimating an effect. OpenTelemetry's
trace model likewise treats a span as one operation with causally related
events, rather than treating every event as an independent copy of the span's
outcome.

Primary references:

- https://www.hsph.harvard.edu/miguel-hernan/causal-inference-book/
- https://opentelemetry.io/docs/concepts/signals/traces/

## Consequences

All Ward, validation, readiness, kernel, and oracle artifacts are re-armed for
panel `2_185_005`. The new reward assay is computed only over command-final
records with their causal inputs. Physical execution reliability remains
queryable and assayable through `action_execution_reward`; it is not allowed to
dilute or inflate command readiness. Host precondition failures such as missing
delivered Windows environment variables become independently measurable causes
instead of existing only in terminal error text.
