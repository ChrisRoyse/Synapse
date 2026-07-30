# FSV — #1901, #1900, #1899, #1898

Date: 2026-07-30
Vault: `%LOCALAPPDATA%\synapse\db-daemon`.
Panel under test: `syn-timeline-v1`, generation **1900001** (superseding 1664001,
which #1900 required because a new lens on a published panel is a new generation).

Four changes, verified together because they are one capability: making free-text
recall on the timeline panel actually work, and making the generation that serves
it survive other panels' ingest.

- **#1901** — the reconciliation delta counted `Base` changes on every panel.
- **#1900** — the "BM25 sparse lane" had no IDF and no length saturation.
- **#1899** — exact-match-by-hash had no query mode.
- **#1898** — decision: encoders only; free-text recall is lexical by construction.

## Method

Every claim below is read from a Source of Truth *independent of the call that
produced it*:

- the **persisted generation artifacts** (`manifest.json`, the per-slot sparse
  sidecars) read as bytes off disk;
- the **authoritative source rows** (`CF_TIMELINE`) read through the timeline
  facade, never through the search path being tested;
- the **published health readback** for the unattended maintainer's own
  measurement.

Expected values are computed by an independent Python reimplementation of
`content_address`/`syn::hash`/`syn::signed_sparse`/`syn::sparse_text_tf` and of
the BM25 law. That reimplementation was **validated against the vault's existing
bytes before being trusted**: it reproduces the stored slot-3 vector of a known
record exactly, and it predicts the slot-2 hash-cell populations that sidecar
actually contains. So a later agreement is evidence about the daemon, not a
restatement of it.

## Binaries and physical state

| | pid | commit | notes |
|---|---|---|---|
| **before** | 19404 | `06fcb9ea` | panel 1664001, generation at seq 77746 over 339 rows |
| **after** | 5560 | `e6b80543` | panel 1900001, generation at seq 84517 over 364 rows |

`synapse-setup` exit 0; tool surface `39875123a8f0416a8c0b0f807e13c55d9b1e3b4273c4ae670ca5f4d3a30e296b`.

### The deployment gate caught a defect in the change itself

The **first** candidate refused to start, on an isolated DB and port, before the
live daemon was touched:

```
CALYX_TIMELINE_CONSTELLATION_MEASUREMENT_FAILED kind=SessionStart
  SYNAPSE_PANEL_SLOT_OUT_OF_BLOCK: panel_version=1900001 panel=syn-timeline-v1
  declared slot id 103, which is outside its exclusive block 1..=7 ...
  Use an id from this panel's block, or allocate a new block in PANEL_SLOT_BLOCKS
TIMELINE_RECORDER_START_FAILED -> MCP_HTTP_STARTUP_TRANSACTION_FAILED
SYNAPSE_CANDIDATE_HEALTH_FAILED ... old live daemon was not touched
```

Slot blocks are **contiguous**, and the timeline panel's `1..=7` is boxed in by
the episode panel at 8, so "take the next free global id" was wrong. A panel may
now own more than one block (`103..=106` for the timeline panel), and
`validate_panel_slot_allocation` checks membership in every block a panel owns
rather than only the first. Boundary audit, observed on the real write path: the
same guard refused 103 before allocation and accepted it after.

```
$ cargo run -p synapse-storage --example panel_slot_guard_fsv
PANEL_SLOTS builder=build_timeline_constellation panel=syn-timeline-v1
            count=8 ids=1,2,3,4,5,6,7,103 min=1 max=103
```

### Bits on disk after the restart

`cf/slot_103` **exists in the live vault** — created by the real ingest path when
the activity recorder wrote its first row under the new panel, before any
rebuild:

```
slot_01 … slot_10, slot_103, slot_11, slot_12, slot_23 … slot_95
```

## Migration: backfill then rebuild

`storage operation=temporal_backfill source_cf=CF_TIMELINE` re-measured every
authoritative row into the new generation, paginated:

```
page 1: examined 200 inserted 200 backfilled 0 already_current 0 more=true
page 2: examined 164 inserted 161 backfilled 0 already_current 3 more=false
totals: examined 364  inserted 361  already_current 3
```

`storage operation=search_rebuild expected_panel_version=1900001` then published
`manifest.json` (`5af6c2f06c868802ca14d630988631e9ae351e0ab6abed5d4ebbeb6f3ea0d72b`),
read back off disk:

| slot | kind | len | dim |
|---|---|---|---|
| 1 | flat_dense | 364 | 32 |
| 2 | sparse_dot | 221 | 1024 |
| 3 | sparse_dot | 364 | 2048 |
| 4 | flat_dense | 364 | 2 |
| 5 | flat_dense | 364 | 2 |
| 6 | flat_dense | 364 | 8 |
| 7 | flat_dense | 364 | 1 |
| **103** | **sparse_bm25** | **364** | **2048** |

## Known-answer corpus

Five real files opened in Notepad, so the titles are exact and the tokens known
before anything is queried. Read back from `CF_TIMELINE` — the authoritative
source, not the index:

```
07:40:44 title_change Notepad.exe  synfsv1900 quarterly ledger review notes.txt - Notepad
07:40:47 title_change Notepad.exe  review.txt - Notepad
07:40:50 title_change Notepad.exe  notes.txt - Notepad
07:40:53 title_change Notepad.exe  quarterly review.txt - Notepad
07:40:56 title_change Notepad.exe  ledger.txt - Notepad
07:42:28 title_change Notepad.exe  synfsv1900 quarterly ledger review notes.txt - Notepad   <- second row, same title
```

The second row carrying the target title matters below: two rows with identical
text **must** score identically, and they do.

---

# #1900 — the lexical lane

## The lane is a raw term-frequency BM25 index (bytes on disk)

`slot_00103_seq_00000000000000084517_n_0000000364.sparse.json`:

```
format                              = calyx-search-sparse-index-v3
scoring                             = bm25
rows                                = 364
field_bearing_rows                  = 113
every_weight_positive_integer_count = True     <- raw counts, not normalized fractions
doc_len_equals_tf_sum               = True
avg_doc_len_stored                  = 3.8318584
avg_doc_len_over_field_bearing_rows = 3.831858  <- agrees
avg_doc_len_over_all_rows_would_be  = 1.18956   <- the defect this build fixed
```

The last two lines are the second self-caught defect, quantified: 251 of the 364
rows carry no title, and averaging over them would have put `avgdl` at 1.19 —
making every title-bearing document look 3.2x longer than average and
re-introducing the short-document bias through the `b` term meant to remove it.

Each known-answer row's vector was predicted here and then **located** in the
sidecar by its exact cell set:

| row | cx_id | doc_len | cells |
|---|---|---|---|
| T1 target | `e00a80e8…` | 7 | 169, 299, 495, 897, 1306, 1568, 1608 |
| T2 `review.txt` | `e96c388a…` | 3 | 1306, 1568, 1608 |
| T3 `notes.txt` | `16953481…` | 3 | 897, 1306, 1608 |
| T4 `quarterly review.txt` | `eb8eef10…` | 4 | 169, 1306, 1568, 1608 |
| T5 `ledger.txt` | `efb5be84…` | 3 | 299, 1306, 1608 |

## The old lane, on the same generation's bytes

`slot_00003…sparse.json` is still built (`sparse_dot`, 364 rows) because the
normalized signed lane remains a similarity feature for `by_example`. Scored with
the validated oracle against the target's own verbatim title:

```
top score 0.125000, and DOCUMENTS TIED AT IT: 19
```

Nineteen documents tie exactly, including every one whose title shares only
`txt`/`notepad`. That is #1900's defect, on the current generation.

## The new lane, live

`find query_mode=by_text` with the target's verbatim title. `consulted_slots: [103]`
— the normalized lane no longer votes on free-text ranking:

```
rank 1  1f60cf46…  slot103 raw=15.353860     <- verbatim title (07:42:28 row)
rank 2  e00a80e8…  slot103 raw=15.353860     <- verbatim title (07:40:44 row)
rank 3  939bed5c…  slot103 raw= 9.588137
rank 4  eb8eef10…  slot103 raw= 9.588137     <- T4 "quarterly review.txt"
rank 5  16953481…  slot103 raw= 7.660436     <- T3 "notes.txt"
rank 6  efb5be84…  slot103 raw= 7.660436     <- T5 "ledger.txt"
rank 7  e96c388a…  slot103 raw= 7.164398     <- T2 "review.txt"
rank 8  1a5704d6…  slot103 raw= 3.837486
```

Both rows carrying the verbatim title hold the top two ranks with identical
scores. The short-title documents that used to tie with the exact match now sit
at 7.16–7.66 against its 15.35 — a **2.14x margin** where the old lane had an
exact tie. The reported law travels with the result:

```
slot 3   scoring_law: dot: score(d) = SUM_t q_t * d_t over stored sparse weights;
                      NO idf and NO document-length saturation, so a term shared with a
                      short document can outscore a full exact match (#1900)
slot 103 scoring_law: bm25: score(d) = SUM_t qtf_t * idf_t * (tf_td*(k1+1)) /
                      (tf_td + k1*(1-b+b*dl_d/avgdl)) with k1=1.2, b=0.75,
                      idf_t = ln(1 + (N-df_t+0.5)/(df_t+0.5))
```

## The score is the law, term by term

Not "the ordering looks right" — the arithmetic, recomputed independently from
the sidecar's own `postings`/`doc_lengths`/`avg_doc_len` and compared to the
daemon's reported `raw_score`:

```
N (field-bearing docs) = 113   avg_doc_len = 3.8318584   k1=1.2  b=0.75
target doc_len = 7   len_norm = 1.826790

token                cell    df       idf   tf   contribution
quarterly             169     4   3.23212    1       2.415217
ledger                299     3   3.48344    1       2.603013
synfsv1900            495     2   3.81991    1       2.854443
notes                 897     3   3.48344    1       2.603013
txt                  1306    10   2.38482    1       1.782070
review               1568     5   3.03145    1       2.265265
notepad              1608    37   1.11186    1       0.830840
TOTAL                                               15.353861

daemon reported raw_score = 15.353860        match (1e-4): True
```

Every one of the eight returned hits matched its independently computed score.
Note what IDF is doing: `synfsv1900` (df 2) contributes 2.85 while `notepad`
(df 37) contributes 0.83 — discriminativeness, which the old lane weighted at a
flat 0.25 per token regardless of document frequency.

## Discrimination and honest refusal

```
query 'synfsv1900'                       -> exactly 2 hits, the two rows that carry it, raw=2.8544
query the verbatim title                 -> those same two rows first
query 'financial statements audited by an accountant'  -> 0 hits
```

The last line is **#1898's verification**: a paraphrase sharing no tokens returns
nothing rather than a fabricated semantic match, while the verbatim phrase hits.
Lexical by construction, and now stated as such in the `find` tool description
and AGENTS.md.

---

# #1899 — exact match by hash, with confirmation

The collision candidate was **constructed, not hoped for**. The `syn::hash`
reimplementation was first validated against the vault's own slot-2 sidecar
(`Code.exe -> cell 984 sign +1.0`, and that sidecar holds exactly 85 rows in
`(984,+1)` — matching an independent count of `app='Code.exe'` rows in
`CF_TIMELINE`). Brute-forcing that cell yields strings that are not any app name
yet land in the same bucket with the same sign.

```
'Code.exe'            -> cell (984, +1.0)
'synfsv-collide-1264' -> cell (984, +1.0)     <- deliberate collision
'Notepad.exe'         -> cell ( 83, -1.0)
'chrome.exe'          -> cell ( 45, -1.0)
```

## Happy path

```
find query_mode=by_exact exact_slot=2 exact_value='Code.exe'
  lens=syn.timeline.app_hash.v1  probe_cells=[984]     <- exactly one cell, as a whole-string hash must
  source=CF_TIMELINE.app
  probed=6  confirmed=6  dropped=0
  every candidate: verdict=confirmed  observed='Code.exe'
  returned hits: 6
```

Each `observed` value is an **independent read of the authoritative source row**,
not a restatement of the index.

## Edge case 1 — the constructed collision

```
find query_mode=by_exact exact_slot=2 exact_value='synfsv-collide-1264'
  probe_cells=[984]                                   <- same bucket as Code.exe
  probed=6  confirmed=0  dropped=6
  every candidate: verdict=bucket_collision  observed='Code.exe'
  returned hits: 0
```

The lane returned six records; confirmation dropped all six and said why. This is
the 1-in-1024 hazard #1899 describes, made deterministic and then made visible.

## Edge case 2 — a real collision, found by accident

Querying an app name that does not exist anywhere in the corpus:

```
find query_mode=by_exact exact_slot=2 exact_value='NoSuchApp-synfsv.exe'
  probe_cells=[1018]
  probed=5  confirmed=0  dropped=5
  every candidate: verdict=bucket_collision  observed='PickerHost.exe'
  returned hits: 0
```

`NoSuchApp-synfsv.exe` happens to share `PickerHost.exe`'s bucket. Nothing was
engineered here — a first guess at a nonexistent app name collided on a dim-1024
lane. Had the hash lane stayed in free-text fusion, this query would have injected
five unrelated records at full lane weight with nothing in the result to say so.

## Edge case 3 — the wrong kind of lane

```
find query_mode=by_exact exact_slot=103 exact_value='review'
  CALYX_SEARCH_EXACT_LENS_NOT_EXACT_VALUE_QUERYABLE: panel 1900001 slot 103 lens
  8e752e90… does not content-address a whole value, so it cannot answer an
  exact-match query; only a whole-string hash lane can
```

## Edge case 4 — empty value

```
find query_mode=by_exact exact_slot=2 exact_value=''
  TOOL_PARAMS_INVALID: query_mode=by_exact requires a non-empty exact_value
```

Both refuse rather than returning a lexical near-match under the name "exact".

---

# #1901 — the reconciliation delta is scoped to its own panel

## The composition, published by the unattended maintainer

Read from `health.subsystems.calyx_search_generation` four minutes after the
rebuild — the maintainer's own measurement, not a measurement made for this test:

```
state = built   delta_changed_keys = 0
composition:
  panel_version=1900001 base_seq=84517 pinned_seq=84729 distinct_changed=0
  base_scanned=98  base_panel=0  base_other_panels=98  base_unattributed=0
  slot_1=0 slot_2=0 slot_3=0 slot_4=0 slot_5=0 slot_6=0 slot_7=0 slot_103=0
maintainer: last_search_action=none_needed
            reason="delta_changed_keys 0 is inside the refresh threshold 4096 (seq_lag 212)"
```

This single line is the defect and the fix side by side. In 212 sequences of
drift the vault committed **98 changed `Base` keys**, and **not one of them
belongs to the timeline panel**. The generation's delta is **0**.

The old code counted `base_scanned` — it would have reported **98** here, and
kept climbing with every MCP call and every agent-transcript row until it crossed
`MAX_RECONCILED_DELTA_KEYS = 8192` and failed every timeline query closed. That is
exactly how #1901 was found: 739 sequences carrying 17,785 keys against a 329-row
generation.

Note also `base_unattributed=0`: no key in this window had an entirely tombstoned
visible history, so the conservative bucket is empty and every counted key was
positively attributed.

## Controlled experiment: other-panel churn, then own-panel churn

Every MCP tool call writes one `syn-mcp-usage-v1` (panel 1776006) constellation,
so tool calls are a clean, countable source of *another panel's* `Base` writes. A
real desktop title change is this panel's own churn. Between the two, the
maintainer's published composition is read as the source of truth.

**Other-panel churn only** — 24 deliberate `health` calls, no desktop activity:

```
base_seq=84517 pinned_seq=84729 distinct_changed=0
base_scanned=98   base_panel=0   base_other_panels=98   base_unattributed=0
slot_1=0 slot_2=0 slot_3=0 slot_4=0 slot_5=0 slot_6=0 slot_7=0 slot_103=0
```

**Then own-panel churn** — one file opened and closed in Notepad:

```
base_seq=84517 pinned_seq=85057 distinct_changed=32
base_scanned=208  base_panel=32  base_other_panels=176  base_unattributed=0
slot_1=32 slot_2=32 slot_3=32 slot_4=32 slot_5=32 slot_6=32 slot_7=32 slot_103=32
```

Read across the two measurements:

| | window 1 | window 2 | delta |
|---|---|---|---|
| `base_scanned` (all panels) | 98 | 208 | +110 |
| `base_other_panels` | 98 | 176 | +78 |
| **`base_panel`** | **0** | **32** | **+32** |
| **`distinct_changed`** | **0** | **32** | **+32** |

The panel's delta moves **only** when the panel itself is written, and it moves by
exactly the number of rows written. Every slot CF independently reports 32,
consistent with 32 new rows each carrying all eight slots — and `distinct_changed`
is 32, not 288, because it is a distinct-key union rather than a sum.
`slot_103=32` is also direct evidence that the new BM25 lane is measured on live
ingest, not only by the backfill.

## The delta path is live, not merely bounded

A query served during window 2 reports its freshness reconciliation:

```
generation base_seq 84517, manifest 5af6c2f0…
rank 1 02204fc5… freshness_policy=fresh_reconciled  freshness_built_at_seq=84969
```

The immutable generation is at 84517 and the query answered against it reconciled
forward — so the bounded reconciliation path is exercised, with a delta it can
actually afford.

## Ask 2 (ambient rate) and ask 4 (re-tune the limit)

Measured, with the scope corrected:

- A **quiet** timeline panel drifts by **0 keys** while the vault commits ~100
  `Base` writes of other panels' churn.
- An **active** burst of desktop use cost 32 keys.

At that rate the 8192-key budget absorbs roughly 250 such bursts — hours to days
of ordinary use — and the 5-minute maintenance tick is generously sufficient.
Under the unscoped count the same budget was consumed by unrelated writes at the
~20–30 keys/minute this host produces while an agent is working, which is how
#1901 measured recall dying twelve minutes after a full rebuild.

**`MAX_RECONCILED_DELTA_KEYS` is therefore left at 8192.** Ask 4 said to re-tune
it only after the scope was fixed; with the scope fixed, the value needs no
change, and changing it now would be a second guess rather than a measurement.

---

# What this FSV found in its own subject

Recorded because both were caught by reality rather than by review, and both
would have shipped silently:

1. **The slot-block guard refused the new lens id** on a fresh candidate vault,
   naming the id, the block, and the repair, and the setup script refused to
   promote the build. The fail-closed chain worked end to end; the live daemon was
   never touched. Fixed by allocating the timeline panel a second block and
   teaching the validator that a panel may own more than one.

2. **`avg_doc_len` averaged over rows that have no title**, which on a raw-count
   lane would have re-introduced the very short-document bias #1900 exists to
   remove — through the `b` term meant to remove it. Found by reading the live
   sidecar (238 of 339 rows title-less, 0.2956 stored vs 0.992 true), before the
   lane ever shipped. The fixed lane's stored `avg_doc_len` is 3.8318584, which is
   the mean over the 113 field-bearing rows; over all 364 it would have been
   1.18956.

# Issues filed from this work

- **#1902** — every `sparse_bm25` lane's length saturation is inert, because
  `sparse_keywords` normalizes too: `doc_len` is 1.0 for every row, so `b`/`avgdl`
  cannot act. Same defect family, the lane that *is* scored BM25.
- **#1903** — `put_observation_constellation` silently ignores a changed slot set
  on an existing row and reports `ExistingIdentical`. This is the trap the #1900
  migration walked into: a lens addition can look like it worked.
- **#1904** — the episode and agent-transcript panels have no term-frequency lane
  yet, so retiring the normalized lane from free-text left them with none. No live
  capability lost (neither has a generation), but the transcript panel is the one
  that most needs lexical recall.
- **#1905** — slot CFs are exclusive per panel *name*, not per panel *version*, so
  two live generations of one panel would charge each other's delta. Latent today
  because the superseded generation is inert.

# Verdict

| issue | asks | verdict |
|---|---|---|
| #1901 | scope the `Base` scan (1), re-measure the rate (2), report composition (3), re-tune only after (4) | **met** — panel delta 0 under 98 keys of other-panel churn; 32 under 32 of its own; composition on the query error, the trace, the status and health; limit deliberately unchanged |
| #1900 | real BM25 (1), persisted stats (2), report the law (3), bare `-` (4), correct the wording (5) | **met** — `sparse_bm25` lane whose every reported score equals the independently computed law; exact-title match beats a one-word title 15.35 vs 7.16 where it used to tie 19-way |
| #1899 | its own query mode (1), confirmation step (2), report the counts (3) | **met** — `by_exact` with source-field confirmation; a constructed collision and an accidental real one both probed and dropped, counted, and explained |
| #1898 | decide and record (1), wire or correct (3) | **met** — encoders-only kept, lexical lane made real, decision in AGENTS.md, paraphrase honestly returns nothing |

One deviation from the letter of #1900 ask 1, argued in the issue: the encoder had
to change, because IDF can be added in the scoring layer but length saturation
cannot — L1 normalization leaves `doc_len` at 1.0 for every row. The change is an
*additional* frozen encoder; no existing lens was modified.
