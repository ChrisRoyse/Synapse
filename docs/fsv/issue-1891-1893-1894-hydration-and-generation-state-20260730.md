# FSV — issues #1891, #1893, #1894 (and #1892): hydrated slot vectors, tie collapse, search-generation state

Date: 2026-07-30 (UTC) · Host: configured Windows 11 host
Daemon: live, pid **4768**, started 22:34:54 local, exe
`C:\Users\hotra\.cargo\bin\synapse-mcp.exe`
Binary identity chain, all three agreeing:

| artifact | sha256 |
|---|---|
| `target\release\synapse-mcp.exe` (built 22:33) | `988582B4860B3EB3D832AEFB852551646E4021E26A8B6FE52BE5A22062273B95` |
| installed `~\.cargo\bin\synapse-mcp.exe` | `988582B4860B3EB3D832AEFB852551646E4021E26A8B6FE52BE5A22062273B95` |
| `synapse-setup.ps1` own readback | `988582B4860B3EB3D832AEFB852551646E4021E26A8B6FE52BE5A22062273B95` |

Source commit: `7445e47a`. Vault: `%LOCALAPPDATA%\synapse\db-daemon`, durable_seq 69511 at open.

Driven over MCP-on-HTTP at `127.0.0.1:7700`. Every "after" reading is taken from a
**separate MCP session** from the one that fired the trigger, and cross-checked
against bytes on disk wherever a physical artifact exists.

Research lane: **Exa (live on this host today — see #1895)** plus the built-in web
lane. Sources for the tie-handling decision are cited in the #1893 comment.

---

## Two root causes were not what the issues said, and reality caught both

This is the part worth recording. Both #1893 and #1894 named a plausible cause,
and in both cases the first fix I would have written from the issue text would
not have fixed the defect.

**#1893** offered "unsorted" or "tied" as candidates. Unsorted was ruled out by
code (`intelligence.rs` sorts, then `series_times` sorts again; NaN/Inf is a
separate branch — so after an ascending sort `gap <= 0.0` can *only* be a tie).
The obvious remaining fix was that the reader discards precision: the writer
persists `source_event_time_raw` in nanoseconds *and* a whole-second truncation
beside it, and the reader took the truncation. But probing the vault showed the
ties are genuine at **nanosecond** resolution — three distinct constellations
share `ts_ns=1785186524706880300`:

| `CF_AGENT_EVENTS` key_hex | seq | cx_id | value len |
|---|---|---|---|
| `18c641af96f7bb2c00000000` | 0 | `f8f6106851c2bf920653811d07e7cd1f` | 824 |
| `18c641af96f7bb2c00000001` | 1 | `6caac4473f1e91b7cbb23f03c5031900` | 476 |
| `18c641af96f7bb2c00000002` | 2 | `8711cfebc7aef30b61560825190afd92` | 574 |
| `18c641af96f7bb2c00000003` | 3 | — | `STORAGE_READ_FAILED` (absent) |

So precision recovery alone would still have failed at index 0. The key codec
(`agent_events.rs:24`) carries a `seq` disambiguator *precisely because* rows
share a `ts_ns` — the schema already said this. Both layers were needed.

**#1894** concluded "nothing populates content lenses on the Synapse panels."
The opposite is true: `build_agent_event_constellation` measures and persists
**twelve** content slots per agent event (slots 23–34). The zeros came from
`decode_constellation_base`, which inserts `SlotVector::Absent { NotApplicable }`
for *every* slot by design — a Base row stores `(slot_id, slot_hash)` pairs and
the vectors live in the per-slot CFs. Five loaders read vectors off Base rows, so
their lens sets were **structurally always empty on every panel**, including
`syn-timeline-v1` whose manifest carries seven built index lanes. One defect,
five surfaces, and it never raised an error: no row was missing, nothing failed,
and every count was a truthful zero over nothing.

Swept both workspaces afterwards: every other `decode_constellation_base` call
site reads slot **ids** (`slots.keys()`, `slots.contains_key`), scalars, or
metadata — all of which *are* on the Base row. The defect was confined to the
five loaders fixed here.

---

## #1891 — health reports the persisted search generation

**Source of truth:** the manifest bytes at
`%LOCALAPPDATA%\synapse\db-daemon\idx\search\panel_0001664001\manifest.json`,
read directly off disk, independent of the daemon.

```
panel_version = 1664001   base_seq = 55908   slot_count = 7
rows_covered  = 222 (max per-slot len)
dense lanes   = 5   sparse lanes = 2
mtime_ms      = 1785350694536
```

`health` subsystem `calyx_search_generation`, read from an independent session:

```
status                          = error
calyx_search_generation_state   = lagging
panel_version                   = 1664001
manifest_present                = True
built_at_seq                    = 55908
seq_lag                         = 13685
max_reconciled_delta_keys       = 8192
rows_covered                    = 222
dense_slot_count                = 5
sparse_slot_count               = 2
age_ms                          = 31873631   (~8.85 h)
remediation                     = the generation is further behind the vault than the
                                  bounded delta-reconciliation limit, so queries are
                                  expected to fail closed with
                                  CALYX_SEARCH_DELTA_REBASE_REQUIRED. Rebuild it with
                                  storage operation=search_rebuild.
health.ok (whole payload)        = False
```

Every check passed:

```
PASS  panel_version matches disk
PASS  built_at_seq matches disk base_seq
PASS  rows_covered matches disk max slot len
PASS  dense lane count matches disk
PASS  sparse lane count matches disk
PASS  manifest_present is true
PASS  state is a known verdict
PASS  remediation is present
PASS  a non-built generation reports status=error
PASS  seq_lag 13685 > limit 8192 classifies as lagging
```

Independent corroboration from a different observer: `synapse-setup.ps1`'s own
post-install health readback printed
`calyx_search_generation=error` in its subsystem list without being asked to look
for it.

**The issue's stated cause was wrong and is corrected.** The generation is
**built**, not absent, and the claim carried in three remediation strings — "the
live vault's `index_inverted` CF is empty, so the BM25 sparse lane is unbuilt" —
is false: the manifest carries two `sparse_dot` lanes (slots 2 and 3, dim 1024
len 125 and dim 2048 len 222). Those strings were themselves a lying surface and
were corrected. The real blocker is staleness, confirmed by driving a real
`by_example` query, which failed with
`CALYX_SEARCH_DELTA_REBASE_REQUIRED: 15413 changed keys between manifest base seq
55908 and pinned seq 67346, exceeding the bounded reconciliation limit 8192`.

**Not in scope here:** deciding who *keeps* the generation fresh (ask 2) and
splitting an initial build from a destructive rebuild (ask 3). Both change
unattended write behaviour on the production vault and deserve their own FSV; a
recommendation is recorded on the issue rather than guessed at.

---

## #1894 — hydrated slot vectors make the lens count real

**Source of truth:** the Calyx `XTerm` and `Graph` CF row counts, read by
`intelligence abundance` (a separate read-only operation that physically scans
those CFs) from a session distinct from the one that ran the weave.

Trigger: `storage intelligence weave` on panel 1665001, through the documented
break-glass ceremony (`act lease_acquire` → `profile set break_glass` → run →
restore → release).

Against the measurement recorded in the issue on the same vault and the previous
binary:

| metric | before (#1894 as filed) | after |
|---|---|---|
| `n_lenses` | 0 | **7** |
| `records_scanned` | 1740 | 1951 |
| `records_woven` | **0** | **1951** |
| `lens_pairs_possible` | 0 | **21** |
| `lens_pairs_co_present` | 0 | **20** |
| `cross_terms_materialized` | 0 | **1954** |
| `agreement_edges_persisted` | 0 | **2** |
| `between_record_edges_persisted` | 0 | **74174** |
| `blind_spot_records` | **1740** | **0** |
| `xterm_cf_rows` | 0 | **1954** |
| `graph_cf_rows` | 0 | **74177** |

```
PASS  weave n_lenses > 0 (was structurally 0 before the fix)
PASS  abundance n_lenses > 0 on an independent read
PASS  abundance agrees with weave on n_lenses
PASS  lens_pairs_possible > 0 (so the C(N,2) criterion is not vacuous)
PASS  XTerm rows after == the count weave reported persisting
PASS  Graph rows after == the count weave reported persisting
PASS  XTerm rows did not shrink
```

`n_lenses = 7`, not 12, and that is honest: `DenseRecord` keeps only
`SlotVector::Dense`, and the four hash-based slots
(`provider_hash`, `request_model_hash`, `response_model_hash`, `tool_hash`) are
sparse. `blind_spot_records` falling 1740 → 0 is the alarm #1894 said nothing
raised — there is now nothing to raise, because the records genuinely carry
co-present lens pairs.

### The kernel reaches its recall gate

`storage intelligence kernel content_slot=34` (`AE_SLOT_RECORD_VECTOR`, the dense
dim-64 record vector):

```
before: CALYX_KERNEL_EMPTY_RESULT — panel 1665001 slot 0 has 0 embedded concept(s)
after:  CALYX_KERNEL_RECALL_BELOW_GATE
```

The change of *which* error fires is the evidence: the kernel now embeds
concepts, builds an index, measures recall, and **reaches its recall gate**, then
honestly reports recall below it. That is exactly what #1894's verification asked
for.

### Four other epic surfaces stopped reporting vacuous zeros

The same five-loader fix un-blanks surfaces filed under other issues, because
they read the same loaders:

`hygiene grounding_gap` (#1670), read-only:

```
grounded_fraction = 0.0132   ungrounded_records = 1937   base_cf_rows = 16243
per-slot coverage entries = 10        (was 0 lenses)
provisional = true
```

`hygiene blind_spot` (#1674), read-only:

```
n_lenses = 7   slot_pairs_evaluated = 14   alerts_total = 90     (was 0 / 0 / 0)
```

`intelligence redundancy` (#1672) now **reaches the estimator** and fails closed
on real data rather than passing vacuously:
`CALYX_ASSAY_DEGENERATE_INPUT: NMI x column is constant (zero entropy)`. That is
progress and a new finding — one of the seven lenses has zero entropy over 1,951
records — but the message names no slot and one degenerate lens discards the 15
measurable pairs. Filed as **#1897**.

`intelligence bits anchor_kind=outcome` (#1672) reports honestly and cannot
measure: `anchored_records=0 distinct_outcomes=0 total_bits=-0.0 grounded=false
domain_provisional=true domain_grounded_fraction=0.0132`. Correct behaviour, and
precisely the condition recorded in the acceptance-criteria amendment posted to
#1671/#1672/#1675/#1676.

---

## #1893 — drift and hazard over collapsed distinct instants

Both operations previously failed on the very first comparison with
`SYNAPSE_CALYX_ASSAY_INSUFFICIENT_SAMPLES: occurrence times must be strictly
increasing; violation at index 0; remediation=anchor more outcomes`.

Trigger: `storage intelligence drift` / `hazard` on panel 1665001,
`group_key=agent_event_kind`, under break-glass.

```
drift
  n_occurrences = 1955   n_distinct_instants = 1867
  ties_collapsed = 88    max_multiplicity = 16      n_gaps = 1866
  cusum_change_detected = true   direction = slow_down   mmd_p_value = 0.01
  temporal_xterm_cf_rows_after = 2

hazard
  n_occurrences = 1955   n_distinct_instants = 1867
  ties_collapsed = 88    max_multiplicity = 16      n_gaps = 1866
  survival = 0.0915      overdue = false     expected_next_seconds = 1785382751.9
  temporal_xterm_cf_rows_after = 3
```

```
PASS  drift/hazard returned a measured result at all
PASS  n_distinct_instants > 0
PASS  n_occurrences >= n_distinct_instants
PASS  ties_collapsed == n_occurrences - n_distinct_instants
PASS  ties_collapsed > 0 (the corpus really does tie)
PASS  max_multiplicity >= 2 given ties exist
PASS  n_gaps == n_distinct_instants - 1
PASS  persisted a TemporalXTerm row
```

`max_multiplicity = 16` is the decisive quantitative confirmation of the
diagnosis: even at full nanosecond precision, up to sixteen agent events share
one instant. The arithmetic is internally consistent —
`1955 − 1867 = 88 = ties_collapsed`, and `n_gaps = 1867 − 1 = 1866` — so nothing
was dropped silently and the reported collapse accounts for every occurrence.

### Boundary and edge-case audit — each condition reports its own code

```
EDGE 1  filter_value = "__no_such_agent_event_kind__"   (matches nothing)
        -> SYNAPSE_CALYX_TEMPORAL_INSUFFICIENT_EVENTS
        PASS  reported one of the expected conditions
        PASS  did NOT misreport as ASSAY_INSUFFICIENT_SAMPLES

EDGE 2  filter_value = "spawn_ready"                    (17 real events)
        -> measured: n_occurrences=17  n_distinct_instants=17  ties_collapsed=0
        PASS  reported one of the expected conditions
        PASS  did NOT misreport as ASSAY_INSUFFICIENT_SAMPLES

EDGE 3  panel_version = 999999                          (no such panel)
        -> SYNAPSE_CALYX_TEMPORAL_INSUFFICIENT_EVENTS
        PASS  reported one of the expected conditions
        PASS  did NOT misreport as ASSAY_INSUFFICIENT_SAMPLES
```

EDGE 2 is the important control: a stream with **no** ties reports
`ties_collapsed=0`, proving the collapse is not applied blanket-wise and only
fires where ties actually exist. EDGE 1 and EDGE 3 prove
`INSUFFICIENT_*` now means only what it says — a genuine sample-count shortfall —
and no longer absorbs an ordering condition.

### The new error code is catalogued and mapped

`CALYX_ASSAY_OCCURRENCES_NOT_MONOTONIC` cannot be triggered through the daemon
any more, which is the point: every daemon path now sorts and collapses before
the estimator sees the series. Its presence is instead proved structurally.
`SynapseCalyxVault` calls `validate_calyx_error_bridge()` on every vault open,
which fails closed if the PRD-18 catalog and the Synapse mapping table disagree
in count or content, and `error_bridge.rs` carries a compile-time
`const _: [(); 42]` assertion on the catalog size. So a clean vault open is proof
the code exists and maps into the `SYNAPSE_CALYX_` namespace:

```
calyx_vault.status  = ok
calyx_vault_open    = True
calyx_vault_phase   = open
last_error_code     = None
PASS  vault opened cleanly => validate_calyx_error_bridge() accepted the 42-code
      catalog including CALYX_ASSAY_OCCURRENCES_NOT_MONOTONIC
```

---

## #1892 — calyx-forge CUDA root probe

Verified before deployment, since it is a build script. All three branches:

```
CASE 1  CUDA_PATH unset, no toolkit on this host
  CALYX_FORGE_NVCC_NOT_FOUND: the `cuda` feature needs the CUDA 13.3 toolkit, and
  no nvcc was found.
  probed:
    C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v13.3\bin\nvcc.exe
  CUDA_PATH is unset, so the platform default 13.3 root(s) were probed; set
  CUDA_PATH to a CUDA 13.3 root to override

CASE 2  CUDA_PATH -> a scratch root containing bin\nvcc.exe (0 bytes)
  probe ACCEPTED it; execution proceeded past locate_nvcc and failed later at
  `nvcc --version`: "%1 is not a valid Win32 application. (os error 193)"
  -> the override branch is exercised, not just the default branch

CASE 3  CUDA_PATH -> a directory with no bin\nvcc.exe
  probed:
    ...\scratchpad\empty-cuda\bin\nvcc.exe
  CUDA_PATH is set, so only it was probed (13.3 kernels)
```

The pre-fix message named `/usr/local/cuda-13.3\bin\nvcc.exe` — a Linux default
with a Windows suffix, a path that cannot exist on any platform. Every candidate
is now built with `Path::join`, defaults are per-platform, and the panic states
the probed roots and the override separately.

---

## Daemon-wide ERROR baseline after the change

The new daemon's hourly log (`synapse.log.2026-07-30-03`) contains **4 ERROR
lines in total**, and all four are the FSV's own deliberate edge-case calls:

```
2  SYNAPSE_CALYX_TEMPORAL_INSUFFICIENT_EVENTS   (EDGE 1, EDGE 3)
1  CALYX_KERNEL_RECALL_BELOW_GATE               (kernel FSV)
1  SYNAPSE_CALYX_ASSAY_DEGENERATE_INPUT         (redundancy FSV)
```

No unexplained ERROR traffic, and zero occurrences of the #1801 operator-panic
CDP refusal signature (`operator panic disabled`, `refusing closeTab`,
`stale tab ids will not be mutated`) — see the #1801 note below.

---

## Instruments

Written for this run, kept out of the repo (scratchpad):

- `syn.py` — minimal MCP-over-HTTP client (real `initialize` +
  `notifications/initialized` + `tools/call`, SSE-framed responses).
- `bg.py` — the documented break-glass ceremony as a context manager, so every
  gated call restores `normal_agent` and releases the lease.
- `fsv.py` — the checks transcribed above; each block reads its Source of Truth
  from a session separate from the trigger.

## What this run did NOT establish

- **#1891 asks 2 and 3** (who keeps the generation fresh; splitting initial build
  from destructive rebuild) are not implemented, so recall on this vault is still
  dead until a `search_rebuild` ceremony runs. Reported, not fixed.
- **`by_text` recall** remains unreachable on every Synapse panel for an
  unrelated reason: the query gate requires `Modality::Text` and all 78 Synapse
  lens declarations are `Modality::Structured`. Filed as **#1896**, proven by a
  `by_example` control that reaches delta reconciliation while `by_text` never
  gets past query measurement.
- **`intelligence redundancy`** cannot complete on this panel until #1897 is
  addressed.
- **#1801** is implemented in code (backoff 250 ms → 30 s, ERROR-once then
  5-minute rollup, fail-closed disposition preserved) across three landed
  commits, and the log evidence above shows zero occurrences. But the triggering
  condition (operator panic active *and* stale sessions holding tabs) did not
  arise during this window, so absence of spam is evidence the fix is in place,
  not a demonstration of the backoff firing.
- Weave over 1,951 records with per-record slot hydration is materially slower
  than the previous (vacuous) pass, since it now performs real per-slot CF reads.
  It completed well inside the call budget here; it has not been characterised at
  the `SYNAPSE_INTELLIGENCE_MAX_RECORDS` cap of 20,000.
