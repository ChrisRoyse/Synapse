# Issue #1690: readiness-gated routine arming (partial FSV)

Date: 2026-08-06 (America/Chicago)

## Scope and sources of truth

This run verifies only the new fail-closed arming eligibility boundary. It does
not claim #1690 complete.

- Trigger: real HTTP MCP `routine operation=update action=arm`.
- Routine truth: `CF_ROUTINES`, `CF_ROUTINE_STATE`, and the optional CF_KV
  `armed_routine/v1/<routine_id>` row, independently read with
  `routine operation=inspect` before and after the trigger.
- Executable truth: installed image SHA-256 plus the OS listener PID.
- Audit truth: independent `audit operation=command_query` scan of
  `CF_ACTION_LOG`.
- Raw captured evidence:
  `%TEMP%\synapse-1690-unready-refusal.json`,
  `%TEMP%\synapse-1690-edge-cases.json`, and
  `%TEMP%\synapse-1690-action-audit.json`.

## Diagnosis and research

`arm_routine` previously checked only that a mined routine and installed
automation existed. It did not require a confirmed lifecycle, identity lock,
persisted six-tier Oracle readiness, or grounded recurrence evidence.

The Exa MCP lane was probed before research and was `live` (server 3.4.0; a
real `web_search_exa` call succeeded). Built-in web research used primary
sources. NIST AI RMF Measure 2.6 requires deployed AI to fail safely beyond its
knowledge limits, and AWS Step Functions documents explicit conditional states,
error transitions, and execution/idempotency guarantees. The implementation
therefore uses explicit, named predicates and refuses rather than defaulting to
an executable path.

- https://airc.nist.gov/airmf-resources/airmf/5-sec-core/
- https://docs.aws.amazon.com/step-functions/latest/dg/state-choice.html
- https://docs.aws.amazon.com/step-functions/latest/dg/choosing-workflow-type.html

## Installed reality

- Commit containing the gate: `9abada7a`.
- Predicate-order correction: `566b4ac9`.
- Installed image SHA-256:
  `ADC70191318514D4B36DE6CEF7A66FC4940FB8D1B9434EF84132D6ABF03E42E4`.
- Listener: PID `23264`, `127.0.0.1:7700`.
- Tool-surface SHA-256:
  `ef5577b42935ecd20ecb0400511b471c2dc310bcfe355b0ca581d7ee43d941f8`.

## Happy behavior for an unready domain

Routine `rt1-47c106132ec1dcf4` was read before the trigger:

- lifecycle: `candidate`
- armed row: absent
- mined confidence: `0.3006418425824019`
- support: 3 days / 5 occurrences

The real arming trigger returned:

```text
ROUTINE_AUTONOMY_NOT_READY
failing_predicate=lifecycle_not_confirmed
remediation: confirm the currently mined routine before arming
```

A separate post-trigger inspection read lifecycle `candidate` and armed row
absent. The failed transition therefore produced no autonomous state.

## Boundary audit

Each case printed the same routine state before and after the action.

| Case | Error | Before | After |
| --- | --- | --- | --- |
| malformed id `not-a-routine` | `TOOL_PARAMS_INVALID`, exact id contract | candidate, armed absent | candidate, armed absent |
| both triggers false | `TOOL_PARAMS_INVALID`, at least one trigger required | candidate, armed absent | candidate, armed absent |
| failure threshold 21 | `TOOL_PARAMS_INVALID`, allowed range 1..20 | candidate, armed absent | candidate, armed absent |

## Supporting gates

- `cargo check --workspace`: passed.
- Required lint entry point: both workspace Clippy passes completed; the first
  run found only formatting. `scripts/lint.ps1 -Fix -SkipClippy` then proved
  format, lock graph, deny, shared config, and public-API ratchet clean in both
  workspaces.
- Deployment used `scripts/synapse-setup.ps1`, not the release output directly.

## Remaining blockers

The positive ready-domain path cannot be claimed:

1. `measure_action_readiness` hardcodes `goodhart_defended` and
   `mistake_closed` false, so readiness can never pass. Filed as #2017.
2. The independent CF_ACTION_LOG query scanned 311 physical rows and returned
   no routine arming decision. Durable decision-ledger integration is absent.
   Filed as #2018.

Until both are fixed and a genuinely ready high-regularity routine passes, #1690
must remain open.
