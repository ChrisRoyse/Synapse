# Manual FSV Handoff: Issue #1661 Storage Migrate

Date: 2026-07-16

Status: implementation and host setup are complete; D1 acceptance FSV is not
complete in the current Codex process.

## Root Cause

The Calyx storage work had inspect, summary, retention, pressure, and GC parity
surfaces, but it did not have an operator-safe migration path from the existing
RocksDB daemon store to a Calyx vault. The missing capability meant cutover
would have required either an ad hoc copy path or accepting Calyx without
byte-exact proof that every column-family row survived migration.

The root problem was not a formatting or facade routing bug. It was a missing
storage capability: Synapse needed a fail-closed migration operation whose final
verdict comes from physical source/target readback and a durable manifest.

## Research

Exa and native web research converged on the same migration design points:

- AWS DMS data validation compares source and target rows and reports
  mismatches:
  `https://docs.aws.amazon.com/dms/latest/userguide/CHAP_Validating.html`.
- CockroachDB MOLT validation documents row-count, checksum/hash, and row-value
  verification:
  `https://www.cockroachlabs.com/docs/molt/migration-considerations-validation`.
- Snowflake migration validation uses schema, metrics, and row/cell-level
  comparison, including chunked MD5 row fingerprints:
  `https://docs.snowflake.com/en/migrations/snowconvert-docs/general/technical-documentation/data-migration-and-validation/data-validation`.
- MariaDB staged migration records SHA-256, byte size, and row count in a
  manifest and verifies the manifest during load/finalize:
  `https://mariadb.com/docs/server/server-management/install-and-upgrade-mariadb/migrating-to-mariadb/moving-from-mysql/mysql-to-mariadb-migrator/migrate-with-offline-copy`.
- RocksDB documents read-only database instances, which matches the migration
  source-read requirement:
  `https://github.com/facebook/rocksdb/wiki/Read-only-and-Secondary-instances`.

Implementation choices from that research:

- Stream from RocksDB read-only; never mutate the source during copy.
- Verify every source and target key/value byte, not only row counts.
- Keep per-CF and total SHA-256 digests in the manifest.
- Write the manifest only after target verification succeeds.
- Stage the manifest with create-new semantics, sync it, and read it back before
  source retirement.
- Retire the source only as an explicit option and only after the verified
  manifest is staged.
- Fail loudly with structured log codes; no fallback copy path or mock success.

## Source of Truth

Acceptance SoT after a fresh Codex restart:

- Strict MCP client schema: `mcp__synapse.storage` must expose
  `operation=migrate`.
- Trigger: real `mcp__synapse.storage` `tools/call`, not CLI, direct HTTP, or a
  helper.
- Source data: physical RocksDB directory, normally
  `C:\Users\hotra\AppData\Local\synapse\db-daemon` or an operator-created
  real-data copy/checkpoint of it.
- Target data: physical Calyx vault directory supplied as `target_calyx_path`.
- Verdict artifact: manifest JSON supplied as `manifest_path`.
- Verification readback: separate source and target scans plus committed
  manifest readback showing equal total rows, per-CF rows, per-CF digests, and
  total source/target digests.

Setup SoT read in this session:

- `mcp__synapse.health`: `ok=true`, daemon PID `64508`, bind
  `127.0.0.1:7700`, tool count `40`, tool surface
  `f762a7df57aac03adc41d80a8cb8c8ab72496f142491b60c55f39957b5f8a069`.
- Daemon run ledger:
  `C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-run-current.json`
  contains PID `64508`, bind `127.0.0.1:7700`, and `ended_at_unix_ms=null`.
- Installed binary:
  `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`
  SHA-256 `359A7503CE13F3589C294448E869CB359E5232F426BDA8F72001B984D05CAC75`.
- Host tool-surface snapshot:
  `C:\Users\hotra\AppData\Roaming\synapse\codex-tool-surface.json`
  SHA-256 `5e8b748b747acc311930af0086288cd7dc90af1c922a6ba70c370f40990893af`,
  tool count `40`, storage input schema
  `da21736f669100c11fc7733927b5df2d5191e88321182d040dbf7a556a80a9db`,
  storage output schema
  `986fb156305c084487811014737553cd078b36c3abc750239f39bb628a9354a4`.
- Snapshot storage operation enum:
  `inspect,summary,gc_once,migrate`.
- Snapshot `StorageMigrateParams` required fields:
  `source_rocksdb_path,target_calyx_path,manifest_path`.
- Snapshot `batch_rows` bounds: minimum `1`, maximum `1000000`.
- Setup continuation:
  `C:\Users\hotra\AppData\Local\synapse\setup-continuations\post-exit-33520-20260716T204014129Z\continuation.json`
  ended `state=completed`, `exit_code=0`, `daemon_pid=64508`.

Current blocker SoT:

- Current Codex parent PID is `27984`.
- `mcp__synapse.profile operation=status` reports
  `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` for live Codex PID `27984`.
- The callable `mcp__synapse.storage` schema loaded in this Codex process still
  advertises only `operation=inspect|summary|gc_once`.

Because D1 requires the real strict MCP client `tools/call` as the trigger,
direct HTTP calls, CLI mode, or helper binaries cannot be used as accepted FSV
for #1661 in this process.

## Code Change

- Added `crates/synapse-storage/src/migration.rs`.
- Added `CalyxMigrationRow` and a Calyx migration batch path that bypasses
  normal retention/pressure deletion during migration while preserving the
  logical row bytes.
- Added Calyx read-only scans that include expired rows for byte-exact
  migration verification.
- Added the M3 storage API `migrate_storage`.
- Added the public storage facade operation `migrate`.
- Added a CLI diagnostic mode `storage-migrate`; this is not an FSV trigger.

The migration flow:

1. Validate source path, target path separation, optional retired path, and
   source-retirement manifest placement.
2. Open the Calyx target backend.
3. For each known RocksDB column family, read source rows read-only.
4. Convert each row into a Calyx migration envelope with source key/value bytes
   unchanged and retention metadata derived from source timestamps.
5. Flush target writes.
6. Scan the target including expired rows.
7. Verify source and target row count, key bytes, value bytes, and row order.
8. Compute per-CF and total digests.
9. Stage a durable manifest, sync it, and read it back byte-for-byte.
10. Optionally rename the verified source to a retired path.
11. Commit the final manifest and read back structured JSON.

Structured log codes added include:

- `STORAGE_MIGRATION_START`
- `STORAGE_MIGRATION_CF_VERIFIED`
- `STORAGE_MIGRATION_VERIFY_ROW_COUNT_MISMATCH`
- `STORAGE_MIGRATION_VERIFY_ROW_MISMATCH`
- `STORAGE_MIGRATION_TOTAL_DIGEST_MISMATCH`
- `STORAGE_MIGRATION_MANIFEST_STAGED`
- `STORAGE_MIGRATION_SOURCE_RETIRE_FAILED`
- `STORAGE_MIGRATION_MANIFEST_WRITTEN`
- `STORAGE_MIGRATION_SOURCE_RETIRED`
- `STORAGE_MIGRATION_COMPLETE`

## Structural Checks

These are structural checks only, not FSV:

```text
cargo fmt --all --check
cargo check -p synapse-storage -p synapse-mcp
cargo clippy -p synapse-storage -p synapse-mcp --all-targets
cargo build --release -p synapse-mcp
git diff --check
```

All passed. `git diff --check` emitted only CRLF normalization notices for
existing touched files, with no whitespace errors. No automated tests,
benchmarks, FSV harnesses, or FSV scripts were created or run.

## Host Setup Readback

The repo-built release daemon was installed and launched through
`scripts\synapse-setup.ps1`.

Observed setup evidence:

- Candidate daemon preflight passed on `127.0.0.1:60308`.
- Candidate tool count: `40`.
- Candidate tool surface:
  `f762a7df57aac03adc41d80a8cb8c8ab72496f142491b60c55f39957b5f8a069`.
- Installed binary hash:
  `359A7503CE13F3589C294448E869CB359E5232F426BDA8F72001B984D05CAC75`.
- Post-exit continuation completed with exit code `0` after daemon, Chrome
  bridge, tool-surface, and client-config readbacks passed.
- Final daemon PID: `64508`.
- Final daemon bind: `127.0.0.1:7700`.
- Final daemon DB:
  `C:\Users\hotra\AppData\Local\synapse\db-daemon`.

## Manual FSV Plan For Fresh Codex

Do this only after starting a fresh Codex process through the patched launcher.
The first readback must prove the current client schema now exposes
`mcp__synapse.storage operation=migrate`.

### Precondition Readbacks

Read and record:

- `mcp__synapse.health detail=compact`.
- `mcp__synapse.profile operation=status`.
- Current `mcp__synapse.storage` callable schema including `operation=migrate`.
- Windows process table for daemon PID and executable path.
- TCP listener for `127.0.0.1:7700`.
- Installed binary SHA-256.
- Source RocksDB path existence and size/count inventory.
- Target Calyx path absence or empty state.
- Manifest path absence.

### Happy Path

Smallest sufficient real-data proof:

- Use a current real daemon RocksDB source or a real-data checkpoint/copy of it.
- Use a new empty target directory under `%LOCALAPPDATA%\synapse\fsv\1661`.
- Use `batch_rows=2` to exercise multi-batch streaming without needing a huge
  dataset.
- Use `rename_source_on_success=false` first so the live source remains intact.

Trigger:

```text
mcp__synapse.storage operation=migrate
source_rocksdb_path=<real RocksDB source>
target_calyx_path=<new Calyx target>
manifest_path=<new manifest JSON>
batch_rows=2
rename_source_on_success=false
```

Expected after-state:

- Manifest file exists and decodes as `StorageMigrationManifest`.
- `source_backend=rocksdb`, `target_backend=calyx`.
- `total_rows > 0`.
- `source_digest_sha256 == target_digest_sha256`.
- Every `cf_reports[]` row has:
  `source_rows == target_rows == rows_written`,
  `source_digest_sha256 == target_digest_sha256`,
  `verified_byte_exact=true`.
- Source RocksDB directory still exists.
- Target Calyx directory exists and independently scans to the manifest counts.

### Edge 1: Normal Profile / No Write Grant

Before state:

- Target path absent.
- Manifest path absent.
- Profile status shows no `WRITE_STORAGE`.

Trigger the same migrate request without a reality-write grant.

Expected after-state:

- Tool call fails with permission denial.
- Target path remains absent.
- Manifest path remains absent.
- Source path unchanged.

### Edge 2: Invalid Batch Boundary

Before state:

- Target path absent.
- Manifest path absent.

Trigger with `batch_rows=0`.

Expected after-state:

- Strict client/server validation rejects the request.
- Target path remains absent.
- Manifest path remains absent.
- Source path unchanged.

### Edge 3: Missing Source Path

Before state:

- Source path is known absent.
- Target path absent.
- Manifest path absent.

Trigger with the absent source path and otherwise valid target/manifest paths.

Expected after-state:

- Tool call fails with source-open or invalid-config error.
- Target path remains absent or empty.
- Manifest path remains absent.
- No source retirement path exists.

### Edge 4: Source Equals Target

Before state:

- Source path exists.
- Manifest path absent.

Trigger with `source_rocksdb_path == target_calyx_path`.

Expected after-state:

- Validation rejects the request before opening the target.
- Manifest path remains absent.
- Source path unchanged.

### Edge 5: Existing Retired Path

Before state:

- Real source path exists.
- Retired path exists before trigger.
- Target path absent.
- Manifest path absent.

Trigger with `rename_source_on_success=true` and the existing retired path.

Expected after-state:

- Validation rejects the request before copying.
- Source path unchanged.
- Retired path unchanged.
- Target path remains absent.
- Manifest path remains absent.

The accepted #1661 closeout must include the actual before/after readbacks for
the happy path and at least three edges, including manifest bytes and row/digest
values residing on disk after the trigger.
