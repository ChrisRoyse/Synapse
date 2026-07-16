# Manual FSV Closeout: Issue #1661 Storage Migrate

Date: 2026-07-16

Issue: https://github.com/ChrisRoyse/Synapse/issues/1661

## Result

Accepted. The real operator RocksDB store was migrated to a Calyx target through
the strict MCP client path, the manifest was read from disk after the trigger,
and separate source/target readbacks proved byte-exact parity.

No automated tests, FSV harnesses, benchmarks, or CI were created or run. The
commands listed here are structural checks or manual readback commands only.

## Research Used

The design was checked against Exa and native web research before finalizing the
implementation:

- RocksDB checkpoints: consistent snapshot/read-only source material.
  https://github.com/facebook/rocksdb/wiki/Checkpoints
- RocksDB backup verification: checksum/verify-before-trust behavior.
  https://github.com/facebook/rocksdb/wiki/How-to-backup-RocksDB
- Cockroach MOLT validation: row count, hashes, row-value validation, and
  checkpointed validation.
  https://www.cockroachlabs.com/docs/molt/migration-considerations-validation
- AWS DMS validation: source/target row comparison and explicit mismatch
  reporting.
  https://docs.aws.amazon.com/dms/latest/userguide/CHAP_Validating.html

The resulting shape is fail-closed: read source physically, stream every row,
verify row count plus key/value bytes plus digests, then persist a manifest only
after verification succeeds.

## Source Of Truth

- MCP precondition SoT: process table and strict Codex MCP client connected to a
  repo-built `synapse-mcp.exe`.
- Trigger: real `mcp__synapse.storage` `tools/call` through fresh `codex exec`
  clients pointed at the maintenance daemon.
- Source data SoT:
  `C:\Users\hotra\AppData\Local\synapse\db-daemon`
- Target data SoT:
  `C:\Users\hotra\AppData\Local\synapse\fsv\issue-1661\happy-final\target-calyx`
- Verdict artifact SoT:
  `C:\Users\hotra\AppData\Local\synapse\fsv\issue-1661\happy-final\migration-manifest.json`
- Direct row readback SoT:
  `C:\Users\hotra\AppData\Local\synapse\fsv\issue-1661\happy-final\direct-dumps-include-expired`
- Edge-case SoT:
  `C:\Users\hotra\AppData\Local\synapse\fsv\issue-1661\edges-final`

## MCP Precondition Readback

Maintenance daemon used for the write-capable migration trigger:

```text
pid=77300
bind=127.0.0.1:7781
exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
db=C:\Users\hotra\AppData\Local\synapse\fsv\issue-1661\maintenance-daemon-20260716T164837796Z\daemon-db
tool_count=40
tool_surface_sha256=f762a7df57aac03adc41d80a8cb8c8ab72496f142491b60c55f39957b5f8a069
storage_schema_has_migrate=true
profile_grants=READ_STORAGE,WRITE_STORAGE,READ_EVENTS,READ_REFLEX,READ_PROFILE
```

The normal daemon remained live separately:

```text
pid=64508
bind=127.0.0.1:7700
db=C:\Users\hotra\AppData\Local\synapse\db-daemon
backend=rocksdb
```

## Happy Path

Before trigger:

```text
source exists=true
target exists=false
manifest exists=false
source backend=rocksdb
```

Trigger:

```text
mcp__synapse.storage operation=migrate
source_rocksdb_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
target_calyx_path=C:\Users\hotra\AppData\Local\synapse\fsv\issue-1661\happy-final\target-calyx
manifest_path=C:\Users\hotra\AppData\Local\synapse\fsv\issue-1661\happy-final\migration-manifest.json
batch_rows=50000
rename_source_on_success=false
```

Tool result, then separate manifest readback:

```text
migrate_ok=true
total_rows=1330306
cf_count=17
source_digest_sha256=sha256:aa140fe14bcd89685dec3d28b65b582f7ef6c9ff0055dc7f93324a0ae6f7665d
target_digest_sha256=sha256:aa140fe14bcd89685dec3d28b65b582f7ef6c9ff0055dc7f93324a0ae6f7665d
mismatched_cfs=0
manifest_len_bytes=15192
target_exists=true
source_still_exists=true
```

Selected physical CF rows from the manifest:

```text
CF_EVENTS rows=240 expired_at_migration_rows=115 verified_byte_exact=true
CF_OBSERVATIONS rows=12 expired_at_migration_rows=1 verified_byte_exact=true
CF_ROUTINE_STATE rows=179 verified_byte_exact=true
CF_PROFILES rows=2 verified_byte_exact=true
CF_KV rows=531526 verified_byte_exact=true
CF_TIMELINE rows=246625 verified_byte_exact=true
CF_AGENT_TRANSCRIPTS rows=469208 verified_byte_exact=true
```

Direct row readback was then performed outside the migration return value. The
first read used the logical Calyx scan and exposed the root-cause distinction:
logical scans correctly omit expired rows, while migration verification is a
physical byte-exact check that must include retained expired rows. The diagnostic
dump was therefore extended with `--include-expired` for Calyx physical readback.

Separate source/target dump comparison after that fix:

```text
CF_OBSERVATIONS source_rows=12 target_rows=12 row_diff_count=0
first key=sha256:8ae47c447f570d9e3911c8b8aa86076b17200300e0b754f4e6d6d0baaabdc4fd
first value=sha256:7649f982c59fcc1ad1724ab882d564e03726073d93553b40967961cd7528b3af
last key=sha256:480e8146aafdddddffdbb5452aec2c845c237fbfb939cee4763d8145d48403c0
last value=sha256:cd195c12e977fd1d72870891331935c135bd00fea910e822b7e05c10fe4d3c0a

CF_ROUTINE_STATE source_rows=179 target_rows=179 row_diff_count=0
first key=sha256:ce586e86865856b3a87e32a7fc75daf7f0b9ed65bece96f1b3ae6f5ebd3cab42
first value=sha256:ab450f087b8a3119cb1cf7c29e18d591e64a755bb41d89b91dbf195dc8b39c91
last key=sha256:50e0ad9023379a4c15635187a2ae87730604a891713ed301ed303acaf6df1f33
last value=sha256:6563bbd7b0d7a58ceda252ce6aced831827c0561642205cbce4ee5071be9df5e

CF_EVENTS source_rows=240 target_rows=240 row_diff_count=0
first key=sha256:9608dd700979392cbfd3928d150416b9707f47c0962fde1d0d48715ead5c1253
first value=sha256:c40e74af0b7706a0e7081da7f785b2909cd5354f81f4fb1520ec8868e8eebddb
last key=sha256:3c22b3e52fe14958abbedb1bdf1c1ea86ff679d4c6a387fec5c91300aa79665a
last value=sha256:1a52fc8218fd1faf251a2bbd72ba73b9868962542f8b18110496e23d5b5dbf74

CF_PROFILES source_rows=2 target_rows=2 row_diff_count=0
first key=sha256:940c0079291a39a190145470b1e94b165f365566c9e01e683ff5117c503b86a2
first value=sha256:25a71c054526ea9e8f0a680577099fe94dac848c65ebda5a35d4c19ba80525c2
last key=sha256:d3578c3f6c48e0fbb75a62cdbf7c857acdd2981cf5318db82729b6d94bce49a4
last value=sha256:582e871c045c105b3acb9979eb7a5e25298fa40d6ee6278b85af6ae9102c1366
```

The include-expired dump support is diagnostic/readback support only. It does
not change logical Calyx reads, retention, or migration behavior.

## Edge Cases

All edge triggers used the same strict MCP daemon/client path. The before and
after state was read from the filesystem, not inferred from tool return values.

### Missing Source

Before:

```text
source exists=false
target exists=false
manifest exists=false
```

Trigger:

```text
mcp__synapse.storage operation=migrate source_rocksdb_path=<absent path>
```

After:

```text
error=STORAGE_OPEN_FAILED: source RocksDB path is not an existing directory
source exists=false
target exists=false
manifest exists=false
```

### Source Equals Target

Before:

```text
source exists=true
target is same path as source
manifest exists=false
```

Trigger:

```text
mcp__synapse.storage operation=migrate source_rocksdb_path=<source> target_calyx_path=<same source>
```

After:

```text
error=STORAGE_BACKEND_INVALID_CONFIG: source_rocksdb_path and target_calyx_path must be different
source exists=true
manifest exists=false
```

### Existing Retired Path

Before:

```text
source exists=true
target exists=false
manifest exists=false
retired_rocksdb_path exists=true
```

Trigger:

```text
mcp__synapse.storage operation=migrate rename_source_on_success=true retired_rocksdb_path=<existing path>
```

After:

```text
error=STORAGE_BACKEND_INVALID_CONFIG: retired_rocksdb_path already exists; refusing to overwrite
source exists=true
target exists=false
manifest exists=false
retired_rocksdb_path exists=true
```

## Query Parity

Source query daemon:

```text
pid=64508
bind=127.0.0.1:7700
backend=rocksdb
```

Target query daemon, copied from the migrated Calyx target:

```text
pid=41996
bind=127.0.0.1:7782
backend=calyx
```

The query comparison window ended at the manifest completion timestamp
`1784238871232000000`.

Observed parity:

```text
timeline_equal=true
timeline total_rows=100000 scanned_rows=100000 invalid_rows=0
timeline rows_by_kind browser_nav=7906 file_activity=2 focus_change=12023 idle_end=805 idle_start=1033 interaction_summary=27553 purge=6 session_end=198 session_start=437 title_change=50037

episode_equal=true
episode returned=3 scanned_rows=3
episode ids=ep1-358cc3561360622f,ep1-2a8609e9ad2ffafa,ep1-cf756bd936f94741

routine_equal=true
routine matched=0 returned=0 total_mined=0 total_state_rows=179

bounded_cost_equal=true
spawn_id=agent-spawn-ambient-claude-009c30bc-32bc-419f-9e8c-6113609ca36c
query_strategy=spawn_prefix_scan
scanned_rows=40
spawns_total=1
computed_micro_usd=0
source_reported_micro_usd=0
```

The broad seven-day Calyx cost query was scan-bound and timed out. That was
recorded on existing issue #1688 and did not hide a migration mismatch because a
bounded spawn-prefix cost query returned exact parity on both stores.

The target Calyx daemon health also exposed a separate GC WAL max-record-size
failure:

```text
storage_gc_last_error=storage write failed in storage_gc: write Calyx CF batch: SYNAPSE_CALYX_DISK_PRESSURE: write Calyx CF batch: encode WAL record: WAL payload exceeds max record size 67108864
```

That is tracked separately as #1716 and is not accepted as part of #1661.

## Code Delta From Final FSV

The final FSV found one verification-tooling gap: physical Calyx readback needed
an explicit include-expired mode so direct dumps could prove migration rows that
retention would normally hide from logical reads.

Files changed:

- `crates/synapse-storage/src/backend.rs`
  - added `scan_cf_read_only_with_expired`
  - added `dump_cf_read_only_with_expired`
- `crates/synapse-storage/src/lib.rs`
  - exported the include-expired readback APIs
- `crates/synapse-storage/examples/dump_cf.rs`
  - added `--include-expired`

No fallback or workaround was added. The default logical dump path still omits
expired rows. The new flag only makes physical migration verification explicit.

## Structural Checks

These are compile/lint/format checks only, not FSV:

```text
cargo fmt --all --check
git diff --check
cargo check -p synapse-storage
cargo clippy -p synapse-storage --all-targets
```

All passed after the final FSV document was added.

## Follow-Ups

- #1688: broad fleet cost query remains scan-bound; existing issue was updated
  with this FSV evidence.
- #1716: new P1 issue for chunking Calyx GC write batches below the WAL max
  record size.
