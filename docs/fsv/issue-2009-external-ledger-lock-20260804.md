# Issue #2009 Full State Verification - 2026-08-04

## Diagnosis and research

A real native Anneal proposal on a new durable vault stopped after the external Ledger row was
committed. Structured tracing localized the stop to `append_external_ledger_row`: it owned
`durable.commit.lock`, then reconciliation called writable recovery. Writable recovery entered
stale-SST reclamation and attempted to acquire the same byte-range lock through a second file
handle. CPU use was idle and no completion event or subsequent durable row appeared.

The Exa lane was verified live with `scripts/check-research-lane.ps1`, including its real MCP
query. Independent research then confirmed the diagnosed contract: Rust documents that locking a
file which is already locked by the same thread may deadlock, and Microsoft documents that a
second handle cannot access the locked byte range until it is unlocked:

- <https://doc.rust-lang.org/stable/std/fs/struct.File.html#method.lock>
- <https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-lockfileex>

The repair makes the ownership contract explicit in
`recover_current_batches_under_commit_lock` and forces that reconciliation read to be read-only.
It can recover durable truth, but cannot enter repair or reclamation paths which acquire the
already-held commit lock. No timeout, retry, fallback, or skipped ledger reconciliation was added.

## Source of truth and execution

The source of truth was a real temporary Calyx vault: physical `Kv`, `AnnealRollback`, and
`Ledger` column-family rows plus its manifest durable sequence. Before the repair, the trace ended
at a nested `CALYX_ASTER_RECOVERY_START` with writable recovery and the process remained blocked.
After the repair, each same-position recovery logged `read_only=true`, the ledger hook logged
`lock=AlreadyHeld`, and the complete promotion/rejection/rollback sequence exited successfully in
6.4 seconds.

After closing the writer, a separate selected-CF, read-only vault open observed:

```
latest_seq=17
Kv tuning artifact rows=4
AnnealRollback rows=4
Ledger rows=5
live artifact=8ed7f65833059660a0f15ebcf2afea1701bbd42bd9d63db3a1b5842230872a78
```

The fifth Ledger row is the independently sealed raw-commitment cohort. This proves the fix did
not obtain progress by suppressing the append-only ledger work.

## Boundary audit

1. Empty replay: the native transaction persisted a rejected change and left the live pointer at
   the baseline hash.
2. Bad replay: recall `0.0` crossed `RecallAtK`; the candidate was rejected and the promoted live
   hash remained unchanged.
3. Explicit rollback: the prior 505-byte artifact was restored by exact content hash, then decoded
   from independently reopened physical storage.

The authoritative `scripts/lint.ps1` completed all seven gates in both workspaces.
