# FSV — #1903, #1902, #1905

Date: 2026-07-30
Vault: `%LOCALAPPDATA%\synapse\db-daemon` (live readbacks) plus disposable
scratch vaults for the constructed cases.
Panel under test: `syn-timeline-v1`, generation **1900001**.

Three defects filed while implementing #1900/#1901, verified together because
they share one property: each is a contract that was *believed* to hold and had
never been measured.

- **#1903** — a re-measured slot set was reported as a corrupt shard.
- **#1902** — a BM25 lane's length saturation could not act.
- **#1905** — the reconciliation delta was scoped per panel *name*, not per
  panel *version*.

## Method

Every claim is read from a Source of Truth independent of the call that produced
it:

- the **`Base` column-family row on disk**, re-read through a *separately opened*
  vault handle after the write returned;
- the **live persisted search generation's sidecar bytes**, copied out of the
  production vault and read as JSON;
- the **live daemon's own health readback**, for the delta composition it
  measured itself;
- the **production encoders and the production BM25 scorer**, never a
  reimplementation.

Where a number could be predicted before the run, it was: the corpora below are
built so the expected answer is known from the law under test rather than read
off the output.

---

## #1903 — the refusal existed; it was the classification that was broken

### The premise did not survive measurement

The issue reports that `put_observation_constellation` "silently ignores a
changed slot set". It does not. Both merge policies have always compared slots
(`merge_observation_anchors` directly, `merge_duplicate_anchors` through
`anchor_merge_identity_differences`), and `existing` comes from
`get_at_snapshot`, which hydrates the vectors from the per-slot CFs — so the
comparison is over real measured vectors, not the `SlotVector::Absent`
placeholders a bare `Base` decode carries (#1894).

Run before any change, against a real durable vault:

```
CASE2 slot_added  before=1,2
      result=Err(CALYX_ASTER_CORRUPT_SHARD: content-addressed observation
                 collision; differing identity fields: slots)
      after=1,2
```

A backfill that added a lens without bumping the panel version would therefore
have **errored**, not returned `examined=339 inserted=0 already_current=339`.
That reading came from a measurement that did not carry the new slot at all.

### What was real

`CALYX_ASTER_CORRUPT_SHARD`'s catalog remediation is *"restore from
restic/snapshot"* — pointed at an entirely intact vault, for what is an ordinary
schema-evolution event. That is the #1875 misclassification class: a version
mismatch reported as corruption sent an operator toward destroying a vault with
no backup. And the message named one word, `"slots"`.

### After

`crates/synapse-calyx/examples/slot_membership_drop_fsv.rs`. Every `before` /
`after` is the stored slot membership read back through an independently opened
handle *after the call returned*.

```
CASE0 first_put disposition=Inserted
CASE0 base_membership_on_disk=1,2
CASE1 identical      before=1,2 result=Ok(ExistingIdentical)              after=1,2
CASE2 slot_added     before=1,2 result=Err(CALYX_ASTER_PANEL_SLOT_SET_IMMUTABLE:
        slots_added=[3] slots_removed=[] slots_remeasured=[]
        stored_slots=[1,2] proposed_slots=[1,2,3])                       after=1,2
CASE3 slot_removed   before=1,2 result=Err(... slots_added=[] slots_removed=[2]
        slots_remeasured=[] stored_slots=[1,2] proposed_slots=[1])       after=1,2
CASE4 slot_revalued  before=1,2 result=Err(... slots_added=[] slots_removed=[]
        slots_remeasured=[2] stored_slots=[1,2] proposed_slots=[1,2])    after=1,2
CASE4 slot_2_vector_before=Sparse dim=64 entries=[3:1 11:2]
CASE4 slot_2_vector_after =Sparse dim=64 entries=[3:1 11:2]
```

**Edge cases, and what each proves.** Case 2 (slot added) and case 3 (slot
removed) are the two directions of a membership change and are named separately,
because the remediation differs in emphasis. Case 4 is the one the issue only
raised as a question — same membership, different measured vector — and it is
refused too, because the `Base` slot hash is the vector's integrity record.

Case 4's last two lines are the "the refusal must not half-write" requirement,
answered from the bytes: the stored vector is unchanged after a refused
re-measurement that proposed `3:9`.

---

## #1902 — the length term was computed, persisted, validated, and inert

### Known-answer corpus

Three documents of deliberately different lengths, each containing `alpha`
**exactly once**. With `tf` equal across all three, the length term is the only
thing that can separate them, so a working BM25 must order
`short > medium > long` and an inert one must tie them exactly.

`crates/synapse-calyx/examples/bm25_length_saturation_fsv.rs`, production
encoders and production scorer:

```
=== lane sparse_keywords (L1-normalized) ===
  short_1tok    cells=1    doc_len=1          tf(alpha)=1
  medium_10tok  cells=10   doc_len=1.0000001  tf(alpha)=0.1
  long_100tok   cells=96   doc_len=0.99999934 tf(alpha)=0.01
  avg_doc_len=0.99999976
  b=0.75: short=0.13353142 medium=0.02259762 long=0.00242784
  b=0.00: short=0.13353144 medium=0.02259763 long=0.00242784
  VERDICT b_acts=false (max_relative_score_change_from_b=2.877e-7)
          short_over_long=55.00x tied_at_b0=false

=== lane sparse_keywords_tf (raw counts) ===
  short_1tok    cells=1    doc_len=1          tf(alpha)=1
  medium_10tok  cells=10   doc_len=10         tf(alpha)=1
  long_100tok   cells=96   doc_len=100        tf(alpha)=1
  avg_doc_len=37
  b=0.75: short=0.22182570 medium=0.19035831 long=0.07870717
  b=0.00: short=0.13353144 medium=0.13353144 long=0.13353144
  VERDICT b_acts=true (max_relative_score_change_from_b=6.612e-1)
          short_over_long=2.82x tied_at_b0=true
```

`doc_len` is 1.0 for every row on the normalized lane, and the two `b` settings
differ by less than one ULP. `b=0.75` is operationally `b=0`.

**Two findings the analysis did not predict.**

The normalized lane still *orders* the documents, which makes it worse rather
than better: the order comes from `tf` having become a relative frequency
(1.0 / 0.1 / 0.01), so the 1-token document outscores the 100-token one **55x** —
exactly the unbounded short-document dominance `b` exists to bound. The
raw-count lane bounds it to **2.8x**.

The raw-count lane ties all three **bit-identically** at `b=0`
(`0.13353144` three times). That is the control: the entire separation at
`b=0.75` is attributable to the length term.

### A trap, recorded because it would have hidden the defect

The obvious discriminator — "did any score bit change when `b` changed" —
reports `b_acts=TRUE` on the **broken** lane, because f32 rounding flips a
last-ULP bit. A verification written that way passes on the defect. The
*magnitude* of the change is the discriminator; the instrument now thresholds on
relative change and says so in a comment.

### The guard, against real production bytes

`crates/synapse-calyx/examples/bm25_count_law_guard_fsv.rs` copies the live
generation to scratch and changes **exactly one number** — the minimal
difference between a count lane and a normalized one:

```
copied live generation 1900001 -> <scratch>\idx\search\panel_0001900001
sidecar scoring="bm25" dim=2048 rows=364
chosen row #2 cx_id="02204fc5cbf8fd994044112261e4fdc0" cell=190 weight=1
CASE1 pristine_production_bytes -> Ok(5 hits)
perturbed row #2 weight 1 -> 1.5, resealed sha256=c24ada8a...
CASE2 one_fractional_weight -> Err(CALYX_STALE_DERIVED: persistent sparse BM25
  row 02204fc5... weight 1.5 at index 190 is not a whole term-frequency count ...)
```

`doc_len`, `doc_lengths` and the posting `tf` were moved with the weight and the
sidecar resealed, so the run is stopped by *this* guard rather than by an
integrity check that would have proved nothing.

### Why the issue's proposed build-time test was not the one implemented

The issue proposes failing closed when every row's `doc_len == 1.0`. That
false-positives: a legitimate raw-count corpus of one-token documents has
`doc_len == 1.0` everywhere, and there `b` is genuinely inert *because the
documents genuinely are all the same length*. Refusing that build would break a
correct index. "Every BM25 document weight is a whole number" is the checkable
form of BM25's own definition of `tf`, and cannot false-positive on a count
lane.

### Live-vault impact: none, established before changing anything

```
panel_0001900001  slot 103  kind sparse_bm25  len 364  dim 2048  <- only BM25 lane
                  slots 2,3 kind sparse_dot
panel_0001664001  no sparse_bm25 lane at all
slot_00103 sidecar: rows 364, rows with non-integer entry weights = 0
  doc_len histogram: 0.0x251, 5.0x46, 1.0x18, 2.0x17, 3.0x13, 4.0x6, 7.0x6,
                     6.0x5, 8.0x2
```

Real, varied lengths and zero fractional weights: the live lane passes the new
guard, and `avg_doc_len=3.83` confirms `b` has been acting there since #1900.

---

## #1905 — a slot id is exclusive per panel name, not per panel version

### The enumeration, established rather than asserted

Every writer that can stage a row into a quantized slot CF:

| writer | slot write | same-batch `Base` write? |
|---|---|---|
| ingest — `stage_validated_constellation_rows` (single **and** batch) | live | **yes**, one atomic batch |
| `put_slot_vector` — the only qualified slot write | live | **yes**; the `Base` slot hash *is* the integrity record (#1888) |
| erase — `erase::targets` | tombstone | **yes**; `collect_slot_targets` is reached only alongside the row's own `Base` key |
| orphan-slot GC / reconciler | tombstone | n/a — `Base` is absent by definition |
| anchor merge | none | `Base` only |
| compaction / tiering | SSTs, not commit-domain rows | n/a |

The erase row is the one that could have broken this and does not: a key whose
visible chain is entirely tombstoned is already returned in the panel's key set
as `unattributed`, precisely so a deleted row is still masked.

`mvcc::store` also refuses **at commit** any batch carrying a live
quantized-slot write with neither a same-batch nor a visible `Base` row.

### The constructed second live generation

The live vault cannot show this defect — only one generation of the timeline
panel ingests. `crates/synapse-calyx/examples/panel_generation_delta_fsv.rs`
builds the missing condition: two generations of one panel, both holding live
rows, both measuring slot 3, sharing one physical `cf/slot_03`. After pinning
`base_seq=5`, **exactly one row is written, and it belongs to the OLD
generation**:

```
BEFORE (no writes after base_seq)
  measured_for_OLD  distinct_changed=0 ... slot_other_generation=0 slot_3=0
  measured_for_NEW  distinct_changed=0 ... slot_other_generation=0 slot_3=0

TRIGGER wrote 1 row cx_id=190509a2000000000000000000000000 panel=1905001 (slot 3)
        latest_seq now 6

AFTER
  measured_for_OLD  distinct_changed=1 base_panel=1 base_other_panels=0
                    slot_other_generation=0 slot_orphaned=0 slot_3=1
  measured_for_NEW  distinct_changed=0 base_panel=0 base_other_panels=1
                    slot_other_generation=1 slot_orphaned=0 slot_3=1

VERDICT newer_generation_unmoved_by_older_ingest=true
```

The NEW line carries its own before-and-after: `slot_3=1` is exactly the key the
pre-fix delta unioned into `changed` — the scan still observes it — sitting next
to `distinct_changed=0`.

### Live baseline, read from the running daemon before the change

From `health` on the `e6b80543` build (pid 5560):

```
panel_version=1900001 base_seq=84517 pinned_seq=90025
distinct_changed=85 base_scanned=1114 base_panel=85 base_other_panels=1029
base_unattributed=0
slot_1=85 slot_2=85 slot_3=85 slot_4=85 slot_5=85 slot_6=85 slot_7=85 slot_103=85
```

Every slot CF reports the same 85 keys the panel-scoped `Base` scan already
found, and `distinct_changed == base_panel == 85`: eight full CF scans producing
zero new keys. This is the redundancy claim, measured on production rather than
argued. (`base_other_panels=1029` is the agent-transcript churn #1901 excluded.)

### What the fix keeps rather than drops

The scans still run, as a **cross-check** rather than a contribution. A changed
slot key absent from the panel's `Base` set is classified as another
generation's or as an orphan; one that is neither — a live row of *this*
generation whose slot changed without its `Base` — fails closed naming the
constellation and the CF. That branch is unreachable through the public surface
today, by construction, which is the invariant holding. It exists so the next
writer that breaks the contract is reported where the consequence appears rather
than silently under-reconciling.

---

## Research lane

`pwsh -File scripts\check-research-lane.ps1` → `exa_mcp live`
(`exa-search-server` v3.2.1, `tools/call` returned content). Research was done
through Exa. Primary sources actually read:

- Lucene `BM25Similarity` — `freq` is *"a raw, i.e., unnormalized term
  frequency"*; `avgdl = sumTotalTermFreq / docCount`; the constructor throws when
  `b` is outside `[0..1]`.
- gensim `bm25model.py` — `num_tokens / avgdl` in the denominator, `b=0.0`
  documented as "no length normalization".
- Lv & Zhai, CIKM'11, *Lower-Bounding Term Frequency Normalization*.
- Singhal, Buckley & Mitra, SIGIR'96, *Pivoted Document Length Normalization*.
- FoundationDB Record Layer, *Schema Evolution and Meta-data Maintenance* —
  `validate` throws on an evolution requiring migration of stored records.
- Milvus `schemautil.ValidateSchemaEvolution` — kept/added/dropped fields
  validated separately so the error names the class of change.
- EventSourcingDB, *Versioning Events Without Breaking Everything* — a
  registered schema is immutable; the system forces a new version instead.
