# Issue #2018: durable routine-autonomy policy ledger

Date: 2026-08-06 (America/Chicago)

## Scope and sources of truth

- Trigger: real installed-daemon MCP `routine operation=update action=arm`.
- Decision truth: the native Calyx `CF_LEDGER` append-only hash chain, read
  independently with `audit operation=verify_chain` and `read_seq`.
- Routine truth: `CF_ROUTINES`, `CF_ROUTINE_STATE`, and the optional CF_KV
  `armed_routine/v1/<routine_id>` row, read independently with
  `routine operation=inspect` before and after every trigger.
- Executable truth: installed image bytes, OS process table, and daemon build
  provenance.

## Diagnosis and research

`arm_routine` previously returned readiness decisions to the caller but wrote
no durable policy record. The action log consequently could not establish who
made the decision, which routine it governed, or whether autonomy was allowed
or refused.

The Exa research lane was probed live before research. Built-in research then
used primary sources. NIST SP 800-53 AU-3 defines the required content of audit
records; Microsoft's event-sourcing guidance requires an append-only event
store and idempotent consumers; OpenTelemetry's log data model separates event
identity, severity, attributes, and trace context. The fix therefore uses the
existing native append-only Calyx ledger, a typed Policy entry, a stable actor
and subject, and hashes free-form diagnostic text rather than weakening the
ledger's secret scanner.

- https://csrc.nist.gov/CSRC/media/Projects/risk-management/800-53%20Downloads/800-53r5/SP_800-53_v5_1-derived-OSCAL.pdf
- https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing
- https://opentelemetry.io/docs/specs/otel/logs/data-model/

## Installed reality

- Checkout and installed build: `6142ea98db9b`.
- Installed image: `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`.
- Image length: `192013929` bytes.
- Image SHA-256:
  `EE8694505EDE19C68D9EE85521032B3B7D9694DAFAE4D061105E5A6055472088`.
- Serving OS process: `synapse-mcp`, PID `20376`.
- Daemon provenance reported `build_matches_checkout=true` and a clean tree.

## Full-state verification

Before the trigger, a full physical chain walk reported:

```text
verdict=intact head_height=327552 verified=[0..327552)
tip_hash=9cdbfd7f8e34df20fcf839baa5dd874029f59d0f6bfd029399ed9aa9dbbe808c
```

Routine `rt1-47c106132ec1dcf4` independently read as `candidate`, mined with
confidence `0.3006418425824019`, and with no armed row.

The real arm trigger refused with `ROUTINE_AUTONOMY_NOT_READY` and
`failing_predicate=lifecycle_not_confirmed`. A separate post-trigger routine
inspection still read `candidate` and `armed_present=false`.

The post-trigger full chain walk reported:

```text
verdict=intact head_height=327556 verified=[0..327556)
tip_hash=5489ba24d232646b3b6f2861d703d6d019fca48355d523bb684ff867fbe52dcc
```

Reading each new sequence isolated the autonomy decision at sequence 327554:

```text
seq=327554
kind=policy
actor=Service("synapse-autonomy")
subject=query:7274312d34376331303631333265633164636634
payload_len=410
payload_sha256=771f7fb9c2bc4bb8e7849788a6e607990f8fbeb61c84d3ec7b32f2993d649514
prev_hash=e067cd3ff98cc1a8d8965260aa1f9f8375d4b20512190ca3a02fad5db22156e6
entry_hash=81249cfcd1b17879bddd50c06917fbb0ca10a4d8b8208ad4a6f8c58b55303850
self_verifies=true
```

The subject hex decodes to the exact routine id
`rt1-47c106132ec1dcf4`. Sequences 327552, 327553, and 327555 were independently
read and were unrelated `synapse-mcp-usage` grounding entries. This proves the
policy record is the decision caused by the trigger rather than coincident
background traffic.

## Boundary audit

Each boundary was run against the installed daemon. State was read before and
after the action; every read remained `candidate` with `armed_present=false`.

| Case | Result | Before | After |
| --- | --- | --- | --- |
| empty routine id | `TOOL_PARAMS_INVALID`; exact `rt1-` contract | candidate, armed absent | candidate, armed absent |
| both triggers false | `TOOL_PARAMS_INVALID`; trigger required | candidate, armed absent | candidate, armed absent |
| failure threshold 21 | `TOOL_PARAMS_INVALID`; range is 1..20 | candidate, armed absent | candidate, armed absent |

## Fail-closed observations

The first deployed implementation attempted to include free-form error text in
the policy payload. The real ledger rejected it with
`SYNAPSE_CALYX_LEDGER_GROUP_COMMIT_FAILED` caused by
`CALYX_LEDGER_SECRET_IN_PAYLOAD`; no armed row was written. The root fix keeps
the precise error in the caller response but commits SHA-256 bindings for the
error code and detail. It does not bypass or loosen redaction.

The first setup command outlived its command wrapper. A second setup invocation
failed with `SYNAPSE_SETUP_MAINTENANCE_LOCK_HELD` naming exact owner PID 5776.
The owner was waited to completion; no lock was deleted and no broad process
cleanup was used.

## Gates

`scripts/lint.ps1` passed all seven gates in both workspaces, including format,
dependency policy, root and Calyx Clippy, and the public-API ratchet. Deployment
used `scripts/synapse-setup.ps1`; the daemon was never run from `target/release`.

