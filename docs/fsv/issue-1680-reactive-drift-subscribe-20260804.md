# Issue #1680 Reactive drift-to-subscribe FSV (2026-08-04)

## Source of Truth

- Vault: `C:\Users\hotra\AppData\Local\Temp\synapse-reactive-fsv-1785853042442\vault`
- Repo-built daemon: PID 12876 on `127.0.0.1:7793`
- Active MCP subscription: `019fcd28-a7ea-7202-a28b-24745f66585e`
- Captured tool/SSE evidence: sibling `drift-result-final.json` and
  `events-final.sse`
- Physical readback: a new read-only vault handle scanning `ColumnFamily::Reactive`

## Diagnosis and research

The existing producer wrote every lens measurement, including
`significant=false`, under a `panel+slot` key. Thus the Reactive CF was a
latest-value cache, not an audited trigger history. No reader or subscriber
publisher existed. The hygiene facade also opened Reflex before dispatch, so a
Calyx-only drift request failed when Reflex was disabled (#2001).

After diagnosis, `check-research-lane.ps1` made a real Exa MCP query and
reported server 3.4.0 live (8,177 returned characters). Built-in research used
the Microsoft and AWS transactional-outbox guidance: commit the event before
publication, preserve ordering, and use stable identities/idempotent consumers
because relay can be at least once.

## Known-answer trigger

The fixture wrote 100 real one-dimensional constellations to panel 1680001.
The first 50 center on 0.02 and the recent 50 center on 10.02. A real MCP
session validated the 40-tool schema, acquired the foreground lease, persisted
`full_capability`, subscribed to `calyx.reactive.drift`, and invoked:

```json
{"operation":"drift","drift":{"panel_version":1680001,"max_records":100,"recent_fraction":0.5,"permutations":200}}
```

Observed:

```text
records reference/recent       50 / 50
mmd2                           0.7905847464861928
p_value                        0.004975124378109453
significant                    true
Reactive rows persisted        1
notifications matched/queued   1 / 1
notifications dropped          0
observed vault sequence        174
```

The live SSE stream delivered one non-lossy `calyx.reactive.drift` event.
After daemon shutdown, a new read-only handle found exactly one physical row at
snapshot 176:

```text
key_hex=524452494654310000000000000000ae0019a2810001
value={"bandwidth":9.970000229775906,"dimension":1,"mmd2":0.7905847464861928,"observed_seq":174,"p_value":0.004975124378109453,"panel_version":1680001,"recent_n":50,"reference_n":50,"significant":true,"slot":1}
```

A separate comparison checked all ten evidence fields across the tool response,
SSE payload, and physical value: zero mismatches; row/event counts were 1/1/1.

## Boundary audit

| Case | Before | Trigger | After |
| --- | --- | --- | --- |
| Invalid permission config | no listener, Reactive rows 0 | `SYNAPSE_MCP_ALLOWED_PERMISSIONS=full_capability` | startup exited with unknown concrete permission; no listener/row |
| Reflex disabled | Reactive rows 0 | rebuilt daemon, `hygiene drift` under normal profile | reached `TOOL_PROFILE_POLICY_DENIED`, proving no Reflex dependency; rows stayed 0 |
| Invalid SSE authorization | active subscription, rows 0 | malformed curl headers | `HTTP_TOKEN_INVALID`; no event/row |
| Significant drift | active authorized subscription, rows 0 | real MCP drift call | one committed row and one identical non-lossy event |

Only significant measurements become Reactive triggers. Their keys include the
observed sequence, panel, and slot, so later runs append audit identities rather
than overwrite the prior event. Dropped subscriber notifications produce a
structured failure naming the durable Reactive rows as the recovery source.

## Gates

`pwsh -File scripts/lint.ps1` passed all seven gates in both workspaces.

This proves the drift slice. New-region, recurrence, and unattended post-ingest
production remain required before #1680 can close.
