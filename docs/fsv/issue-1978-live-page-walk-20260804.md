# Issue #1978: live bounded page-walk occupancy

Date: 2026-08-04

## Source of truth

The running daemon's `calyx_vault.calyx_row_guard_sites` counters are monotonic
process-local instrumentation around the actual MVCC row-table guards. The
trigger is `storage operation=panel_coverage`, whose independent Base-CF census
reported 245,975 rows, 14 panels, zero decode failures, and
`accounting_holds=true`.

## Before, trigger, after

Before the trigger, `scan_cf_range_page_latest` held:

```text
holds=53718 total_held_us=23434765 over_budget=4 starved=0
```

After one real whole-vault panel-coverage census:

```text
holds=54693 total_held_us=23676446 over_budget=4 starved=0
```

The exact trigger delta was 975 holds / 241,681 us, or 247.9 us per hold, with
zero new over-budget holds and zero starvation. The pre-fix live baseline was a
6,660 us mean, 6,435 over-budget holds, and 678 starved pager holds.

This proves the daemon's `full_mvcc_restore` handle serves the bounded latest
walk from the complete MVCC row table and no longer re-opens/re-reads the CF
router SST set under every page guard. Frozen-copy parity and page-size boundary
measurements are in `page_walk_router_parity_fsv` and `page_walk_cost_fsv`.
