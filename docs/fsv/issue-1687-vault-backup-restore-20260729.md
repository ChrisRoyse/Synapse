# FSV — issue #1687: vault backup, verify_restore, restore runbook

Date: 2026-07-29 (UTC) · Host: configured Windows 11 host · Daemon: live, pid 18628, deployed from `36890419`

Instrument: `scripts/diagnostics/issue-1687-vault-backup-restore-fsv.ps1`. It drives the
**real** `storage operation=backup` through the **live daemon** over MCP-on-HTTP, against the
**live production vault** as source. Backup is read-only on its source, so production is a
safe subject; every mutation lands under a scratch directory. Nothing was restored onto
`%LOCALAPPDATA%\synapse\db-daemon`.

## Why this was never accepted before

The code shipped 2026-07-23 (`db6c590f`). Running it for the first time against a live
daemon revealed it **had never worked in the only configuration that matters**, in two
independent ways.

**Layer 1 — byte-range locks (fixed in `7fe53725`).**

```
CALYX_ASTER_BACKUP_IO: read vault file …\db-daemon\daemon.lock:
The process cannot access the file because another process has locked
a portion of the file. (os error 33)
```

`os error 33` is `ERROR_LOCK_VIOLATION`. The exclusion list covered `vault.lock`/`vault.pid`
but not the daemon's own lock files, which live in the same directory. The full live
inventory turned out to be wider than the error suggested: `daemon.lock`,
`daemon-lifecycle.lock`, `daemon.pid`, the five tokens under `locks/` (one of which,
`native.compaction.lock`, the backup itself holds), and **`wal/.append.lock`, which is
taken and released around every WAL append**. Excluding only the top-level names would have
turned a reliable failure into an intermittent one — strictly worse.

**Layer 2 — manifest rotation race (fixed in `36890419`).**

With the locks excluded, the backup then failed:

```
CALYX_ASTER_BACKUP_IO: stat vault entry …\manifest-00000000000000004544.json:
The system cannot find the file specified. (os error 2)
```

Measured on the live vault: it keeps a **rolling window of exactly 32 manifests and rotates
one every ~12 seconds**, always deleting the oldest. A directory-listing-then-copy backup of
a live vault therefore does not have a race it might lose — it has one it will **always**
lose, on a 12-second clock.

The fix is manifest **pinning**, not guard extension. Rotation serialises on
`locks/manifest.publish.lock`, a different lock from the backup's
`locks/native.compaction.lock`; `MANIFEST_GENERATIONS_RETAINED = 32` matches the measurement
exactly. Extending the compaction guard would either fail closed or stall every durable
commit for the whole backup — and `backup_consistent` drives checkpoints in its own
preflight. This follows RocksDB (`GetLiveFilesStorageInfo` + copied `CURRENT`/`MANIFEST`
bytes; `DisableFileDeletions` explicitly lets compaction continue) and PostgreSQL
`basebackup.c`, which tolerates a vanished file only with a named justification and never
for `pg_control`.

Pinning also closed a latent torn-state bug: `CURRENT`, `MANIFEST` and `manifest-<seq>` are
all rewritten every publication, so copying them independently could leave `CURRENT` naming
one generation while `MANIFEST` mirrored the next.

## Manual FSV — all checks passed

Live vault at run time: 1,272 cf files, `latest_seq=49396`, `vault_id=01KYJPGWATPD4XNMZY3ERGTKQW`.

**Precondition, asserted first so a pass cannot be vacuous.** The instrument probes the lock
files before doing anything and requires them to be genuinely locked:
`daemon.lock=LOCKED(-2146233087)`, `vault.lock=LOCKED(-2146233087)`. A green run cannot mean
"the daemon happened not to be holding them".

| Property | Evidence |
|---|---|
| backup succeeds against a **live** daemon | passed (previously `CALYX_ASTER_BACKUP_IO`, twice) |
| every manifested file present | 1,365 / 1,365 |
| every SHA-256 independently reproduces | 0 mismatches under `Get-FileHash` |
| **the hash check was not vacuous** | 1,365 entries actually hashed — an earlier harness bug `continue`d past all of them and reported 0 mismatches having verified nothing |
| `file_count` agrees with its own array | 1,365 |
| runtime locks not copied **and** not manifested | `daemon.lock`, `daemon-lifecycle.lock`, `vault.lock`, `vault.pid`, `daemon.pid` |
| `wal/.append.lock` not manifested as data | passed, with a recorded reason |
| exclusions recorded, not silent | 11 entries, each with a justification |
| manifest generation pinned | `manifest-00000000000000004715.json`, `manifest_seq=4715`, `durable_seq=49396` |
| retention window matches measurement | `generations_retained=32` |
| `CURRENT` in the copy names the pinned generation | exact match |
| every referenced path copied (mandatory) | 3 / 3, 0 missing |
| tolerated absences each justified | 0 this run — a positive assertion nothing vanished |
| lineage journal captured | `vault_id=01KYJPGWATPD4XNMZY3ERGTKQW`, `high_water_seq=49163` |

**`restore_verify` on the backup copy (read-only):**

```
success=true  chain_intact=true  failure_reasons=[]
constellations=11323  anchors=278  ledger_entries=16058
ledger_tip_hash=047bf6c916d48eed1474cc2680d656628b1c8d900f8391995fd1039a0ccd9123
```

**Edge — containment guard.** A backup target inside the vault is refused with
`CALYX_ASTER_BACKUP_TARGET_INSIDE_VAULT`, and leaves nothing behind on disk.

## Note on `covers_full_history`

`hygiene operation=vault_verify` deliberately excludes `covers_full_history` from its green
predicate. That field is permanently `false` on every vault because no code path writes
`chain_origin="vault-genesis"` — filed as **#1884**. The exclusion is a workaround for that
bug and should be removed when it is fixed.

## Research lane

Exa remains `EXA_API_CREDITS_EXHAUSTED_402` (#1864) — not re-diagnosed, per AGENTS.md.
Built-in web lane; primary sources read: RocksDB `DisableFileDeletions` /
`GetLiveFilesStorageInfo` documentation, and PostgreSQL `src/backend/backup/basebackup.c`.
