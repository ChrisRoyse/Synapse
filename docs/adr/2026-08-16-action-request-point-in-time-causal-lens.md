# Point-in-time action request causal lens

- **Status:** accepted
- **Date:** 2026-08-16
- **Issues:** #1690, #1689
- **Supersedes:** action panel generation `2_185_001`

## Context

The action panel could distinguish action kind, target, coarse record shape, and
time, but not the request that existed before execution. Two actions with the
same tool/verb/target therefore measured identically even when their payloads
were materially different. Adding terminal status, error, response, or
after-state would make the panel look predictive by encoding the outcome rather
than its cause.

The audit corpus has two physical shapes:

- command audit rows retain the same bounded redacted payload and digest on
  intent and final rows; and
- action audit rows historically stored requests in preflight/started details,
  while terminal details varied by outcome and often contained responses or
  errors.

The old action target and record-vector paths also crossed this boundary:
target lookup fell through to terminal `details.request`, and the record vector
counted terminal `details` fields. Those fields existed asymmetrically across
success and failure and were post-treatment leakage, not causal predictors.

Primary guidance agrees on the time boundary. Google BigQuery and Azure ML
define point-in-time correctness as using only feature values available at the
prediction cutoff to prevent train/serve leakage. Montgomery, Nyhan, and Torres
show why conditioning on post-treatment variables biases causal inference.

## Decision

1. Action panel generation `2_185_002` adds dense slot 118,
   `syn.action.request_vector.v1`.
2. Command rows measure `payload_bounded`, the SHA-256 of the complete redacted
   payload, payload byte/truncation facts, and pre-action tool/verb/channel/target
   envelope. The digest preserves exact identity when the stored structural
   view is bounded.
3. Action-audit writers persist `request_snapshot`, its byte length, and its
   canonical `sha256:<hex>` digest only for preflight/started rows and persist
   null for all three on every terminal outcome. Historical rows without this
   status-independent field are explicitly absent. Terminal
   `details`, `details.request`, response, status, error, and after-state are
   never request features.
4. Structural request traversal is deterministic, key-sorted, depth/node
   bounded, and hashes feature values into names so raw request content is not
   copied into the vector contract. Overflow is accepted only when the writer
   sealed the complete redacted payload with a digest; otherwise measurement
   fails closed. Request byte length is log-normalized against the frozen 1 MiB
   authenticated MCP request ceiling; a larger stored claim fails instead of
   dominating the unit-field vector or being silently clamped.
5. Target lookup removes terminal details fallthrough. Target hash/vector and
   the record vector become frozen v2 instruments. The record vector no longer
   counts terminal detail fields.
6. The action reward's determining fields (`outcome`, `status`) are declared in
   the structural provenance table. No active lens may read either field; a
   future carrier becomes an explicit sufficiency refusal.
7. The immutable panel change invalidates inherited Ward and held-out evidence.
   Backfill, calibration, sufficiency, kernel recall, and held-out validation
   must be re-measured from physical generation `2_185_002`.
8. Public `act` command-audit intent and final rows carry the same immutable
   target snapshot for every target-consuming verb. The target is captured
   before the durable intent, re-read after the per-session authority gate, and
   must still match before dispatch. Finalization and its retries reuse the
   captured value; they never reconstruct it from mutable post-action session
   state. `run_shell`, lease, and operator-panic operations remain explicitly
   target-independent even when the session happens to have a bound window.

## Consequences

- New command outcomes can support request-to-outcome measurement without
  look-ahead. Action preflight candidates can be guarded on their actual
  request, while legacy terminal rows cannot manufacture predictive signal from
  asymmetric error details.
- Historical rows may have an absent request slot. Absence is retained as
  absence; it is never a zero vector or a reconstructed request.
- Target absence follows the same typed rule. A successful target-independent
  action with no session target and no explicit real-foreground lease keeps
  target slots `Absent`; it is not warned toward an unrelated target.
  `CALYX_ACTION_TARGET_ABSENT_ON_SUCCESS` is reserved for a physical
  contradiction where the row claims a target-bearing foreground state but
  every exact target field is absent. Unknown foreground status vocabulary
  fails measurement and requires an explicitly versioned contract.
- Target-bearing `act` outcomes retain the actual operation target after the
  caller clears or changes its session binding. A target change while the
  command is queued refuses before dispatch and records both immutable intent
  and admitted snapshots instead of attributing the outcome to whichever
  window exists later.
- Changed frozen instruments have new lens names even though their slot ids stay
  stable inside the new panel generation.
- Readiness can remain false after deployment. That is the correct result until
  the new physical corpus carries enough grounded, diverse evidence.

## References

- Google Cloud, “Feature serving — Point-in-time correctness”: <https://docs.cloud.google.com/bigquery/docs/feature-serving>
- Microsoft Azure ML, “Point-in-time join concepts”: <https://learn.microsoft.com/en-us/azure/machine-learning/offline-retrieval-point-in-time-join-concepts?view=azureml-api-2>
- Montgomery, Nyhan, and Torres, “How Conditioning on Posttreatment Variables Can Ruin Your Experiment and What to Do about It,” *American Journal of Political Science* (2018): <https://doi.org/10.1111/ajps.12357>
- OpenTelemetry, “Logs Data Model”: <https://opentelemetry.io/docs/specs/otel/logs/data-model/>
- OpenTelemetry, “Context”: <https://opentelemetry.io/docs/specs/otel/context/>
- OWASP, “Logging Cheat Sheet”: <https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html>
