# FSV — issue #1877: physical snapshots sized open files from stale directory metadata

Date: 2026-07-28/29 (UTC) · Host: configured Windows 11 Pro 10.0.26200 host · Daemon: live (`synapse-mcp` pid 18800)

Instrument: `scripts/diagnostics/issue-1877-physical-snapshot-fsv.ps1`. It parses
`scripts/synapse-setup.ps1` with the PowerShell AST parser and dot-sources the
**real** function definitions out of the file on disk — nothing is copied — then
exercises them against physical files.

## Root cause

NTFS replicates a file's size and last-write time into its **directory entry** as
a performance tweak for directory enumeration. Since Windows Vista that
replication happens when the **last handle to the file object closes**. Enumeration
APIs (`FindFirstFile`, which is what `Get-ChildItem` uses) read the directory
entry, so any file a live writer holds open reports an arbitrarily stale size.

Microsoft states the remedy directly in the `FindNextFile`/`FindFirstFile`
remarks: *"In rare cases or on a heavily loaded system, file attribute
information on NTFS file systems may not be current at the time FindFirstFile is
called. To be assured of getting the current NTFS file system file attributes,
call the GetFileInformationByHandle function."*

`Get-SynapseCalyxPhysicalSnapshot` summed `Get-ChildItem … | Measure-Object Length -Sum`
for `manifest_bytes`, `cf_bytes` and `wal_bytes`. The vault WAL is exactly a file
the daemon holds open and appends to continuously.

Two independent things depend on those numbers:

1. `Write-SynapseVaultDeletionRecord` (#1875) quotes them back to the operator in
   `SYNAPSE_SETUP_VAULT_PURGE_REFUSED` — the message deciding whether to destroy a
   vault. Understating the size of the thing about to be deleted is the wrong
   direction to be wrong in.
2. `Get-SynapseVaultRecoveryFingerprint` is the **startup-stall detector**. It
   used both the byte totals and `LastWriteTimeUtc` as its progress signal. Both
   are replicated metadata, so both go stale on precisely the file that proves
   progress — making an advancing vault look frozen.

## Evidence the defect was real

Observed twice on this host against the live daemon log, an actively-appended file:

```
Get-ChildItem synapse.log.2026-07-28-20  -> Length 0        (issue reporter; true size 1,400,283)
Get-ChildItem synapse.log.2026-07-29-00  -> Length 0        (this session;    true size 2,157,446)
```

The staleness is **intermittent** — re-measured 30 minutes later, the directory
entry had caught up. That is what makes it dangerous: it cannot be relied on to
show up, and when it does it silently understates.

Deterministic reproduction (instrument CASE 2), writer holding a `.wal` open
after 1,048,576 bytes of appends:

```
Get-ChildItem .Length  [directory entry / FindFirstFile] = 0
handle       .Length   [file object]                     = 1048576
understatement                                            = 1048576 bytes
>>> the OLD code would have reported wal_bytes = 777 instead of 1049353
```

## The fix

`scripts/synapse-setup.ps1`:

- `Ensure-SynapseFileSizeProbeType` — P/Invoke `CreateFileW` with
  `FILE_READ_ATTRIBUTES` only (the minimal access; asking for no data access is
  what lets the probe succeed against files an exclusive writer holds open) and
  share `READ|WRITE|DELETE`, then `GetFileInformationByHandle`. Size and
  last-write time come from the **one** call, because both are replicated the
  same way and both were stale. Failures throw a `Win32Exception` naming the path
  and the win32 code.
- `Get-SynapseAuthoritativeFileLength` / `Measure-SynapseAuthoritativeFileBytes` —
  per-file probe and aggregate. A file that cannot be sized is **never** replaced
  by its stale directory-entry value; it is counted in `unsized_file_count`,
  named in `unsized_files` with the win32 error, and flips `complete=$false`,
  which means the reported total is an explicit lower bound.
- `Get-SynapseCalyxPhysicalSnapshot` — bumped to `synapse_calyx_physical_snapshot/v2`;
  all three byte totals and `largest_wal` now come from handles. New fields
  `byte_source`, `bytes_complete`, `unsized_file_count`, `unsized_files`.
- `Write-SynapseVaultDeletionRecord` — the purge refusal now quotes `wal_bytes`,
  `byte_source` and `bytes=exact|lower_bound_unsized_files=N`, and `Warn`s loudly
  when any file could not be sized.
- `Get-SynapseVaultRecoveryFingerprint` — byte totals *and* the newest-write
  ticks now come from handles, so the stall detector cannot see a growing vault
  as frozen.

## Manual FSV — all cases passed

| Case | Source of truth | Result |
|---|---|---|
| 1 — happy path, all handles closed | 3×100 manifest, 2×4096 cf, 1×777 wal on disk | `manifest_bytes=300 cf_bytes=8192 wal_bytes=777`, `bytes_complete=True` |
| 2 — **WAL held open by a live writer** | handle length vs enumeration entry | entry `0`, handle `1048576`; snapshot reported `wal_bytes=1049353` (truth), not `777` (stale) |
| 2b — growth while open | handle `1048576 → 1572864` | snapshot `1049353 → 1573641`; **stall fingerprint changed**, so progress is visible |
| 2c — after clean close | entry and handle both `1572864` | snapshot `1573641`, agrees |
| 3 — empty vault dir | 0 files | all totals `0`, `largest_wal=null`, `bytes_complete=True`, `error=null` |
| 3b — vault dir absent | — | `exists=False`, no spurious incompleteness |
| 4 — **file vanishes between enumeration and sizing** (real compaction TOCTOU) | `keep.dat`=1234, `ghost.dat`=999999 deleted after enumeration | `bytes=1234` (lower bound, stale 999999 **not** used), `unsized_file_count=1`, `complete=False`, error names the path and `win32=2` |
| 5 — boundary, file > 4 GiB | sparse file of exactly 5,000,000,000 bytes | `wal_bytes=5000000000`, no int32 overflow |
| 6 — **live production vault** | independent handle-sum oracle, bracketed before/after to be race-free | `manifest=58400 cf=76210000 wal=4530826` — snapshot inside bracket on every total; `current_manifest_durable_seq=35916` |
| 7 — **operator-facing purge refusal** | synthetic populated vault, WAL open with 41,943,040 bytes written | entry said `0`; refusal quoted `wal_bytes=41943040 byte_source=handle:GetFileInformationByHandle bytes=exact durable_seq=1560000`; purge refused; no deletion record written |

Case 7's actual message, as an operator would see it:

```
SYNAPSE_SETUP_VAULT_PURGE_REFUSED vault_dir=…\purge-vault vault_id=fsv-1877-vault
durable_seq=1560000 manifest_files=1 cf_files=1 cf_bytes=4096 wal_segments=1
wal_bytes=41943040 byte_source=handle:GetFileInformationByHandle bytes=exact
source_of_truth=physical vault directory contents read immediately before deletion
remediation=this vault holds durable captured history and there is no automatic
backup (issue #1687). Take a backup first, then rerun with -ConfirmVaultDestruction
to delete it deliberately. -Purge alone will not destroy a populated vault.
```

Before the fix that line would have read `wal_bytes=0`.

## Research lane

Exa remains `registered_but_unusable` (`EXA_API_CREDITS_EXHAUSTED_402`, #1864) — not
re-diagnosed, per AGENTS.md. Research done through the built-in web lane against
primary sources:

- Microsoft Learn — `FindNextFileA` remarks (stale NTFS attributes; use `GetFileInformationByHandle`)
- Microsoft Learn — `GetFileSizeEx` (`FILE_READ_ATTRIBUTES` is sufficient access)
- Microsoft Learn — `CreateFileW` (share-mode/access conflict rules)
- Raymond Chen, *The Old New Thing*, 2011-12-26 — "Why is the file size reported
  incorrectly for files that are still being written to?" (directory-entry
  replication; Vista+ replicates on last-handle-close)
