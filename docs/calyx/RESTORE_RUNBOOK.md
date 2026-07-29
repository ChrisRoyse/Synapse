# Calyx Vault Backup & Restore Runbook (#1687)

Operational durability for the Calyx-backed Synapse vault: how to take a
consistent online backup, how to restore it to a fresh data directory, and how
to prove the restored daemon is byte-faithful to the source. Includes the
encryption-at-rest decision (ADR note) for the vault.

The vault is the **single source of truth**. Operator decisions (routine state)
and the provenance ledger are irreplaceable — regenerable artifacts (search
indexes, kernels, calibrations) are not. This runbook backs up the sacred data
and rebuilds the regenerable artifacts after restore.

---

## 1. What a backup is (and is not)

`storage operation=backup` performs a **consistent online backup** of the live
vault. It is not a raw `cp -r` of an open database directory:

1. **WAL barrier** — the group-committer is synced so every accepted write is
   durable before the copy begins.
2. **Residency enforcement** — if the vault has a pinned dataset root
   (`residency.json`), an off-dataset target fails closed
   (`CALYX_RESIDENCY_VIOLATION`) unless the pin allows off-dataset placement.
3. **Consistent copy under the native-compaction guard** — the copy holds the
   same cross-process maintenance guard that compaction / GC / tombstone-purge
   serialize on, so no maintenance pass can delete or rewrite a file underneath
   the enumeration. Under that guard a single checkpoint flush advances the
   durable manifest; the captured `durable_seq` names the consistent point. SSTs
   are immutable (atomic create-new), so every copied file is complete.
4. **Verify-after-copy (fail closed)** — the freshly written copy is immediately
   re-derived with the read-only `verify_restore` verifier. A manifest is
   published **only** if the ledger chain verifies and the sacred rows read
   back. A backup that does not verify is refused (`SYNAPSE_CALYX_BACKUP_VERIFY_FAILED`).
5. **Vault identity captured** — the lineage journal is copied in as
   `vault_lineage.json` and hashed into the manifest. A backup that cannot name
   the vault it restores is refused (`SYNAPSE_CALYX_BACKUP_LINEAGE_MISSING`).

Backups run **off the MCP Tokio runtime** on the blocking pool under a single
admission permit; a concurrent backup fails closed (`STORAGE_BACKUP_IN_PROGRESS`).
The target must also resolve **outside** the vault directory, or the copy would
enumerate its own output (`CALYX_ASTER_BACKUP_TARGET_INSIDE_VAULT`); both paths
are canonicalised before comparison so `..` and symlinks cannot defeat the check.

### On-disk layout of a backup target

```
<target_dir>/
├── vault/                     # a standalone, restorable vault directory
│   ├── vault-identity.json    # vault identity (required to reopen)
│   ├── residency.json         # residency pin (if the source had one)
│   ├── CURRENT, MANIFEST, manifest-*.json
│   ├── cf/<CF_NAME>/*.sst      # per-column-family sorted tables (sacred rows)
│   ├── wal/*.wal              # write-ahead log segments
│   ├── locks/                 # present but EMPTY (lock tokens are not copied)
│   └── ledger_head/current.json, latest_checkpoint.json
├── vault_lineage.json         # sidecar copy of the vault lineage journal (§7)
└── backup_manifest.json       # per-file SHA-256 + durable_seq + verify report
                               # + lineage identity + excluded_runtime list
```

`backup_manifest.json` is `schema_version=2`: version 1 recorded only bytes,
version 2 also asserts the vault identity (`lineage`) and lists what was
deliberately not copied (`excluded_runtime`).

#### What is excluded, and why it is a named list

Unless `include_regenerable=true`, the rebuildable `ann/`, `kernel/`, `guard/`
directories are skipped. Beyond those, the vault directory is also the **runtime
root of the daemon serving it**, and that runtime state is excluded by an
explicit named list:

| Entry | Owner | Why |
| --- | --- | --- |
| `vault.lock`, `vault.pid` | Aster substrate | Block reopen / bind the copy to the source process |
| `daemon.lock`, `daemon.pid`, `daemon-lifecycle.lock` | Synapse daemon | Held with `LockFileEx` byte-range locks — **unreadable even by the locking process through a second handle** |
| `daemon-run-current.json`, `daemon-tool-last.json` | Synapse daemon | Describe the run that is live *now*, not the one that will open the restored copy |
| `daemon-tool-events.jsonl`, `daemon-exit.jsonl` (+ rotated `.1`…`.5`) | Synapse daemon | Host-scoped, rotated observability ledgers; not vault data, not covered by the ledger chain |
| `locks/` **contents** | Aster substrate | Byte-range lock tokens, zero state. The directory itself *is* recreated, because the lock guard does not create its parent |
| `.append.lock` (any depth, e.g. `wal/.append.lock`) | Aster substrate | Held around every WAL append, so a live writer can hold it for the instant the copy reads it — an intermittent failure, not a permanent one |

This is **not** a "skip whatever cannot be read" rule. Any other unreadable file
still fails the backup closed, so a genuinely damaged SST can never quietly drop
out of a backup. Every skipped entry appears in the manifest's
`excluded_runtime` array with a reason, so a restore operator can tell an
intentional omission from a gap.

Before this list existed, `storage operation=backup` **could not complete at all
against a live daemon**: the copy died on `daemon.lock` with
`CALYX_ASTER_BACKUP_IO … os error 33` (`ERROR_LOCK_VIOLATION`). Microsoft's
`LockFileEx` contract is explicit — "if the locking process opens the file a
second time, it cannot access the specified region through this second handle
until it unlocks the region", and "locking a region that goes beyond the current
end-of-file position is not an error", which is why even a 0-byte lock token
failed. The exclusion follows the same reasoning PostgreSQL gives for omitting
`postmaster.pid`/`postmaster.opts` from a base backup.

**`machine-salt.bin` lives in the parent data directory, outside `vault/`, and is
not part of the vault backup** (see §5).

---

## 2. Take a backup (MCP `tools/call`)

```json
{
  "name": "storage",
  "arguments": {
    "operation": "backup",
    "backup": {
      "target_dir": "D:/synapse-backups/2026-07-23T18-00Z",
      "include_regenerable": false
    }
  }
}
```

`backup` is **maintenance-gated**. Acquire a foreground input lease, switch to
`break_glass` (confirm + reason), run the backup, then restore `normal_agent`
and release the lease. `target_dir` must be an absolute path to a fresh, empty
directory.

The response (and `backup_manifest.json`) contains: `vault_id`, `durable_seq`,
`latest_seq`, `file_count`, `total_bytes`, `manifest_sha256`, the per-file
`sha256` list, and the green `verify` report (`chain_intact=true`,
`constellation_count>0`, `anchor_count>0`, `wal_bytes_present>0`,
`ledger_tip_hash`).

---

## 3. Restore to a fresh data directory

Restore is a deliberate, offline operation.

1. **Stop the daemon** cleanly (the source keeps running for a hot backup; the
   *restore target* daemon must be stopped so nothing holds the destination
   vault lock).
2. **Choose a fresh data dir**, e.g. `D:/synapse-restore/data`. The vault lives
   at `<data_dir>/vault`.
3. **Copy the backup vault into place**:
   ```pwsh
   Copy-Item -Recurse "D:/synapse-backups/2026-07-23T18-00Z/vault" `
                      "D:/synapse-restore/data/vault"
   ```
   Do **not** copy `backup_manifest.json` or `vault_lineage.json` into the vault
   (they are sidecars; the vault ignores unknown files, but keep them beside the
   data dir for audit). **Read §7 before choosing the destination directory** —
   restoring onto the existing production vault and restoring to a fresh
   directory have different, non-obvious lineage outcomes, and only one of them
   fails closed.
4. **Provide the machine salt** (see §5). On the **same machine**, copy
   `machine-salt.bin` from the source data dir into `<data_dir>/`. Encryption is
   off (§6), so the salt does not gate value decryption, but restoring the
   original salt keeps the vault identity self-consistent across hosts.
5. **Point the daemon at the restored data dir** and start it:
   ```pwsh
   $env:SYNAPSE_CALYX_VAULT_DIR = "D:/synapse-restore/data/vault"
   # start the repo-built synapse-mcp daemon
   ```
6. **Rebuild regenerable artifacts** (only if `include_regenerable=false`): run
   `storage operation=search_rebuild` to reconstruct the persisted search
   generation from the restored base rows.

---

## 4. Prove the restore (FSV checklist)

Verification proves `observed == expected` against the physical source of truth.

- **Read-only byte verification** — before or after opening the daemon, run:
  ```json
  { "name": "storage", "arguments": {
      "operation": "restore_verify",
      "restore_verify": { "vault_path": "D:/synapse-restore/data/vault" } } }
  ```
  Expect `success=true`, `chain_intact=true`, and the same `ledger_tip_hash`
  reported by the backup manifest.
- **Manifest byte-integrity** — re-hash each file under the restored `vault/`
  and compare to `backup_manifest.json`'s per-file `sha256`. Every sacred file
  must match.
- **CF row-count parity** — `storage operation=summary` on the restored daemon;
  `cf_row_counts` must equal the source daemon's counts (spot-check the sacred
  CFs: routines/decisions, action log, ledger).
- **Ledger chain** — `chain_intact=true` and matching `ledger_tip_hash` prove
  the provenance chain restored intact.
- **Spot-row readback** — pick a known `cx_id` / anchor from the source
  (`storage operation=anchors`) and read it back on the restored daemon; bytes
  and decoded fields must match.

**Acceptance (per #1687):** backup → wipe → restore on a scratch data dir yields
a daemon whose CF row counts, ledger `verify_chain`, and spot rows byte-match the
source.

---

## 5. Machine salt & cross-host restore

`machine-salt.bin` is stored in the **parent data directory**, not inside
`vault/`, so it is intentionally outside the vault backup. For a same-machine
restore the existing salt is already present. For a **cross-host** restore or a
truly clean data dir, copy `machine-salt.bin` from the source data directory
alongside the restored `vault/`. Because encryption-at-rest is currently
disabled (§6), a missing/mismatched salt does not make plaintext values
unreadable, but restoring the original salt preserves vault-identity
consistency. If encryption-at-rest is later enabled, the salt (and its KMS-held
key material) becomes **mandatory** for restore and must be backed up and
transported through a separate, access-controlled channel — never in the same
artifact as the ciphertext.

---

## 6. Encryption-at-rest decision (ADR note)

**Decision: DEFER application-level value encryption (value-crypto) for now;
rely on OS/volume full-disk encryption as the at-rest control. Revisit when the
vault leaves a single-operator, full-disk-encrypted host or when a regulatory
obligation requires field-level encryption.**

### Context

The Aster substrate already ships a value-crypto path: `seal_value` / `open_value`
seal per-CF SST values, and `verify_restore_with_value_crypto` verifies an
encrypted restored vault. Synapse currently opens the vault with
`VaultOptions { value_crypto: None }` — SST values are **plaintext**. The vault
holds sensitive operator-activity data (clipboard, window titles, transcripts).

### What the research says (2025 best practice)

- **Layered encryption** is the recommended posture: application/database-level
  SST encryption **plus** OS/volume encryption, with a dedicated KMS and key
  rotation.
- **OS/volume (full-disk) encryption** protects against raw-media theft, lost
  drives, and offline access with the least performance and operational
  overhead, but leaves data in cleartext to the running application, DB, and
  filesystem layers — it does not defend against a compromised host process.
- **Application-level (value/SST) encryption** additionally protects the on-disk
  bytes from filesystem-level and some insider access, at the cost of key
  management complexity (a lost key = unrecoverable vault) and per-value
  crypto overhead.

Sources:
- [Database Encryption 2025: Protect Data At Rest — onlinehashcrack](https://www.onlinehashcrack.com/guides/best-practices/database-encryption-2025-protect-data-at-rest.php)
- [Protecting database data at rest: TDE, Backup Encryption or Always Encrypted — Andreas Wolter](https://andreas-wolter.com/en/protecting-database-data-at-rest-tde-backupencryption-alwaysencrypted/)
- [Storage-level vs. application-level encryption — Quora](https://www.quora.com/What-is-the-storage-level-encryption-vs-application-level-encryption)
- [Cybersecurity in Embedded Systems Best Practices 2025 — Cranes Varsity](https://cranesvarsity.com/cybersecurity-in-embedded-systems-best-practices-for-2025/)

### Rationale for deferring

1. **Threat model.** Synapse runs as a single-operator local daemon on the
   operator's own machine. The dominant at-rest threat (device theft / lost
   disk) is already covered by OS full-disk encryption (BitLocker / FileVault /
   LUKS), which is the operator's responsibility to enable and is the standard
   posture for a personal workstation.
2. **Key-management risk outweighs benefit here.** Enabling value-crypto binds
   vault readability to a machine-salt-derived key. Without a KMS and a
   disciplined backup of that key material, a lost/rotated key makes **backups
   unrestorable** — directly undermining the durability this issue delivers. A
   half-implemented encryption story is worse than none for a durability
   feature.
3. **Substrate is ready; the switch is reversible.** `value_crypto` is a
   construction-time `VaultOptions` field and `verify_restore_with_value_crypto`
   already exists, so enabling it later is a localized, testable change (new
   vault generation + migration), not a redesign.

### If/when we enable it (explicit criteria)

Enable value-crypto for the sensitive collections (clipboard, titles,
transcripts) when **any** of: the vault is hosted off the operator's
full-disk-encrypted machine; a shared/multi-tenant host is introduced; or a
compliance obligation mandates field-level encryption. At that point:

- Derive/hold the key in a KMS (not only the machine salt); rotate on a schedule.
- Back up key material through a **separate** access-controlled channel — never
  in the same artifact as the backup ciphertext.
- Switch backups/verification to the `*_with_value_crypto` verifier path and add
  a key-availability precondition to restore.

Until then, this decision is recorded as a deliberate, revisitable deferral —
**not** a silent omission.

---

## 7. The vault lineage journal on restore (#1875)

This section postdates the rest of this runbook. Every claim below was checked
against `crates/synapse-calyx/src/lineage.rs` rather than inferred.

`<vault-dir-name>.lineage.json` is a **sibling** of the vault directory, never a
file inside it — on the production host,
`%LOCALAPPDATA%\synapse\db-daemon.lineage.json` beside
`%LOCALAPPDATA%\synapse\db-daemon\`. That placement is the whole point: it is the
only witness of vault identity that survives `Remove-Item -Recurse -Force` on the
vault. It records, per generation, the vault id and the highest durable sequence
ever observed (`high_water_seq`).

Because the journal lives outside the vault, **restoring a vault directory does
not restore its lineage**, and the journal you already have on the host wins.
That produces two materially different outcomes.

### 7.1 Restoring ONTO the existing production vault directory → fails closed

The backup carries the same `vault_id` as the journal's active generation, but a
`latest_seq` at or below the moment the backup was taken, while the journal
records the high-water mark the *live* vault reached. Same id, fewer sequences,
so the next open refuses:

```
SYNAPSE_CALYX_VAULT_SEQ_REGRESSION
vault <vault-id> at <vault-dir> opened at latest_seq=<N> but the lineage journal
<journal-path> records high_water_seq=<M> for this same vault id: <M-N>
sequences are missing
```

This is correct and desirable: an in-place restore *is* a rollback of committed
rows, and the daemon refuses to serve it until an operator says so. Follow §7.3.

If the backup came from a **different** vault, the id differs instead and you get
`SYNAPSE_CALYX_VAULT_RESET_UNACKNOWLEDGED` — same procedure, but the id to
acknowledge is the newly observed one.

### 7.2 Restoring to a FRESH directory → starts a new lineage, does not fail

A fresh vault directory has no sibling journal, so the first open **seeds one**
and proceeds. It does not fail, and no operator acknowledgement is asked for.
It is not literally silent — a warn record is emitted:

```
SYNAPSE_CALYX_VAULT_LINEAGE_SEEDED  (level=warn)
seeded the vault lineage journal; nothing before this open is attested by it
```

— but nothing blocks, and it is easy to miss in a log. The seeded journal holds
`generation=1`, `started_reason="lineage-seeded"`, and `high_water_seq` equal to
whatever the restored copy happened to contain. From then on `verify_chain`
reports `chain_origin="lineage-seeded"` and `covers_full_history=false`: the
chain is intact, but the journal attests nothing before this open, so the vault
cannot claim its full history.

Practical note: `covers_full_history` is `true` only for
`chain_origin="vault-genesis"`. Since #1884 that origin is reachable and is
recorded at the vault's own creation: when a vault open both mints
`vault-identity.json` (nothing existed before it) **and** finds `latest_seq=0`,
the journal is written with `started_reason="vault-genesis"` and
`SYNAPSE_CALYX_VAULT_LINEAGE_GENESIS_RECORDED` is logged at info. A vault
created that way reports `covers_full_history=true` for its whole life, until an
acknowledged reset flips it to `post-reset`/`false`.

`chain_origin="lineage-seeded"` now means what it says: the journal was attached
to a vault that already existed — a restored copy, or any vault predating the
journal — so an unattested prefix genuinely precedes it and
`covers_full_history=false` is a true statement about that vault, not a
placeholder. Every vault created before #1884 is permanently `lineage-seeded`;
that is correct, because the daemon never witnessed its genesis.

(Before #1884 no code path wrote `vault-genesis` at all, so the field was a
constant `false` on every vault — including brand-new ones whose chain genuinely
did cover their whole history. `hygiene operation=vault_verify` excludes it from
its green predicate as a workaround for that bug; with the field now reporting a
real property, that exclusion can be removed — see §8.)

If you want the restored vault to be recognised as a *continuation* rather than a
new lineage, copy `vault_lineage.json` from the backup target to
`<parent-of-restored-vault>\<restored-vault-dir-name>.lineage.json` **before the
first open**, and expect §7.1's regression check to then apply. The journal
records the vault directory it describes and refuses to be applied to another
one (`SYNAPSE_CALYX_VAULT_LINEAGE_PATH_MISMATCH`), so the file name must match
the destination directory name.

### 7.3 Acknowledging the reset (`SYNAPSE_CALYX_ACKNOWLEDGE_VAULT_RESET`)

The acknowledgement is deliberately **not** a blanket switch: its value must be
the **exact vault id echoed from the error text**, compared after trimming. A
wrong, stale, or unrelated id is refused and the open still fails.

1. Read the vault id out of the error. For a regression it is the id
   immediately after `vault ` (`vault <vault-id> at <vault-dir> opened at
   latest_seq=…`). For an unacknowledged reset it is the **new** id
   (`now holds vault_id=<vault-id>`), not the recorded predecessor.
2. Confirm the loss is understood and a better recovery source does not exist.
   This step is the point of the whole mechanism; #1875 was a vault emptied with
   no record and no backup.
3. Set the variable for the **one** reopen and start the daemon:
   ```pwsh
   $env:SYNAPSE_CALYX_ACKNOWLEDGE_VAULT_RESET = "<vault-id-echoed-from-the-error>"
   # start the daemon, confirm it opens
   Remove-Item Env:\SYNAPSE_CALYX_ACKNOWLEDGE_VAULT_RESET
   ```
4. Confirm what was recorded, and clear the variable. The journal is rewritten on
   that open, so leaving the variable set would pre-authorise the *next*
   replacement too:
   - **Sequence regression** — the active generation's `high_water_seq` is
     lowered to the restored value and annotated
     (`operator-acknowledged sequence regression from high_water_seq=… to
     latest_seq=…`). No new generation is created; `chain_origin` is unchanged.
   - **Vault replacement** — a **new generation** is appended with
     `started_reason="reset-acknowledged"`, carrying `predecessor_vault_id` and
     `predecessor_high_water_seq`. `SYNAPSE_CALYX_VAULT_RESET_RECORDED` is
     logged, and `verify_chain` reports `chain_origin="post-reset"` with
     `covers_full_history=false` from then on.

Never delete the journal to clear one of these errors. Deleting it converts a
fail-closed, fully described loss back into the silent one that cost ~1.56M
sequences.

---

## 8. Scheduled verification (`hygiene operation=vault_verify`)

Backups prove themselves at write time; the *live* vault needs a recurring check
that it still verifies. `hygiene operation=vault_verify` runs the same two
verifiers this runbook uses for restores — `verify_restore` over the live vault
and the provenance hash-chain verifier — under the vault maintenance guard that
backup, erase, and compaction already share, so a scan can never race a tree
rewrite and misreport the torn intermediate state as corruption. A concurrent
backup/erase therefore makes it fail closed rather than lie.

```json
{ "name": "hygiene", "arguments": {
    "operation": "vault_verify",
    "vault_verify": { "full_chain": false, "tail_entries": 4096 } } }
```

- **Incremental by default.** The chain scan re-hashes the newest
  `tail_entries` ledger entries (default 4096), not the whole Ledger CF. A
  routine check expensive enough to be skipped is a check that does not exist;
  this is the posture restic recommends with `check --read-data-subset`.
  Note the raw-write commitment seals are always re-derived in full — that scan
  is bounded by the commitment CF, not by the ledger height.
- **Full scan on request.** `full_chain: true` re-hashes the entire chain. Run it
  after a restore, after any acknowledged reset, and on a slower cadence.
- **Loud on failure.** Any non-green surface — restore verifier, chain verdict,
  raw-write commitments, or a missing lineage journal — fails with
  `SYNAPSE_HYGIENE_VAULT_VERIFY_FAILED` naming exactly which one and the verified
  window. There is no "mostly fine" verdict.
- **`covers_full_history` is reported, not alarmed on** (§7.2).
