# Issue #2123 — exact registry point reads

Date: 2026-08-09 (UTC)

## Defect and Source of Truth

`ReflexRuntime::storage_profile_row` and `storage_kv_row` accepted one exact
key but called `Db::scan_cf`, materialized the complete logical column family,
and then searched the returned `Vec`. The failure was algorithmic: callers
asked for a point read while the implementation performed O(N) reads and
allocations.

The fix at code commit `a903d72650407bc9b6fbc008ce60a6aae28b996e`
routes both methods directly to the existing `Db::get_cf` primitive for the
same column family and key. Storage errors still propagate unchanged; there is
no fallback to a scan and no tolerated malformed result.

Acceptance used these independent Sources of Truth:

- the installed executable bytes and authenticated daemon build provenance;
- the Calyx vault row-guard counters for `read_latest` and `scan_cf_latest`;
- offline, read-only inspection of physical `CF_KV` and `CF_PROFILES` rows with
  the repository's existing `dump_cf` production diagnostic;
- the OS process table, Task Scheduler row, daemon lifecycle record, foreground
  input lease, and durable supervisor stop request.

Tool return values alone were not accepted.

## Root cause and research

The repository research-lane probe drove a real MCP initialize, tools/list,
and tools/call and reported Exa MCP 3.4.0 `live`. Exa and the built-in web lane
were used after diagnosis. The primary references agreed with the local
design:

- Rust's [collection documentation](https://doc.rust-lang.org/std/collections/index.html)
  distinguishes direct keyed access from sequential search, and
  [`BTreeMap::get`](https://doc.rust-lang.org/std/collections/struct.BTreeMap.html#method.get)
  is the standard exact-key operation.
- PostgreSQL's official
  [Indexes and ORDER BY](https://www.postgresql.org/docs/current/indexes-ordering.html)
  documentation explains why a matching ordered index can return a bounded
  first-N result without scanning the remainder. This informed the separate
  newest-N work rather than broadening this point-read change.

The smallest robust solution is the direct substitution: the key and column
family already form the complete lookup identity, and `Db::get_cf` is the
backend's exact read. Adding caching, snapshots, preloaded maps, or scan
fallbacks would create coherence and failure modes without adding capability.

Diagnosis also corrected three assumptions in the original issue instead of
silently implementing them:

- `CF_AGENT_TRANSCRIPTS` is keyed by spawn plus line, not timestamp. A reverse
  primary-key tail cannot implement newest-by-time. The durable timestamp
  index/summary work is tracked by #2189.
- the reflex recursion-clamp total must remain durable across restart; the
  process-lifetime telemetry counter is not an equivalent source. Durable
  aggregates/indexes are tracked by #2190.
- profile-registry implementation tools cannot be called from any public
  40-tool session, even after selecting `full_capability`. The missing facade
  route and unrelated denial remediation are tracked by #2191.

## Build, lint, and deployment

`cargo check --workspace` exited 0 in 29.7 seconds. The only build-script note
was the expected statement that CUDA is not compiled. The canonical
`pwsh -File scripts/lint.ps1` gate exited 0 across both workspaces with warnings
fatal, formatting clean, the shared lint contract and toolchain pins aligned,
and all root and Calyx clippy targets clean.

The exact code commit was deployed through the supported setup path:

```text
pwsh -File scripts\synapse-setup.ps1 \
  -SourceDir C:\code\synapse -ForceRestart -EnableAudio \
  -AllowedPermissions READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE,READ_AUDIO \
  -ActiveIssue 2123 -SkipClientWiring

release_build_elapsed_s=704.755
setup_total_elapsed_s=823.801
cargo_cmake_jobs=12
installed_exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
installed_len=256645961
installed_sha256=3A13C3FF7B04EAB60FC330A4C4A0DB97292B0C563C5960999BB09833656DED10
build_commit=a903d72650407bc9b6fbc008ce60a6aae28b996e
build_tree_state=clean
build_profile=release
```

This host has an Intel Core i7-1355U (10 physical cores / 12 logical CPUs), 32
GB RAM, and Intel Iris Xe graphics with no NVIDIA/CUDA device. Setup used all 12
logical build workers. The installed daemon independently reports Calyx
`math_backend=cpu`, `calyx_math_cpu_simd_path=avx2`, a bit-for-bit math probe of
`ok`, zero inert tuning knobs, and zero row-guard over-budget/starved holds.
That is the fastest supported backend for the hardware actually present; no
machine-specific compile target or nonexistent GPU path was forced.

## Manual Full State Verification — `CF_KV`

The real public trigger was `act operation=invoke` routing
`target_act verb=run_shell` through its durable idempotency path. That path
calls `storage_kv_row` before executing or replaying a command.

Before all triggers, offline read-only vault inspection showed:

```text
CF_KV rows_in_cf=15513
prefix=m4/act_run_shell/idempotency/v1/ rows_with_prefix=1
happy_digest=3b974ac67042dec5774c50ab42ae4d3093e4396e64cba75bf237b8b1642915b7 rows_matched=0
max_digest=146ed9b290441c47b9d45cad6a791e40732d29a973f32a9bdba5f0bf1343d4ba rows_matched=0
invalid_digest=3b8d2ce381127155d42ff6fa3bc260c350f43e18e840c3ba305eab1afe28c703 rows_matched=0
empty_marker_exists=false
invalid_marker_exists=false
```

### Happy path and replay

The key was `synapse-issue-2123-happy-a903d726`; the real PowerShell command
printed the known value `SYNAPSE_2123_HAPPY_EXACT_READ_OK`.

```text
first_call_exit=0
first_call_stdout=SYNAPSE_2123_HAPPY_EXACT_READ_OK
first_call_actual_duration_ms=233
after_CF_KV_rows=15515
after_prefix_rows=2
after_happy_digest_rows_matched=1
```

The exact same request was then replayed. It returned the stored response with
the same session id, duration, exit code, and stdout. Immediately surrounding
that replay, authenticated Calyx guard readback was:

```text
before: sequence=1303590 read_latest_holds=15245 scan_cf_latest_holds=0
after:  sequence=1303599 read_latest_holds=15264 scan_cf_latest_holds=0
```

Thus the exact-read site advanced while the full-CF scan site did not execute.
The sequence and total read deltas also contain the independently visible
health/usage bookkeeping occurring on the live daemon; the load-bearing
observation is that `scan_cf_latest` remained exactly zero.

### Boundary 1 — maximum accepted key

A key of exactly 256 UTF-8 bytes (`M` repeated 256 times) ran a real command
that printed `SYNAPSE_2123_MAX_256_OK`:

```text
before: sequence=1303601 read_latest_holds=16191 scan_cf_latest_holds=0
trigger: exit=0 stdout=SYNAPSE_2123_MAX_256_OK actual_duration_ms=211
after:  sequence=1303612 read_latest_holds=16211 scan_cf_latest_holds=0
after_CF_KV_rows=15522
after_prefix_rows=3
after_max_digest_rows_matched=1
```

### Boundary 2 — one byte over maximum

A 257-byte key was paired with a command that would create
`%TEMP%\synapse-2123-invalid-marker.txt` if launched. The action failed closed:

```text
error_code=TOOL_PARAMS_INVALID
error=act_run_shell idempotency_key must be <= 256 bytes
before: sequence=1303614 read_latest_holds=17482 scan_cf_latest_holds=0
after:  sequence=1303623 read_latest_holds=17500 scan_cf_latest_holds=0
invalid_digest_rows_matched_before=0
invalid_digest_rows_matched_after=0
invalid_marker_exists_before=false
invalid_marker_exists_after=false
```

Both independent outcomes prove validation occurred before process launch or
durable idempotency storage.

### Boundary 3 — empty key

An empty key was paired with a different command that would create
`%TEMP%\synapse-2123-empty-marker.txt` if launched:

```text
error_code=TOOL_PARAMS_INVALID
error=act_run_shell idempotency_key must not be empty
before: sequence=1303625 read_latest_holds=18077 scan_cf_latest_holds=0
after:  sequence=1303634 read_latest_holds=18095 scan_cf_latest_holds=0
empty_marker_exists_before=false
empty_marker_exists_after=false
```

### Physical final row evidence

The offline read-only diagnostic correlated the logical keys by SHA-256 and
reported their actual persisted value metadata:

```text
row[3887] key_len_bytes=169
key_sha256=sha256:ed59333895e2a3c987392ff4ce9ef9da2261e27ad39bd5a8a9eacd8bf579d725
value_len_bytes=1129
value_sha256=sha256:8842e9ffc37ff6cfca2d65cd5edee3c71138fc57299cae22ae67d78115fcb786
value_encoding=json

row[3888] key_len_bytes=169
key_sha256=sha256:8f352291d6ba04d35d5f3c3a2f6760ce67cd5da2b03d76dcd58e114b56a43855
value_len_bytes=1147
value_sha256=sha256:0b2dff6b854dd19ede36b2dc07fb8838e31a9d305c7f18166043e52887fe528d
value_encoding=json
```

The final live-vault census later read 15,543 `CF_KV` rows, exactly three rows
under the idempotency prefix, one happy digest match, one maximum-key digest
match, zero invalid digest matches, and both marker files absent. The changing
overall count is normal live-daemon activity; the exact prefix and digest
counts are the acceptance state.

## Manual Full State Verification — `CF_PROFILES`

The profile-registry query is intentionally hidden from every session-scoped
public profile. To execute the actual installed method without inventing a
facade or a test harness, the supported setup lifecycle stopped the supervised
daemon, the same installed executable opened the same production vault in its
documented unscoped stdio-admin mode, and setup then restored supervision.

Physical state before the trigger was:

```text
CF_PROFILES rows_in_cf=0
prefix=profile_registry/v1/ rows_with_prefix=0
```

The empty family made an absent exact row the smallest input with a known
physical outcome. The final controlled run was:

```text
after_stop: daemon_count=0 task_state=Disabled
stdio_pid=22232
stdio_server=synapse-mcp 0.1.0
stdio_tool_count=242
profile_registry_query_advertised=true
trigger.view=inspect
trigger.row_key=profile_registry/v1/fsv/issue-2123-absent-a903d726
result.cf_name=CF_PROFILES
result.row_key=profile_registry/v1/fsv/issue-2123-absent-a903d726
result.found=false
result.row=null
before: read_latest_holds=13 scan_cf_latest_holds=0 over_budget=0 starved=0
after:  read_latest_holds=20 scan_cf_latest_holds=0 over_budget=0 starved=0
stdio_exit=0
```

The stdio process reached `ending_reason=stdio_service_completed` and
`ending_phase=calyx_vault_close`. Setup's independent next boot readback
reported `previous_shutdown=clean`. Offline physical inspection after the
trigger still showed:

```text
CF_PROFILES rows_in_cf=0
prefix=profile_registry/v1/fsv/issue-2123-absent-a903d726 rows_matched=0
```

That matches `found=false`, proves the read did not manufacture state, and
proves the old full-scan primitive never ran.

## Final installed state

After all manual triggers and controlled lifecycle transitions:

```text
health_ok=true
daemon_pid=2044
daemon_build=a903d7265040
daemon_tool_count=40
daemon_tool_surface_sha256=19141c12655dd85c5931f24c41ad7065c8fa49bea61c49b9d680c971c8394ffd
installed_sha256=3A13C3FF7B04EAB60FC330A4C4A0DB97292B0C563C5960999BB09833656DED10
scheduled_task_state=Running
supervisor_stop_request_exists=false
foreground_input_lease_held=false
chrome_bridge_status=ok
chrome_bridge_host_count=1
chrome_bridge_extension_stale=false
calyx_vault_id=01KYJPGWATPD4XNMZY3ERGTKQW
calyx_row_guard_over_budget_total=0
calyx_row_guard_starved_total=0
```

No automated test, CI job, mock data, branch, worktree, alternate target
directory, fallback, or new FSV harness was created or run. The two rejected
marker files were never created. The two accepted idempotency records are real
durable production evidence and remain subject to the product's normal storage
retention policy.
