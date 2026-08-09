# Issues #1595 and #2179 — production probe removal and action-log integrity

Date: 2026-08-09 (America/Chicago)

Code commit verified: `55c323c48d5716b0697746ebbedbad2adb7faa9d`

## Acceptance and Sources of Truth

This batch closes two coupled defects:

- #1595: the debug-only synthetic storage writer had been projected through
  the always-public `storage` facade again;
- #2179: `audit operation=command_query` counted malformed physical
  `CF_ACTION_LOG` rows and skipped them into an otherwise successful result.

The authoritative Sources of Truth were:

1. `%APPDATA%\synapse\codex-tool-surface.json` for the freshly published MCP
   input/output schema;
2. the installed executable and live `/health` build provenance for the code
   actually running;
3. the production Calyx vault at
   `%LOCALAPPDATA%\synapse\db-daemon`, read independently with the supported
   read-only `dump_cf` and `dump_action_log` diagnostics;
4. the OS process table, scheduled task, supervisor stop request, and live
   health response for final daemon state;
5. the daemon error stream for the exact rejected storage-boundary row.

Return values from the triggers were correlated with separate reads of those
sources. No automated test, CI job, mock row, alternate database, branch,
worktree, or alternate Cargo target directory was used.

## Root cause

The regression had two independent causes.

First, `storage_put_probe_rows` was correctly registered only in the
`SYNAPSE_DEBUG_TOOLS` raw-tool lane, but `StorageOperation`, `StorageParams`,
`StorageResponse`, validation, facade dispatch, and the public operation
contract separately re-exported it. Compilation could not distinguish that
invalid projection from a legitimate operation. The first #1595 fix had no
structural source gate, so unrelated commit `b546f3dd983c4404ebee4768eb5b1ce1c2f691e3`
could restore the projection without failing lint.

Second, the audit reader implemented a weaker codec than the writers. It only
checked JSON decoding, 12-byte key length, and `ts_ns`; it incremented
`corrupt_row_count` or `noncanonical_key_count` and continued. The public
summarizer failed on `corrupt_row_count` but not `noncanonical_key_count`, so a
physically malformed key was explicitly converted into success with omissions.
There was no storage-level action-log codec, so every generic put/mutate method
could write rows that the authoritative reader could not interpret.

The debug generator also contained two silent fallbacks: if JSON encoding
failed it substituted pattern bytes (or an empty value) and continued. Those
were removed as part of the same root-cause fix.

## Independent research after diagnosis

`pwsh -File scripts/check-research-lane.ps1` drove real Exa MCP
`initialize`, `tools/list`, and `tools/call` requests and reported
`exa_mcp live` (Exa MCP 3.4.0). Exa was used to find upstream RocksDB material
on atomic batches, conflict-checked mutation, and online integrity validation.
The built-in web lane independently read the primary sources:

- <https://github.com/facebook/rocksdb/wiki/Basic-Operations>
- <https://github.com/facebook/rocksdb/wiki/Transactions>
- <https://rocksdb.org/blog/2021/05/26/online-validation.html>

The applicable design lessons were: validate before the atomic write boundary,
do not expose partial data after integrity failure, retain bounded evidence that
identifies the offending physical object, and use revision/conflict protection
for a repair mutation. The production vault contained no legacy invalid row,
so performing a repair would have been an unjustified mutation rather than a
best practice.

## Implemented production behavior

- Removed `put_probe_rows` from every always-public storage facade projection.
  The raw `storage_put_probe_rows` implementation remains available only when
  `SYNAPSE_DEBUG_TOOLS=1`.
- Added Gate 0b to `scripts/lint.ps1`. Any future facade projection or exact
  public `"put_probe_rows"` operation contract fails with
  `SYNAPSE_LINT_SYNTHETIC_WRITER_PUBLIC`.
- Added one `synapse-storage::action_log` codec for:
  - exact 12-byte big-endian `u64 ts_ns || u32 seq` keys;
  - schema version 1;
  - exact agreement among key, `ts_ns`, `seq`, and canonical `audit_id`;
  - required action-audit or command-audit fields;
  - refusal of unsupported or non-string explicit row kinds.
- Applied that codec before every public storage put/mutate boundary, including
  guarded/multi-CF mutations and atomic terminal action/Oracle publication.
- Rejections log and return a stable failure code, row index, lengths, and
  SHA-256 identifiers without exposing raw key/value material.
- Command snapshot, forward query, and newest-first query use the same codec.
  Any invalid row in the claimed scan produces
  `COMMAND_AUDIT_INTEGRITY_FAILED`, bounded to eight diagnostic examples, and
  no partial result.
- Newest-first query continues validating the bounded tail even after the
  requested result limit is filled.
- The supported `dump_action_log` diagnostic now validates the complete codec,
  not merely JSON syntax.
- Both JSON-encode fallbacks in the debug writer now fail before any write as
  `SYNAPSE_STORAGE_PROBE_JSON_ENCODE_FAILED`.

## Compile and lint

```text
cargo check --workspace
Finished dev profile; exit 0

pwsh -NoProfile -File scripts/lint.ps1
Gate 0: 1356 Rust files, 32 manifests, 52 Cargo targets — OK
Gate 0b: production storage facade has no put_probe_rows projection — OK
Gate 1 shared lint contract — OK
Gate 2 toolchain pin 1.97.1 — OK
Gate 3 lock graph — OK
Gate 4 fmt, root and calyx — OK
Gate 5 cargo deny, root and calyx — OK
Gate 6 clippy --workspace --all-targets -D warnings, root and calyx — OK
Gate 7 public Calyx API ratchet — OK
LINT OK
```

Two reported Clippy defects were fixed at the source during the gate: an
`expect` in hex formatting was replaced with direct nibble encoding, and a
nested tuple return used the existing `RawRow` type. No lint was suppressed.

The standalone link of the supported diagnostic emitted rust-lld's Windows SDK
`xinput.lib` imported-`DllMain` warning. It did not affect the successful
diagnostic execution and is independently tracked as #2192; it was not hidden
or misclassified as these issues.

## Baseline reality before deployment

Installed old build:

```text
build=a903d7265040
pid=2044
installed_sha256=3A13C3FF7B04EAB60FC330A4C4A0DB97292B0C563C5960999BB09833656DED10
```

The old public facade accepted the smallest nonmutating synthetic request:

```text
operation=put_probe_rows
cf_name=CF_ACTION_LOG
rows=0
before_rows=68
after_rows=68
rows_added=0
```

The old audit reader then scanned all retained rows:

```text
scanned_rows=68
returned_count=68
complete=true
corrupt_row_count=0
noncanonical_key_count=0
```

After the daemon was gracefully stopped, the new codec was run independently
over that same physical vault before installation:

```text
daemon_process_count=0
dump_cf row_count=68
all key_len_bytes=12
dump_action_log exit=0
rows=68 invalid=0
```

Thus the previously reported legacy probe row had already left the 24-hour
retention set. There was no physical legacy row to repair or migrate.

## Exact deployment and hardware path

Setup built and candidate-validated the exact commit using the canonical
checkout target. The first invocation correctly failed before handoff because
the supplied active-issue string contained two IDs; rerunning with the single
owner `-ActiveIssue 1595` installed the same already-built artifact. Setup then
finished installation and daemon verification but returned the expected
`SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` verdict because this Codex process
started with the old schema. The fresh snapshot and live server were used for
the schema proof below.

```text
installed_exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
installed_len=256590665
installed_sha256=FFA2E9E37437BA4B221371A2EE44CE49FD80F8B3B22E1CC0417390EDC5C5431D
build_commit=55c323c48d5716b0697746ebbedbad2adb7faa9d
build_checkout_commit=55c323c48d5716b0697746ebbedbad2adb7faa9d
build_tree_state=clean
build_profile=release
```

Host and selected acceleration path:

```text
CPU=13th Gen Intel Core i7-1355U
physical_cores=10
logical_processors=12
physical_memory_bytes=33854390272
GPU=Intel Iris Xe Graphics
NVIDIA/CUDA devices=0
setup CARGO_BUILD_JOBS=12
setup CMAKE_BUILD_PARALLEL_LEVEL=12
calyx_math_backend=cpu
calyx_math_cpu_simd_path=avx2
calyx_math_probe_status=ok
process execution_speed_throttling_disabled=true
calyx_inert_tuning_knob_count=0
```

CPU/AVX2 is the fastest supported math backend on the hardware actually
present. Setup did not force nonexistent CUDA or create a machine-specific
alternate target tree.

## Manual FSV — #1595 public surface and three boundaries

The fresh physical tool snapshot reported:

```text
snapshot=%APPDATA%\synapse\codex-tool-surface.json
snapshot_sha256=09BE9CFDE082C0D8125164307FC7F18CAEF29EB47F30DB746DD577D04A139168
daemon_pid=7980
tool_count=40
tool_surface_sha256=a356f4865fec1d7f8517e6ae2b06e339f9b31f5a5ac92ec843108e2f880e7c8a
storage_input_has_put_probe_rows=false
storage_output_has_put_probe_rows=false
```

Before the three public rejection triggers, a complete audit scan still showed
68 physical rows. The stale client schema was intentionally used to send the
old shapes to the new server:

1. Empty boundary: `rows=0`, `value_bytes=0`.
   Result: `TOOL_PARAMS_INVALID`, unknown operation variant
   `put_probe_rows`.
2. Maximum former boundary: `rows=10000`, `value_bytes=65536`.
   Result: the same `TOOL_PARAMS_INVALID` before allocation or write.
3. Unexpected payload: valid `operation=summary` plus a `put_probe_rows`
   object. Result: `TOOL_PARAMS_INVALID`, unknown field `put_probe_rows`, with
   the exact accepted-field list and remediation.

An immediate complete audit scan after those rejected calls remained:

```text
scanned_rows=68
returned_count=68
complete=true
corrupt_row_count=0
noncanonical_key_count=0
```

The three invalid triggers therefore changed no action-log state.

## Manual FSV — #2179 happy path

Source state before trigger: 68 canonical physical rows.

Real trigger:

```text
act operation=lease_status
held=false
is_owner=false
this_session_id=c41014d5-98fd-4b46-8bd9-cae6bd6d700e
```

The write path added the expected intent and final command-audit rows. A new
complete public query using the fixed reader reported:

```text
scanned_rows=70
returned_count=70
complete=true
corrupt_row_count=0
noncanonical_key_count=0

intent key_hex=18ca3b8a124e96c400000000
intent audit_id=01786305666404488900-0000000000
intent value_sha256=sha256:2e0909941761172ec88eaaf6c54d51bb42734ac56866bf6c848ece22138ff506

final key_hex=18ca3b8a128e1a8800000001
final audit_id=01786305666408651400-0000000001
final value_sha256=sha256:ec8255c58e6d8fc9b3142654b4d66ccd823b232d13eb6ad865a2a065bd0a6f72
```

With the daemon stopped, separate physical reads proved those bytes were on
disk and the entire family passed the new codec:

```text
dump_action_log: rows=70 invalid=0
dump_cf: row_count=70

row[68] key_len_bytes=12
key_sha256=sha256:f7a20d37edb1b9ff551962d7f94eae5f13daba9088bc846e653f0ca44af2011f
value_len_bytes=1664
value_sha256=sha256:2e0909941761172ec88eaaf6c54d51bb42734ac56866bf6c848ece22138ff506

row[69] key_len_bytes=12
key_sha256=sha256:4bfc3f2b62c297c9088085da8ada11b2ecfa10d612f950b758463ac4d58d86d3
value_len_bytes=2150
value_sha256=sha256:ec8255c58e6d8fc9b3142654b4d66ccd823b232d13eb6ad865a2a065bd0a6f72
```

## Manual FSV — #2179 query edges

Each edge was read before/after against `CF_ACTION_LOG`; all failed before a
mutation and the later offline census remained exactly 70 rows:

1. `limit=0` -> `TOOL_PARAMS_INVALID`, limit must be 1..250.
2. odd-length `start_key_hex=abc` -> `TOOL_PARAMS_INVALID`, cursor must be
   even-length hex.
3. `start_ts_ns=200`, `end_ts_ns=100` -> `TOOL_PARAMS_INVALID`, start must be
   <= end.

These validate empty/minimum, invalid-format, and inconsistent-boundary input
without weakening the physical-row integrity contract.

## Manual FSV — storage codec rejection and physical nonexistence

The supervised daemon was stopped. The exact installed executable opened the
same production vault in supported unscoped stdio-admin mode with
`SYNAPSE_DEBUG_TOOLS=1`. Fresh `tools/list` proved the intended separation:

```text
stdio_pid=20576
stdio_server=synapse-mcp 0.1.0
stdio_tool_count=246
raw storage_put_probe_rows advertised=true
public storage schema has put_probe_rows=false
```

Before the trigger: `rows=70 invalid=0`. The raw diagnostic attempted one
synthetic row in `CF_ACTION_LOG`. Its generated key was 47 bytes, so the shared
storage boundary rejected it before mutation:

```text
error_code=STORAGE_WRITE_FAILED
failure=ACTION_LOG_CODEC_INVALID
failure_code=ACTION_LOG_KEY_LENGTH_INVALID
row_index=0
key_len_bytes=47
key_sha256=sha256:35e6f8197ee8c4e10299316cec1659a933a885f37fa2c039d90d404794304275
value_len_bytes=178
value_sha256=sha256:c4cbaf1844716333026af0d9b6669e6e62e1dc27f044090857de5ed248d8a830
detail=key length must be 12 bytes, actual=47
stdio_exit=0
```

The error stream independently contained the same stable code, lengths, and
hashes. After the process closed gracefully:

```text
dump_action_log: rows=70 invalid=0
dump_cf: row_count=70
rejected_key_sha_present=false
rejected_value_sha_present=false
```

This proves both that the invalid write was detected and that neither rejected
physical object exists in the Source of Truth.

## Final live state

Setup `-Start` read the stdio lifecycle record as clean and restored normal
supervision:

```text
health_ok=true
daemon_pid=13880
daemon_build=55c323c48d57
tool_count=40
tool_surface_sha256=a356f4865fec1d7f8517e6ae2b06e339f9b31f5a5ac92ec843108e2f880e7c8a
scheduled_task_state=Running
supervisor_pid=16164
supervisor_stop_request_present=false
chrome_bridge_status=ok
chrome_bridge_host_count=1
chrome_bridge_extension_stale=false
storage_status=ok
vault_status=ok
vault_id=01KYJPGWATPD4XNMZY3ERGTKQW
storage_pressure=Normal
calyx_row_guard_over_budget_total=0
calyx_row_guard_starved_total=0
final audit scanned_rows=70 returned_count=70 complete=true
final audit corrupt_row_count=0 noncanonical_key_count=0
```

The installed daemon, fresh schema snapshot, live query, offline codec sweep,
and physical row hashes all agree. #1595 and #2179 are accepted against the
real production state.
