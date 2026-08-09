# Issue #2165 — canonical terminal-error telemetry

Date: 2026-08-09 (America/Chicago)

Code commit verified: `af61ea4715b4e5cfdf15553db4fe33bb11b6b5d5`

## Acceptance and Sources of Truth

The acceptance Sources of Truth were:

1. the active lifecycle ledger and its retained rotations at
   `%LOCALAPPDATA%\synapse\db-daemon\daemon-tool-events.jsonl{,.1,.2}`;
2. a separate live `telemetry operation=status` read of those physical files;
3. the installed executable, OS process table, live `health` build provenance,
   and `%APPDATA%\synapse\codex-tool-surface.json`;
4. one disposable real stdio daemon and physical ledger under `%TEMP%` for
   malformed and maximum-boundary rows that must never enter production.

Each real trigger was followed by an independent disk read. Return values were
not accepted as proof. The disposable daemon used the installed production
binary and real Calyx vault, not a test, mock, harness, or alternate Cargo
target. Its exact root was removed after the process exited and both daemon and
vault PID sidecars were absent.

## Root cause

The writer and reader had independently evolved incompatible JSON contracts.
`server/handler.rs::error_snapshot` persisted canonical values at
`error.synapse_code` and `error.data.code`, while
`daemon_lifecycle.rs::recent_tool_usage` still read only `error.code` and
`error.detail_code`.

The initial physical audit found 4,156 terminal error rows across three retained
segments. All 4,156 carried both current canonical paths; zero carried either
path read by telemetry. Live telemetry scanned 10,000 rows but therefore
reported `latest_error_code=null`, including an aggregate with 1,421 storage
errors. This was writer/reader schema drift, not missing error production.

A first v2 implementation exposed a second root cause during manual FSV. It
added the typed projection but serialized the enclosing raw RMCP error too, so
the rejected synthetic operation appeared at active-ledger line 2036. Immediate
response state and durable telemetry state still shared one representation.
Acceptance stopped. The final v3 writer now clones a persistence-only event,
removes the raw error, and writes only the typed projection. The append-only v2
row remains explicitly decoded as compatibility evidence; no v3 row permits a
raw error.

## Independent research after diagnosis

`pwsh -File scripts/check-research-lane.ps1` performed real Exa MCP
`initialize`, `tools/list`, and `tools/call` requests and reported Exa MCP 3.4.0
`live`. Exa was used as the supplemental lane. The built-in web lane read the
primary OpenTelemetry specifications:

- <https://opentelemetry.io/docs/specs/semconv/general/recording-errors/>
- <https://opentelemetry.io/docs/specs/semconv/registry/attributes/error/>
- <https://opentelemetry.io/docs/specs/otel/schemas/>
- <https://opentelemetry.io/docs/specs/semconv/general/events/>

The applied guidance was to use a predictable low-cardinality error type, keep
that identifier consistent across events and aggregates, version telemetry
schemas with explicit consumer transforms, and exclude sensitive or
high-cardinality payloads from durable aggregation.

## Implemented behavior

- Tool-event schema v3 persists a shared typed terminal-error projection:
  schema version, facade, normalized operation, route, status, canonical code,
  duration, profile, and tool-surface SHA-256.
- The immediate MCP response retains its full structured error, while the
  durable lifecycle clone removes the entire raw error before append.
- Both the v3 write boundary and v3 reader reject any raw error payload.
- Canonical error codes are 1..128 bytes, uppercase ASCII/digit/underscore,
  and all documented paths must agree. Human-readable messages are never
  parsed.
- Facade, operation, route, and profile fields have explicit byte/character
  contracts. Tool-surface identity must be exactly
  `sha256:<64 lowercase hex digits>`; malformed or unbounded values are
  diagnostic errors rather than new metric dimensions.
- Schema v1 and v2 have explicit compatibility decoders. Unknown versions,
  malformed JSON, missing projections, mismatched fields, and malformed hashes
  increment a bounded visible decode-error counter carrying segment and line.
- Telemetry reports canonical/compatibility row counts, decode counters, exact
  per facade/operation/code counts, distinct-code counts, and explicit
  truncation flags. A nullable latest code is no longer the only evidence.
- Operation normalization is contract-derived. Invalid operations use the
  bounded `invalid` bucket and unknown tools use `unknown`; arbitrary rejected
  input cannot become a metric dimension.
- Audit replay prefers the canonical terminal projection for new rows.

## Compile and lint

The exact final source passed:

```text
cargo check --workspace
Finished dev profile; exit 0

pwsh -File scripts/lint.ps1
Gate 0 zero-test/zero-harness doctrine — OK
Gate 0b production surface invariants — OK
Gate 1 shared lint contract — OK
Gate 2 toolchain 1.97.1 agreement — OK
Gate 3 lock graph — OK
Gate 4 fmt, root and calyx — OK
Gate 5 cargo deny, root and calyx — OK
Gate 6 clippy --workspace --all-targets -D warnings, root and calyx — OK
Gate 7 public Calyx API ratchet — OK
LINT OK
```

The first full lint run found only the formatter's required line wrapping in
the new SHA predicate. `scripts/lint.ps1 -Fix -SkipClippy` applied that
mechanical format, and the full gate above was rerun. No lint was suppressed.

## Final deployment and hardware path

Setup built and candidate-validated the clean `main` checkout with the
canonical target directory. It used `CARGO_BUILD_JOBS=12` and
`CMAKE_BUILD_PARALLEL_LEVEL=12`, verified every registered model bundle, and
installed the final release artifact. Its final nonzero verdict was solely the
expected `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` for the already-running
Codex client; daemon handoff, bridge recovery, and fresh snapshot publication
had completed first.

```text
commit=af61ea4715b4e5cfdf15553db4fe33bb11b6b5d5
installed_exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
installed_bytes=256731977
installed_sha256=3039EB567278DFA8B6990F215392D7CE065250A034DB9159614CFB546076CCA4
daemon_pid=20684
build_profile=release
build_tree_state=clean
tool_count=40
tool_surface_sha256=80c43d2b9c6f43587901eab53ac544569816a6cda5b9fbcb0428e0235145c573
```

Host and live runtime proof:

```text
CPU=13th Gen Intel Core i7-1355U
physical_cores=10
logical_processors=12
physical_memory_bytes=33854390272
GPU=Intel Iris Xe Graphics
CUDA/NVIDIA device count=0
calyx_math_backend=cpu
calyx_math_cpu_simd_path=avx2
calyx_math_probe_status=ok
execution_speed_throttling_disabled=true
calyx_inert_tuning_knob_count=0
calyx_row_guard_over_budget_total=0
calyx_row_guard_starved_total=0
```

The host device probe proved there is no CUDA device, so CPU AVX2 is the fastest
supported backend on this hardware. Selection and its reason are explicit in
health; there is no silent accelerator degradation. Live health independently
reported `ok=true`, build `af61ea4715b4`, and healthy storage, vault, and Chrome
bridge subsystems. Audio and the existing permission grant were preserved in
the live command line.

## Production FSV — final binary

Before the final four triggers, the active ledger was:

```text
lines=2090
bytes=3181170
live_pid=20684
```

### 1. Known invalid operation

Trigger: real `telemetry` call with the unique invalid operation
`AF61_FINAL_PRIVATE_TOKEN_2165_MUST_NOT_PERSIST`.

The call returned `TOOL_PARAMS_INVALID`. The independent disk read found two
new PID 20684 rows at sequence 4:

```text
started: schema=3 tool=telemetry operation=invalid route=telemetry.invalid
error:   schema=3 code=TOOL_PARAMS_INVALID duration_ms=1
raw_error_present=false
private_token_disk_matches=0
```

The later independent telemetry read reported `telemetry.invalid` with
`error_total=4` and exact code count `TOOL_PARAMS_INVALID=4`.

### 2. Valid omitted optional input

Trigger: real `telemetry` call with `{ operation: "status" }` and no `status`
payload. It succeeded. The two appended PID 20684 sequence-5 rows were exactly
`started` and `ok`; neither carried `error` nor `terminal_error`.

Immediately before that valid call's completion telemetry reported canonical
count 7, unchanged from the preceding invalid call. The physical rows prove the
valid omission created no new error code.

### 3. Structurally invalid payload

Trigger: `telemetry operation=status` with an unexpected field whose value was
`AF61_STRUCTURAL_PRIVATE_TOKEN_2165_MUST_NOT_PERSIST`.

The typed facade rejected it with `TOOL_PARAMS_INVALID`, an accepted-field list,
and remediation. The independent disk read found PID 20684 sequence 6,
operation `status`, route `telemetry.status`, and canonical code
`TOOL_PARAMS_INVALID`; the raw error was absent and the unique value had zero
disk matches.

### 4. Long real canonical code

Trigger: real `storage operation=corpus_histogram` against
`CF_AGENT_EVENTS`, with one deliberately unknown dimension and the smallest
`max_rows=1`, `max_buckets=1` bounds.

The public operation returned
`SYNAPSE_STORAGE_CORPUS_HISTOGRAM_DIMENSION_UNKNOWN`. The independent physical
row at PID 20684 sequence 7 stored all 50 code bytes without truncation,
operation `corpus_histogram`, route `storage.corpus_histogram`, duration 1 ms,
and no raw error. The unique rejected dimension had zero disk matches.

After these four triggers the active file was exactly 2,090 + 8 = 2,098 rows.
The eight rows were the expected started/terminal pair for each call. A separate
telemetry call then reported:

```text
rows_scanned=10000
segment_count=3
terminal_error_projection_schema_version=1
canonical_terminal_error_rows=9
compatibility_terminal_error_rows=1741
decode_error_total=0
read_error=null
telemetry.invalid / TOOL_PARAMS_INVALID=4
telemetry.status / TOOL_PARAMS_INVALID=3
storage.corpus_histogram / SYNAPSE_STORAGE_CORPUS_HISTOGRAM_DIMENSION_UNKNOWN=4
```

The final independent physical read after that successful telemetry call was:

```text
path=%LOCALAPPDATA%\synapse\db-daemon\daemon-tool-events.jsonl
lines=2100
bytes=3189585
sha256=0E17FCE36D68611DB1BEA5AEF30D7759C9F42D364DAD1292F5AFC4CF8C3BA6FD
v3_terminal_rows=9
v3_terminal_rows_with_raw_error=0
all three final private-token disk matches=0
```

## Retained-history proof

Physical schema-v1 rows 2000 and 2002 in the active segment are real retained
`audit.command_query` errors carrying `error.synapse_code=TOOL_PARAMS_INVALID`.
Line 2002's raw-line SHA-256 is
`DBF88AB17D8BC4E63C40E28CD5642938F706C187D673AB4BB5804973E7516F3D`.
The live compatibility decoder counted 1,741 retained terminal rows, and the
matching `audit.command_query` aggregate reported exactly three
`TOOL_PARAMS_INVALID` errors. This proves old `synapse_code` data is read through
the explicit decoder rather than silently converted to null.

## Disposable maximum/malformed decoder FSV

The exact installed binary opened a disposable real vault under `%TEMP%` and
read a four-line physical lifecycle ledger:

1. one valid v3 terminal projection with maximum accepted fields: 128-byte
   code, 64-byte facade, 64-byte operation, 129-byte route, 32-byte profile, and
   canonical 71-byte tool-surface SHA-256;
2. one truncated JSON object;
3. one v3 terminal error missing its projection;
4. one projection with a 70-byte, 63-hex-digit tool-surface identifier.

The source file before trigger had SHA-256
`08DC3676267EA21ED37C73D65BA9E7117F0AB508D3A16AD0AB661B1B572A9C61`.
A real stdio MCP handshake and `telemetry operation=status` returned:

```text
process_id=8956
exit_code=0
rows_scanned=6
canonical_terminal_error_rows=1
compatibility_terminal_error_rows=0
decode_error_total=3

line 2 MCP_TOOL_USAGE_JSON_INVALID
line 3 MCP_TOOL_USAGE_TERMINAL_ERROR_PROJECTION_MISSING
       detail="v3 status=error event has no terminal_error projection"
line 4 MCP_TOOL_USAGE_TOOL_SURFACE_SHA256_INVALID
       detail="byte_length=70 required=71"
```

The maximum-boundary aggregate retained the exact 128 `A` code bytes, exact
maximum facade/operation/route/profile fields, exact canonical hash, one call,
one error, and `duration_ms=999`. Thus the accepted boundary is lossless while
out-of-contract rows are visible and excluded.

After graceful exit, both daemon and vault PID sidecars were absent and no
matching process remained. The disposable root then contained 88 files and
122,393 bytes including its intentional sibling machine salt and lineage
journal. The exact verified root was recursively deleted; a separate read
reported `root_exists_after=false`. The production ledger was never modified by
the malformed probe, and the repository returned to a clean state.

## Verdict

The writer and reader now share one bounded, versioned terminal-error contract.
New production failures persist actionable canonical codes without arguments or
raw errors, retained schemas are counted explicitly, malformed data is visible
with physical location, and live aggregates agree with the bytes on disk. Issue
#2165 passes manual Full State Verification against the final installed release.
