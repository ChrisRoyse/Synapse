# Issue #2248 — checkpoint SST publication into the live router

Date: 2026-08-15

Commit under verification: `5fe87c71c1d9e0e9efaac7274887cef1038fe691`

Method: manual FSV only. No test, benchmark, harness, script, or CI job was used as behavioral evidence.

## Source of Truth

- Runtime identity: the `synapse-mcp.exe` process and the listening socket at `127.0.0.1:7700`.
- Production trigger: `audit` with `operation=verify_chain` through a fresh Codex MCP client which completed strict `tools/list` schema validation.
- Durable state: `CF_LEDGER`, `cf/raw_commitment/*.sst`, `CURRENT`, and `MANIFEST` under `C:\Users\hotra\AppData\Local\synapse\db-daemon`.
- Publication state: `CALYX_ASTER_CHECKPOINT_ROUTER_INSTALL_DONE` and `CALYX_ASTER_CHECKPOINT_MATERIALIZED_OFF_COMMIT_LOCK` records in `C:\Users\hotra\AppData\Local\synapse\logs\daemon-stderr-gen1-20260815134143.log`.

The verifier response is not the verdict by itself. Acceptance requires the separately read process/socket, log, manifest, and SST state below.

## Root cause and invariant

The defective startup loaded a live `CfRouter` version at durable sequence `4,098,195`, then recovery checkpointing materialized `cf/raw_commitment/00000000000004098231-0000.sst` for sequences `4,098,196..4,098,231`. The manifest advanced and the recovered MVCC delta was retired, but the already-constructed router version never installed that SST. The verifier therefore paired the seal for `4,098,196..4,098,230` with the next visible cohort beginning at `4,098,232`.

The fix makes every checkpoint publication return its exact `(ColumnFamily, SstSummary)` set and install that complete set into the current router version before manifest/replay-floor advancement. It covers paced checkpointing, locked checkpointing, startup recovery, and post-WAL reconciliation. Prepared SSTs are validated before all touched shards are locked in canonical order; replacement levels are built before the atomic swaps. Installation failures log `CALYX_ASTER_CHECKPOINT_ROUTER_INSTALL_FAILED`, refuse manifest advancement, and re-stage the exact reserved prefix. SST insertion is canonical newest-first rather than I/O-completion order, and sealed memtables remain visible until their replacement level validates.

Post-diagnosis Exa and independent native-web research both supported the same publication invariant: a flush creates a new current Version containing the SST, readers consume that Version, and WAL retirement follows durable SST publication. Primary sources:

- <https://github.com/facebook/rocksdb/wiki/How-we-keep-track-of-live-SST-files>
- <https://github.com/facebook/rocksdb/wiki/Write-Ahead-Log-%28WAL%29>
- <https://github.com/facebook/rocksdb/wiki/MANIFEST>
- <https://github.com/facebook/rocksdb/wiki/Track-WAL-in-MANIFEST>
- <https://github.com/facebook/rocksdb/blob/main/db/version_set.h>

## Installed runtime precondition

Separate host reads after installation showed:

```text
daemon PID:       21648
executable:       C:\Users\hotra\.cargo\bin\synapse-mcp.exe
command line:     --mode http --bind 127.0.0.1:7700 --db ...\db-daemon
socket:           127.0.0.1:7700 LISTEN, owning PID 21648
health build:     5fe87c71c1d9
checkout commit:  5fe87c71c1d9e0e9efaac7274887cef1038fe691
build tree:       clean, changed input count 0
advertised tools: 40
tool surface:     978be4deae7b176323fe6310d535bbb5b7e6af76d68c9779ceb7d5cb9b6e3946
installed SHA256: AED959C8107AEE48B25899807D07447966417678A4449DE054B9B69554442351
```

A fresh strict Codex session initialized MCP session `78addb95-fd52-4eae-bd23-9b62ce49795a`, loaded all 40 schemas, and physically called `health`, `profile`, and `audit`. A second fresh strict session, thread `01a006bf-70cc-7a20-ab3e-cf6eaa018931`, exercised the invalid requests and final valid audit below. The restarted root client also loaded the same 40-tool surface and called the real `audit` tool.

## Happy path: a newly published cohort stays visible without restart

Before trigger/readback:

```text
PID=21648
audit head=1470507
raw commitments=1203217
raw sealed=1203217
raw pending=0
sealed through commit seq=4103987
```

The same daemon then sealed a fresh five-commit cohort and published its checkpoint:

```text
CALYX_ASTER_RAW_COMMITMENT_COHORT_SEALED
  first_commit_seq=4103992 last_commit_seq=4104029
  commitment_count=5 ledger_seq=1470559

CALYX_ASTER_CHECKPOINT_ROUTER_INSTALL_DONE
  operation="periodic checkpoint"
  cfs=...,ledger,time_index,raw_commitment
  sst_files=24 elapsed_ms=2

CALYX_ASTER_CHECKPOINT_MATERIALIZED_OFF_COMMIT_LOCK
  base_durable_seq=4103991 first_seq=4103992 last_seq=4104046
  batches=55 rows=705 remaining_batches=0
```

After trigger, the fresh strict client separately read the Ledger:

```text
PID=21648 (unchanged; no restart)
audit head=1470563
raw commitments=1203222
raw sealed=1203222
raw pending=0
raw seals=53829
sealed through commit seq=4104029
raw commitments intact=true
verdict=intact
tip=76cda15f25ba15df6f747a4e4334fdf7ae7a5c92bf6500b02ef7f57ce4787c1f
```

Independent disk and manifest reads after that MCP call showed the actual bytes:

```text
cf/raw_commitment/00000000000004104046-0000.sst
  length=544
  SHA256=3780EDF04900A994F7E72C988A3FA0D0066C5A14D224638A14EBA3805FCF749B

CURRENT=manifest-00000000000000121408.json
MANIFEST manifest_seq=121408 durable_seq=4104046 derived_content_seq=4104045
MANIFEST length=29601
MANIFEST SHA256=52806ADAA85FF92689F69B601130426957C0FDAF428002EF2A22214F82184F2E
```

The proof is the association: the same PID installed an SST containing the sealed range, the manifest durably named sequence `4,104,046`, and a later strict MCP audit read every physical commitment in the named cohort and recomputed an intact seal without a daemon restart.

## Edge case 1: startup recovery after router construction

Before:

```text
router baseline durable seq=4103341
staged recovery range=4103342..4103354
raw cohort=4103342..4103352, count=9
```

Trigger and after-state in the same startup process:

```text
18:42:13.125 CALYX_ASTER_CHECKPOINT_ROUTER_INSTALL_DONE
  operation="write-stall readiness preflight"
  cfs=...,ledger,registry,recurrence,graph,time_index,raw_commitment
  sst_files=20 elapsed_ms=3

18:42:13.179 CALYX_ASTER_CHECKPOINT_MATERIALIZED_OFF_COMMIT_LOCK
  base_durable_seq=4103341 first_seq=4103342 last_seq=4103354
  batches=13 rows=13555 remaining_batches=0 router_install_ms=4
```

The repaired startup path therefore publishes the exact recovery SST set into the live router before retiring the recovered range. The later full-chain MCP audits remained intact on the same PID.

## Edge case 2: exact 256-batch checkpoint boundary

Before: durable sequence `4,103,354`, 570 queued batches.

After each physical publication:

```text
base=4103354 -> first=4103355 last=4103610 batches=256 remaining=314
base=4103610 -> first=4103611 last=4103866 batches=256 remaining=58
base=4103866 -> first=4103867 last=4103924 batches=58  remaining=0
```

Each chunk logged `CALYX_ASTER_CHECKPOINT_ROUTER_INSTALL_DONE` before its materialization completion, included `raw_commitment`, and advanced continuously with no gap or overlap. The following strict full-chain audit returned `raw_commitments_intact=true`.

## Edge case 3: checkpoint with no raw commitments

Before:

```text
newest raw SST=00000000000004103952-0000.sst
raw commitments=1203209
raw sealed=1203209
raw pending=0
sealed through=4103950
```

Trigger:

```text
base_durable_seq=4103952 first_seq=4103953 last_seq=4103954
batches=2 rows=58
CALYX_ASTER_CHECKPOINT_ROUTER_INSTALL_DONE cfs=base,kv,slot_82..slot_92,slot_115,scalars,anchors,ledger,time_index
```

After: `raw_commitment` was correctly absent from the installed CF set, no raw SST or seal was invented for the empty cohort, and the next real cohorts advanced the raw counts normally. The subsequent strict read showed `1203217 == 1203217`, pending `0`, and an intact chain.

## Structurally invalid request edges

Before all three calls:

```text
head=1470507 raw=1203217 sealed=1203217 pending=0 sealed_through=4103987
```

Manual strict-client calls:

1. `operation=verify_chain` without `verify_chain` payload: MCP `-32602`, `TOOL_PARAMS_INVALID`, with the exact matching-payload remediation.
2. `verify_chain={from_seq:1470507,to_seq:1470506}`: MCP `-32099`, `SYNAPSE_CALYX_LEDGER_VERIFY_RANGE_INVALID`, with exact half-open-range remediation.
3. `operation=verify_chain`, `verify_chain={}`, plus mismatched `reproduce={}`: MCP `-32099`, `TOOL_PARAMS_INVALID`; no partial audit ran.

After the refusals, the valid call returned:

```text
head=1470563 raw=1203222 sealed=1203222 pending=0
sealed_through=4104029 verdict=intact
```

The ambient daemon legitimately appended its ordinary records between reads; all three invalid calls failed before executing a chain audit or mutating commitment/seal state.

## Restarted-client independent readback

After Codex itself restarted, the real wired `mcp__synapse__audit` tool performed another full read. It returned:

```text
PID=21648
head=1470590
raw commitments=1203272
raw sealed=1203222
raw pending=50
raw seals=53829
sealed through=4104029
raw commitments intact=true
verdict=intact
tip=c87693869f97534b456717897acb75b83879c4a1acc7ab6f9f1bde08d7970c36
```

The 50-row tail begins at `4,104,051`, beyond the last sealed checkpoint cohort, and is therefore correctly reported as pending rather than mispaired with the `4,103,992..4,104,029` seal.

One periodic checkpoint later, another real MCP audit and an immediately separate disk read closed that normal pending tail:

```text
MCP audit:
  PID=21648
  head=1470604
  raw commitments=1203281
  raw sealed=1203281
  raw pending=0
  raw seals=53830
  sealed through=4104148
  raw commitments intact=true
  verdict=intact
  tip=8b0134845c98b2614184d5f2fc56a21467d805b5e1377f9f379e695895659a3f

independent disk read:
  cf/raw_commitment/00000000000004104149-0000.sst
  length=5840
  SHA256=503DE492F0B4E4358BBE95DA5485F85A3D32F16A2BCAD3DFEA01A336CFCB67DE
  CURRENT=manifest-00000000000000121410.json
  manifest_seq=121410 durable_seq=4104150
  MANIFEST SHA256=C16152BBF26E2D49DBCD594C6B2738389B4DFB4E109B2FA293B71248425ACEFF
```

## Structural checks and resource/non-interference readback

Structural-only checks completed successfully:

```text
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cd calyx; cargo fmt --all --check
cd calyx; cargo check --workspace
cd calyx; cargo clippy --workspace --all-targets -- -D warnings
```

No automated tests or CI ran. Installed-daemon samples remained below 1 GiB without a memory cap: approximately 724 MiB working set / 799 MiB private bytes during the main run, and 498 MiB / 918 MiB after the final audit. CUDA remained physically dormant. The filesystem watcher had capacity 32, queued 0, evicted 0, high-water 0, and only Desktop/Documents/Downloads roots. No foreground window, game, browser tab, or GPU reservation was touched.
