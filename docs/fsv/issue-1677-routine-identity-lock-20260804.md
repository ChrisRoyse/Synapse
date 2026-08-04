# Issue #1677: confirmed-routine identity lock FSV (2026-08-04)

## Scope and root cause

Confirmed routines retained only a lifecycle enum. `CF_ROUTINES` is replaceable
derived state, so a later mining pass could change or remove the canonical
routine while `CF_ROUTINE_STATE` continued to say `confirmed`. Suggestion and
execution paths trusted that stale enum. There was no durable digest binding the
operator's confirmation to the exact identity they reviewed.

The fix stores a SHA-256 digest of a canonical, versioned projection of the
routine identity (id, granularity, ordered steps, day class, mean minute, and
tolerance). Every complete mining reconciliation checks confirmed rows. A
missing or changed identity moves the durable state to `quarantined`, records an
`identity_quarantine` transition, and excludes the routine from matching,
suggestion, and execution until an operator confirms an extant identity again.

Research was performed after diagnosis. The Exa MCP lane was verified `live`
with `scripts/check-research-lane.ps1` (exa-search-server 3.4.0, real
`tools/call` returned content). Built-in research used NIST AI RMF guidance on
monitoring, human intervention, and deactivation, and OWASP logging guidance on
security-event integrity:

- https://airc.nist.gov/airmf-resources/playbook/
- https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html

## Source of truth

- Canonical identity: exact `CF_ROUTINES` JSON row.
- Operator decision and lock: exact `CF_ROUTINE_STATE` JSON row.
- Durability boundary: the Calyx vault at
  `C:\Users\hotra\AppData\Local\Temp\synapse-routine-fsv-1677-1785849340208`
  plus its sibling lineage journal.
- Trigger: real MCP stdio handshake and `tools/call` using
  `scripts/manual_mcp_stdio_probe.py` and the built `synapse-mcp.exe`.
- Readback: `routine_inspect` in independently reopened daemon processes; each
  response is decoded from the physical CF rows. Vault shutdown reported a
  durable high-water mark of 63 and successful lock re-acquisition.

## Happy path

The bounded public diagnostic writer wrote six valid typed timeline records to
`CF_TIMELINE`. Its physical readback reported `before_rows=0`, `after_rows=6`,
`rows_added=6`, and `after_cf_size_bytes=1008`. Segmentation and mining produced
two routines from three support days. `routine_update action=confirm` then
persisted record version 3 with:

```text
routine_id = rt1-9cbf202d68dda96f
lifecycle = confirmed
canonical_sha256 = 51243c965af3faada27daad9a42da34a830b17ea268fbffa4a5af77a0441335e
last_observed_sha256 = 51243c965af3faada27daad9a42da34a830b17ea268fbffa4a5af77a0441335e
confirmed_by = stdio
present_in_last_mine = true
```

An independent reopen read the same values from `CF_ROUTINE_STATE`.

## Adverse transition

Before: the independent read above showed the routine mined, confirmed, and
digest-matched. Trigger: a complete mine over an empty future window replaced
the derived routine set. The mine readback reported `routines_deleted=2`,
`routines_written=0`, `state_rows_marked_unmined=2`, and
`state_rows_identity_quarantined=1`.

After: physical state readback showed `mined=false`,
`present_in_last_mine=false`, `lifecycle=quarantined`, the original identity
lock retained, and this appended transition:

```text
action = identity_quarantine
from = confirmed
to = quarantined
by = miner
note = confirmed routine disappeared from the complete mining result; canonical identity is no longer present
```

## Boundary and edge cases

Each case had a `routine_inspect` before and after the trigger. The final row
remained quarantined with the same digest, timestamps, and three transitions.

1. Arm an unmined quarantined routine: refused with `ROUTINE_NOT_MINED`; no
   armed state or lifecycle mutation appeared.
2. Confirm malformed id `bad-id`: refused with `TOOL_PARAMS_INVALID` and the
   exact required `rt1-` plus 16 lowercase-hex format; the target row was
   unchanged.
3. Confirm a quarantined id whose canonical `CF_ROUTINES` row is absent:
   refused with `ROUTINE_IDENTITY_SOURCE_ABSENT`; the state remained
   quarantined and the lock was not rewritten.

## Build gates

- `cargo check -p synapse-mcp`: passed.
- `cargo build -p synapse-mcp --bin synapse-mcp`: passed.
- `pwsh -File scripts/lint.ps1`: all seven gates passed in both workspaces,
  including format, dependency policy, and all-target Clippy.
