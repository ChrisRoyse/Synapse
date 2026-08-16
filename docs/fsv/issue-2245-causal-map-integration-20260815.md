# Issue #2245 — durable exhaustive causal-map integration

Date: 2026-08-15 (runtime evidence continued into 2026-08-16 UTC)

Commit under verification before this evidence document was added:

- `4298c81d27f43a9c7bc332f0abf0360fe5117162` — causal-map serving, autonomous maintenance, finite-membership repair, bounded whole-corpus admission, and native strict-CUDA discrete transfer entropy.

Method: manual FSV only. No test, benchmark, harness, driver, script, mock row, direct database write, or CI job was used as behavioral evidence. `cargo check`, format, and Clippy are structural evidence only. Every behavioral trigger below used the real strict-client `mcp__synapse__storage` tool. Shell reads were independent physical Source-of-Truth inspection only.

## Sources of Truth

- Installed runtime: process image `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, its exact PID/command line, and the TCP listener owner for `127.0.0.1:7700`.
- Strict MCP surface: authenticated `health`, initialized MCP session, and strict client discovery of all 40 sanitized tools, including `storage`.
- Source population: real retained `syn-agent-event-v1` constellations in panel `1965001`, partitioned by exact metadata `agent_event_kind` and bounded by source-event time.
- Durable causal state: a normalized-scope `GCMI1` pointer and immutable content-addressed `GCMP1` artifact in the native Calyx `Graph` CF.
- Independent serving read: `storage/intelligence/causal_map_read`, which follows and validates both Graph rows and separately reloads and fingerprints the closed source population without recomputing an estimator.
- Physical readback: Aster MVCC `snapshot_open` / `snapshot_read cf_name=Graph` / `snapshot_release`, including exact byte length and SHA-256.
- Admission state: durable daemon log records under `%LOCALAPPDATA%\synapse\logs\synapse.log.2026-08-16-04` plus before/after causal-pointer reads.
- Finite search membership: `idx\search\panel_0001965001\manifest.json` and its exact generation-scoped filter sidecar.

A tool return value alone is not a verdict. Acceptance below requires the independent pointer/artifact, filesystem, process/socket, snapshot, or durable-log read named for each claim.

## Root causes fixed

The causal estimators existed in pieces but were not one durable, complete, consumable intelligence capability. The reachable behavior either selected one temporal pair, silently depended on bounded-prefix membership, recomputed expensive evidence, or left consumers unable to distinguish incomplete estimator coverage from complete observational evidence. Manual FSV then exposed four deeper production defects:

1. A finite-only panel had no ANN slots, so its persisted search generation had no membership source and source-window enumeration could remain stale after vault recovery.
2. Scheduled whole-corpus work and foreground intelligence shared an unbounded wait path, so a valid MCP request could time out before its work was admitted, with no stable explanation of the active owner.
3. The strict-CUDA discrete transfer-entropy selector terminated in `CALYX_TE_DISCRETE_CUDA_UNSUPPORTED`; every temporal pair was typed but unresolved.
4. Setup reached the unrelated Chrome-bridge checkpoint before publishing the Codex MCP surface, leaving a newly installed daemon invisible to the configured strict client after an otherwise successful runtime handoff.

The resulting invariants are:

- A causal map is one immutable `GCMP1` artifact plus one revision-guarded `GCMI1` scope pointer. Content bytes include exact scope, source fingerprint/frontiers, stream census, all canonical pairs, every typed estimator lane, and four family-local Benjamini-Hochberg corrections.
- Neither pair named means exhaustive `C(n,2)`; both named means exactly one pair; one-sided selection fails closed.
- The entire closed source population must fit `max_records`, every record must carry the group key, and all canonical pairs must exist before publication.
- `causal_map_read` is read-only, validates both Graph rows, checks completeness, and re-fingerprints the physical source population. Missing, corrupt, future, cross-scope, or source-stale state is an error; it never recomputes or falls back.
- The nine declared Synapse temporal panels are scheduled autonomously on a six-hour rolling window; exact artifact keys, source fingerprints, actions, and failures are surfaced in health.
- Finite-only panels publish a membership-only zero-slot generation. Recovery-history gaps rebase from authoritative Base rows only for the exact stale/delta-history conditions; corrupt, wrong-panel, and future generations still fail closed.
- Foreground whole-corpus calls wait at most 1,000 ms. Failure is `STORAGE_MAINTENANCE_BUSY` and names the requested operation, active operation, active duration, wait budget, and `foreground_work_dispatched=false`. Background work waits fairly on the same single permit; overlap is impossible.
- Discrete transfer entropy stays discrete and strict-CUDA. Host code performs exact symbol interning and seeded bootstrap selection; the GPU performs every entropy estimate with dense integer histograms and Miller-Madow correction. There is no CPU retry and no KSG substitution.
- Observational estimator agreement remains `observational_predictive`; `structural_effect_identified=false` until a future identification contract supplies an intervention contrast and confounding assumptions.

## Research applied

Research was performed through Exa MCP and the native web tool. Causal assumptions and family-local multiplicity are bound in `docs/adr/2026-08-15-exhaustive-typed-causal-evidence-maps.md`, with primary references for PC-stable, transfer entropy, Granger, CCM, Hawkes, Benjamini-Hochberg, and causal identification.

The native CUDA repair additionally followed NVIDIA's primary guidance:

- CUDA Programming Guide, histogram/shared-memory/atomic patterns: <https://docs.nvidia.com/cuda/cuda-programming-guide/02-basics/writing-cuda-kernels.html>
- CUDA C++ Best Practices Guide, coalescing, shared memory, and memory budgeting: <https://docs.nvidia.com/cuda/cuda-c-best-practices-guide/>

That guidance is reflected in batch-private integer histograms, shared-memory rows through 8,192 bins, explicitly VRAM-budgeted disjoint global rows above that threshold, coalesced setup, fixed reductions, and scheduling-independent integer atomic counts.

Bounded admission follows Tokio's documented fair semaphore and timeout cancellation semantics:

- <https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html>
- <https://docs.rs/tokio/latest/tokio/time/fn.timeout.html>

## Installed runtime precondition

The standard setup path was run with the explicit repository source directory and built `synapse-mcp` in release with the `calyx-cuda` feature. Its later unrelated Chrome checkpoint returned `SYNAPSE_CHROME_BRIDGE_ACTIVATION_PENDING`; no Chrome window was touched and that checkpoint is not reported as setup success. Before the final documentation amend, separate host and MCP reads proved the installed causal runtime itself:

```text
daemon PID:             45260
parent PID:             18340
executable:             C:\Users\hotra\.cargo\bin\synapse-mcp.exe
bind/listener owner:    127.0.0.1:7700 / PID 45260
installed length:       262411593 bytes
installed SHA-256:      0E477ED19F04EB2517ED39769911B02F18788F331BEBE60288DA8DEAC4548596
health build:           4298c81d27f4
build commit/checkout:  4298c81d27f43a9c7bc332f0abf0360fe5117162
build profile/tree:     release / clean
changed build inputs:   0
build input manifest:   02a49895d660f5ae6dabad50af44301c041c40db14588e66a7aadf1ba761cc99
advertised tools:       40
tool surface SHA-256:   716ff94d0b510d610ab9f3c208bab7879e1b5b0482e16212954e8ec607431e3c
strict client:          health and storage loaded without schema error
```

The installed Codex surface JSON independently reported the same daemon PID, tool count, and surface hash. A final post-documentation installation/readback is recorded in the issue comment so its embedded build provenance names the final commit rather than this pre-documentation commit.

## Finite membership repair

Before the trigger, the physical panel-`1965001` generation was finite-only:

```text
old base_seq:           4123558
old member rows:        52384
old slots:              []
old filter SHA-256:     C002AA78... (superseded generation)
```

The installed repair rebased authoritative membership without inventing a vector slot:

```text
new base_seq:           4127037
new member rows:        52457
new slots:              []
filter sidecar:         filters_seq_00000000004127037_n_00000000000052457.jsonl
filter length:          19248685 bytes
filter SHA-256:         1B610AD90E609162BF175F8F254EC80196F86C7247A8FA99D3FCB3388C5DA690
manifest length:        658 bytes
manifest SHA-256:       2FEACF805C24D096A4B37952E34688288A99C7A3F4E046C5A0421B7932F658A0
old generation files:   absent after atomic publication
```

This proves the search generation contains exact membership while preserving the truthful zero-slot contract.

## Happy path: exhaustive durable map

### Before trigger

An independent snapshot read of the complete-scope pointer returned:

```text
snapshot lease:         1089 at seq 4128777
pointer key:            47434d4931001dfbc91167648483b29bd739bea5f8e0a3a710
physical/logical:       true / true
length:                 798 bytes
SHA-256:                db8b1378c0b72830b4d214e34476b8fa4f921b8ce75ae98c28bfc2e19df23f10
lease after release:    absent; active leases=0
```

### Real MCP trigger

The production strict-client trigger was:

```text
tool:                   mcp__synapse__storage
operation:              intelligence / causal_map
panel_version:          1965001
max_records:            20000
since_ts_ns:            1786832771000000000 inclusive
until_ts_ns:            1786854371000000000 exclusive
group_key:              agent_event_kind
group_a/group_b:        omitted => exhaustive C(n,2)
bin_seconds:            60
max_lag:                8
causal_fdr_alpha:       0.05
elapsed:                139319 ms
```

The trigger enumerated all real records and persisted/read back:

```text
source records:         274
source fingerprint:     d9f2165d03e015339c2f82b04e1842f7094a9059b1933ddd102b784df26d3e17
source event frontier:  1786833080402760700 .. 1786854332765000000
bins:                   355
streams:                9
expected/actual pairs:  36 / 36
all records loaded:     true
all pairs enumerated:   true
evidence class:         observational_predictive
structural identified:  false

pointer key:            47434d4931001dfbc91167648483b29bd739bea5f8e0a3a710
pointer SHA-256:         c0820d1872c265ef5000c85737849d74e6c3cbe7edad1a51091dc7252bf30305
pointer bytes:          798
artifact key:           47434d5031001dfbc9d07753038fa8ef71b0e2b7dff0f7ac43
artifact SHA-256:        d07753038fa8ef71b0e2b7dff0f7ac43e6547df53cab167f64987c7c00b271f4
artifact bytes:         665053
Graph CF rows after:    8148476
pointer readback:       true
artifact readback:      true
```

### Estimator and correction completeness

The immutable artifact contained one typed lane for every method rather than flattening or substituting results:

```text
transfer-entropy lanes: 36/36 measured
TE lags:                288/288 numerical estimates non-provisional and error-free
TE estimator:           discrete_plugin / auto_integral_samples
TE backend:             native strict-CUDA dense-histogram Miller-Madow kernel
lag-correlation lanes:  36/36 measured
Granger A->B:           35 measured, 1 typed refusal
Granger B->A:           35 measured, 1 typed refusal
CCM:                    6 measured, 30 typed assumption/data refusals
cross-K:                28 measured, 8 typed assumption/data refusals
Granger BH family:      560 decisions, 87 significant
cross-correlation BH:   612 decisions, 47 significant
PC/partial BH families: 0 decisions because their parent lanes failed closed
```

TE estimates retain provisional causal trust because the source is observational; “non-provisional” above means the numerical estimator had enough finite samples and a complete strict-CUDA computation, not that a structural causal claim was identified.

PC-stable and partial correlation failed closed on constant/degenerate sparse series with `CALYX_ASSAY_DEGENERATE_INPUT`. Hawkes failed on tied timestamps with an exact order-invariant violation. The remaining Granger/CCM/cross-K refusals likewise preserve their actual data and method assumptions. No jitter, row drop, CPU fallback, estimator substitution, or fake structural conclusion was introduced to make these lanes green.

### Independent read and physical bytes

A later real `causal_map_read` independently reloaded the 274 source rows and reconfirmed the source fingerprint, 9 streams, 36 canonical pairs, 36 native-CUDA TE lanes, and both Graph hashes. A separate Aster snapshot then proved the actual bytes:

```text
snapshot lease:         1661 at seq 4128834
pointer present:        physical=true logical=true length=798
pointer SHA-256:         c0820d1872c265ef5000c85737849d74e6c3cbe7edad1a51091dc7252bf30305
artifact present:       physical=true logical=true length=665053
artifact SHA-256:        d07753038fa8ef71b0e2b7dff0f7ac43e6547df53cab167f64987c7c00b271f4
report/hash agreement:  true / true
lease after release:    absent; active leases=0
```

## Boundary and edge-case audit

Each case below used a synthetic parameter defect whose expected result was a named fail-closed error. Before and after each real MCP call, a separate snapshot read returned the same pointer length `798`, SHA-256 `c0820d...0305`, and `active_leases=0`; no rejected call published a new pointer.

### 1. Empty group key

```text
before snapshot seq:    4128841
trigger:                group_key=""
actual:                 SYNAPSE_CALYX_CAUSAL_MAP_GROUP_KEY_REQUIRED
remediation:            supply a non-empty metadata key
elapsed:                201 ms
after snapshot seq:     4128845
after pointer SHA-256:  c0820d...0305 (unchanged)
```

### 2. Lag boundary below minimum

```text
before snapshot seq:    4128848
trigger:                max_lag=0
actual:                 TOOL_PARAMS_INVALID
remediation:            max_lag must be in 1..=32
elapsed:                78 ms; rejected before storage dispatch
after snapshot seq:     4128852
after pointer SHA-256:  c0820d...0305 (unchanged)
```

### 3. Structurally incomplete pair scope

```text
before snapshot seq:    4128855
trigger:                group_a="exited", group_b omitted
actual:                 SYNAPSE_CALYX_CAUSAL_MAP_PAIR_SCOPE_INCOMPLETE
remediation:            supply both group values or neither
elapsed:                199 ms
after snapshot seq:     4128859
after pointer SHA-256:  c0820d...0305 (unchanged)
```

### 4. FDR boundary above valid open interval

```text
before snapshot seq:    4128863
trigger:                causal_fdr_alpha=1.0
actual:                 TOOL_PARAMS_INVALID
remediation:            alpha must be finite and strictly inside (0,1)
elapsed:                93 ms; rejected before storage dispatch
after snapshot seq:     4128867
after pointer SHA-256:  c0820d...0305 (unchanged)
```

### 5. Real whole-corpus contention

The pointer was independently read immediately before the trigger and returned `c0820d...0305`. While the scheduled `storage_derived_state` pass physically owned the single maintenance lane, the same valid production causal-map call was issued:

```text
trigger elapsed:               1029 ms
actual code:                   STORAGE_MAINTENANCE_BUSY
requested_operation:           storage_intelligence
active_operation:              storage_derived_state
active_for_ms:                 188236
wait_budget_ms:                1000
foreground_work_dispatched:    false
remediation:                   wait for the named completion record, then retry;
                               do not increase timeout or overlap whole-corpus work
```

The durable daemon log independently recorded the same fields at `2026-08-16T04:33:20.339924Z`. A separate post-trigger `causal_map_read` returned:

```text
pointer SHA-256:         c0820d1872c265ef5000c85737849d74e6c3cbe7edad1a51091dc7252bf30305
artifact SHA-256:        d07753038fa8ef71b0e2b7dff0f7ac43e6547df53cab167f64987c7c00b271f4
pointer bytes:          798
artifact bytes:         665053
both physical matches:  true
```

This proves bounded foreground admission, exact active-owner diagnosis, no dispatched work after timeout, no overlapping whole-corpus pass, and no state mutation on refusal.

## Structural verification (not FSV)

The following compile/lint checks were green before documentation was added:

```text
cargo check -p calyx-assay --features cuda
cargo clippy -p calyx-assay --features cuda --all-targets -- -D warnings
cargo check -p synapse-mcp --features calyx-cuda
cargo clippy -p synapse-mcp --features calyx-cuda --all-targets -- -D warnings
root cargo clippy --workspace --all-targets -- -D warnings
calyx cargo clippy --workspace --all-targets -- -D warnings
```

The exact pre-push format/Clippy gates are rerun after this file is committed. They prove compilation and static conformance only. The process/socket, real strict-client triggers, separate Graph reads, finite membership files, and durable admission log above are the manual behavioral evidence.

