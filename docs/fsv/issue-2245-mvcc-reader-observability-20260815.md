# Manual FSV: public Calyx MVCC reader observability (#2245)

Date: 2026-08-15 CDT

This is a manual Full State Verification record under `AGENTS.md` D1. No test,
harness, benchmark, helper driver, CI job, or direct HTTP caller was used.

## Runtime precondition

- Client: fresh Codex process PID `55716` (not the stale handoff PID `34764`).
- Trigger client: the production `mcp__synapse` Streamable HTTP client.
- Daemon: PID `64144`, `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, build
  `0ea2eff0825e`, SHA-256
  `48D6D58C3C64798895DCB9BA613CAB372BBBA6B4892043BE463BDA3EB1D42ED7`.
- Socket Source of Truth: `127.0.0.1:7700 LISTEN`, owner PID `64144`.
- Authenticated `health`: `ok=true`, 40 sanitized tools, facade contract `ok`,
  surface SHA-256
  `1db320303a8fe2ed88dec2e76a2b49b6a6afb15d154f7cb789e5b3438a63f506`.
- Required real tool: `storage`, with `snapshot_gc_status`, `snapshot_open`,
  `snapshot_read`, `snapshot_release`, and `anchors` present and callable.

## Source of Truth

The lease Source of Truth is Aster's process-local reader registry plus its
snapshot-GC floor. The row Source of Truth is the exact logical `CF_KV` row
resolved through the retained Aster snapshot, independently reread through the
typed `storage/anchors` owner path against the current physical row and
physical `Anchors` row.

The known row was:

- logical key:
  `mcp-usage/v1/call/1786827397016-64144-01a007365798782181cc58bade3f7279/00000000000000000018`
- payload length: `941`
- payload SHA-256:
  `bda0708d1afb8a5c09d83daba9d9abe3b750af679def8718a2fffbf1d1ca6baf`

## Happy path: before -> trigger -> independent after reads

1. Before read: real `storage/snapshot_gc_status` reported one unrelated live
   internal reader, `active_reader_leases=1`, `oldest_pinned_seq=4108517`, and
   `reader_lease_expired_total=1`.
2. Trigger: real `storage/snapshot_open(max_age_ms=60000)` opened public lease
   `16802` at sequence `4108573`; the public lease-table readback was exactly
   `active_lease_count=1`.
3. Independent registry read: `storage/snapshot_gc_status` reported total live
   readers `2`, proving the new public lease was present alongside the
   unrelated internal reader. Current sequence had advanced to `4108575`.
4. Trigger: real `storage/snapshot_read` read the exact `CF_KV` key through
   lease `16802`. It reported physical and logical presence, length `941`, and
   SHA-256 `bda0708d...d1ca6baf` at retained sequence `4108573` while current
   sequence was `4108576`.
5. Separate physical row read: real `storage/anchors` independently reread the
   current source row and its physical Anchors entry. It returned the same
   length `941` and SHA-256 `bda0708d...d1ca6baf`, plus one grounded
   `synapse:mcp_tool_call_outcome=ok` anchor for constellation
   `738388743990426ee81a9a0963b2d6c1`.
6. Trigger: `storage/snapshot_release(lease_id=16802)` reported
   `released=true` and public `active_lease_count=0` at current sequence
   `4108578`.
7. Independent registry read: `storage/snapshot_gc_status` returned to one
   unrelated internal reader. The public reader was absent; no public lease
   leaked.

## Edge audit

### Minimum lifetime and expiry

Before: expired total `1`. Trigger: open lease `16880` at the accepted lower
bound `100 ms`. Independent status after expiry reported expired total `2` and
no additional live reader. A subsequent `snapshot_read` failed loudly with
`SYNAPSE_CALYX_SNAPSHOT_LEASE_EXPIRED`, naming lease, expiry, read time, and
remediation.

### Below-minimum boundary

Before: total live readers `1`, expired total `2`. Trigger:
`snapshot_open(max_age_ms=99)`. It failed with
`SYNAPSE_CALYX_SNAPSHOT_LEASE_AGE_INVALID` and the exact `100..=60000` range.
Independent status remained at one live reader and expired total `2`; no lease
was admitted.

### Empty key

Before: public lease table empty. Trigger: opened lease `16924`, then called
`snapshot_read` with an empty key. It failed with `TOOL_PARAMS_INVALID` and the
exact logical-hex-key remediation. Independent status still showed the public
lease present (two total readers including the unrelated internal reader), so
the invalid read did not corrupt or remove it. Explicit release returned the
public lease table to zero.

### Structurally invalid payload

Before: one unrelated internal reader. Trigger: `snapshot_open` with unknown
field `unexpected`. Typed `deny_unknown_fields` validation rejected it as
`TOOL_PARAMS_INVALID`, listed the sole accepted field `max_age_ms`, and did not
call Aster. Independent status remained at one reader and expired total `2`.

## Verdict

Accepted. The public MVCC lifecycle is visible through a separate O(1)
physical registry read, exact historical row metadata agrees byte-for-byte
with a separate current owner read, valid leases disappear on release/expiry,
and invalid inputs fail closed without hidden state mutation.
