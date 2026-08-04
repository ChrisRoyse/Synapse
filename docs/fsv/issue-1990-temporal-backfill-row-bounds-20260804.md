# Issue #1990 - temporal backfill row bounds fail closed

Date: 2026-08-04

## Source of truth

The source of truth is `%LOCALAPPDATA%\synapse\db-daemon\CURRENT` and the exact
manifest it names. If validation occurs before storage, both hashes remain
unchanged.

## Root cause and fix

`StorageTemporalBackfillParams.max_rows` published JSON Schema range 1..=1000,
but schemars metadata does not validate deserialized Rust values. The runtime
passed zero to the storage backend, which returned a zero-row success and
advanced durable sequence state. Commit `b09b53e9` validates the exact range at
the typed facade boundary before decoding cursors or calling storage, returning
`TOOL_PARAMS_INVALID` with the accepted range and remediation.

The audit found other storage numeric ranges whose enforcement is not uniform;
that larger inventory and correction is tracked as #1991 rather than silently
expanded here.

## Full State Verification

Before the invalid triggers:

- live daemon PID: 10560
- installed executable SHA-256:
  `03F93B1A3D5601CA25B55B0DA104B61676CC6550FF5945FD389BE53ADE382FAB`
- `CURRENT`: `manifest-00000000000000030837.json`
- `CURRENT` SHA-256:
  `8908D2F7511A8B363497FE019F903D092148D8378808D37202677423BE0D5D08`
- manifest SHA-256:
  `BD25F2A980EC5D5F28C95403A32580D56F081D01E774DD8F4808C720A91A76C5`

Triggers and independent after-state:

1. `max_rows=0` returned `TOOL_PARAMS_INVALID` and stated no storage operation
   was attempted.
2. `max_rows=1001` returned the same structured boundary error.
3. After both, `CURRENT`, its SHA-256, and the named manifest SHA-256 were all
   byte-identical to the before state above.

The two accepted edges were then exercised against the real `CF_ACTION_LOG`:

- `max_rows=1`: accepted, bounded one physical candidate, `more=true`
- `max_rows=1000`: accepted, examined all 110 rows,
  `already_current_rows=110`, `inserted_rows=0`, `more=false`

No mock rows or test double were used.
