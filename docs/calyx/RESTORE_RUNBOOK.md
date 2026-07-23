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

Backups run **off the MCP Tokio runtime** on the blocking pool under a single
admission permit; a concurrent backup fails closed (`STORAGE_BACKUP_IN_PROGRESS`).

### On-disk layout of a backup target

```
<target_dir>/
├── vault/                     # a standalone, restorable vault directory
│   ├── vault-identity.json    # vault identity (required to reopen)
│   ├── residency.json         # residency pin (if the source had one)
│   ├── CURRENT, MANIFEST, manifest-*.json
│   ├── cf/<CF_NAME>/*.sst      # per-column-family sorted tables (sacred rows)
│   ├── wal/*.wal              # write-ahead log segments
│   └── ledger_head/current.json, latest_checkpoint.json
└── backup_manifest.json       # per-file SHA-256 + durable_seq + verify report
```

Excluded from the copy: `vault.lock` and `vault.pid` (would block reopen), and —
unless `include_regenerable=true` — the rebuildable `ann/`, `kernel/`, `guard/`
directories. **`machine-salt.bin` lives in the parent data directory, outside
`vault/`, and is not part of the vault backup** (see §5).

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
   Do **not** copy `backup_manifest.json` into the vault (it is a sidecar; the
   vault ignores unknown files, but keep it beside the data dir for audit).
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
