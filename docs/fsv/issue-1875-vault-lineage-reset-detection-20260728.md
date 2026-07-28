# FSV — #1875: a replaced vault can no longer open silently

Date: 2026-07-28
Host: CABTOP (Windows 11 Pro 10.0.26200)
Commit: `f801b386`
Research lane: built-in web search (Exa remains `EXA_API_CREDITS_EXHAUSTED_402`
per AGENTS.md, not re-diagnosed). Primary analogues read: PostgreSQL's
`pg_control` `system_identifier` and the "database system identifier differs"
failure used by streaming replication / pgBackRest / Patroni, and Kafka's
`meta.properties` `cluster.id` mismatch, on which a broker **refuses to start**.
Both keep the system identity outside the volatile data and treat a replaced
substrate as fail-closed, never as a fresh start.

## Cause, established

The production vault was emptied and recreated on 2026-07-27 and nothing
recorded it. The reason nothing recorded it is structural, not incidental:

Every witness to the vault's existence — `vault-identity.json`, the manifest,
the ledger, the CF directories — lived **inside the directory being deleted**.
After `Remove-Item -Recurse -Force <vault>` there was nothing left to compare
against, so the next open was an ordinary, correct, successful open of an empty
directory. `latest_seq=0` carried no information because nothing knew it should
have been 1,580,914.

The second half of the cause is a **classification defect** that pointed at the
deletion in the first place. A vault holding a durable column family this build
does not know about was reported as:

```
CALYX_ASTER_CORRUPT_SHARD  ("base shard hash mismatch")
remediation: restore from restic/snapshot
```

That is a version mismatch, not damage: the bytes are intact and the build that
created the CF reads them normally. With no backup in existence, "restore from
snapshot" leaves an operator exactly one apparent move — delete the vault. It is
now a distinct code that says the opposite:

```
CALYX_ASTER_VAULT_SCHEMA_AHEAD
remediation: the vault is intact and newer than this build: do NOT delete,
             purge, or clear it. Run the build that created the named column
             family, or upgrade this build to one that registers it
```

(The registry-duplication defect that produced the original strand — a CF created
by one commit and rejected by the next router open — was already fixed in #1804's
`parse_cf_dir_name`. This fixes what the operator is told when it happens again
for any other reason.)

## The fix

A **vault lineage journal** kept outside the blast radius:
`<parent>/<vault-dir-name>.lineage.json`, a sibling of the vault directory.
Per generation it records the vault id, first/last observed time, and the highest
durable seq ever observed. Every open compares the vault that is physically
present against it and fails closed on a replacement or a sequence regression.
Clean close records the closing high-water mark, so the journal knows how much
was there even if the directory is deleted before the next open.

`synapse-setup.ps1 -Purge` now writes a deletion record beside the vault
directory before deleting, and refuses to delete a populated vault without
`-ConfirmVaultDestruction`.

## Verification — disposable vault, real code

All vault work below is against a disposable vault under the session scratchpad,
driven by `crates/synapse-storage/examples/provenance_tamper_fsv.rs` running the
real `SynapseCalyxVault`. The production vault was never used as a test subject.

### Happy path — journal is seeded and lives outside the vault

12 real batches, then close.

```
SEED_AFTER latest_seq=13
SEED_CLOSED latest_seq=Some(13)
```

Scratch root listing — SoT:

```
db-fsv/                 (the vault)
db-fsv.lineage.json     445 bytes   <- SIBLING, not inside
machine-salt.bin
```

`db-fsv.lineage.json`:

```json
{ "schema_version": 1,
  "generations": [ { "generation": 1,
      "vault_id": "01KYN9KD9JTHXQK70HQMAXDHAG",
      "started_reason": "lineage-seeded",
      "high_water_seq": 13 } ] }
```

`vault-identity.json` inside the vault carries the same id — the journal is not
inventing one.

### The 2026-07-27 event, reproduced exactly

```
Remove-Item -Recurse -Force <vault>
vault_dir_exists_after_delete=False
lineage_journal_survived=True
```

Reopening the now-empty directory — the step that used to be silent:

```
SYNAPSE_CALYX_VAULT_RESET_UNACKNOWLEDGED
  vault directory …\db-fsv now holds vault_id=01KYN9KPGGQ34NKHN1YMRHD6GA
  (latest_seq=0) but the lineage journal …\db-fsv.lineage.json records
  vault_id=01KYN9KD9JTHXQK70HQMAXDHAG at generation 1 with high_water_seq=13;
  approximately 13 durable sequences are unaccounted for
```

Exit code 1. The daemon does not come up on a substrate it cannot account for.

### Edge 1 — acknowledge with the wrong id (the OLD vault id)

`SYNAPSE_CALYX_ACKNOWLEDGE_VAULT_RESET=01KYN9KD9JTHXQK70HQMAXDHAG` →
still `SYNAPSE_CALYX_VAULT_RESET_UNACKNOWLEDGED`, exit 1.

### Edge 2 — acknowledge with an unrelated but well-formed id

`SYNAPSE_CALYX_ACKNOWLEDGE_VAULT_RESET=01AAAAAAAAAAAAAAAAAAAAAAAA` →
still `SYNAPSE_CALYX_VAULT_RESET_UNACKNOWLEDGED`, exit 1.

The acknowledgement must name the exact new vault id, so it can never become a
blanket "ignore all resets" switch.

### Acknowledged reset — recorded, not just permitted

With the exact new id:

```
LINEAGE {"generation":2,"reset_count":1,"chain_origin":"post-reset",
         "predecessor_vault_id":"01KYN9KD9JTHXQK70HQMAXDHAG",
         "predecessor_high_water_seq":13,"seeded_this_open":false}
```

Journal on disk now holds both generations, generation 2 carrying
`started_reason: "reset-acknowledged"` and the predecessor's identity and
high-water mark. The reset is durable provenance, not a one-time log line.

### Ask 3 — `verify_chain` reports the chain's true origin

Seeded 8 batches into the post-reset vault:

```json
{"intact":true,"verdict":"intact",
 "raw_commitment_coverage_from_seq":1,
 "covers_full_history":false,
 "chain_origin":"post-reset",
 "vault_generation":2,
 "vault_reset_count":1,
 "predecessor_vault_id":"01KYN9KD9JTHXQK70HQMAXDHAG",
 "predecessor_high_water_seq":13}
```

This is the exact qualification #1875 asked for. `coverage_from_seq=1` is still
true, and it no longer reads as full historical coverage: the same response says
the chain begins after a recorded replacement of a vault that had reached 13.

### Edge 3 — same identity, fewer sequences (a rollback)

A vault-id comparison alone would miss this entirely. The vault was snapshotted
at `latest_seq=9`, advanced to 18, then rolled back to the snapshot — same
`vault_id`, less history:

```
SYNAPSE_CALYX_VAULT_SEQ_REGRESSION
  vault 01KYN9KPGGQ34NKHN1YMRHD6GA at …\db-fsv opened at latest_seq=9 but the
  lineage journal records high_water_seq=18 for this same vault id:
  9 sequences are missing
```

Exit code 1.

### `-Purge` guard — the shipped setup functions, extracted verbatim

`Get-SynapseCalyxPhysicalSnapshot` and `Write-SynapseVaultDeletionRecord` were
extracted from `scripts\synapse-setup.ps1` by AST and run against the disposable
vault, so the code under test is the shipped code.

Populated vault, no `-ConfirmVaultDestruction`:

```
SYNAPSE_SETUP_VAULT_PURGE_REFUSED vault_dir=…\db-fsv
  vault_id=01KYN9F1E19NCQATFGYKVF05EH durable_seq=7 manifest_files=1
  cf_files=8 cf_bytes=3756 wal_segments=1
  remediation=this vault holds durable captured history and there is no
              automatic backup (issue #1687). Take a backup first, then rerun
              with -ConfirmVaultDestruction…
vault_still_present=True
deletion_records_after=0
```

With `-ConfirmVaultDestruction`, a record is written *before* anything is
deleted, and it outlives the deletion:

```
db-fsv.deleted-20260728T212202Z.json
{ "schema":"synapse_vault_deletion_record/v1",
  "reason":"fsv-1875-confirmed","confirmed":true,
  "vault_id":"01KYN9F1E19NCQATFGYKVF05EH",
  "deleted_by_pid":15724,"deleted_by_user":"CABTOP\\hotra",
  "physical_snapshot":{ "current_manifest_durable_seq":7,
    "manifest_file_count":1,"cf_file_count":8,"cf_bytes":3756,
    "wal_segment_count":1, … } }

vault_present=False   record_present=True
```

Absent vault: a no-op with an explicit line, not an error.

## Result

| Path | Expected | Observed |
| ---- | -------- | -------- |
| first open | journal seeded beside the vault | 445-byte sibling, gen 1, high_water 13 |
| vault deleted + reopened | loud fail-closed naming the loss | `…RESET_UNACKNOWLEDGED`, 13 unaccounted |
| wrong ack id (old) | still refused | refused, exit 1 |
| wrong ack id (unrelated) | still refused | refused, exit 1 |
| exact ack id | opens and records the reset | gen 2, predecessor id + high_water 13 |
| verify_chain after reset | true origin reported | `covers_full_history:false`, `post-reset` |
| rollback, same id | fail-closed | `…SEQ_REGRESSION`, 9 missing |
| `-Purge` populated, unconfirmed | refuse, delete nothing | refused, vault intact, no record |
| `-Purge` confirmed | record first, then delete | record written and outlives the vault |
| `-Purge` absent vault | no-op | explicit skip line |

## Production — the live daemon, the live vault

Deployed through `scripts\synapse-setup.ps1 -SourceDir C:\code\synapse
-ForceRestart` (exit 0). Installed binary
`sha256=8BCA88E27CA842CCC2893B7E2D8F1235AFD6DAB2C733A619C8C1B49AA9D374E4`.

**Before deploy** — SoT read directly:

```
lineage_files_before = 0
db-daemon\vault-identity.json  vault_id = 01KYJPGWATPD4XNMZY3ERGTKQW
CURRENT = manifest-00000000000000002571.json   durable_seq = 30783
```

**Setup's own candidate preflight** started the new binary against a fresh
throwaway vault and it came up clean — independent evidence the lineage check
does not break startup:

```
SYNAPSE_CALYX_VAULT_LINEAGE_SEEDED
  vault_dir=…\setup-candidates\candidate-…\db  vault_id=01KYNAM5NFC1F5DY2WJ2TPP1EJ
  latest_seq=0
Candidate daemon health preflight passed pid=8140 tool_count=40
```

**Production vault, first open on the new binary:**

```
[WARN] SYNAPSE_CALYX_VAULT_LINEAGE_SEEDED
  vault_dir=…\synapse\db-daemon
  lineage_path=…\synapse\db-daemon.lineage.json
  vault_id=01KYJPGWATPD4XNMZY3ERGTKQW  latest_seq=31095

[INFO] SYNAPSE_CALYX_VAULT_OPENED
  … lineage_path=…\db-daemon.lineage.json
  vault_generation=1  vault_lineage_reset_count=0  chain_origin=lineage-seeded
```

`%LOCALAPPDATA%\synapse\db-daemon.lineage.json` (368 bytes) records generation 1
with `vault_id 01KYJPGWATPD4XNMZY3ERGTKQW` — the same id in the vault's own
identity file, and the same id named in this issue's original evidence.

**Live `verify_chain` through the real wired MCP surface** (`tools/call`,
`audit operation=verify_chain`, session `b5ea5235-…`):

```json
{"verdict":"intact","intact":true,"head_height":11625,"entry_count":11625,
 "raw_commitments_intact":true,"raw_commitment_coverage_from_seq":1,
 "covers_full_history":false,"chain_origin":"lineage-seeded",
 "vault_generation":1,"vault_reset_count":0}
```

`coverage_from_seq=1` and `verdict=intact` are both still there, and they no
longer read as full historical coverage.

**Restart safety** — the case that would brick the host if the detector were
wrong. A second deploy (`-SkipBuild -ForceRestart`) stopped and restarted the
daemon (pid 18280 → 14296). The journal was updated in place, still generation 1,
no reset, no failure:

```
last_observed_unix_ms  1785274770400 -> 1785274915210
high_water_seq         31095         -> 31297
```

Post-restart live `verify_chain`: `verdict=intact`, `head_height=11674`,
`covers_full_history=false`, `chain_origin=lineage-seeded`, `vault_generation=1`,
`vault_reset_count=0`.

## What this does not fix

The ~1.56M sequences lost on 2026-07-27 are unrecoverable; no backup exists.
This work makes the *next* such event impossible to miss and hard to cause. The
backup itself is #1687, which this raises the priority of exactly as the issue
asked.

The production vault's own lineage journal begins at generation 1 with
`started_reason: "lineage-seeded"`, and `verify_chain` there reports
`chain_origin: "lineage-seeded"` / `covers_full_history: false`. That is the
honest statement: nothing before the journal's creation is attested by it, and
in particular the pre-2026-07-27 history is not part of this chain.
