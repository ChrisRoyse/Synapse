# Issue #1679: reproduce and privacy erase FSV (2026-08-04)

## Source of truth

The fixture uses an isolated real vault at
`C:\Users\hotra\AppData\Local\Temp\synapse-provenance-erase-final-1785870068865`.
The source of truth is the physical Calyx Base, per-slot, Scalar, Anchor,
Recurrence, and Ledger column families read after closing the writer.

## Trigger

One terminal action containing the marker `must-be-unrecoverable` was published
through the same atomic source/constellation/recurrence path used by the daemon.
The fixture then called the public storage reproduce and erase operations.

## Readback

```text
BEFORE seq=10 source_present=false
REPRODUCE seq=12 cx=645febed8b130c0db9e1667a7d7b700d
  reproduced=true entry_present=true entry_self_verifies=true drift=none
ERASE before_seq=12 after_seq=13 records_deleted=1
  tombstone_present=true tombstone_seq=4
  chain verdict=intact entries=5 covers_full_history=true
  raw_commitments_intact=true sealed=8/8
EDGE_ALREADY_ABSENT before_seq=13 after_seq=13
  code=SYNAPSE_CALYX_ERASE_ALREADY_TOMBSTONED
EDGE_INVALID_ID before_seq=13 after_seq=13
  code=SYNAPSE_CALYX_CX_ID_INVALID
PHYSICAL_SOT snapshot=13 erased_cx_rows=0 tombstone_ledger_seq=4
  ledger_entry_present=true ledger_rows=5
```

`erased_cx_rows=0` is independently computed over Base, all five action slot
families (quantized and raw), Scalars, Anchors, and Recurrence. The ledger entry
at sequence 4 remains present. Repeated erase and invalid-id attempts make no
state change.

## Prior acceptance evidence

The scheduled verifier, live full/tail chain walk, and sealed-SST byte-flip
negative control are recorded in the issue comments and
`docs/fsv/issue-1679-scheduled-vault-verification-20260804.md`. This run closes
the remaining derived-record reproduction and lawful erase conjunction without
destroying production data.
