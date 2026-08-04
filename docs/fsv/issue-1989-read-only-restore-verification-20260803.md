# Issue #1989 - restore verification has zero write side effects

Date: 2026-08-03

## Source of truth

The source of truth is the complete recursive filesystem inventory of the real
backup at:

`%LOCALAPPDATA%\synapse\fsv\issue-1987-marker-20260803T2332Z`

Each inventory row contains relative path, byte length, and SHA-256. Equality of
the digest before and after `storage restore_verify` proves that verification did
not create, remove, truncate, or alter a file.

## Root cause and fix

The backup manifest declared 1455 vault files. The first independent read found
1456. The undeclared file was `vault/wal/.append.lock`, length zero, with the
SHA-256 of an empty file. `verify_restore` called the ordinary WAL replay path;
that path acquired the append lock, opened WAL segments writable, and could
truncate a torn tail. The function therefore violated its zero-write contract
and also retained the handle that prevented #1987's Windows directory rename.

Commit `7061b6e8` added `wal::replay_dir_read_only` and routed restore
verification through it. This path acquires no append lock, opens segments
without write permission, and returns a structured error if a torn tail would
need repair. It never repairs verification input.

## Full State Verification

The old verifier-created lock was removed once to restore the artifact to its
manifest-declared state. Before the fixed trigger:

- recursive files: `1457` (1455 vault files plus manifest and lineage)
- `.append.lock`: absent
- `backup_in_progress.json`: absent
- `backup_manifest.json`: present
- path/length/SHA-256 inventory digest:
  `50C74FDA5DF1124A549D98243A5F41058236236F6D75817674BCAFD1DAEFFC12`

Trigger: the deployed live daemon ran `storage restore_verify` against the
backup's physical `vault` directory. It reopened successfully with 234935
constellations, 50558 anchors, 296569 ledger entries, and ledger tip
`11cf632486b5a0521ce1ed9cde1440df3eb297f9a647369a6c0f10533728b047`.

After the trigger, a new independent recursive inventory had the same 1457 file
count and the exact same digest
`50C74FDA5DF1124A549D98243A5F41058236236F6D75817674BCAFD1DAEFFC12`.
`.append.lock` remained absent. The return value supports the result; the
unchanged physical inventory is the acceptance evidence.

The #1987 boundary audit additionally proved that rejecting an in-progress
artifact leaves its marker byte-identical, and that rejecting malformed params
creates no filesystem entry.
