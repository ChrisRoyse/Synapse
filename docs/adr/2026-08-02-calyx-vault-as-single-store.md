# ADR: The Calyx vault is Synapse's single store

## Status

Accepted — 2026-08-02. Recorded retrospectively: the migration had already
shipped and been verified, but #1683 noted that no ADR captured the decision, so
a reader had to reconstruct it from the tree.

## Context

Synapse originally persisted through RocksDB. It now persists through a Calyx
Aster vault, and RocksDB is gone from the tree entirely — `librocksdb-sys`
appears in no `Cargo.toml` and no `Cargo.lock` entry, and neither does `bindgen`
or `clang-sys`.

The two are not interchangeable, which is why this was a decision rather than a
port. RocksDB stores opaque bytes under keys. The vault stores *constellations*:
typed records whose slots are separately addressable measured lenses, with
grounded anchors, hash-chained provenance, and MVCC snapshots. The capabilities
Synapse builds on — bits about outcomes, redundancy and effective rank, kernel
distillation, conformal guards, provenance verification — are expressible only
because the substrate keeps that structure. Reducing it to a byte store would
have kept the storage and discarded the reason for it, as
`2026-07-23-calyx-intelligence-integration-doctrine.md` sets out.

## Decision

The Calyx Aster vault is the single durable store. There is no second backend
and no fallback path.

**Location and identity.** The vault lives at the daemon's `--db` path
(`%LOCALAPPDATA%\synapse\db-daemon` on this host). Its identity is
`vault-identity.json`; the key material it derives from is the **sibling**
`machine-salt.bin`, one level up, not inside the vault directory. A vault copied
without that sibling is unopenable, which is a deliberate property and a
recurring trap when moving vaults between paths.

**Key mapping.** Rows are addressed by `(ColumnFamily, key)`. The CF set is
declared in code (`calyx_aster::cf::ColumnFamily`) rather than by string
convention, so an unknown CF is a compile error rather than a silently created
namespace. Physical layout under the vault root:

| path | holds |
|---|---|
| `cf/<name>/*.sst` | per-CF sorted string tables; `flush-*.sst` are memtable flushes |
| `wal/` | write-ahead log; fsynced before the MVCC apply |
| `ledger_head/current.json` | head anchor for the provenance chain, a 4096-byte CRC record with the `CLXLHEAD` magic |
| `idx/search/panel_*/manifest.json` | derived search generations, rebuildable |
| `codebooks/`, `panel/`, `registry/`, `lowered/` | panel and lens metadata |

`cf/`, `wal/` and `ledger_head/` are authoritative. `idx/` is derived and may be
rebuilt without data loss; that distinction is what makes
`storage operation=search_rebuild` safe to run and a backup of `idx/` optional.

**Commit ordering.** A durable group commit runs `admission → sidecar → wal →
anchor_publish → mvcc → checkpoint_stage`. The WAL fsync completes before the
MVCC apply, so recovery never depends on the in-memory apply having finished.

**Provenance.** Every storage mutation appends to an append-only hash chain in
`CF_LEDGER`, with `raw_commitment` rows sealed into Merkle cohorts.

## Migration evidence

Verified against the live daemon on 2026-08-02 (pid 20700, binary sha256
`269054DA…`), read from the physical source of truth rather than from return
values:

- `audit verify_chain` — `verdict: intact`, **124,011** entries re-walked and
  re-hashed from genesis to head, 129,489 raw commitments verified against
  16,260 Merkle cohort seals, tip hash `9f56624f…`. It reports its own coverage
  honestly (`covers_full_history: false`, `chain_origin: "lineage-seeded"`)
  because the chain was seeded over a pre-existing vault rather than written
  from genesis.
- `health` — `calyx_panel_base_cf_rows: 96,845`, `calyx_vault_latest_seq:
  249,900`, `calyx_vault_open: true`, `storage_backend: "calyx"`.
- Reopen readback — manual FSV closed a vault and reopened it through the
  production storage surface, then separately read every key written across its
  phases: 1,180 present, 0 lost, 0 stale, `latest_seq` identical across the
  reopen. The former automated driver was removed by D1 policy; this row records
  historical evidence, not a supported invocation path.

## Consequences

**Gained.** Typed slots, grounded anchors, MVCC snapshots with leases,
tamper-evident provenance, and the intelligence surface built on them.

**Given up.** RocksDB's operational maturity and its large body of external
tuning knowledge. Tuning is now this project's own problem, and the perf work in
#1948/#1949/#1950 is what that costs: a synchronous SST write under both global
write locks (652 ms), twelve column families sharing one memtable cap so they
sealed on the same commit, and a vault-wide row-table `RwLock` whose read side
can be held for over a second by one unbounded scan. None of these are Calyx
design faults; they are the tuning an engine accumulates and that RocksDB had
already paid for.

**Load-bearing consequence.** Because there is no second backend, a vault that
will not open is a stopped daemon, not a degraded one. Recovery paths must fail
closed and say exactly what they could not read —
`CALYX_ASTER_ROUTER_ONLY_ROWS`, `CALYX_ASTER_CORRUPT_SHARD` and the
projection-unreadable codes exist for that reason, and none of them should ever
be softened into a fallback.

## Notes for a reader inspecting the vault

- `audit verify_chain` walks the provenance chain; `storage operation=inspect` /
  `summary` report CF state; `storage operation=restore_verify` checks a vault
  directory read-only.
- `storage operation=backup` writes a **consistent** self-verifying copy with a
  per-file SHA-256 manifest. A file-level copy of a live vault is a torn
  snapshot: the daemon is writing while it runs, and such a copy has been
  observed to open with an empty chain (`head_height = 0`) rather than failing
  outright. Use the backup operation for any offline verification.
- An offline read-only open needs both `restore_mvcc_rows` and
  `restore_ledger_hook` enabled. With either off, the chain reads as empty and
  verifies *vacuously* — `Intact { count: 0 }` is a pass over nothing.
