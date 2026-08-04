# Issue #1987 - durable backup publication

Date: 2026-08-03

## Source of truth

The source of truth is the backup directory on NTFS, not the MCP return value:

`%LOCALAPPDATA%\synapse\fsv\issue-1987-marker-20260803T2332Z`

A backup is published only when `backup_manifest.json` exists and
`backup_in_progress.json` does not. `storage backup_status` reads those files
independently. `storage restore_verify` reopens the copied vault and verifies its
physical files, hashes, metadata, WAL, and ledger chain.

## Diagnosis and research

The original synchronous copy outlived the MCP client's 300 second deadline.
Dropping Tokio's `spawn_blocking` join handle does not cancel its blocking work,
so the copy continued while its final-looking target remained externally
visible. A directory rename was then attempted, but a verifier-created WAL lock
made publication fail persistently on Windows.

Research lanes used after diagnosis:

- Exa MCP was proved live with `scripts/check-research-lane.ps1`; the query was
  `Tokio spawn_blocking cancellation dropped JoinHandle atomic directory rename Windows durable backup publication best practices`.
- Tokio documents that started `spawn_blocking` tasks cannot be aborted and a
  detached `JoinHandle` continues running:
  <https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html> and
  <https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html>.
- Microsoft documents the constraints around directory moves on Windows:
  <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexa>
  and <https://learn.microsoft.com/en-us/windows/win32/fileio/moving-directories>.
- MCP Tasks describes durable, pollable state for work that outlives a request:
  <https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks>.

The implemented protocol creates the final directory with
`backup_in_progress.json`, copies and verifies the vault, writes the SHA-256
manifest, then removes the marker and syncs the parent directory. Readers reject
the marker. `backup_status` exposes `working`, `completed`, `absent`, or invalid
mixed state after a client timeout.

## Happy path

The live production vault was backed up through `storage backup` after deploying
commit `271df4ea`:

- vault id: `01KYJPGWATPD4XNMZY3ERGTKQW`
- durable/latest sequence: `465359` / `465362`
- vault files: `1455`
- vault bytes: `2163343466`
- manifest SHA-256: `1fb1952305cd34f92752ec59647af56dc7f9cbe3cd2914738504bf91554b6788`
- constellations: `234935`
- anchors: `50558`
- ledger entries: `296569`
- ledger tip: `11cf632486b5a0521ce1ed9cde1440df3eb297f9a647369a6c0f10533728b047`
- WAL bytes: `14817721`

Independent filesystem readback found the manifest, no marker, and the exact
declared vault byte count. `CURRENT` named
`manifest-00000000000000030642.json` and hashed to
`62451965626BB8B9E0181D3FAD3167D61356BA574878BBCFE8D50637D4C0F71C`.
After the read-only verifier fix in #1989, a complete relative-path, length, and
SHA-256 inventory contained 1457 files (1455 vault files plus the backup
manifest and lineage journal) and hashed to
`50C74FDA5DF1124A549D98243A5F41058236236F6D75817674BCAFD1DAEFFC12`.
An independent restore verification passed with the counts and ledger tip above.
`backup_status` separately read `state=completed`, `manifest_exists=true`,
`marker_exists=false`, and no staging directories.

## Boundary audit

1. Existing completed target. Before: 1457 files, manifest SHA-256
   `1FB1952305CD34F92752EC59647AF56DC7F9CBE3CD2914738504BF91554B6788`,
   no marker. Trigger: a second backup to the same path. Result:
   `SYNAPSE_CALYX_BACKUP_TARGET_EXISTS`. After: 1457 files, the same manifest
   hash, no marker, and 2163623993 total bytes including root metadata.
2. In-progress target. Before: a minimal real directory with
   `backup_in_progress.json`, no manifest, and marker SHA-256
   `CF5E8C82D3415322D54F3F99E57205069A3B403C311FF7B9EA63861F21A2F8DE`.
   `backup_status` read `working`. Trigger: `restore_verify` on its `vault`
   directory. Result: `STORAGE_BACKUP_IN_PROGRESS`. After: the same one file,
   same marker hash, and no manifest.
3. Empty target parameter. Before: the FSV parent contained 14 entries and its
   sorted-name digest was
   `5E4C20EE2A5B20A024AECEB16F5BB629DBF45F357464B190F4F38476805D562D`.
   Trigger: backup with an empty target. Result: `TOOL_PARAMS_INVALID`. After:
   the parent still contained 14 entries with the identical digest.

The production-sized happy path is also the large-file boundary: 2.16 GB and
1455 vault files were copied, hashed, reopened, and verified.
