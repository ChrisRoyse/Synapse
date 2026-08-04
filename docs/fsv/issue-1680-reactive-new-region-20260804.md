# Issue #1680 Reactive new-region FSV (2026-08-04)

## Source of Truth

- Vault: `C:\Users\hotra\AppData\Local\Temp\synapse-novelty-fsv-1680-1785856000000\declared`
- Repo-built MCP executable: `target\debug\synapse-mcp.exe`
- Corpus: 550 physical panel `1963001` constellations (400 adjudicated good,
  150 adjudicated bad), guarded dense slot 4
- Trigger: real MCP `hygiene guard_verify`
- Durable readback: a separate read-only vault handle scanning
  `ColumnFamily::Reactive`
- Provenance binding: Ward `Ledger` sequence and hash copied into the typed
  Reactive row only after the verdict append succeeds

## Diagnosis and research

Ward already supported `NewRegion`, `Quarantine`, and `RejectClosed`, but the
Synapse calibration adapter always persisted `RejectClosed`. Therefore no real
request could produce the new-region event required by #1680. Guard verification
also declared only `READ_STORAGE` even though it already appended every verdict
to the physical Ledger.

After diagnosis, `scripts/check-research-lane.ps1` performed a real Exa MCP
query and reported `live`. Built-in research used Microsoft's reliable web-app
guidance and AWS's transactional-outbox guidance: commit event state in the
system of record before publication, preserve a stable event identity, and make
delivery failure explicit because durable relays can be at least once.

The fix exposes the three Ward dispositions at calibration while retaining
`reject_closed` as the default. Only actual `new_region` or `quarantine`
verdicts create a typed Reactive outbox row. The vault writer commits, flushes,
and independently point-reads that exact row before MCP can publish it. A
dropped subscriber notification fails the request and names the durable row as
the recovery source.

Research sources:

- https://learn.microsoft.com/azure/architecture/patterns/transactional-outbox
- https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html

## Known-answer trigger

Before the trigger, an independent read-only scan reported:

```text
REACTIVE_READBACK snapshot=570 rows=0
```

Calibration used `novelty_action=new_region`. The physical corpus produced a
calibrated `tau=-0.707106769`, FAR `0`, FRR `0`, with 400 good and 150 bad
scores. Known-bad record `19190190000000000000000000000000` scored
`-0.707106829`, failed slot 4, and returned `action=new_region`.

The MCP response and separately read physical row agreed on all fields:

```text
ledger_seq=567
ledger_hash=5101aacf8949a9e54c99e9b5ef4c2de25d93cb751429c549f96d6bd5ff6b52cd
guard_id=0f974d99-a376-43cc-b59d-3a7a254084fa
action=new_region
failing_slots=[4]
```

Independent physical readback:

```text
REACTIVE_READBACK snapshot=583 rows=1
key_hex=524e4f56454c3100000000000000023719190190000000000000000000000000
value={"action":"new_region","failing_slots":[4],"guard_id":"0f974d99-a376-43cc-b59d-3a7a254084fa","ledger_hash":"5101aacf8949a9e54c99e9b5ef4c2de25d93cb751429c549f96d6bd5ff6b52cd","ledger_seq":567,"panel_version":1963001,"query_cx_id":"19190190000000000000000000000000"}
```

## Boundary audit

| Case | Before | Trigger | After |
| --- | --- | --- | --- |
| Known-good record | Reactive rows 1 | verify good record `191900...`; cosine 1.0 | pass, action null, persisted novelty null; rows 1 |
| Invalid identifier | Reactive rows 1 | verify `not-a-cx` | structured parse failure before verdict/outbox; rows 1 |
| Default rejection | Reactive rows 1 | recalibrate with omitted action, verify known-bad record | action `reject_closed`, persisted novelty null; rows 1 |

The final independent scan was `snapshot=591 rows=1` and returned the same key
and exact JSON. The negative cases advanced other audited vault state where
appropriate, but did not manufacture Reactive trigger rows.

## Gates

- `cargo check --workspace` passed.
- `cargo build -p synapse-mcp` passed and produced the executable used above.
- `pwsh -File scripts/lint.ps1` passed all seven gates in both workspaces.

This proves the calibrated new-region producer and durable outbox slice.
Unattended post-ingest production and durable relay/replay remain before #1680
can close.
