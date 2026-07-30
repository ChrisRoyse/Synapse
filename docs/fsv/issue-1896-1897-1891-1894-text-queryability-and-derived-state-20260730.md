# FSV — #1896, #1897, #1891 (asks 2/3), #1894 (ask 2), #1895

Date: 2026-07-30
Vault: `%LOCALAPPDATA%\synapse\db-daemon` (`db-daemon`), panels `syn-timeline-v1`
(1664001), `syn-agent-event-v1` (1665001), `syn-agent-transcript-v1` (1665002).

## Binaries under test

| | pid | exe sha256 | commit |
|---|---|---|---|
| **before** | 4768 | `988582B4860B3EB3D832AEFB852551646E4021E26A8B6FE52BE5A22062273B95` | `505e6cf8` |
| **after** | 14012 | `D3E12E34E450343F568DF4A4AC1EEB5B534489664C0C5775B0525E13DC67220B` | `1a029061` |

The `after` hash agrees across the built binary, the installed binary, and
`synapse-setup`'s own post-install readback.

## Known-answer test data (source of truth: bytes on disk)

Read through `storage operation=anchors`, which decodes the physical Calyx
Anchors CF row for an exact source row — independent of any search path:

```
CF_TIMELINE key_hex     = 18c6d9c833409ad400000007
source_value_len_bytes  = 364
source_value_sha256     = c9e568badf1f19248e28525bd638e8cbe3010d4607639edd40023ce558417163
derived panel           = syn-timeline-v1 / 1664001
derived cx_id           = fc571cacaaa3ddc7885a7a3960892dcf
title (verbatim)        = issue844-notepad-demo-20260624-091816.txt - Notepad
```

The token `issue844-notepad-demo-20260624-091816` occurs in exactly one title
across the corpus. So the input and the expected output are both known before
the query runs: `by_text` on that token **must** return that `cx_id`.

---

## #1896 — text-queryability declared per encoder

### The gate moved, with the index held constant

Both calls issue the same query against the same vault. Between them the
persisted generation is **unchanged** — still `built_at_seq=55908`,
`rows_covered=222`:

```
before (4768):  SYNAPSE_CALYX_FIND_NO_INDEXABLE_QUERY
                fused find over panel 1664001 produced no query vector on any
                persisted-index slot

after  (14012): CALYX_SEARCH_DELTA_REBASE_REQUIRED
                search delta contains 17443 changed keys between manifest base
                seq 55908 and pinned seq 75773, exceeding the bounded
                reconciliation limit 8192
```

The failure moved **past query measurement** to delta reconciliation — the exact
boundary `by_example` reached in #1891's diagnosis, and the control that pinned
the defect to the gate rather than the index. Since nothing about the index
changed between the two runs, only the gate can account for the difference.

### The round trip, after the generation was refreshed

`find query_mode=by_text text="issue844-notepad-demo-20260624-091816.txt"`:

```
consulted_slots = [3]                       <- ONLY syn.timeline.title_sparse.v1
hit rank 1      = fc571cacaaa3ddc7885a7a3960892dcf   <- the cx_id derived from disk
per_lens        = [{slot: 3, rank: 1, raw_score: 0.25, contribution: 0.016393442}]
RRF check       = 1 / (60 + 1) = 0.016393442        <- matches exactly
provenance_seq  = 18529
generation.manifest_sha256 = ea927e076f5772e9f591a748badaa97f0312e28064b5380bfecf1f1739179eb5
```

```
PASS  source bytes on disk -> constellation -> index -> text query -> same cx_id
PASS  consulted_slots is exactly [3]; slots 1,4,5,6,7 (one-hot / cyclic / rank)
      were NOT consulted, so the gate still refuses lenses that cannot answer text
PASS  slot 2 (syn.timeline.app_hash.v1, syn_hash) was NOT consulted (#1899)
PASS  the RRF contribution equals the published formula to the last digit
PASS  generation.manifest_sha256 equals the sha256 of the manifest file read
      directly off disk, so the query used the artifact that was hashed
```

The `by_example` control on the same `cx_id` reports
`consulted_slots=[1,2,3,4,5,6,7]` — all seven — confirming the gate is specific
to the text path and does not narrow stored-vector recall.

### Edge cases

| input | outcome |
|---|---|
| whitespace-only `"   "` | rejected at param validation: `TOOL_PARAMS_INVALID … requires non-empty text` |
| punctuation-only `"!!! ??? ..."` | no tokens survive; empty result (see below) |
| `"zzzqqqxxnonexistenttoken"` | no document matches; empty result (see below) |

The last two originally returned `SYNAPSE_CALYX_FIND_INDEX_STALE` with a
"reingest or backfill stale slot rows" remediation, against a generation
rebuilt 40 seconds earlier that health reported as `built`. That is a normal
outcome presented as corruption, and it was unreachable until this gate opened.
Fixed in `3e191f24`: an empty per-slot map after the empty-index case has
already been handled means the query matched nothing, so it returns the empty
outcome the sibling branches return, carrying the generation.

---

## #1897 — redundancy fails closed per pair, and names the lens

`storage intelligence redundancy panel_version=1665001 max_records=2000`,
driven through the break-glass ceremony.

```
before: SYNAPSE_CALYX_ASSAY_DEGENERATE_INPUT: estimate pairwise redundancy NMI:
        NMI x column is constant (zero entropy)
        -> whole pass dead, no slot named, 0 measurements kept
```

```
after:  n_lenses            = 7
        records_scanned     = 2155
        pairs_possible      = 21
        pairs_evaluated     = 10
        pairs_skipped       = 11
        effective_rank      = 3.6652
        effective_rank_slots  = 23, 24, 31, 32, 34
        effective_rank_lenses = syn.agent_event.kind_onehot.v1,
                                syn.agent_event.operation_onehot.v1,
                                syn.agent_event.hour_cyclic.v1,
                                syn.agent_event.dow_cyclic.v1,
                                syn.agent_event.record_vector.v1
        assay_cf_rows_after = 1
```

```
PASS  pairs_evaluated + pairs_skipped == pairs_possible   (10 + 11 == 21)
PASS  10 real measurements survive where 0 survived before
PASS  effective_rank names the 5 lenses it covers, so it is not read as covering 7
```

**The dead lens is now named** — the finding the old message could not localise:

```
low_signal_lenses:
  slot=30  lens=syn.agent_event.end_state_onehot.v1  code=CALYX_ASSAY_LOW_SIGNAL
  constant_value=-0.34534848  records_observed=154
```

Every skipped pair carries its reason and the offending slot:

```
23(kind_onehot)      x 30(end_state_onehot)  constant_column  offending=23  n_paired=154
30(end_state_onehot) x 31(hour_cyclic)       constant_column  offending=30  n_paired=154
30(end_state_onehot) x 32(dow_cyclic)        constant_column  offending=30  n_paired=154
30(end_state_onehot) x 34(record_vector)     constant_column  offending=30  n_paired=154
23(kind_onehot)      x 29(error_onehot)      insufficient_paired_samples     n_paired=3
24(operation_onehot) x 29(error_onehot)      insufficient_paired_samples     n_paired=1
24(operation_onehot) x 30(end_state_onehot)  insufficient_paired_samples     n_paired=0
29(error_onehot)     x 30(end_state_onehot)  insufficient_paired_samples     n_paired=2
29(error_onehot)     x 31(hour_cyclic)       insufficient_paired_samples     n_paired=3
29(error_onehot)     x 32(dow_cyclic)        insufficient_paired_samples     n_paired=3
29(error_onehot)     x 34(record_vector)     insufficient_paired_samples     n_paired=3
```

Two further real findings fall out, both of which the old message hid:

* `slot 30` (`end_state_onehot`) is constant over all 154 records that carry it.
* `slot 29` (`error_onehot`) is co-present with other lenses on at most 3
  records, so it is effectively unmeasurable on this corpus.

Note the `23 x 30` skip names **23** as the offender, not 30: on the 154-record
intersection where `end_state` exists, `kind_onehot` is *also* constant. That is
the per-pair check working on the intersection the estimator actually sees,
rather than on each column in isolation.

### bits: zeros that are the absence of a measurement now say so

`storage intelligence bits panel_version=1665001 anchor_kind=outcome`:

```
before: anchored_records=0 distinct_outcomes=0 total_bits=-0.0 slots=[]
after:  anchored_records=0 distinct_outcomes=0 total_bits=0.0  slots=[]
        measurable=false
        unmeasurable_reason="no record in the 2155 scanned panel row(s) carries an
          anchor of kind 'outcome', so there is nothing to measure bits about;
          the panel's domain grounded fraction is 0.0125"
```

```
PASS  total_bits renders 0.0, not -0.0
PASS  the report states outright that it could not measure, and why
```

---

## #1891 asks 2/3 — the generation is maintained unattended

No break-glass ceremony, no operator action. The daemon started at
`08:02:43Z`; the first derived-state tick fired 5 minutes later:

```
SYNAPSE_CALYX_SEARCH_GENERATION_MAINTENANCE_STARTED
  panel_version=1664001 action="refresh_over_existing" destructive=true
  reason=seq_lag 20204 exceeds the refresh threshold 4096
         (half the query-time reconciliation limit 8192)
  before_state=lagging before_built_at_seq=55908 vault_latest_seq=76112

SYNAPSE_CALYX_SEARCH_GENERATION_MAINTENANCE_COMMITTED
  action="refresh_over_existing" destructive=true
  after_state=built after_built_at_seq=76112 after_seq_lag=0
  after_rows_covered=329 after_dense_lanes=5 after_sparse_lanes=2
  elapsed_ms=448
```

### Source of truth: the manifest file itself

Read directly off disk, independent of the daemon:

```
mtime_utc = 2026-07-30T08:08:01.3563796Z     (was 2026-07-29T18:44:54Z)
len_bytes = 2711
sha256    = EA927E076F5772E9F591A748BADAA97F0312E28064B5380BFECF1F1739179EB5
base_seq  = 76112                            (was 55908)
slot_count= 7
max_slot_len = 329                           (was 222)
```

```
PASS  the manifest bytes changed, and base_seq advanced 55908 -> 76112
PASS  rows_covered rose 222 -> 329
PASS  health's built_at_seq/rows_covered/lane counts match the file exactly
PASS  the sha256 of the file equals generation.manifest_sha256 in the find
      response, so the query engine used the artifact that was hashed
PASS  no human ceremony ran: this vault's search_rebuild is break-glass gated,
      and no lease was acquired for it
PASS  the action is classified as refresh_over_existing / destructive=true,
      distinct from the non-destructive initial-build path (ask 3)
PASS  elapsed_ms=448 on the admitted blocking pool, off the runtime workers
```

### Health after the tick

```
calyx_search_generation : ok
  state=built built_at_seq=76112 vault_latest_seq=76166 seq_lag=54
  max_reconciled_delta_keys=8192 rows_covered=329 dense_lanes=5 sparse_lanes=2
  age_ms=41057 rebuild_required=none remediation=none

calyx_derived_state : ok
  attempts=1 success=1 failure=0 skipped=0
  last_search_action=refresh_over_existing
  last_search_reason=seq_lag 20204 exceeds the refresh threshold 4096
                     (half the query-time reconciliation limit 8192)
  last_search_elapsed_ms=448
  refresh_seq_lag_threshold=4096  min_rebuild_interval_ms=600000

health.ok = True        (was False)
```

`seq_lag=54` against a refresh threshold of `4096` and a query-time limit of
`8192`: the generation now sits two orders of magnitude inside the budget that
killed it, and the threshold that repairs it is deliberately half the threshold
at which queries fail, so it is repaired **before** recall dies rather than after.

### Edge case: it does not rebuild every tick

Read 21 minutes after daemon start, i.e. after four ticks:

```
calyx_derived_state:
  attempts=4 success=4 failure=0 skipped=0
  last_search_action=none_needed
  last_search_reason=seq_lag 478 is inside the refresh threshold 4096
  last_search_elapsed_ms=11

calyx_search_generation:
  state=built built_at_seq=76112 vault_latest_seq=76644 seq_lag=532 age_ms=986867
```

```
PASS  only the first of four ticks rebuilt; the manifest mtime and base_seq are
      unchanged since 08:08:01Z / 76112
PASS  a no-op tick costs 11 ms against 448 ms for the one that rebuilt
PASS  the reason names the measured lag and the threshold it was compared against
PASS  health.ok stayed True across all four
```

At the vault's live rate (~33 seq/min) the generation takes roughly two hours to
reach the refresh threshold again, which is the intended amortisation: the
maintainer is cheap when there is nothing to do and only pays the rebuild cost
once per ~4096 sequences of drift.

---

## #1894 ask 2 — lens coverage is a named health deficiency

```
calyx_lens_coverage : ok
  panels_measured=3  deficient_panels=[]  blind_spot_ceiling=0.5
  sample_records_per_panel=256  measured_at_unix_ms=1785398883505
  1664001: n_lenses=5 slots=[1,4,5,6,7]                measured=256/329    blind_spot_records=0
  1665001: n_lenses=6 slots=[23,24,30,31,32,34]        measured=256/2166   blind_spot_records=0
  1665002: n_lenses=9 slots=[35,36,37,42,43,44,45,46,47] measured=256/14030 blind_spot_records=0
```

```
PASS  the alarm #1894 identified as unraised is now a health subsystem
PASS  it reports a real measurement (256 hydrated records per panel), not a guess
PASS  the empty episode panel is skipped rather than reported as a false deficiency
PASS  health reads a published readback, so the corpus is never scanned on a
      health request
```

`n_lenses=6` here vs `n_lenses=7` from the redundancy pass on the same panel is
consistent, not contradictory: coverage samples 256 records while redundancy
scanned 2155, and slot 29 (`error_onehot`) appears on ~3 records in thousands,
so it does not occur in a 256-record sample. The sample size is reported
alongside the number for exactly this reason.

---

## #1895 — the Exa research lane

`pwsh -File scripts\check-research-lane.ps1` on 2026-07-30:

```
initialize OK: exa-search-server v3.2.1
tools/list -> 2 tool(s): web_search_exa, web_fetch_exa
tools/call web_search_exa -> OK (2153 chars returned)
VERDICT: exa_mcp  live
```

Ask 2 — how it authenticates — established from the host, not inferred:

* `~/.claude.json` declares `npx -y exa-mcp-server` with an **empty `env`**; the
  script confirms no `EXA_API_KEY` in the server environment;
* the server's own stderr, captured in the structured readback, opens with
  `[smithery] Configuration loaded`, so the package routes through the Smithery
  gateway and the Exa credential is supplied there;
* `%APPDATA%\smithery\settings.json` holds only a `userId` and an analytics
  consent flag.

There is no operator-held credential on this host at all. The lane runs on
someone else's quota, which is why it went 402 for three days and returned
without anyone acting — and why the script, not any recorded verdict, is the
authority. AGENTS.md and the local memory note now say so.

---

## Defects found while verifying, filed rather than left

* **#1898** — no Synapse lens is a semantic embedding, so free-text recall is
  lexical by construction. Split from #1894 ask 1 and #1896.
* **#1899** — excluding whole-string hash lanes from free-text fusion (correct,
  see below) leaves exact-match-by-hash with no reachable query mode.
* **#1900** — the sparse text lane is a plain normalized dot product, not BM25:
  with no IDF and L1 length normalization, a document titled just `Notepad`
  **ties** the document whose title is the query verbatim, and won the tiebreak.
  Measured on this vault; blocks #1676's stated deliverable.

### One design call reversed mid-implementation

#1896's own table listed `syn_hash` as "arguably" text-queryable. It is not:
`syn::hash` content-addresses the whole byte string into one cell, so **any**
query yields an indexable one-nonzero vector that the built `sparse_dot` lane
will probe. On the dim-1024 timeline lane that is a ~1-in-1024 chance per query
of injecting an entire spurious lane into the rank fusion at full weight, with
nothing in the result distinguishing a collision from a real exact match. It is
excluded, and the capability it removes is filed as #1899.
