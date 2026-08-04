# Issue #2005: atomic action Oracle publication FSV (2026-08-04)

## Invariant and source of truth

A terminal action audit row, its `syn-action-v1` constellation, and its typed
Oracle recurrence occurrence must enter one Aster WAL/MVCC commit. A stable
action recurrence-subject Base row may pre-exist as schema/identity scaffolding;
it carries no outcome claim.

Sources of truth are the physical Calyx KV, Base, Slot, Scalar, Recurrence,
Ledger, and raw-commitment rows. The FSV records the batch sequence returned
under the durable commit lock, then uses independent reads and provenance
reproduction after the trigger.

Research was completed after diagnosis. The design follows the transactional
outbox atomic-write rule and explicit event/provenance identities:

- https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html
- https://learn.microsoft.com/azure/architecture/databases/guide/transactional-out-box-cosmos
- https://opentelemetry.io/docs/specs/semconv/general/events/
- https://www.w3.org/TR/prov-o/

## Known-answer atomic publication

Fresh vault:

`C:\Users\hotra\AppData\Local\Temp\synapse-action-oracle-atomic-1785866135324`

The real storage facade published a known-success action. Separate readback:

```text
HAPPY before_latest_seq=10
HAPPY publication=... occurrence_id: 0, committed_seq: 12, latest_seq: 12, source_row_count: 1
HAPPY_READBACK latest_seq=12 source_exact=true frequency=1 active_occurrences=1
reproduced=true entry_present=true entry_self_verifies=true subject_matches=true drift="none"
chain intact=true covers_full_history=true chain_origin="vault-genesis"
raw_commitments_intact=true raw_commitment_count=8 pending_count=0
```

The source bytes, constellation provenance, recurrence count, ledger chain, and
raw commitment set therefore agree at the same post-trigger state.

## Boundary audit

```text
OVERSIZED before_latest_seq=12
OVERSIZED error_code=STORAGE_WRITE_FAILED after_latest_seq=12 source_present=false
EMPTY_TOOL before_latest_seq=12
EMPTY_TOOL error_code=STORAGE_WRITE_FAILED after_latest_seq=12 source_present=false
DUPLICATE before_latest_seq=12
DUPLICATE error_code=SYNAPSE_CALYX_LEDGER_APPEND_ONLY_VIOLATION after_latest_seq=12 frequency=1 active_occurrences=1
```

A 257-byte context, empty action identity, and duplicate terminal publication
all fail before terminal source visibility and do not advance durable sequence.

## Wired MCP FSV

Isolated repo-built daemon vault:

`C:\Users\hotra\AppData\Local\Temp\synapse-action-oracle-atomic-mcp-1785866559894\declared`

Real stdio `tools/call act_focus_window` with HWND `1` produced the expected
Win32 `ACTION_WINDOW_NOT_FOUND`. The atomic commit log recorded:

```text
ACTION_AUDIT_ORACLE_ATOMIC_COMMITTED
tool=act_focus_window status=error
source_key_hex=18c8ac2d72fb23e000000001
subject_cx_id=e72c1ba377eacbdce63c3e0d8c2edfb3
constellation_cx_id=cfdb0c1f7449a34a8f685257eff3911d
occurrence_id=0 committed_seq=15 latest_seq=15 source_row_count=1
```

After daemon shutdown, an independent process reopened the vault and read:

```text
ACTION_LOG rows=2
terminal key_hex=18c8ac2d72fb23e000000001 status=error error_code=ACTION_WINDOW_NOT_FOUND
ORACLE_ACTION ... frequency=1 active_occurrences=1
ORACLE_OCCURRENCE id=0 ... outcome_anchor.bool=false source_action_audit_key_hex=18c8ac2d72fb23e000000001
```

The later `latest_seq=17` belongs to orderly shutdown work; the structured
atomic log fixes the action publication itself at sequence 15.
