# FSV — #1776: panel-local SlotIds collide in global physical slot CFs

Date: 2026-07-28
Host: CABTOP (Windows 11 Pro 10.0.26200)
Commit: `3e873097`
Research lane: not required for the identity-model decision — the constraint is
read directly out of this repo's own code (`calyx-aster::cf::slot_key`,
`calyx-search::persisted::rebuild`), not from external practice.

## Scope of this record

This covers the **allocation** half of #1776: making slot ids globally unique,
making that structurally enforced, and correcting the AP-60 claim. It does
**not** cover migrating rows already written under the old panel-local ids. That
remains open, and the audit mode added here measures exactly what is left.

## Ask 3 was already satisfied

The issue's third required fix — "replace AP-60 hardcoded IDs with temporal
classification derived from the exact panel/slot contract" — is already in the
tree. `calyx-sextant::temporal::search::split_primary_slots` partitions on
`engine.is_retrieval_only_slot(slot)`, and a repo-wide search for the hardcoded
`[20, 21, 22]` returns nothing. No change was needed; the issue text predates
that fix.

## The collision, measured rather than argued

New read-only mode `dump_cf --panel-slot-audit <vault>` walks the Base CF,
groups each constellation's declared slot ids by its `synapse_panel_name`, and
reports every slot id claimed by more than one panel. It does not take the
writer lock, so it runs against the live vault while the daemon owns it.

Production vault, before the change:

```
panel_slot_audit vault_id=01KYJPGWATPD4XNMZY3ERGTKQW snapshot=32023
base_rows=9227 undecodable=0 unlabelled_panel=0 panels=9 distinct_slot_ids=37

panel syn-timeline-v1           slot_ids=1..7
panel syn-action-v1             slot_ids=1..5
panel syn-process-v1            slot_ids=1..7
panel syn-observation-v1        slot_ids=1..8
panel syn-outcome-v1            slot_ids=1..7
panel syn-mcp-usage-v1          slot_ids=1..12
panel syn-recurrence-subject-v1 slot_ids=1,2
panel syn-agent-event-v1        slot_ids=23..34     <- disjoint, clean
panel syn-agent-transcript-v1   slot_ids=35..47     <- disjoint, clean

COLLISION slot_01 panel_count=7  action=224 mcp-usage=135 observation=1
                                 outcome=6 process=2 recurrence-subject=7
                                 timeline=178
COLLISION slot_02 panel_count=7
COLLISION slot_03 panel_count=6   COLLISION slot_04 panel_count=6
COLLISION slot_05 panel_count=6   COLLISION slot_06 panel_count=5
COLLISION slot_07 panel_count=5   COLLISION slot_08 panel_count=2
collision_slot_ids=8
```

553 rows of seven different meanings in `cf/slot_01` alone. Any association,
assay, kernel or search result computed over that column family has been
comparing a timeline kind-onehot against an action kind-onehot against an
mcp-usage tool-onehot against a process hash.

## The identity model chosen, and why

The issue offered two options. **Globally unique slot ids with an allocation
registry** was taken, because the physical and query layers already assume it:

- `calyx-aster::cf::slot_key(cx_id)` has no panel component — the CF *is* the
  namespace.
- `calyx-search::persisted::rebuild` scans a whole global `ColumnFamily::slot`.
- `calyx-sextant` selects global `SlotId`s.
- `SlotId` is a `u16`; Synapse uses ~100 of 65,535.

Making `(panel_id, slot_id)` first-class would mean changing CF layout, keys,
manifests, indexes, compaction, erase, backup/restore and inspection to buy
nothing at this scale. Global ids fit the existing model exactly and make the CF
directory name self-describing.

`calyx-core::PanelSlotId` (`panel_version` + `slot_id`) already exists and is
used at the intelligence layer, but it is a *logical* qualifier, not the
physical identity — the column family is still global. It does not conflict with
this change; it becomes strictly more meaningful, because the pair is now
unambiguous instead of describing a slot that several panels also write.

A sweep for other code holding these ids found none: the only remaining
`SlotId::new(...)` call sites outside `constellations.rs`
(`synapse-calyx/src/find.rs`, `synapse-calyx/src/intelligence.rs`) build the id
from a caller-supplied parameter rather than a hardcoded panel constant, and no
renumbered constant is referenced outside its own module.

Allocation (contiguous, exclusive):

| panel | block |
| --- | --- |
| timeline | 1..=7 |
| episode | 8..=22 |
| agent-event | 23..=34 |
| agent-transcript | 35..=47 |
| action | 48..=52 |
| reflex | 53..=59 |
| process | 60..=66 |
| observation | 67..=74 |
| outcome | 75..=81 |
| mcp-usage | 82..=93 |
| recurrence-subject | 94..=95 |
| graphpos-app | 96..=97 |
| graphpos-process | 98..=99 |
| path-hierarchy | 100..=102 |

The four already-disjoint panels keep their ids, so nothing that is currently
correct has to move. The graph-position app and process panels were sharing
slots 1-2 through one shared pair of constants; they are now distinct panels
with distinct blocks, since they measure different graphs.

## Guard 1 — compile time. Verified by making the build fail.

`PANEL_SLOT_BLOCKS` disjointness is a `const` assertion. Introducing an overlap
(action `48..=52` → `47..=52`, colliding with agent-transcript `35..=47`):

```
error[E0080]: evaluation panicked: PANEL_SLOT_BLOCKS must be well formed, within
CALYX_DURABLE_SLOT_ID_MAX, and mutually disjoint: Calyx stores every slot in a
global cf/slot_<id> column family, so two panels sharing an id share one
physical column family (issue #1776)
   --> crates\synapse-storage\src\constellations.rs:294:15
error: could not compile `synapse-storage` (lib) due to 1 previous error
```

The collision cannot be reintroduced by a table edit: the build stops it.

## Guard 2 — write time. Verified by making a real builder fail.

A wrong *literal* inside one panel's builder keeps the table disjoint, so the
compile-time check cannot see it. `validate_panel_slot_allocation` (which
replaces the old "is the id ≤ 47" budget check) fails closed on it. Typing
timeline's slot 3 into the action builder — which still compiles — then building
a real action constellation through the real public
`build_action_constellation`:

```
Error: WriteFailed { cf_name: "calyx_constellation", detail:
  "SYNAPSE_PANEL_SLOT_OUT_OF_BLOCK: panel_version=1666001
   pointer=synapse://CF_ACTION_LOG/... panel=syn-action-v1 declared slot id 3,
   which is outside its exclusive block 48..=52. Calyx stores every slot in a
   global cf/slot_03 column family keyed by CxId alone, so writing it here would
   mix this panel's vectors into another panel's physical column family
   (issue #1776). Use an id from this panel's block, or allocate a new block in
   PANEL_SLOT_BLOCKS" }
```

Two further fail-closed cases exist for completeness:
`SYNAPSE_PANEL_SLOT_UNSCOPED` (a constellation with no `synapse_panel_name`, so
its slots cannot be scoped at all) and `SYNAPSE_PANEL_SLOT_BLOCK_MISSING` (a
panel with no entry in the table).

## Real builders now emit their own blocks

`panel_slot_guard_fsv` drives the real public builders on real record shapes and
prints the slot ids they declare:

```
PANEL_SLOTS builder=build_action_constellation  panel=syn-action-v1
            count=5 ids=48,49,50,51,52 min=48 max=52
PANEL_SLOTS builder=build_process_constellation panel=syn-process-v1
            count=7 ids=60,61,62,63,64,65,66 min=60 max=66
```

Observed ids, not assumed ones, and each inside its declared block.

## Panel versions

A `panel_version` identifies a slot layout, so the renumbered panels take new
versions (`1_776_001`..`1_776_007`). Reusing the old ones would make a single
version mean two different slot maps and reinterpret every pre-move row under
the new meaning — exactly what the issue forbids. The superseded versions are
kept as `SYN_PRE_1776_PANEL_VERSIONS` so the migration and any audit can name
them exactly instead of by magic number.

## What remains open

The rows already written under the old panel-local ids are still physically in
`cf/slot_01`..`cf/slot_08`. This change stops that set growing and makes it
impossible to add to by accident; it does not move it. The remaining work is the
migration, and its exact scope is measurable at any time with
`dump_cf --panel-slot-audit`.
