# Issue #2245 — bounded Calyx read admission and allocation-reusing CF walks

Date: 2026-08-15

Commits under verification:

- `f1390a6b250beaf451dd855e2c4aaffcdaca05f9`
- `efbda2c5db877250c9548a3faa90dd88a95124c1`

Method: manual FSV only. No test, benchmark, harness, FSV driver, script, or CI job was used as behavioral evidence.

## Source of Truth

- Runtime identity: `synapse-mcp.exe` process, executable hash, command line,
  and listening socket at `127.0.0.1:7700`.
- Production triggers: strict Codex MCP `health`, `storage
  operation=intelligence`, and `storage operation=panel_coverage` calls.
- Admission/cadence state: `STORAGE_MAINTENANCE_*`,
  `STORAGE_BOUNDED_READ_*`, and derived-state tick records in the daemon log.
- Physical coverage state: Calyx Base/source-CF rows plus the panel-coverage
  response accounting equality.
- Memory state: Windows process `PrivateMemorySize64` and sampled
  `SYNAPSE_CALYX_CF_WALK_MEMORY_RELEASED` fields read independently after the
  MCP trigger.
- Search/kernel state: immutable manifests, filter bytes, and Kernel SSTs under
  `C:\Users\hotra\AppData\Local\synapse\db-daemon`.

Tool returns are not the verdict by themselves. Acceptance requires the
separate process/socket/log/file reads below.

## Root causes and invariants

### Foreground starvation and cadence debt (`f1390a6b`)

The old daemon serialized all Calyx work through one whole-corpus permit. One
scheduled pass ran for 471,131 ms; a bounded abundance read waited 355,696 ms,
exceeded the 300-second MCP client boundary, and still executed later because a
started Tokio blocking task cannot be aborted. The completed interval was
already overdue, so the next derived pass entered only 2,847 ms later.

The fix keeps exactly one whole-corpus owner but adds one separate permit only
for statically resident-bounded, read-only operations (`abundance` and
`kernel_answer`). It resets the derived timer after terminal completion, so
cadence is measured from the end of real work rather than from stale interval
debt. OLAP and every mutating/materializing operation remain on the exclusive
lane.

### Repeated page ownership (`efbda2c5`)

After the admission fix, a real scheduled Base coverage pass still reached
1,122,111,488 private bytes. It walked 2,300,021 rows in 143,752 16-row pages.
Aster's immutable cursor already owned allocation-reusing source buffers, but
`scan_cf_range_pages_at` converted every small `Vec<SstEntry>` into another
owned tuple `Vec` before Synapse visited it. Mid-cursor allocator collection
reclaimed only 458,752 bytes; lowering a threshold could not remove the
allocation churn.

The fix exposes a typed Aster borrowing walk over the existing row cursor. It
preserves one pinned snapshot, the atomic rows-to-router hand-off, the bounded
MVCC overlay, lease checks every 256 rows, read barriers, typed visitor errors,
and early stop. No output page crosses the crate boundary. Synapse and Aster
independently count rows/early-stop and fail closed on disagreement. Logical
page counts remain provenance only. Base progress memory is sampled once per
65,536 rows and collection occurs after the cursor is destroyed.

Post-diagnosis research used both Exa and the native web connector. Primary
guidance supports one forward iterator over an explicit consistent snapshot and
explicit iterator lifetime; Rust distinguishes lazy iterator consumption from
`collect`, which creates a collection without a general allocation guarantee:

- <https://github.com/facebook/rocksdb/wiki/Iterator>
- <https://github.com/facebook/rocksdb/wiki/RocksDB-FAQ>
- <https://github.com/facebook/rocksdb/wiki/RocksDB-Overview>
- <https://github.com/facebook/rocksdb/wiki/Snapshot>
- <https://doc.rust-lang.org/std/iter/>
- <https://doc.rust-lang.org/std/iter/trait.FromIterator.html>
- <https://learn.microsoft.com/en-us/windows/win32/memory/memory-performance-information>

## Installed runtime precondition

Separate host and strict MCP reads after installation showed:

```text
daemon PID:       37224
executable:       C:\Users\hotra\.cargo\bin\synapse-mcp.exe
command line:     --mode http --bind 127.0.0.1:7700 --db ...\db-daemon
socket:           127.0.0.1:7700 LISTEN, owning PID 37224
health build:     efbda2c5db87
checkout commit:  efbda2c5db877250c9548a3faa90dd88a95124c1
build tree:       clean, changed input count 0
advertised tools: 40
tool surface:     978be4deae7b176323fe6310d535bbb5b7e6af76d68c9779ceb7d5cb9b6e3946
installed SHA256: 108433A2B9D358EF00D41470ACFDD1621DA7ADABE1AA556F493C2A9BC037BD5E
```

The strict Codex client loaded the full sanitized surface and physically called
the real `health` and `storage` tools. Setup's unrelated Chrome bridge step
remained fail-closed at `SYNAPSE_CHROME_BRIDGE_ACTIVATION_PENDING` and did not
touch a human Chrome window.

## Happy path: strict MCP panel coverage

Before the real trigger:

```text
captured=2026-08-15T20:13:46.4123176Z
PID=37224
private bytes=799363072
working set=738463744
storage_panel_coverage admissions=0
storage_panel_coverage completions=0
```

Trigger: strict `mcp__synapse__storage` with
`operation=panel_coverage,panel_coverage={}`.

The response reported exact physical accounting:

```text
base_cf_rows=2311265
records_total=2311265
decode_failures=0
accounting_holds=true
panels=14
```

Independent log readback after the call:

```text
20:14:07.663 STORAGE_MAINTENANCE_ADMITTED operation=storage_panel_coverage
  admission_wait_ms=0 active_after_admission=1 max_concurrent=1

20:14:17.724 SYNAPSE_CALYX_CF_WALK_MEMORY_RELEASED cf=base
  rows_examined=2311265
  logical_groups=144455
  private_bytes_start=895119360
  private_bytes_peak=958324736
  private_bytes_before_release=941223936
  private_bytes_after_release=940699648
  progress_memory_samples=35

20:14:17.856 STORAGE_MAINTENANCE_COMPLETED operation=storage_panel_coverage
  exec_ms=10192 is_ok=true
```

The separately read Windows process state immediately afterward was
`PrivateMemorySize64=809754624`, `WorkingSet64=740601856`, PID unchanged. The
sampled physical peak was 115,417,088 bytes below 1 GiB and 163,786,752 bytes
below the old scan peak. The direct coverage operation completed in 10,192 ms
without sampling, truncation, or a RAM limit; the earlier 319,662 ms figure was
for an entire derived-state pass containing comparable coverage work, not for
the coverage phase alone.

## Unattended scheduled-pass readback

The same installed daemon then entered the real background derived-state path;
this was not a direct storage call or a helper-driven substitute. The daemon
log and Windows process state were read separately while it ran and again after
its blocking owner completed:

```text
20:18:02.261 STORAGE_MAINTENANCE_ADMITTED operation=storage_derived_state
  admission_wait_ms=28520 lane_occupied_at_request=true
  active_after_admission=1 max_concurrent=1

20:19:36.094 SYNAPSE_CALYX_CF_WALK_MEMORY_RELEASED cf=base
  rows_examined=2314731
  logical_groups=144671
  private_bytes_start=927674368
  private_bytes_peak=994258944
  private_bytes_before_release=973148160
  private_bytes_after_release=972558336
  progress_memory_samples=35

20:23:42.105 STORAGE_DERIVED_STATE_TICK_COMPLETED
  tick_failed=false subpass_failures=0 success_total=1 failure_total=0

20:23:42.150 STORAGE_MAINTENANCE_COMPLETED operation=storage_derived_state
  exec_ms=339888 admission_wait_ms=28520 is_ok=true

20:24:14.833 separate Windows process read
  PID=37224 private_bytes=859643904 working_set=256393216
```

Five separate Windows reads during the pass observed private-byte values of
929,865,728; 878,882,816; 942,813,184; 864,268,288; and 871,096,320. The Base
walk's internal Windows sample peaked at 994,258,944, leaving 79,482,880 bytes
below 1 GiB. The owner completed cleanly and the PID/socket remained stable.

## Invalid boundary audit

The admission/completion counts below come from separate daemon-log reads, not
from the error return.

### Edge 1 — unknown nested field

```text
before: admissions=1 completions=1 private_bytes=806051840
trigger: panel_coverage={unexpected_probe:true}
error:   TOOL_PARAMS_INVALID, serde deny_unknown_fields
after:  admissions=1 completions=1 private_bytes=799395840
```

### Edge 2 — missing matching payload

```text
before: admissions=1 completions=1 private_bytes=803876864
trigger: operation=panel_coverage with no panel_coverage object
error:   TOOL_PARAMS_INVALID, matching_payload_present=false
after:  admissions=1 completions=1 private_bytes=812642304
```

### Edge 3 — structurally mismatched extra payload

```text
before: admissions=1 completions=1 private_bytes=805974016
trigger: panel_coverage={} plus summary={}
error:   TOOL_PARAMS_INVALID, extra_payloads=[summary]
after:  admissions=1 completions=1 private_bytes=802201600
```

All three invalid shapes failed before entering the whole-corpus lane.

## Bounded reads during whole-corpus maintenance

On daemon PID 68084 / build `f1390a6b250b`, a derived pass held the exclusive
lane while strict MCP abundance reads used the separate bounded lane:

```text
max_records=1:   admission wait=0; honest SYNAPSE_CALYX_STALE_DERIVED while
                 the replacement generation was still unpublished; success
                 after physical manifest publication
max_records=256: admission wait=0; execution=394 ms; client=453 ms
result:          constellations=256 lenses=9 XTerm=310617 Graph=7764766
count provenance: maintained_exact
```

The replacement search manifest was independently read at
`idx\search\panel_0001963001\manifest.json`: Base sequence 4,105,291, 344,514
filter rows, 8 slots, SHA-256
`DAF012DAF84B15F1B4C4145BB6D11DDF050753C0478C10875958B2DCB51B775A`.

Invalid abundance inputs (`max_records=0`, `max_records=20001`, and unknown
`unexpected_probe`) each returned `TOOL_PARAMS_INVALID` with zero bounded-lane
admissions.

## Completion-relative cadence

The first live derived pass completed at `19:38:35.936527Z` after 319,662 ms.
No immediate pass followed. The next pass was admitted at
`19:43:35.947791Z`, 300,011 ms after completion. At `19:43:16.605Z`, a
separate log read still showed exactly one admission and one completion. This
proves `reset_after(cadence)` discarded the overdue tick instead of carrying
interval debt forward.

## Lodestar kernel-answer reality check

The same strict client exercised the bounded Lodestar route:

- exact-domain hygiene built kernel `1a92bd9a2b2790f202cb8505b1c06001`,
  1,249 members, 1,269 graph nodes, recall `1.0` against minimum `0.95`;
- unmeasured query `0164248e...` failed honestly with
  `SYNAPSE_CALYX_KERNEL_QUERY_UNMEASURED`;
- grounded query `11b6a3b7fec9cf86a36dd048c762103d` returned score `1`, zero hops;
- maximum-boundary `max_hops=64` query
  `00055475fa159bd221fac6a7e9be3c7b` found a grounded six-hop path to
  `220b8a4617aed5d822917b6347b78cf4` in 2.207 s.

Independent filter-file readback proved every returned path identity existed
and the anchor root carried the exact domain. The manifest SHA-256 was
`FA019EA22C74929B2BAA5E806956E5B6C123AF055C4C11716F3996FB4CF0DF70`;
the filter SHA-256 was
`C51E30398888FF16F65DB7A02E338DBBF4A2E208DDCCE1A391284BF7769AF640`.

## Structural checks and non-interference

Structural-only checks completed successfully across both workspaces:

```text
cargo fmt --all --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cd calyx; cargo fmt --all --check
cd calyx; cargo check --workspace --all-targets
cd calyx; cargo clippy --workspace --all-targets -- -D warnings
```

No automated tests or CI ran. Perception remained inactive; filesystem watch
queue capacity was 32 with queued/high-water/evicted all zero. CUDA remained
dormant. No foreground window, game, browser tab, or GPU reservation was used
by this verification.
