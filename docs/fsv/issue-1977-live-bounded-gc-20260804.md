# Issue #1977: live bounded GC Base walk

Date: 2026-08-04

## Source of truth

The running daemon's storage-maintenance state proves a real scheduled GC tick
completed, while the monotonic MVCC row-guard site counters prove which physical
read primitive it used.

At PID 9472 / uptime 2,083 seconds, storage reported:

```text
gc attempts=1
started=1785823563046 completed=1785823567096 duration_ms=4050
examined_rows=54830 cf_readback_count=17 evicted_rows=0
retry_exhausted=false
```

The row-guard source of truth after that trigger reported:

```text
scan_cf_at_overlay: holds=0 total_held_us=0 over_budget=0 starved=0
scan_cf_range_page_latest: holds=55123 mean_us=431.4 starved=0
```

Before the change, each GC tick produced a `scan_cf_at_overlay` hold over the
whole Base family: 32/32 holds exceeded budget, mean 425,644 us, max 1,964,095
us. The completed live tick examined physical rows and independently read back
17 column families without entering that site once. Its bounded page reads are
visible in the pager counter, and the pager has no starved holds.

Frozen-copy output-equivalence and boundaries (empty range, early stop,
unpageable family) are exercised by `whole_family_hold_fsv` and
`base_walk_hold_fsv`.
