# Issue #1677: Ward quarantine relay and escalation FSV (2026-08-04)

## Diagnosis and research

Ward already calibrated and verified per slot, appended its verdict to the
hash-chained Ledger, persisted novelty to Reactive, guarded find, and identity
locked confirmed routines. The remaining boundary was incomplete:

- the Ledger verdict and Reactive row were separate commits, leaving a crash
  gap;
- the facade published directly with no durable delivery cursor;
- quarantine never entered the escalation source of truth;
- typed Reactive readers spent their row budget on unrelated prefixes, allowing
  permanent starvation (#2004).

After diagnosis, `scripts/check-research-lane.ps1` reported Exa MCP live and a
targeted transactional-outbox/security-alert query completed (3,757 response
characters). Built-in research used primary sources:

- AWS transactional outbox guidance:
  https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html
- Microsoft transactional outbox guidance:
  https://learn.microsoft.com/azure/architecture/databases/guide/transactional-out-box-cosmos
- NIST SP 800-171r3 audit-failure and incident-handling controls:
  https://nvlpubs.nist.gov/nistpubs/SpecialPublications/800-171r3/NIST.SP.800-171r3.html

Applied contract: verdict plus outbox commit atomically; relay in ledger order
from a durable cursor; tolerate at-least-once replay through a deterministic
open-escalation identity; hold the cursor on dropped notification, missing
subscriber, or escalation failure.

## Source of truth

- Vault:
  `C:\Users\hotra\AppData\Local\Temp\synapse-ward-quarantine-fsv-1785862835408\declared`
- Trigger: repo-built `target/debug/synapse-mcp.exe` over a real MCP stdio
  initialize, schema-bearing `tools/list`, and `tools/call` sequence.
- Corpus: 550 physical timeline constellations, 400 known-good and 150
  known-bad, slot 4; calibrated `tau=-0.707106769`, FAR 0, FRR 0.
- Independent readback: `ward_quarantine_readback_fsv` reopened the vault and
  read typed Reactive/Registry plus raw logical `CF_KV` prefixes.

The first launch attempt failed before initialize because the inherited
`SYNAPSE_CALYX_VAULT=1` was not the strict boolean `true`. The second named the
real production shell-job lock owner and refused startup. The successful run
used `true` and an isolated temporary shell-job root; neither failure invoked a
guard tool or changed the Ward fixture.

## Happy path

MCP calibrated the profile with `novelty_action=quarantine`, registered a real
`calyx.reactive.quarantine` subscription, and verified known-bad CxId
`19190190000000000000000000000000`.

The response reported `overall_pass=false`, `failing_slots=[4]`,
`notifications_matched=1`, `notifications_queued=1`, `dropped=0`. Independent
readback after process exit found:

```text
NOVELTY cursor=557 rows=1
ledger_seq=557
ledger_hash=9c79f8643ee918a5340b92f06b53a1d2c0f62bdd3ae7cb052dd6425bda01c618
action=quarantine
```

Raw `CF_KV` contained exactly one each of:

```text
escalation/v1/item/    severity=critical status=pending attention_state=quarantine
escalation/v1/audit/   event=opened source=ward_guard_quarantine ledger_seq=557
escalation/v1/open/    is_open=true, same escalation id
approval/v1/item/      kind=agent_escalation status=pending
approval/v1/audit/     event=requested, same approval id
```

Every row carried the same guard id, query CxId, slot 4, ledger sequence and
hash. A separate daemon reopen returned the same one pending escalation.

## Boundary audit

Before the audit: Reactive rows 1, cursor 557, escalation/item/audit/open and
approval/item/audit each 1.

1. Known-good CxId: passed at cosine 1.0; action null; notification counts zero;
   no Reactive or escalation row added.
2. Invalid CxId `not-a-cx`: refused with
   `SYNAPSE_CALYX_CX_ID_INVALID`; no verdict/outbox/escalation row added.
3. Repeat known-bad CxId: appended a second real security decision at Ledger
   sequence 569 and queued exactly one notification. Registry cursor advanced
   to 569. The open-index CAS coalesced it into the existing escalation, so all
   five escalation/approval prefix counts remained exactly one.

Final physical state:

```text
NOVELTY cursor=569 rows=2 (ledger sequences 557, 569)
escalation item/audit/open rows = 1/1/1
approval item/audit rows = 1/1
```

## Prefix-starvation regression (#2004)

Fresh vault:
`C:\Users\hotra\AppData\Local\Temp\synapse-reactive-prefix-fsv-1785863610762`.
The instrument persisted 300 lexicographically earlier unrelated novelty rows,
then one region row, and requested only one region row:

```text
PREFIX_RANGE unrelated=300 total_reactive_rows=301 requested=1 returned=1 exact_match=true
```

The relay budget now applies inside the native prefix range, not to the whole
Reactive family.

## Gates

- `cargo check -p synapse-mcp`: passed.
- `cargo build -p synapse-mcp --bin synapse-mcp`: passed; that exact executable
  served the MCP FSV.
- `pwsh -File scripts/lint.ps1 -Fix`: all seven gates passed in both workspaces,
  including dependency policy, all-target Clippy, and the Calyx public API
  ratchet at its unchanged baseline of 370.
