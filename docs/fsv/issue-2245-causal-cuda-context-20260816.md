# Issue #2245 — causal CUDA context reuse manual FSV (2026-08-16)

## Scope and root cause

The exhaustive 20-stream / 190-pair action causal map previously completed its
blocking owner in `395895 ms`, after the production MCP client timed out at 300
seconds. Physical log timing showed every strict Calyx Assay entry point
constructing its own `CudaBackend`, which created a fresh CUDA context and fresh
module/function caches. Synapse's configured `VramBudgetedCudaBackend` remained
`dormant`; causal computation therefore bypassed the runtime lease that owns GPU
host reservation and lazy release.

Commit `dc5340353576548bd94cd0d0c790cbe7a42aae99` gives one synchronous
causal-map computation one configured math lease. Nested strict Assay backend
constructors reuse that exact thread-local context. A different nested context,
poisoned ownership, or scope-depth mismatch fails closed. The final lease still
destroys the CUDA context and releases the host reservation before Graph
publication. There is no process-global context, timeout increase, sampled pair
set, CPU fallback, or estimator substitution.

Primary NVIDIA guidance used for the lifetime decision:

- [CUDA Graphs](https://docs.nvidia.com/cuda/cuda-programming-guide/04-special-topics/cuda-graphs.html) explains why repeated workflow setup/launch overhead should be amortized.
- [Driver versus Runtime API](https://docs.nvidia.com/cuda/cuda-driver-api/driver-vs-runtime-api.html) documents context ownership and resource costs.
- [CUDA Lazy Loading](https://docs.nvidia.com/cuda/cuda-programming-guide/04-special-topics/lazy-loading.html) documents first-use module/kernel loading.

## Sources of Truth

- Runtime identity: Windows process table, TCP listener owner, and installed
  executable bytes.
- Strict client/tool surface: the wired `mcp__synapse` client's `health` and
  `profile/status` calls, including its persisted `CF_SESSIONS` policy row.
- Causal result: exact Graph pointer `GCMI1...` and immutable artifact `GCMP1...`
  rows read through a separately opened Aster MVCC snapshot.
- GPU ownership: durable daemon lifecycle events plus
  `C:\ProgramData\Calyx\gpu-reservations\device-0\reservations.json`.
- Agent consumption: the exact `CF_KV` steering-decision row returned by
  `agent/recommend_tools`, read through a separate Aster snapshot.

## Runtime precondition

The standard shipping setup built and handed off the committed release daemon,
then failed closed at the unrelated, already tracked Chrome bridge activation
checkpoint without touching human Chrome. Independent state was:

```text
daemon PID / parent:    44292 / 26892
command:                synapse-mcp.exe --mode http --bind 127.0.0.1:7700 ...
listener:               127.0.0.1:7700 owned by PID 44292
installed executable:   C:\Users\hotra\.cargo\bin\synapse-mcp.exe
bytes:                  263567177
SHA-256:                81793D7E4407A04FFBEDE8D0B472F3F81621EF1C942CF881D5407E62AD266B5F
health build / PID:     dc5340353576 / 44292
strict tool surface:    40 tools; missing/duplicate/forbidden = none
surface SHA-256:        2d3d79aa7cc0e296e2aef9de3fece15f5971cc28ac4868d0c376871be1c82b45
initial math state:     cuda / dormant
initial GPU ledger:     reservations=[]
```

Before the behavior trigger, snapshot lease `1245` at sequence `4198526`
independently read the normalized-scope Graph pointer:

```text
key:                    47434d49310021572a415cad90413aee704c3c47adfb5ccd5a
physical / logical:     true / true
bytes:                  793
SHA-256:                ba34a2ecc6c62304c3a6fd9dcf0726a4ebc4ec21f38c89f83629dadac8efbb20
lease after release:    active_lease_count=0
```

## Happy path — real strict MCP trigger

The trigger was the wired production `mcp__synapse__storage` tool:

```text
operation:              intelligence / causal_map
panel_version:          2185002
max_records:            20000
source window:          [1786918895909000000, 1786940495909000000)
group_key:              action_kind
group_a / group_b:      omitted => every C(n,2) pair
bin_seconds / max_lag:  60 / 8
BH FDR alpha:           0.05
strict-client elapsed:  18552 ms
estimator-scope log:    17384 ms
```

The call returned all admitted work, including failed/unresolved lanes rather
than hiding them:

```text
source records:         111
source fingerprint:     b15e1168c6a4cb662b2ac34b562b21913392825260fab260a85d2d9cb89e6525
event frontier:         1786922806899541800 .. 1786940024709528600
aligned bins / cells:   287 / 5740
streams / pairs:        20 / 190 (complete)
pair-lag points:        9500
PC CI-test upper bound: 375250
typed pair lanes:       903 measured / 225 failed / 12 unresolved
Granger BH decisions:   2905 (285 significant)
lag-correlation BH:     3230 (140 significant)
evidence class:         observational_predictive
structural identified:  false
```

The current source population was byte-identical to its existing immutable
generation, so the new computation reproduced the same content addresses. This
is the expected deterministic outcome, not a skipped computation: the durable
log records `SYNAPSE_CALYX_CAUSAL_MAP_ASSAY_SCOPE_STARTED`, completion after
17,384 ms, CUDA idle release, and final persisted/readback completion.

## Independent Graph and GPU readback

After the call, separate snapshot lease `1661` at sequence `4198562` read both
rows without trusting the tool return:

```text
pointer key:            47434d49310021572a415cad90413aee704c3c47adfb5ccd5a
pointer bytes / SHA:    793 / ba34a2ecc6c62304c3a6fd9dcf0726a4ebc4ec21f38c89f83629dadac8efbb20
artifact key:           47434d50310021572a37eb7be7f7363c8f86ab23398fbf5956
artifact bytes / SHA:   3515419 / 37eb7be7f7363c8f86ab23398fbf5956538376c6c488f5abf62dbd4b6c546cfd
physical / logical:     true / true for both
lease after release:    active_lease_count=0
```

The runtime log separately recorded:

```text
04:54:01.250Z  SYNAPSE_CALYX_CAUSAL_MAP_ASSAY_SCOPE_STARTED backend=cuda_budgeted pairs=190
04:54:18.635Z  SYNAPSE_CALYX_CAUSAL_MAP_ASSAY_SCOPE_COMPLETED elapsed_ms=17384
04:54:18.663Z  SYNAPSE_CALYX_MATH_IDLE_RELEASE_STARTED
04:54:18.711Z  SYNAPSE_CALYX_MATH_IDLE_RELEASE_SUCCEEDED reserved_mib=0 reservation_count=0
04:54:18.779Z  SYNAPSE_CALYX_CAUSAL_MAP_PERSISTED_AND_READ_BACK
```

Post-trigger health reported `calyx_math_probe_status=dormant_verified`. The
independently read host ledger SHA was
`7156764D5C758830736A10280FA37DBF366CDB8AD2A0D9A2E3C00F53DBB7F4E9`
and its physical `reservations` array was empty.

The same workload therefore improved from 395,895 ms to 18,552 ms at the MCP
boundary (21.34x) while preserving every association and reproducing the exact
artifact bytes.

## Boundary and edge-case audit

Every case below used a real strict MCP call. A separate Aster snapshot read the
pointer before and after each trigger. Every read returned `physical=true`,
`logical=true`, `bytes=793`, and SHA
`ba34a2ecc6c62304c3a6fd9dcf0726a4ebc4ec21f38c89f83629dadac8efbb20`.
Every snapshot lease was explicitly released to `active_lease_count=0`.

| Case | Before seq | Trigger and actual error | After seq | Pointer result |
|---|---:|---|---:|---|
| Empty atom name | 4198568 | `group_key=""` -> `TOOL_PARAMS_INVALID`; supply a nonblank key; no permission/storage/admission/compute acquired | 4198585 | unchanged |
| Lower lag boundary | 4198588 | `max_lag=0` -> `TOOL_PARAMS_INVALID`; valid range is `1..=32`; stopped before storage | 4198592 | unchanged |
| Structurally incomplete pair | 4198596 | `group_a="synthetic_one_side"`, no `group_b` -> `TOOL_PARAMS_INVALID`; provide both or neither; no corpus lane acquired | 4198605 | unchanged |
| Open FDR boundary | 4198608 | `causal_fdr_alpha=1.0` -> `TOOL_PARAMS_INVALID`; must be finite and inside `(0,1)`; stopped before storage | 4198614 | unchanged |

A fifth real call used the older six-hour window. It completed every estimator
in 20,554 ms but refused publication with
`SYNAPSE_CALYX_CAUSAL_MAP_POINTER_REGRESSION` because autonomous maintenance had
already advanced the serving source frontier. The subsequent typed read and
Graph snapshot showed the same current pointer/artifact, proving fail-closed
frontier protection without a workaround.

## Agent-tool integration and physical decision row

`mcp__synapse__agent operation=recommend_tools` for the synthetic task class
`issue-2245-causal-integration-fsv` consumed the real rolling MCP-usage map. No
task outcomes existed for that synthetic class, so recommendation confidence
truthfully remained `provisional_insufficient_evidence`; causal context was not
invented or suppressed:

```text
context status/schema:  physically_verified / synapse.steering.causal_map_context.v1
panel / group:          syn-mcp-usage-v1:1965007 / mcp_usage_tool
streams / pairs:        13 / 78 complete
artifact SHA-256:       48456b1864016fba0b77be6b65dfde3ff92cf4e95505fe7a48a1bb52f7361a63
pointer SHA-256:        80656ca8fe702f0d55994ceda9a45aeb48cffc00de12ddff5bd90c74c23a4f8e
evidence class:         observational_predictive
structural identified:  false
```

The call persisted decision row
`steering/v1/decision/tool/1786942599993168800/96e8cc141208cf8b5fbc95804133d6234493ff9a980f936b13ed2cbb4d705e77`.
Separate snapshot lease `5320` at sequence `4198633` found it physically and
logically present in `CF_KV`, 35,500 bytes, SHA
`7c5bdafa210bfba5166b391bbd6043d7a2805d1f3fba0076483089f0540c636f`,
matching the consumer's declared digest. The lease was released to zero.

## Structural checks (not FSV)

Before shipping, these non-behavioral gates were green:

```text
cargo check -p synapse-mcp --features calyx-cuda
cargo clippy -p synapse-mcp --features calyx-cuda -- -D warnings
root cargo fmt --all --check
root cargo clippy --workspace --all-targets -- -D warnings
calyx cargo fmt --all --check
calyx cargo clippy --workspace --all-targets -- -D warnings
```

They establish build/lint conformance only. The process/socket/tool-surface,
real MCP calls, Graph/CF_KV snapshot reads, lifecycle log, and GPU ledger are
the behavioral evidence.
